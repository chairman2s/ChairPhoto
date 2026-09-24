import { useEffect, useRef, useState } from "react";
import { RenderedImage } from "./darkroom/RenderedImage";
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

// The Darkroom's preset browser: the built-in library + the user's saved presets,
// grouped by category, each card showing the *current photo* rendered with that preset
// (Lightroom-style). Each thumbnail is a native render URL from the caller — the
// Darkroom's own source and engine — built lazily on first expand; they deliberately
// exclude the live crop/tone — a thumb communicates the preset's look, not the framing —
// so they stay valid while editing. Saving the current look is the Darkroom's
// "Save as preset"; here user presets can be renamed or deleted.

const THUMB_EDGE = 320; // rendered px (displayed ~160, crisp on hidpi)

/** The full editor state a preset would produce — used to highlight the active card. */
const appliedState = (edit: DevelopPreset["edit"]) =>
  JSON.stringify({
    tone: { ...ZERO_TONE, ...edit.tone, wb: { ...ZERO_TONE.wb, ...edit.tone?.wb } },
    ...lookFields({ ...ZERO_LOOK, ...edit }),
  });

export function PresetBrowser({
  currentTone,
  currentLook,
  renderUrl,
  refreshKey = 0,
  onApply,
}: {
  currentTone: Tone;
  currentLook: Look;
  /** A preset's thumbnail render at a long edge (the caller's source and engine). */
  renderUrl: (edit: DevelopPreset["edit"], maxEdge: number) => string;
  /** Bumped by the caller when the user presets change elsewhere (a new save). */
  refreshKey?: number;
  onApply: (preset: DevelopPreset) => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const [userPresets, setUserPresets] = useState<DevelopPreset[]>([]);
  // Renaming an existing user preset.
  const [naming, setNaming] = useState<{ mode: "rename"; preset: DevelopPreset } | null>(null);
  const [nameInput, setNameInput] = useState("");
  const nameRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    loadUserPresets().then(setUserPresets).catch(() => {});
  }, [refreshKey]);

  useEffect(() => {
    nameRef.current?.focus();
  }, [naming]);

  const presets = [...BUILTIN_PRESETS, ...userPresets];

  const current = JSON.stringify({ tone: currentTone, ...lookFields(currentLook) });

  const saveUser = async (list: DevelopPreset[]) => {
    setUserPresets(list);
    await saveUserPresets(list).catch(() => {});
  };

  const confirmName = async () => {
    const name = nameInput.trim();
    if (!name || !naming) return;
    await saveUser(userPresets.map((p) => (p.id === naming.preset.id ? { ...p, name } : p)));
    setNaming(null);
    setNameInput("");
  };

  const deletePreset = async (preset: DevelopPreset) => {
    await saveUser(userPresets.filter((p) => p.id !== preset.id));
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
                    return (
                      <div
                        key={p.id}
                        className={`preset-card ${active ? "preset-card-active" : ""}`}
                        onClick={() => onApply(p)}
                        title={`Apply ${p.name}`}
                      >
                        <div className="preset-thumb">
                          <RenderedImage
                            src={renderUrl(p.edit, THUMB_EDGE)}
                            loadingClass="preset-thumb-empty"
                            loadingText="…"
                          />
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
        </>
      )}

      {naming && (
        <div className="modal-backdrop" onClick={() => setNaming(null)}>
          <div className="modal preset-name-modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-header">
              <div className="modal-title">
                Rename preset
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
                Rename
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
