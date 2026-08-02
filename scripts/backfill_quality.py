#!/usr/bin/env python3
"""Backfill the Sony `Quality` makernote into photo_metadata for already-imported RAWs.

Photos scanned by the -fast2 metadata pass (before commit d66fa3c, 2026-07-17) never got
MakerNotes:Quality stored, so "Compressed (HQ) RAW" vs "Compressed RAW" (and lossless
"RAW") are indistinguishable in the catalog. This script reads just that tag from each
Sony RAW file and inserts the missing row, exactly as the fixed scanner now does.

Safe to run repeatedly: it only processes photos that have no Quality row yet, commits
as it goes, and can be interrupted/resumed at any point. Run it while ChairPhoto is
closed if possible (it tolerates a running app via a busy timeout, but a bulk import
happening at the same time would contend for the DB).

Usage:  python3 scripts/backfill_quality.py [--db PATH] [--workers N] [--dry-run]
"""

import argparse
import json
import re
import sqlite3
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

DEFAULT_DB = Path.home() / ".local/share/chairphoto/default.chairphoto"
# Sony formats only — `Quality` is a Sony makernote tag.
SONY_EXTENSIONS = (".arw", ".sr2", ".srf")
BATCH_SIZE = 150  # files per exiftool process, same as the app's scanner


def normalize_lookup(value: str) -> str:
    """Mirror catalog::tag_path::normalize_lookup: collapse whitespace, lowercase."""
    return re.sub(r"\s+", " ", value.strip()).lower()


def load_candidates(conn):
    """(photo_id, [candidate absolute paths]) for Sony RAWs missing a Quality row.

    Path resolution mirrors the app's resolver in spirit: try the catalog root +
    logical path first, then every registered volume location for the photo.
    """
    root = Path(
        conn.execute("SELECT value FROM settings WHERE key='catalog_root'").fetchone()[0]
    )
    volumes = {vid: Path(base) for vid, base in conn.execute("SELECT id, base_path FROM volumes")}

    rows = conn.execute(
        """
        SELECT p.id, p.path FROM photos p
        WHERE p.missing = 0
          AND NOT EXISTS (SELECT 1 FROM photo_metadata m
                          WHERE m.photo_id = p.id AND m.key = 'Quality')
        """
    ).fetchall()
    locations = {}
    for pid, vid, rel in conn.execute(
        "SELECT photo_id, volume_id, relative_path FROM photo_locations ORDER BY role"
    ):
        if vid in volumes:
            locations.setdefault(pid, []).append(volumes[vid] / rel)

    candidates = []
    for pid, rel_path in rows:
        if not rel_path.lower().endswith(SONY_EXTENSIONS):
            continue
        candidates.append((pid, [root / rel_path] + locations.get(pid, [])))
    return candidates


def resolve(paths):
    for p in paths:
        if p.is_file():
            return p
    return None


def read_quality_batch(files):
    """Run exiftool over a batch; return {absolute path string: quality string}."""
    cmd = ["exiftool", "-j", "-G", "-Quality", "--"] + [str(f) for f in files]
    try:
        out = subprocess.run(cmd, capture_output=True, timeout=600).stdout
        objects = json.loads(out) if out.strip() else []
    except (subprocess.TimeoutExpired, json.JSONDecodeError):
        return {}
    return {
        o["SourceFile"]: o["MakerNotes:Quality"]
        for o in objects
        if o.get("MakerNotes:Quality")
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", type=Path, default=DEFAULT_DB)
    ap.add_argument("--workers", type=int, default=6, help="parallel exiftool processes")
    ap.add_argument("--dry-run", action="store_true", help="report, but write nothing")
    args = ap.parse_args()

    if not args.db.is_file():
        sys.exit(f"catalog not found: {args.db}")

    conn = sqlite3.connect(args.db, timeout=30)
    conn.execute("PRAGMA busy_timeout = 30000")

    candidates = load_candidates(conn)
    print(f"{len(candidates)} Sony RAW photos missing Quality")

    resolved, unreachable = [], 0
    for pid, paths in candidates:
        p = resolve(paths)
        if p is None:
            unreachable += 1
        else:
            resolved.append((pid, p))
    if unreachable:
        print(f"{unreachable} photos unreachable (volume offline?) — skipped, rerun later")
    if args.dry_run:
        print(f"dry run: would read {len(resolved)} files")
        return

    batches = [resolved[i : i + BATCH_SIZE] for i in range(0, len(resolved), BATCH_SIZE)]
    written = no_tag = 0
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        for i, (batch, qualities) in enumerate(
            zip(batches, pool.map(lambda b: read_quality_batch([p for _, p in b]), batches))
        ):
            rows = []
            for pid, path in batch:
                q = qualities.get(str(path))
                if q:
                    rows.append((pid, "Quality", "MakerNotes", q, normalize_lookup(q)))
                else:
                    no_tag += 1
            if rows:
                conn.executemany(
                    "INSERT OR IGNORE INTO photo_metadata"
                    "(photo_id, key, group_name, value, value_norm) VALUES (?,?,?,?,?)",
                    rows,
                )
                conn.commit()  # per-batch commit → interrupt-safe, resumable
                written += len(rows)
            done = (i + 1) * BATCH_SIZE
            print(f"\r{min(done, len(resolved))}/{len(resolved)}  written={written}", end="", flush=True)

    print(f"\ndone: {written} Quality rows written, {no_tag} files had no Quality tag, "
          f"{unreachable} unreachable")


if __name__ == "__main__":
    main()
