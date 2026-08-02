//! kNN tag-suggestion engine for Smart Tagging (H7c).
//!
//! [`suggest_tags`] is the single entry point for a photo:
//!
//! 1. Load the query photo's CLIP embedding from `smarttags__embeddings`.
//! 2. Brute-force cosine scan over **all** rows in that table, keeping the top-K
//!    nearest neighbours.
//!    *(Fine to ~50 000 photos at 512 f32 each — 100 MB in RAM, < 1 s scan.
//!    At catalog sizes beyond that an ANN index such as HNSW would cut latency
//!    to sub-millisecond; a follow-up should build one if the scan exceeds ~2 s.)*
//! 3. For each neighbour, load the tags assigned to it in `photo_tags` + `tags`.
//!    Only tags already confirmed for the **neighbour** are propagated (the query
//!    photo's own tags are never suggested back to itself).
//! 4. Rank the collected tags: score = sum of cosine similarities of neighbours
//!    that carry the tag, normalized to [0, 1] against the maximum raw score.
//! 5. Dedup ancestors: if a leaf tag (e.g. `Animals/Birds/Gull`) already appears
//!    in the suggestion list, any ancestor in the list (`Animals/Birds`, `Animals`)
//!    is dropped — the leaf implies the ancestors everywhere and is more useful.
//! 6. Skip tags already assigned to the query photo (no value re-suggesting them).
//! 7. Upsert the ranked surviving suggestions into `smarttags__suggestions`
//!    (same `pending`/`accepted`/`rejected` state machine as `ai__suggestions`).
//!
//! ## `smarttags__suggestions` schema
//!
//! Mirrors `ai__suggestions` with two differences:
//! - No `reason` / `description` / `synonyms` — kNN doesn't produce those.
//! - `source_photo_ids` (TEXT, JSON array of i64) records which neighbours drove
//!   the suggestion (useful for provenance display in H7d).
//!
//! The table is created by [`ensure_suggestions_schema`] on first use.
//!
//! ## State machine
//!
//! `pending` → accepted (via [`set_suggestion_state`] → `accepted`) or
//! `pending` → rejected (→ `rejected`). A rejected (photo, path) is never
//! re-proposed; [`suggest_tags`] skips any path already in a non-pending
//! state for this photo.
//!
//! ## Accept / reject
//!
//! - **Accept**: the caller (command layer) assigns the tag via `Catalog::assign_tag`
//!   and then calls [`set_suggestion_state`] with `accepted`. The tag is created
//!   first if it does not exist (same as `ai_accept_suggestion`). Suggestions only
//!   propose existing tags (loaded from `photo_tags`), so new-tag creation is
//!   unlikely here but the path is supported for forward-compatibility.
//! - **Reject**: calls [`set_suggestion_state`] with `rejected`; the row is never
//!   deleted so the rejected path is filtered out on the next suggest run.

use rusqlite::{Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};

use super::store::blob_to_embedding;
use super::classifier::{ensure_schema as ensure_classifiers_schema, load_classifier, score_blended};

// ── Schema ─────────────────────────────────────────────────────────────────────

/// Create `smarttags__suggestions` if it does not yet exist. Idempotent.
pub fn ensure_suggestions_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS smarttags__suggestions (
            photo_id         INTEGER NOT NULL,
            path             TEXT    NOT NULL,
            state            TEXT    NOT NULL DEFAULT 'pending',
            confidence       REAL,
            source_photo_ids TEXT,
            created_at       INTEGER NOT NULL,
            updated_at       INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (photo_id, path)
        );",
    )?;
    // Migration for tables created before updated_at existed. SQLite has no
    // ADD COLUMN IF NOT EXISTS, so on an up-to-date table this fails with a
    // duplicate-column error — expected, and safe to ignore.
    let _ = conn.execute_batch(
        "ALTER TABLE smarttags__suggestions ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;",
    );
    Ok(())
}

// ── In-memory types ──────────────────────────────────────────────────────────

