import { useEffect, useRef, useState } from "react";
import { renderEditBatch } from "../modules/api";
import {
  Look,
  lookFields,
  Tone,
  ZERO_LOOK,
  ZERO_TONE,
} from "../modules/editing";
import {
  BUILTIN_PRESETS,
  DevelopPreset,
  loadUserPresets,
  PRESET_CATEGORIES,
  saveUserPresets,
} from "../modules/presets";

// The develop view's preset browser: the built-in library + the user's saved presets,
// grouped by category, each card showing the *current photo* rendered with that preset
// (Lightroom-style). Thumbnails come from one render_edit_batch call per photo, fired
// lazily on first expand; they deliberately exclude the live crop/tone — a thumb
// communicates the preset's look, not the framing — so they stay valid while editing.

const THUMB_EDGE = 320; // rendered px (displayed ~160, crisp on hidpi)

// Module-level thumbnail cache so re-entering a photo (or the editor) is instant.
// photoId → presetId → dataUrl, evicting the oldest photo past the cap.
const thumbCache = new Map<number, Map<string, string>>();
const CACHE_PHOTOS = 4;

/** The full editor state a preset would produce — used to highlight the active card. */
const appliedState = (edit: DevelopPreset["edit"]) =>
  JSON.stringify({
    tone: { ...ZERO_TONE, ...edit.tone, wb: { ...ZERO_TONE.wb, ...edit.tone?.wb } },
    ...lookFields({ ...ZERO_LOOK, ...edit }),
  });

export function PresetBrowser({
  photoId,
  currentTone,
  currentLook,
  onApply,
}: {
  photoId: number;
  currentTone: Tone;
  currentLook: Look;
  onApply: (preset: DevelopPreset) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const [userPresets, setUserPresets] = useState<DevelopPreset[]>([]);
  const [thumbs, setThumbs] = useState<Map<string, string>>(new Map());
  const [loading, setLoading] = useState(false);
  // Name dialog state: {mode:"save"} creates from the current edit; {mode:"rename"}
  // renames an existing user preset.
  const [naming, setNaming] = useState<{ mode: "save" } | { mode: "rename"; preset: DevelopPreset } | null>(null);
  const [nameInput, setNameInput] = useState("");
  const nameRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    loadUserPresets().then(setUserPresets).catch(() => {});
  }, []);

  useEffect(() => {
    nameRef.current?.focus();
  }, [naming]);

  const presets = [...BUILTIN_PRESETS, ...userPresets];

  // Fetch thumbnails for any presets missing from this photo's cache. Runs when the
  // browser is expanded and re-runs when the preset list grows (new user preset).
  useEffect(() => {
    if (!expanded) return;
    const cached = thumbCache.get(photoId);
    const missing = presets.filter((p) => !cached?.has(p.id));
    if (missing.length === 0) {
      setThumbs(new Map(cached));
      return;
    }
    let cancelled = false;
    setLoading(true);
    renderEditBatch(photoId, missing.map((p) => JSON.stringify(p.edit)), THUMB_EDGE)
      .then((urls) => {
        if (cancelled) return;
        const map = thumbCache.get(photoId) ?? new Map<string, string>();
        urls.forEach((url, i) => {
          if (url) map.set(missing[i].id, url);
        });
        thumbCache.set(photoId, map);
        // Evict the oldest photos past the cap (Map preserves insertion order).
        while (thumbCache.size > CACHE_PHOTOS) {
          const oldest = thumbCache.keys().next().value;
          if (oldest === undefined || oldest === photoId) break;
          thumbCache.delete(oldest);
        }
        setThumbs(new Map(map));
      })
      .catch(() => {})
      .finally(() => !cancelled && setLoading(false));
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [expanded, photoId, presets.length]);

  const current = JSON.stringify({ tone: currentTone, ...lookFields(currentLook) });

  const saveUser = async (list: DevelopPreset[]) => {
    setUserPresets(list);
    await saveUserPresets(list).catch(() => {});
  };

  const confirmName = async () => {
    const name = nameInput.trim();
    if (!name || !naming) return;
    if (naming.mode === "save") {
      const preset: DevelopPreset = {
        id: crypto.randomUUID(),
        name,
        category: "User",
        edit: { tone: { ...currentTone }, ...lookFields(currentLook) },
      };
      await saveUser([...userPresets, preset]);
    } else {
      await saveUser(
        userPresets.map((p) => (p.id === naming.preset.id ? { ...p, name } : p)),
      );
    }
    setNaming(null);
    setNameInput("");
  };

  const deletePreset = async (preset: DevelopPreset) => {
    await saveUser(userPresets.filter((p) => p.id !== preset.id));
    thumbCache.get(photoId)?.delete(preset.id);
  };

  return (
    <div className="develop-section preset-browser">
      <button className="preset-browser-head" onClick={() => setExpanded((e) => !e)}>
        <span className="panel-head develop-group-label">Presets</span>
        <span className="preset-browser-caret">{expanded ? "▾" : "▸"}</span>
      </button>
      {expanded && (
        <>
          {PRESET_CATEGORIES.map((cat) => {
            const group = presets.filter((p) => p.category === cat);
            if (group.length === 0) return null;
            return (
              <div key={cat}>
                <div className="preset-cat-label">{cat}</div>
                <div className="preset-grid">
                  {group.map((p) => {
                    const active = appliedState(p.edit) === current;
                    const thumb = thumbs.get(p.id);
                    return (
                      <div
                        key={p.id}
                        className={`preset-card ${active ? "preset-card-active" : ""}`}
                        onClick={() => onApply(p)}
                        title={`Apply ${p.name}`}
                      >
                        <div className="preset-thumb">
                          {thumb ? (
                            <img src={thumb} alt={p.name} draggable={false} />
                          ) : (
                            <span className="preset-thumb-empty">{loading ? "…" : ""}</span>
                          )}
                        </div>
                        <div className="preset-name">
                          <span>{p.name}</span>
                          {!p.builtin && (
                            <span className="preset-card-actions">
                              <button
                                title="Rename preset"
                                onClick={(e) => {
                                  e.stopPropagation();
                                  setNameInput(p.name);
                                  setNaming({ mode: "rename", preset: p });
                                }}
                              >
                                ✎
                              </button>
                              <button
                                title="Delete preset"
                                onClick={(e) => {
                                  e.stopPropagation();
                                  void deletePreset(p);
                                }}
                              >
                                ×
                              </button>
                            </span>
                          )}
                        </div>
                      </div>
                    );
                  })}
                </div>
              </div>
            );
          })}
          <button
            className="chip preset-save-btn"
            onClick={() => {
              setNameInput("");
              setNaming({ mode: "save" });
            }}
          >
            Save current as preset…
          </button>
        </>
      )}

      {naming && (
        <div className="modal-backdrop" onClick={() => setNaming(null)}>
          <div className="modal preset-name-modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-header">
              <div className="modal-title">
                {naming.mode === "save" ? "Save preset" : "Rename preset"}
              </div>
            </div>
            <div className="modal-body">
              <input
                ref={nameRef}
                className="tag-input"
                type="text"
                placeholder="Preset name"
                value={nameInput}
                onChange={(e) => setNameInput(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") void confirmName();
                  if (e.key === "Escape") setNaming(null);
                }}
              />
            </div>
            <div className="tag-create-footer">
              <button className="chip" onClick={() => setNaming(null)}>
                Cancel
              </button>
              <button className="btn-primary" disabled={!nameInput.trim()} onClick={() => void confirmName()}>
                {naming.mode === "save" ? "Save" : "Rename"}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
