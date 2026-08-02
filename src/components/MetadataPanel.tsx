import { useEffect, useMemo, useState } from "react";
import { getPhotoMetadata, MetadataEntry } from "../modules/api";

// Groups shown expanded by default; the rest (MakerNotes, etc.) collapse to keep
// the long tail out of the way.
const DEFAULT_OPEN = new Set(["EXIF", "Composite", "IPTC", "XMP"]);

// Shows all stored EXIF/IPTC/XMP metadata for a photo, grouped and collapsible.
// Data comes from the catalog (populated at scan time), not from exiftool live.
export function MetadataPanel({ photoId }: { photoId: number }) {
  const [entries, setEntries] = useState<MetadataEntry[]>([]);
  const [open, setOpen] = useState<Set<string>>(DEFAULT_OPEN);

  useEffect(() => {
    let active = true;
    getPhotoMetadata(photoId)
      .then((e) => active && setEntries(e))
      .catch(() => active && setEntries([]));
    return () => {
      active = false;
    };
  }, [photoId]);

  // Group entries by their family, preserving the backend's ordering.
  const groups = useMemo(() => {
    const map = new Map<string, MetadataEntry[]>();
    for (const e of entries) {
      const list = map.get(e.groupName) ?? [];
      list.push(e);
      map.set(e.groupName, list);
    }
    return [...map.entries()];
  }, [entries]);

  const toggle = (group: string) =>
    setOpen((cur) => {
      const next = new Set(cur);
      next.has(group) ? next.delete(group) : next.add(group);
      return next;
    });

  if (entries.length === 0) {
    return <div className="panel-empty">No metadata</div>;
  }

  return (
    <div className="metadata">
      {groups.map(([group, items]) => (
        <div key={group} className="meta-group">
          <button className="meta-group-header" onClick={() => toggle(group)}>
            <span>{open.has(group) ? "▾" : "▸"}</span>
            <span>{group}</span>
            <span className="meta-count">{items.length}</span>
          </button>
          {open.has(group) &&
            items.map((e) => (
              <div key={`${group}:${e.key}`} className="meta-row">
                <span className="meta-key">{e.key}</span>
                <span className="meta-val">{e.value}</span>
              </div>
            ))}
        </div>
      ))}
    </div>
  );
}