/// One pending suggestion row as returned to the caller.
#[derive(Debug, Clone)]
pub struct StoredSuggestion {
    pub path: String,
    pub confidence: f32,
    /// JSON array of source-photo IDs that drove this suggestion (provenance).
    pub source_photo_ids: Vec<i64>,
    /// True when the tag already exists in the catalog at this path.
    pub existing_tag_id: Option<i64>,
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Tune constants.
const TOP_K: usize = 20;
/// Minimum cosine similarity to count a neighbour as "close". Below this the
/// match is considered noise and its tags are ignored.
const MIN_COSINE: f32 = 0.60;

/// Scan all embeddings, find the top-K nearest neighbours of `photo_id`, collect
/// their tags, rank and dedup, then upsert into `smarttags__suggestions`.
///
/// Returns the number of pending suggestions stored (may be 0 if the photo has
/// no embedding or all candidates were filtered out).
///
/// **Not feature-gated** — the schema helpers and state machine work at any
/// feature level; only the embed engine is feature-gated. The caller
/// (`commands::smarttags_suggest_tags`) is feature-gated, so this module is only
/// reachable when `smarttags` is enabled. Keeping the pure DB code outside the
/// cfg block makes unit-testing easier.
pub fn suggest_tags(
    conn: &Connection,
    photo_id: i64,
) -> Result<usize, String> {
    // Ensure all plugin tables exist before touching any of them.
    // This mirrors the defensive pattern used by every other plugin command
    // (ai, faces, map) so that suggest_tags works on a fresh catalog where
    // the indexer has never run on this connection.
    super::store::ensure_schema(conn).map_err(|e| e.to_string())?;
    ensure_suggestions_schema(conn).map_err(|e| e.to_string())?;
    // Classifiers table — created lazily; may not exist yet if train has never run.
    ensure_classifiers_schema(conn).map_err(|e| e.to_string())?;

    // ── 1. Load the query embedding ───────────────────────────────────────────
    let query_blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT vector FROM smarttags__embeddings WHERE photo_id = ?1",
            rusqlite::params![photo_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;

    let query_blob = match query_blob {
        Some(b) => b,
        None => return Ok(0), // not yet indexed; nothing to suggest
    };
    let query_emb = blob_to_embedding(&query_blob).map_err(|e| e.to_string())?;

    // ── 2. Load all other embeddings (brute-force cosine scan) ───────────────
    // Fine to ~50 000 photos (512 dims × 4 bytes × 50 000 = ~100 MB RAM,
    // ~50 M dot-product ops ≈ 0.1–0.5 s).  For catalogs beyond that, an
    // approximate-nearest-neighbour index (e.g. HNSW in a follow-up) would cut
    // latency to sub-millisecond; add one if scan time exceeds ~2 s.
    let all_embeddings: Vec<(i64, Vec<u8>)> = {
        let mut stmt = conn
            .prepare(
                "SELECT photo_id, vector FROM smarttags__embeddings WHERE photo_id != ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![photo_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        rows
    };

    // ── 3. Score every neighbour and keep the top-K above MIN_COSINE ─────────
    let mut scored: Vec<(i64, f32)> = all_embeddings
        .into_iter()
        .filter_map(|(pid, blob)| {
            let emb = blob_to_embedding(&blob).ok()?;
            let sim = cosine_unit(&query_emb, &emb);
            if sim >= MIN_COSINE {
                Some((pid, sim))
            } else {
                None
            }
        })
        .collect();

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(TOP_K);

    if scored.is_empty() {
        return Ok(0);
    }

    // ── 4. Load the current photo's own tags (to skip re-suggesting them) ────
    let own_tag_paths: HashSet<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT t.full_path FROM tags t
                  JOIN photo_tags pt ON pt.tag_id = t.id
                 WHERE pt.photo_id = ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![photo_id], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<HashSet<_>, _>>()
            .map_err(|e| e.to_string())?;
        rows
    };

    // ── 5. Load already-rejected / accepted paths for this photo ─────────────
    let non_pending: HashSet<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT path FROM smarttags__suggestions
                  WHERE photo_id = ?1 AND state != 'pending'",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params![photo_id], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<HashSet<_>, _>>()
            .map_err(|e| e.to_string())?;
        rows
    };

    // ── 6. Aggregate neighbour tags: path → (score, [source_ids]) ────────────
    // score = sum of cosine similarities across neighbours that carry the tag.
    let mut tag_scores: HashMap<String, (f32, Vec<i64>)> = HashMap::new();

    for (neighbour_id, sim) in &scored {
        let mut stmt = conn
            .prepare(
                "SELECT t.full_path FROM tags t
                  JOIN photo_tags pt ON pt.tag_id = t.id
                 WHERE pt.photo_id = ?1",
            )
            .map_err(|e| e.to_string())?;
        let paths: Vec<String> = stmt
            .query_map(rusqlite::params![neighbour_id], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;

        for path in paths {
            if own_tag_paths.contains(&path) || non_pending.contains(&path) {
                continue;
            }
            let entry = tag_scores.entry(path).or_insert_with(|| (0.0, Vec::new()));
            entry.0 += sim;
            if !entry.1.contains(neighbour_id) {
                entry.1.push(*neighbour_id);
            }
        }
    }

    if tag_scores.is_empty() {
        return Ok(0);
    }

    // ── 7. Normalize kNN scores to [0, 1] then blend with classifiers ─────────
    let max_score = tag_scores
        .values()
        .map(|(s, _)| *s)
        .fold(0.0f32, f32::max);

    let mut candidates: Vec<(String, f32, Vec<i64>)> = tag_scores
        .into_iter()
        .map(|(path, (raw_score, sources))| {
            let knn_score = if max_score > 0.0 { raw_score / max_score } else { 0.0 };
            // Blend with a per-tag logistic classifier when one exists (H7e).
            // load_classifier returns Ok(None) when no classifier is trained yet for this
            // tag; score_blended with None is a no-op that returns the kNN score unchanged.
            let clf = load_classifier(conn, &path).unwrap_or(None);
            let conf = score_blended(knn_score, clf.as_ref(), &query_emb);
            (path, conf, sources)
        })
        .collect();

    // Sort descending by confidence, then alphabetically for stability.
    candidates.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });

