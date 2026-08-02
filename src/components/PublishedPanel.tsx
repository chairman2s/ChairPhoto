import { useCallback, useEffect, useState } from "react";
import {
  deletePublication,
  listPublications,
  listVersions,
  Publication,
  PhotoVersion,
  recordPublication,
} from "../modules/api";

// Common destinations offered in the "Mark as published" picker; the user can also type
// any other platform. Instagram posts auto-record themselves (via the export dialog); the
// others are marked by hand here. See docs/publications.md.
const COMMON_PLATFORMS = ["instagram", "flickr", "smugmug"];

function fmtDate(unixSeconds: number): string {
  try {
    return new Date(unixSeconds * 1000).toLocaleDateString();
  } catch {
    return "";
  }
}

function titlecase(s: string): string {
  return s ? s[0].toUpperCase() + s.slice(1) : s;
}

// Inspector "Published to" panel: shows where this photo has been posted and WHICH version
// went to each platform, and lets the user record a manual publication (Flickr/SmugMug/…).
// Version = null means the Original (unedited base).
export function PublishedPanel({
  photoId,
  activeVersionId,
  onChanged,
}: {
  photoId: number;
  /** The version selected in the inspector — the default for a new "Mark as published". */
  activeVersionId: number | null;
  /** Refresh the rest of the app (the published facets depend on this). */
  onChanged: () => void;
}) {
  const [pubs, setPubs] = useState<Publication[]>([]);
  const [versions, setVersions] = useState<PhotoVersion[]>([]);
  const [platform, setPlatform] = useState(COMMON_PLATFORMS[0]);
  const [versionId, setVersionId] = useState<number | null>(activeVersionId);

  const reload = useCallback(() => {
    listPublications(photoId).then(setPubs).catch(() => setPubs([]));
    listVersions(photoId).then(setVersions).catch(() => setVersions([]));
  }, [photoId]);

  useEffect(() => {
    reload();
  }, [reload]);

  // Default the "mark as published" version to the one active in the inspector.
  useEffect(() => {
    setVersionId(activeVersionId);
  }, [activeVersionId, photoId]);

  const afterChange = () => {
    reload();
    onChanged();
  };

  const mark = async () => {
    const p = platform.trim();
    if (!p) return;
    await recordPublication(photoId, versionId, p);
    afterChange();
  };

  return (
    <div className="published">
      <ul className="versions-list">
        {pubs.map((p) => (
          <li key={p.id} className="versions-row">
            <span className="versions-name" style={{ cursor: "default" }}>
              <strong>{titlecase(p.platform)}</strong>
              {" · "}
              {p.versionName ?? "Original"}
              {p.publishedAt ? <span className="nearby-count"> {fmtDate(p.publishedAt)}</span> : null}
            </span>
            <div className="versions-actions">
              <button
                className="chip"
                title="Remove this publication record"
                onClick={async () => {
                  await deletePublication(p.id);
                  afterChange();
                }}
              >
                ✕
              </button>
            </div>
          </li>
        ))}
        {pubs.length === 0 && <li className="panel-empty">Not published yet</li>}
      </ul>

      <div className="published-add">
        <div className="row">
          <input
            className="folder-input"
            list="published-platforms"
            placeholder="Platform (e.g. flickr)"
            value={platform}
            onChange={(e) => setPlatform(e.target.value.toLowerCase())}
            onKeyDown={(e) => {
              if (e.key === "Enter") mark();
            }}
          />
          <datalist id="published-platforms">
            {COMMON_PLATFORMS.map((p) => (
              <option key={p} value={p} />
            ))}
          </datalist>
        </div>
        <div className="row">
          <select
            className="folder-input"
            value={versionId ?? ""}
            onChange={(e) => setVersionId(e.target.value ? Number(e.target.value) : null)}
            title="Which version was published"
          >
            <option value="">Original</option>
            {versions.map((v) => (
              <option key={v.id} value={v.id}>
                {v.name}
              </option>
            ))}
          </select>
          <button className="chip" onClick={mark} title="Record a publication">
            + Mark
          </button>
        </div>
      </div>
    </div>
  );
}