    // ── 8. Ancestor dedup ─────────────────────────────────────────────────────
    // Build the set of all suggested paths first (for prefix-check below).
    let candidate_paths: HashSet<String> =
        candidates.iter().map(|(p, _, _)| p.clone()).collect();
    // A tag path is an ancestor of another if the other starts with the tag
    // path followed by '/'. Example: "Animals/Birds" is an ancestor of
    // "Animals/Birds/Gull" but NOT of "Animals/Birds-of-prey/Hawk".
    let is_ancestor_of_any = |path: &str| -> bool {
        let prefix = format!("{path}/");
        candidate_paths.iter().any(|other| other.starts_with(&prefix))
    };

    let deduped: Vec<(String, f32, Vec<i64>)> = candidates
        .into_iter()
        .filter(|(path, _, _)| !is_ancestor_of_any(path))
        .collect();

    // ── 9. Upsert into smarttags__suggestions ─────────────────────────────────
    let now = unix_now();
    let mut stored = 0usize;

    for (path, conf, sources) in &deduped {
        let sources_json = serde_json::to_string(sources).unwrap_or_else(|_| "[]".into());
        let rows_inserted = conn
            .execute(
                "INSERT INTO smarttags__suggestions
                     (photo_id, path, state, confidence, source_photo_ids, created_at)
                 VALUES (?1, ?2, 'pending', ?3, ?4, ?5)
                 ON CONFLICT(photo_id, path) DO NOTHING",
                rusqlite::params![photo_id, path, *conf as f64, sources_json, now],
            )
            .map_err(|e| e.to_string())?;
        if rows_inserted > 0 {
            stored += 1;
        }
    }

    Ok(stored)
}

/// Load all `pending` suggestions for `photo_id`, ordered by confidence descending.
/// Resolves whether the tag already exists in the catalog (by full_path lookup).
pub fn load_pending_suggestions(
    conn: &Connection,
    photo_id: i64,
) -> Result<Vec<StoredSuggestion>, String> {
    ensure_suggestions_schema(conn).map_err(|e| e.to_string())?;

    let mut stmt = conn
        .prepare(
            "SELECT path, confidence, source_photo_ids
               FROM smarttags__suggestions
              WHERE photo_id = ?1 AND state = 'pending'
              ORDER BY confidence DESC",
        )
        .map_err(|e| e.to_string())?;

    let rows: Vec<(String, f32, Option<String>)> = stmt
        .query_map(rusqlite::params![photo_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<f64>>(1)?.unwrap_or(0.0) as f32,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;

    let mut out = Vec::with_capacity(rows.len());
    for (path, confidence, sources_json) in rows {
        let source_photo_ids: Vec<i64> = sources_json
            .and_then(|s| serde_json::from_str::<Vec<i64>>(&s).ok())
            .unwrap_or_default();

        let existing_tag_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM tags WHERE full_path_norm = lower(?1)",
                rusqlite::params![path],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();

        out.push(StoredSuggestion {
            path,
            confidence,
            source_photo_ids,
            existing_tag_id,
        });
    }
    Ok(out)
}

/// Mark a suggestion as `accepted` or `rejected`. Inserts the row if it was
/// never pending (so a manual reject without a prior suggest run is also
/// remembered and will suppress a future suggestion for this path).
///
/// `updated_at` is always set to `now` so that the classifier staleness check
/// in `train_all` can detect feedback that arrived after the last training run.
pub fn set_suggestion_state(
    conn: &Connection,
    photo_id: i64,
    path: &str,
    state: &str,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO smarttags__suggestions (photo_id, path, state, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(photo_id, path) DO UPDATE SET
             state      = excluded.state,
             updated_at = excluded.updated_at",
        rusqlite::params![photo_id, path, state, now],
    )?;
    Ok(())
}

// ── Cosine helpers ────────────────────────────────────────────────────────────

/// Cosine similarity of two L2-normalized unit vectors.
/// Because both inputs are assumed to be L2-normalized, cosine similarity
/// reduces to a plain dot product. **Only call this with unit vectors** — it
/// does not normalize its inputs. For arbitrary (un-normalized) vectors use
/// [`super::embed::cosine`] which applies the full formula.
pub fn cosine_unit(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    // Both are L2-normalized; cosine = dot product.
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

// ── Internal helpers ─────────────────────────────────────────────────────────

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::store::embedding_to_blob;
    use rusqlite::Connection;

    // ── Helpers ────────────────────────────────────────────────────────────────

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE IF NOT EXISTS photos (
                 id   INTEGER PRIMARY KEY,
                 uuid TEXT NOT NULL DEFAULT '',
                 path TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS tags (
                 id            INTEGER PRIMARY KEY,
                 uuid          TEXT,
                 name          TEXT NOT NULL DEFAULT '',
                 name_norm     TEXT NOT NULL DEFAULT '',
                 parent_id     INTEGER,
                 full_path     TEXT NOT NULL DEFAULT '',
                 full_path_norm TEXT NOT NULL DEFAULT '',
                 description   TEXT NOT NULL DEFAULT '',
                 auto_rule     TEXT,
                 exportable    INTEGER NOT NULL DEFAULT 1,
                 private       INTEGER NOT NULL DEFAULT 0,
                 last_used_at  INTEGER,
                 created_at    INTEGER NOT NULL DEFAULT 0,
                 updated_at    INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS photo_tags (
                 photo_id   INTEGER NOT NULL,
                 tag_id     INTEGER NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (photo_id, tag_id)
             );",
        )
        .unwrap();
        super::super::store::ensure_schema(&conn).unwrap();
        ensure_suggestions_schema(&conn).unwrap();
        conn
    }

    fn insert_photo(conn: &Connection, id: i64) {
        conn.execute("INSERT INTO photos (id, uuid, path) VALUES (?1, '', '')", [id])
            .unwrap();
    }

    /// Insert a tag and return its id.
    fn insert_tag(conn: &Connection, id: i64, full_path: &str, parent_id: Option<i64>) {
        let name = full_path.split('/').last().unwrap_or(full_path);
        conn.execute(
            "INSERT INTO tags (id, name, name_norm, full_path, full_path_norm, parent_id)
             VALUES (?1, ?2, lower(?2), ?3, lower(?3), ?4)",
            rusqlite::params![id, name, full_path, parent_id],
        )
        .unwrap();
    }

    fn assign_tag(conn: &Connection, photo_id: i64, tag_id: i64) {
        conn.execute(
            "INSERT OR IGNORE INTO photo_tags(photo_id, tag_id) VALUES(?1, ?2)",
            [photo_id, tag_id],
        )
        .unwrap();
    }

    fn upsert_emb(conn: &Connection, photo_id: i64, emb: &[f32]) {
        let blob = embedding_to_blob(emb);
        super::super::store::upsert_embedding(conn, photo_id, &blob).unwrap();
    }

    /// Unit vector in direction `seed` (repeating-sine pattern, length `dim`).
    fn unit_emb(dim: usize, seed: usize) -> Vec<f32> {
        let v: Vec<f32> = (0..dim)
            .map(|i| ((seed * 7 + i) as f32).sin())
            .collect();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter().map(|x| x / norm).collect()
        } else {
            v
        }
    }

    // ── cosine_unit ────────────────────────────────────────────────────────────

    #[test]
    fn cosine_unit_identical_is_one() {
        let a = unit_emb(8, 1);
        let sim = cosine_unit(&a, &a);
        assert!((sim - 1.0).abs() < 1e-5, "identical unit vectors → cosine ≈ 1, got {sim}");
    }

    #[test]
    fn cosine_unit_orthogonal_is_zero() {
        // Two axis vectors are orthogonal.
        let a = vec![1.0f32, 0.0, 0.0];
        let b = vec![0.0f32, 1.0, 0.0];
        let sim = cosine_unit(&a, &b);
        assert!(sim.abs() < 1e-6, "orthogonal vectors → cosine ≈ 0, got {sim}");
    }

    #[test]
    fn cosine_unit_opposite_is_minus_one() {
        let a = vec![1.0f32, 0.0];
        let b = vec![-1.0f32, 0.0];
        let sim = cosine_unit(&a, &b);
        assert!((sim + 1.0).abs() < 1e-5, "opposite vectors → cosine ≈ -1, got {sim}");
    }

    #[test]
    fn cosine_unit_dimension_mismatch_is_zero() {
        let a = vec![1.0f32, 0.0];
        let b = vec![1.0f32, 0.0, 0.0];
        assert_eq!(cosine_unit(&a, &b), 0.0);
    }

    #[test]
    fn cosine_unit_empty_is_zero() {
        assert_eq!(cosine_unit(&[], &[]), 0.0);
    }

    // ── ancestor-dedup logic (inline test against the public function) ─────────

    #[test]
    fn ancestor_dedup_removes_coarser_path() {
        // Tags: Animals, Animals/Birds, Animals/Birds/Gull.
        // If all three are candidates, the ancestor-dedup should remove Animals and
        // Animals/Birds, keeping only Animals/Birds/Gull (the leaf).
        let conn = mem_conn();
        insert_photo(&conn, 1); // query photo (no embedding — suggest_tags bails early)
        insert_photo(&conn, 2); // neighbour
        insert_tag(&conn, 1, "Animals", None);
        insert_tag(&conn, 2, "Animals/Birds", Some(1));
        insert_tag(&conn, 3, "Animals/Birds/Gull", Some(2));
        assign_tag(&conn, 2, 1);
        assign_tag(&conn, 2, 2);
        assign_tag(&conn, 2, 3);

        // Give both photos the same embedding so the neighbour has cosine ≥ MIN_COSINE.
        let emb = unit_emb(8, 42);
        upsert_emb(&conn, 1, &emb);
        upsert_emb(&conn, 2, &emb);

        let stored = suggest_tags(&conn, 1).unwrap();
        // The only surviving suggestion should be the leaf.
        let suggestions = load_pending_suggestions(&conn, 1).unwrap();
        assert!(
            stored <= 1 || suggestions.len() == 1,
            "ancestor tags must be deduped; stored={stored}, pending={}",
            suggestions.len()
        );
        if !suggestions.is_empty() {
            assert_eq!(
                suggestions[0].path, "Animals/Birds/Gull",
                "only the most specific (leaf) path should survive"
            );
        }
    }

    // ── suggest_tags: no embedding returns 0 ──────────────────────────────────

    #[test]
    fn suggest_tags_without_embedding_returns_zero() {
        let conn = mem_conn();
        insert_photo(&conn, 1);
        let n = suggest_tags(&conn, 1).unwrap();
        assert_eq!(n, 0, "no embedding → no suggestions");
    }

    // ── suggest_tags: no neighbours ───────────────────────────────────────────

    #[test]
    fn suggest_tags_no_neighbours_returns_zero() {
        let conn = mem_conn();
        insert_photo(&conn, 1);
        upsert_emb(&conn, 1, &unit_emb(8, 1));
        // No other photos in the index.
        let n = suggest_tags(&conn, 1).unwrap();
        assert_eq!(n, 0);
    }

    // ── ensure_suggestions_schema is idempotent ───────────────────────────────

    #[test]
    fn ensure_suggestions_schema_is_idempotent() {
        let conn = mem_conn();
        ensure_suggestions_schema(&conn).unwrap();
        ensure_suggestions_schema(&conn).unwrap(); // must not fail
    }

    // ── set_suggestion_state ──────────────────────────────────────────────────

    #[test]
    fn set_state_upsert_and_overwrite() {
        let conn = mem_conn();
        // Insert directly.
        set_suggestion_state(&conn, 1, "Animals/Birds", "pending", 1000).unwrap();
        // Overwrite with accepted.
        set_suggestion_state(&conn, 1, "Animals/Birds", "accepted", 2000).unwrap();
        let state: String = conn
            .query_row(
                "SELECT state FROM smarttags__suggestions WHERE photo_id = 1 AND path = 'Animals/Birds'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "accepted");
    }

    // ── full suggest + accept/reject cycle ────────────────────────────────────

    #[test]
    fn suggest_creates_pending_rows_and_reject_suppresses_re_suggest() {
        let conn = mem_conn();
        // Query photo.
        insert_photo(&conn, 1);
        // Neighbour photo with a very close embedding and one tag.
        insert_photo(&conn, 2);
        insert_tag(&conn, 10, "Nature/Sunset", None);
        assign_tag(&conn, 2, 10);

        let emb = unit_emb(16, 5);
        upsert_emb(&conn, 1, &emb);
        upsert_emb(&conn, 2, &emb);

        // First suggest run: should store a pending row for "Nature/Sunset".
        let stored = suggest_tags(&conn, 1).unwrap();
        assert!(stored >= 1, "expected at least one suggestion, got {stored}");

        let pending = load_pending_suggestions(&conn, 1).unwrap();
        assert!(
            pending.iter().any(|s| s.path == "Nature/Sunset"),
            "Nature/Sunset must be pending"
        );

        // Reject the suggestion.
        set_suggestion_state(&conn, 1, "Nature/Sunset", "rejected", 9999).unwrap();

        // Second suggest run: the rejected path must not reappear as pending.
        suggest_tags(&conn, 1).unwrap();
        let pending2 = load_pending_suggestions(&conn, 1).unwrap();
        assert!(
            !pending2.iter().any(|s| s.path == "Nature/Sunset"),
            "rejected tag must not be re-proposed as pending"
        );
    }

    // ── own tags are not suggested back ───────────────────────────────────────

    #[test]
    fn own_tags_are_not_re_suggested() {
        let conn = mem_conn();
        insert_photo(&conn, 1);
        insert_photo(&conn, 2);
        insert_tag(&conn, 20, "Sky/Blue", None);

        // Both photos already have the tag.
        assign_tag(&conn, 1, 20);
        assign_tag(&conn, 2, 20);

        let emb = unit_emb(8, 7);
        upsert_emb(&conn, 1, &emb);
        upsert_emb(&conn, 2, &emb);

        suggest_tags(&conn, 1).unwrap();
        let pending = load_pending_suggestions(&conn, 1).unwrap();
        assert!(
            !pending.iter().any(|s| s.path == "Sky/Blue"),
            "own tags must not be re-suggested"
        );
    }

    // ── far-away neighbour stays below MIN_COSINE and is ignored ─────────────

    #[test]
    fn distant_neighbour_produces_no_suggestions() {
        let conn = mem_conn();
        insert_photo(&conn, 1);
        insert_photo(&conn, 2);
        insert_tag(&conn, 30, "Abstract", None);
        assign_tag(&conn, 2, 30);

        // Opposite unit vectors → cosine = -1.0 (far below MIN_COSINE).
        let a = vec![1.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let b = vec![-1.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        upsert_emb(&conn, 1, &a);
        upsert_emb(&conn, 2, &b);

        let n = suggest_tags(&conn, 1).unwrap();
        assert_eq!(n, 0, "far neighbour must not produce suggestions");
    }
}
