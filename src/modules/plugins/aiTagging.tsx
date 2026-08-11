import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import type { ChairPhotoAPI, ChairPhotoModule } from "../registry";
import { useHostSelection } from "../host";

// The AI Tagging plugin — the first real chairphoto module. It contributes an
// inspector panel ("AI tags") and a settings panel (provider config). Its backend
// is behind the `ai` Cargo feature; if that's compiled out, the host won't let the
// module enable. Settings are auto-namespaced under "ai.*" by the host API, matching
// the Rust `read_config`. See docs/ai-tagging.md + docs/plugin-system.md.

// ---------------------------------------------------------------------------
// Cost estimation — bulk cloud runs only (Ollama is exempt).
//
// Figures are ROUGH ESTIMATES based on typical per-image token usage:
//   ~1 000 input tokens (image) + ~500 prompt tokens + ~300 output tokens.
// They are for display/confirmation only — actual billing depends on the
// provider, model, image resolution, and prompt length. Always check your
// provider's current pricing page.
//
// Structure: { [modelId]: costPerImageUSD }
// ---------------------------------------------------------------------------
const COST_PER_IMAGE_USD: Record<string, number> = {
  // Claude — https://www.anthropic.com/pricing (as of 2026-07)
  // ~1 500 input tokens @ $15/MTok + ~300 output tokens @ $75/MTok
  "claude-opus-4-8": 0.045,
  // ~1 500 input tokens @ $3/MTok + ~300 output tokens @ $15/MTok
  "claude-sonnet-4-6": 0.009,
  // ~1 500 input tokens @ $0.80/MTok + ~300 output tokens @ $4/MTok
  "claude-haiku-4-5-20251001": 0.0024,

  // OpenAI — https://openai.com/pricing (as of 2026-07)
  // ~1 500 input tokens @ $2.50/MTok + ~300 output tokens @ $10/MTok
  "gpt-4o": 0.0068,
  // ~1 500 input tokens @ $0.15/MTok + ~300 output tokens @ $0.60/MTok
  "gpt-4o-mini": 0.0004,
  // ~1 500 input tokens @ $2/MTok + ~300 output tokens @ $8/MTok
  "gpt-4.1": 0.0054,
  // ~1 500 input tokens @ $0.40/MTok + ~300 output tokens @ $1.60/MTok
  "gpt-4.1-mini": 0.0011,

  // Gemini — https://ai.google.dev/pricing (as of 2026-07)
  // ~1 500 input tokens @ $0.10/MTok + ~300 output tokens @ $0.40/MTok
  "gemini-2.0-flash": 0.00027,
  // ~1 500 input tokens @ $0.15/MTok + ~300 output tokens @ $0.60/MTok
  "gemini-2.5-flash": 0.0004,
  // ~1 500 input tokens @ $0.075/MTok + ~300 output tokens @ $0.30/MTok
  "gemini-1.5-flash": 0.0002,
  // ~1 500 input tokens @ $1.25/MTok + ~300 output tokens @ $5/MTok
  "gemini-1.5-pro": 0.0034,
};

/** Returns a human-readable cost estimate string, or null if model is unknown. */
function estimateBulkCost(
  model: string,
  photoCount: number,
): { estimatedUSD: number; display: string } | null {
  const perImage = COST_PER_IMAGE_USD[model];
  if (perImage == null) return null;
  const total = perImage * photoCount;
  // Format: $0.0042 for small amounts, $1.23 for larger
  const display = total < 0.01 ? `$${total.toFixed(4)}` : `$${total.toFixed(2)}`;
  return { estimatedUSD: total, display };
}

interface AiSuggestion {
  path: string;
  confidence: number;
  reason: string;
  existingTagId: number | null;
  isNew: boolean;
  description: string;
  synonyms: string[];
  /** H15c provenance: the photo id this suggestion was propagated from, or null for
   * a direct per-photo suggestion. */
  sourcePhotoId: number | null;
  /** H15d: basename of the representative photo (e.g. "DSC01234.ARW") for display,
   * pre-resolved by the backend. Null for direct suggestions. */
  sourcePhotoFilename: string | null;
}

// ── H15d — group-aware suggestion rendering ─────────────────────────────────

/** One group of suggestions: either a direct run (sourcePhotoId null) or a propagated
 * cluster (all items share the same sourcePhotoId + sourcePhotoFilename). */
interface SuggestionGroup {
  sourcePhotoId: number | null;
  sourcePhotoFilename: string | null;
  items: AiSuggestion[];
}

/** Partition a flat suggestion list into groups keyed by sourcePhotoId.
 * Direct suggestions (sourcePhotoId null) go first, then propagated groups. */
function groupSuggestions(suggestions: AiSuggestion[]): SuggestionGroup[] {
  const map = new Map<number | null, AiSuggestion[]>();
  for (const s of suggestions) {
    const key = s.sourcePhotoId ?? null;
    const existing = map.get(key);
    if (existing) {
      existing.push(s);
    } else {
      map.set(key, [s]);
    }
  }
  // Direct suggestions (null key) first, then propagated groups in insertion order.
  const groups: SuggestionGroup[] = [];
  const nullGroup = map.get(null);
  if (nullGroup) {
    groups.push({ sourcePhotoId: null, sourcePhotoFilename: null, items: nullGroup });
    map.delete(null);
  }
  for (const [key, items] of map.entries()) {
    groups.push({
      sourcePhotoId: key,
      sourcePhotoFilename: items[0]?.sourcePhotoFilename ?? null,
      items,
    });
  }
  return groups;
}

function AiPanel({ api }: { api: ChairPhotoAPI }) {
  useHostSelection(); // re-render when the active photo changes
  const photoId = api.getActivePhotoId();
  const [suggestions, setSuggestions] = useState<AiSuggestion[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [batchMsg, setBatchMsg] = useState("");
  const [question, setQuestion] = useState("");
  // Region ("tag this detail"): draw a box on the photo; only that crop is sent to the model.
  const [regionMode, setRegionMode] = useState(false);
  const [region, setRegion] = useState<{ x: number; y: number; w: number; h: number } | null>(null);
  const [previewUri, setPreviewUri] = useState<string | null>(null);
  // Bulk cloud confirm: non-null while waiting for user to confirm/cancel. `count` is the
  // selection size; `representatives` is how many frames actually get dispatched after
  // burst grouping (H15c) — cost is charged per representative, not per photo.
  const [bulkConfirm, setBulkConfirm] = useState<{
    count: number;
    representatives: number;
    model: string;
    display: string;
    unknown: boolean;
  } | null>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const dragStart = useRef<{ x: number; y: number } | null>(null);
  const selectedCount = api.getSelectedPhotos().length;

  // Engine + model picker, right where you run it. Reads/writes the same ai.* settings
  // as Preferences, so the choice sticks and the backend's read_config picks it up.
  const [provider, setProvider] = useState("ollama");
  const [model, setModel] = useState("");
  const [ollamaModels, setOllamaModels] = useState<string[]>([]);

  useEffect(() => {
    (async () => {
      const p = (await api.getSetting("provider")) || "ollama";
      setProvider(p);
      setModel((await api.getSetting(MODEL_KEY[p])) || "");
    })();
  }, []);

  useEffect(() => {
    if (provider !== "ollama") return;
    let alive = true;
    api
      .getSetting("ollama_url")
      .then((u) =>
        api.invoke<string[]>("ai_ollama_models", { ollamaUrl: u || "http://localhost:11434" }),
      )
      .then((m) => alive && setOllamaModels(m))
      .catch(() => alive && setOllamaModels([]));
    return () => {
      alive = false;
    };
  }, [provider]);

  const changeProvider = async (p: string) => {
    // Changing the provider invalidates any pending bulk cost estimate — clear it so the
    // user can't Proceed with a stale estimate for the wrong provider.
    setBulkConfirm(null);
    setProvider(p);
    await api.setSetting("provider", p);
    setModel((await api.getSetting(MODEL_KEY[p])) || "");
  };
  const changeModel = async (m: string) => {
    // Same: a different model means a different price; discard any open confirm dialog.
    setBulkConfirm(null);
    setModel(m);
    await api.setSetting(MODEL_KEY[provider], m);
  };
  const modelChoices = provider === "ollama" ? ollamaModels : CURATED_MODELS[provider] ?? [];
  const modelOpts = model && !modelChoices.includes(model) ? [model, ...modelChoices] : modelChoices;

  // Load persisted suggestions for this photo (no model call) — these survive
  // navigation AND app restarts (stored in the plugin's ai__suggestions table).
  useEffect(() => {
    if (photoId == null) return;
    setError("");
    setSuggestions(null);
    setRegion(null); // a box drawn on one photo must not carry to the next
    setPreviewUri(null);
    api
      .invoke<AiSuggestion[]>("ai_get_suggestions", { photoId })
      .then((s) => setSuggestions(s.length ? s : null))
      .catch(() => setSuggestions(null));
  }, [photoId]);

  // Load the photo's preview for the region picker, only while it's open.
  useEffect(() => {
    if (!regionMode || photoId == null) return;
    let alive = true;
    setPreviewUri(null);
    api
      .invoke<string>("get_preview", { photoId })
      .then((u) => alive && setPreviewUri(u))
      .catch(() => alive && setPreviewUri(null));
    return () => {
      alive = false;
    };
  }, [regionMode, photoId]);

  // Drag on the preview to box a detail; coords are normalized (0..1) so they map to the
  // full-resolution image the backend crops, regardless of the preview's displayed size.
  const normPoint = (e: ReactPointerEvent) => {
    const el = wrapRef.current;
    if (!el) return { x: 0, y: 0 };
    const r = el.getBoundingClientRect();
    return {
      x: Math.min(1, Math.max(0, (e.clientX - r.left) / r.width)),
      y: Math.min(1, Math.max(0, (e.clientY - r.top) / r.height)),
    };
  };
  const onRegionDown = (e: ReactPointerEvent) => {
    e.preventDefault();
    const p = normPoint(e);
    dragStart.current = p;
    setRegion({ x: p.x, y: p.y, w: 0, h: 0 });
    (e.target as Element).setPointerCapture?.(e.pointerId);
  };
  const onRegionMove = (e: ReactPointerEvent) => {
    if (!dragStart.current) return;
    const p = normPoint(e);
    const s = dragStart.current;
    setRegion({
      x: Math.min(s.x, p.x),
      y: Math.min(s.y, p.y),
      w: Math.abs(p.x - s.x),
      h: Math.abs(p.y - s.y),
    });
  };
  const onRegionUp = () => {
    if (!dragStart.current) return;
    dragStart.current = null;
    setRegion((r) => (r && (r.w < 0.02 || r.h < 0.02) ? null : r)); // discard an accidental tap
  };

  if (photoId == null) return <span className="panel-empty">Select a photo</span>;

  const removeLocal = (path: string) =>
    setSuggestions((prev) => prev?.filter((s) => s.path !== path) ?? null);

  const run = async () => {
    setBusy(true);
    setError("");
    try {
      const s = await api.invoke<AiSuggestion[]>("ai_suggest_tags", {
        photoId,
        region: region ?? undefined,
      });
      setSuggestions(s);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  // Ask a follow-up question (e.g. "what kind of boat?") — the model re-examines the photo
  // with the question and adds refined suggestions to the list.
  const ask = async (q: string) => {
    const text = q.trim();
    if (!text) return;
    setBusy(true);
    setError("");
    try {
      const s = await api.invoke<AiSuggestion[]>("ai_suggest_tags", {
        photoId,
        question: text,
        region: region ?? undefined,
      });
      setSuggestions(s);
      setQuestion("");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };
  const refineLeaf = (path: string) => path.split("/").pop() || path;

  const accept = async (s: AiSuggestion) => {
    try {
      await api.invoke("ai_accept_suggestion", { photoId, path: s.path });
      api.showToast(`Tagged: ${s.path}`);
      removeLocal(s.path);
      api.notifyChange(); // refresh the app's tags/counts
    } catch (e) {
      setError(String(e));
    }
  };

  const reject = async (s: AiSuggestion) => {
    try {
      await api.invoke("ai_reject_suggestion", { photoId, path: s.path });
      removeLocal(s.path);
    } catch (e) {
      setError(String(e));
    }
  };

  // Batch: run grouped suggestions across the selection (H15c). The backend clusters the
  // selection into bursts, dispatches ONLY each cluster's representative to the provider
  // (sequential — local Ollama is single-GPU), and propagates the suggestions to the rest
  // of each cluster as reviewable pending rows. This is the ~90% cost cut on burst-heavy
  // shoots. Each member's suggestions are persisted; review them per photo.
  const dispatchBatch = async () => {
    const sel = api.getSelectedPhotos();
    setBusy(true);
    setBulkConfirm(null);
    setError("");
    setBatchMsg(`Grouping ${sel.length}…`);
    try {
      const res = await api.invoke<{
        total: number;
        representatives: number;
        dispatched: number;
        propagated: number;
      }>("ai_suggest_tags_grouped", { photoIds: sel.map((p) => p.id) });
      setBatchMsg(
        `Done — ${res.total} photos → ${res.representatives} representatives, ` +
          `${res.propagated} suggestions. Review each photo.`,
      );
    } catch (e) {
      setError(String(e));
      setBatchMsg("");
    }
    setBusy(false);
    // Refresh the active photo's list.
    api
      .invoke<AiSuggestion[]>("ai_get_suggestions", { photoId })
      .then((s) => setSuggestions(s.length ? s : null))
      .catch(() => {});
  };

  // For cloud providers (non-Ollama), intercept the batch run to show a cost estimate
  // and require an explicit confirm before dispatching. Single-photo runs stay friction-free.
  // The estimate reflects burst grouping: cost is per representative (M), not per photo (N).
  const runBatch = async () => {
    const isCloud = provider !== "ollama";
    const sel = api.getSelectedPhotos();
    if (!isCloud || sel.length <= 1) {
      // Local (Ollama) or single photo — no gate needed.
      void dispatchBatch();
      return;
    }
    // Cloud + multiple photos: ask the backend how the selection collapses to
    // representatives, then price by that reduced count.
    let representatives = sel.length;
    let total = sel.length;
    try {
      const est = await api.invoke<{ total: number; representatives: number }>(
        "ai_grouped_estimate",
        { photoIds: sel.map((p) => p.id) },
      );
      representatives = est.representatives;
      total = est.total;
    } catch {
      /* fall back to worst-case (one call per photo) if grouping estimate fails */
    }
    const estimate = estimateBulkCost(model, representatives);
    setBulkConfirm({
      count: total,
      representatives,
      model: model || "default model",
      display: estimate?.display ?? "unknown",
      unknown: estimate == null,
    });
  };

  // Apply one suggestion's tag to every selected photo.
  const acceptForAll = async (s: AiSuggestion) => {
    const sel = api.getSelectedPhotos();
    for (const p of sel) {
      try {
        await api.invoke("ai_accept_suggestion", { photoId: p.id, path: s.path });
      } catch {
        /* skip */
      }
    }
    api.showToast(`Tagged ${sel.length} photos: ${s.path}`);
    removeLocal(s.path);
    api.notifyChange();
  };

  // ── H15d — group-level actions ──────────────────────────────────────────────

  /** Accept every suggestion in a propagated group (adds each tag to this photo). */
  const acceptGroup = async (group: SuggestionGroup) => {
    for (const s of group.items) {
      try {
        await api.invoke("ai_accept_suggestion", { photoId, path: s.path });
      } catch {
        /* skip individual failures */
      }
    }
    const paths = group.items.map((s) => s.path);
    setSuggestions((prev) => prev?.filter((s) => !paths.includes(s.path)) ?? null);
    api.showToast(`Added ${paths.length} tags from ${group.sourcePhotoFilename ?? "representative"}`);
    api.notifyChange();
  };

  /** Reject every suggestion in a propagated group (won't be re-proposed). */
  const rejectGroup = async (group: SuggestionGroup) => {
    for (const s of group.items) {
      try {
        await api.invoke("ai_reject_suggestion", { photoId, path: s.path });
      } catch {
        /* skip */
      }
    }
    const paths = group.items.map((s) => s.path);
    setSuggestions((prev) => prev?.filter((s) => !paths.includes(s.path)) ?? null);
  };

  /** Re-run the AI directly on this photo, superseding any propagated suggestions. */
  const rerunDirect = async () => {
    setBusy(true);
    setError("");
    try {
      const s = await api.invoke<AiSuggestion[]>("ai_suggest_tags", { photoId });
      setSuggestions(s.length ? s : null);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  // Whether the current photo has any propagated (burst-cluster) suggestions.
  const hasPropagated = suggestions?.some((s) => s.sourcePhotoId != null) ?? false;

  return (
    <div className="ai-panel">
      <div className="ai-engine">
        <select
          className="tag-input"
          value={provider}
          onChange={(e) => changeProvider(e.target.value)}
          title="Which AI engine to use"
          disabled={bulkConfirm != null}
        >
          {PROVIDERS.map((p) => (
            <option key={p.id} value={p.id}>
              {p.id === "ollama" ? "Ollama (local)" : p.id[0].toUpperCase() + p.id.slice(1)}
            </option>
          ))}
        </select>
        <select
          className="tag-input"
          value={model}
          onChange={(e) => changeModel(e.target.value)}
          title="Which model to use"
          disabled={bulkConfirm != null}
        >
          {model === "" && <option value="">Default model…</option>}
          {modelOpts.map((m) => (
            <option key={m} value={m}>
              {m}
            </option>
          ))}
        </select>
      </div>
      <div className="row">
        <button className="chip chip-on" onClick={run} disabled={busy}>
          {busy ? "Thinking…" : region ? "Suggest for region" : "Suggest tags"}
        </button>
        <button
          className={`chip ${regionMode ? "chip-on" : ""}`}
          onClick={() => {
            const next = !regionMode;
            setRegionMode(next);
            if (!next) setRegion(null); // closing the picker clears the box
          }}
          disabled={busy}
          title="Draw a box to tag just part of the photo"
        >
          ▭ Region
        </button>
        {selectedCount > 1 && (
          <button className="chip" onClick={() => void runBatch()} disabled={busy}>
            Suggest for {selectedCount} selected
          </button>
        )}
      </div>
      {regionMode && (
        <div className="ai-region">
          {previewUri ? (
            <div
              className="ai-region-wrap"
              ref={wrapRef}
              onPointerDown={onRegionDown}
              onPointerMove={onRegionMove}
              onPointerUp={onRegionUp}
            >
              <img className="ai-region-img" src={previewUri} draggable={false} alt="" />
              {region && (region.w > 0 || region.h > 0) && (
                <div
                  className="ai-region-box"
                  style={{
                    left: `${region.x * 100}%`,
                    top: `${region.y * 100}%`,
                    width: `${region.w * 100}%`,
                    height: `${region.h * 100}%`,
                  }}
                />
              )}
            </div>
          ) : (
            <div className="panel-empty">Loading preview…</div>
          )}
          <div className="ai-region-hint">
            <span>{region ? "Suggesting for the boxed area." : "Drag on the photo to box a detail."}</span>
            {region && (
              <button className="chip" onClick={() => setRegion(null)}>
                clear
              </button>
            )}
          </div>
        </div>
      )}
      {batchMsg && <div className="ai-sug-reason">{batchMsg}</div>}
      {/* Bulk cloud cost estimate + confirm step.
          Shown only when the user clicks "Suggest for N selected" with a cloud engine.
          Single-photo suggests skip this entirely. */}
      {bulkConfirm && (
        <div className="ai-bulk-confirm">
          <div className="ai-bulk-confirm-title">Send {bulkConfirm.representatives} photos to cloud AI?</div>
          <div className="ai-bulk-confirm-detail">
            <span>Grouping:</span>
            <span>
              {bulkConfirm.count} photos → {bulkConfirm.representatives} representatives → {bulkConfirm.display}
              {bulkConfirm.unknown && (
                <span className="ai-sug-reason"> — unknown model, check provider pricing</span>
              )}
            </span>
            <span>Model:</span>
            <span>{bulkConfirm.model}</span>
            <span className="ai-bulk-confirm-note">
              Burst grouping sends only one representative frame per cluster; its suggestions
              are propagated to the rest for review. Rough estimate only — each image is
              ~1 500 input tokens + ~300 output tokens. Check your provider for exact billing.
            </span>
          </div>
          <div className="row">
            <button
              className="chip chip-on"
              onClick={() => void dispatchBatch()}
              disabled={busy}
            >
              Proceed
            </button>
            <button
              className="chip chip-danger"
              onClick={() => setBulkConfirm(null)}
              disabled={busy}
            >
              Cancel
            </button>
          </div>
        </div>
      )}
      {error && <div className="modal-error">{error}</div>}
      {suggestions?.length === 0 && <div className="panel-empty">No suggestions</div>}
      {/* H15d — group-aware suggestion rendering.
          Direct suggestions (sourcePhotoId null) appear ungrouped at the top. Propagated
          groups show a header with provenance + accept-for-group / reject-for-group. A
          "re-run directly" shortcut replaces propagated rows with a fresh model call on
          this specific photo — non-destructive (the user still reviews before any tag
          is applied). */}
      {hasPropagated && (
        <div className="ai-rerun-row">
          <span className="ai-sug-reason">
            These suggestions were propagated from a burst representative.
          </span>
          <button
            className="chip"
            onClick={() => void rerunDirect()}
            disabled={busy}
            title="Dispatch this photo directly to the AI provider, replacing the propagated suggestions"
          >
            Re-run directly
          </button>
        </div>
      )}
      {suggestions != null &&
        groupSuggestions(suggestions).map((group) => (
          <div key={group.sourcePhotoId ?? "direct"} className="ai-suggestion-group">
            {/* Group header — only shown for propagated groups */}
            {group.sourcePhotoId != null && (
              <div className="ai-group-header">
                <span className="ai-group-provenance">
                  from {group.sourcePhotoFilename ?? `photo #${group.sourcePhotoId}`}
                </span>
                <div className="row">
                  <button
                    className="chip"
                    onClick={() => void acceptGroup(group)}
                    disabled={busy}
                    title={`Accept all ${group.items.length} suggestions from this representative`}
                  >
                    ✓ accept group ({group.items.length})
                  </button>
                  <button
                    className="chip"
                    onClick={() => void rejectGroup(group)}
                    disabled={busy}
                    title="Reject all suggestions from this representative (won't be re-proposed)"
                  >
                    ✗ reject group
                  </button>
                </div>
              </div>
            )}
            {group.items.map((s) => (
              <div key={s.path} className="ai-suggestion">
                <div className="ai-sug-main">
                  <span className="ai-sug-path">
                    {s.path}
                    {s.isNew && <span className="ai-sug-new"> new</span>}
                  </span>
                  <span className="ai-sug-conf">{Math.round(s.confidence * 100)}%</span>
                </div>
                {s.reason && <div className="ai-sug-reason">{s.reason}</div>}
                {s.isNew && s.description && (
                  <div className="ai-sug-desc">"{s.description}"</div>
                )}
                {s.isNew && s.synonyms.length > 0 && (
                  <div className="ai-sug-reason">synonyms: {s.synonyms.join(", ")}</div>
                )}
                <div className="row">
                  <button className="chip" onClick={() => accept(s)}>
                    ✓ add
                  </button>
                  {selectedCount > 1 && (
                    <button className="chip" onClick={() => acceptForAll(s)} title="Add to all selected">
                      ✓ all ({selectedCount})
                    </button>
                  )}
                  <button className="chip" onClick={() => reject(s)} title="Won't be suggested again">
                    ✗ reject
                  </button>
                  <button
                    className="chip"
                    onClick={() =>
                      ask(
                        `What specific kind of ${refineLeaf(s.path)} is this? Suggest a more specific tag.`,
                      )
                    }
                    disabled={busy}
                    title={`Ask the model to drill into a more specific ${refineLeaf(s.path)}`}
                  >
                    ↳ more specific
                  </button>
                </div>
              </div>
            ))}
          </div>
        ))}
      <div className="ai-followup">
        <input
          className="tag-input"
          placeholder="Ask a follow-up… e.g. what kind of boat?"
          value={question}
          onChange={(e) => setQuestion(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && ask(question)}
          disabled={busy}
        />
        <button className="chip" onClick={() => ask(question)} disabled={busy || !question.trim()}>
          Ask
        </button>
      </div>
    </div>
  );
}

// The selectable engines. "ollama" is local/private; the rest send the image to a
// cloud vision API (explicit opt-in via the per-provider API key).
const PROVIDERS = [
  { id: "ollama", label: "Local — Ollama (private, on this machine)" },
  { id: "claude", label: "Cloud — Claude (Anthropic)" },
  { id: "openai", label: "Cloud — OpenAI (GPT)" },
  { id: "gemini", label: "Cloud — Gemini (Google)" },
] as const;

// The setting key holding each provider's model.
const MODEL_KEY: Record<string, string> = {
  ollama: "ollama_model",
  claude: "cloud_model",
  openai: "openai_model",
  gemini: "gemini_model",
};

// Known vision models per cloud provider (a saved custom value is added on top so it
// still shows). Ollama's list is fetched live from the server instead.
const CLAUDE_MODELS = ["claude-opus-4-8", "claude-sonnet-4-6", "claude-haiku-4-5-20251001"];
const OPENAI_MODELS = ["gpt-4o", "gpt-4o-mini", "gpt-4.1", "gpt-4.1-mini"];
const GEMINI_MODELS = ["gemini-2.0-flash", "gemini-2.5-flash", "gemini-1.5-flash", "gemini-1.5-pro"];
const CURATED_MODELS: Record<string, string[]> = {
  claude: CLAUDE_MODELS,
  openai: OPENAI_MODELS,
  gemini: GEMINI_MODELS,
};

// Per-provider settings fields (model + key), shown only for the selected provider.
const PROVIDER_FIELDS: Record<string, readonly (readonly [string, string])[]> = {
  ollama: [
    ["ollama_url", "Ollama URL"],
    ["ollama_model", "Ollama model"],
  ],
  claude: [
    ["cloud_model", "Claude model"],
    ["cloud_api_key", "Claude API key"],
  ],
  openai: [
    ["openai_model", "OpenAI model"],
    ["openai_api_key", "OpenAI API key"],
  ],
  gemini: [
    ["gemini_model", "Gemini model"],
    ["gemini_api_key", "Gemini API key"],
  ],
};
const KEY_FIELDS = new Set(["cloud_api_key", "openai_api_key", "gemini_api_key"]);

const DEFAULTS: Record<string, string> = {
  provider: "ollama",
  ollama_url: "http://localhost:11434",
  ollama_model: "llava:latest",
  cloud_model: "claude-sonnet-4-6",
  cloud_api_key: "",
  openai_model: "gpt-4o",
  openai_api_key: "",
  gemini_model: "gemini-2.0-flash",
  gemini_api_key: "",
  existing_only: "false",
  min_confidence: "0",
  prompt_template: "", // empty = built-in default prompt
};

function AiSettings({ api }: { api: ChairPhotoAPI }) {
  const [values, setValues] = useState<Record<string, string>>(DEFAULTS);
  const [status, setStatus] = useState("");
  const [ollamaModels, setOllamaModels] = useState<string[]>([]);
  const [advanced, setAdvanced] = useState(false);
  const [defaultPrompt, setDefaultPrompt] = useState("");

  // Fetch the built-in prompt so we can show it (as the placeholder + the "Load default").
  useEffect(() => {
    api.invoke<string>("ai_default_prompt", {}).then(setDefaultPrompt).catch(() => {});
  }, []);

  useEffect(() => {
    (async () => {
      const next: Record<string, string> = { ...DEFAULTS };
      for (const key of Object.keys(DEFAULTS)) {
        const v = await api.getSetting(key);
        if (v != null && v !== "") next[key] = v;
      }
      setValues(next);
    })();
  }, []);

  // Live list of installed Ollama models for the model dropdown.
  useEffect(() => {
    if (values.provider !== "ollama") return;
    let alive = true;
    api
      .invoke<string[]>("ai_ollama_models", { ollamaUrl: values.ollama_url })
      .then((m) => alive && setOllamaModels(m))
      .catch(() => alive && setOllamaModels([]));
    return () => {
      alive = false;
    };
  }, [values.ollama_url, values.provider]);

  const set = (key: string, value: string) => setValues((v) => ({ ...v, [key]: value }));

  // A <select> whose options are `choices` plus the current value (so a saved/custom
  // value always appears), falling back to a text input when there are no choices.
  const modelSelect = (key: string, choices: string[]) => {
    const current = values[key] ?? "";
    const opts = choices.includes(current) || current === "" ? choices : [current, ...choices];
    if (opts.length === 0) {
      return (
        <input
          className="tag-input"
          value={current}
          placeholder="No models found — type a model name"
          onChange={(e) => set(key, e.target.value)}
        />
      );
    }
    return (
      <select className="tag-input" value={current} onChange={(e) => set(key, e.target.value)}>
        {current === "" && <option value="">Select a model…</option>}
        {opts.map((m) => (
          <option key={m} value={m}>
            {m}
          </option>
        ))}
      </select>
    );
  };

  const save = async () => {
    for (const key of Object.keys(DEFAULTS)) await api.setSetting(key, values[key] ?? "");
    setStatus("Saved");
  };

  const provider = values.provider ?? "ollama";
  const fields = PROVIDER_FIELDS[provider] ?? PROVIDER_FIELDS.ollama;
  const isCloud = provider !== "ollama";

  return (
    <div>
      <h3>AI Tagging</h3>
      {/* Provider picks which engine "Suggest tags" uses. Local keeps images on this
          machine; cloud sends the image to that provider (explicit opt-in). */}
      <div className="iptc-row">
        <label className="iptc-label">Engine</label>
        <select
          className="tag-input"
          value={provider}
          onChange={(e) => set("provider", e.target.value)}
        >
          {PROVIDERS.map((p) => (
            <option key={p.id} value={p.id}>
              {p.label}
            </option>
          ))}
        </select>
      </div>
      {isCloud && (
        <div className="iptc-row">
          <span className="term-note">
            Cloud mode uploads each photo to this provider when you click Suggest tags.
          </span>
        </div>
      )}
      {fields.map(([key, label]) => (
        <div key={key} className="iptc-row">
          <label className="iptc-label">{label}</label>
          {key === "ollama_model" ? (
            modelSelect(key, ollamaModels)
          ) : key.endsWith("_model") ? (
            modelSelect(key, CURATED_MODELS[provider] ?? [])
          ) : (
            <input
              className="tag-input"
              type={KEY_FIELDS.has(key) ? "password" : "text"}
              value={values[key] ?? ""}
              onChange={(e) => set(key, e.target.value)}
            />
          )}
        </div>
      ))}
      <div className="iptc-row">
        <label className="term-export">
          <input
            type="checkbox"
            checked={values.existing_only === "true"}
            onChange={(e) => set("existing_only", e.target.checked ? "true" : "false")}
          />
          Suggest only existing tags (no new-tag discovery)
        </label>
      </div>
      <div className="iptc-row">
        <label className="iptc-label">Min confidence (0–1)</label>
        <input
          className="tag-input"
          type="number"
          min="0"
          max="1"
          step="0.05"
          value={values.min_confidence ?? "0"}
          onChange={(e) => set("min_confidence", e.target.value)}
        />
      </div>
      <div className="iptc-row">
        <button className="chip" onClick={() => setAdvanced((a) => !a)}>
          {advanced ? "▾ Advanced — prompt" : "▸ Advanced — edit prompt"}
        </button>
      </div>
      {advanced && (
        <div className="iptc-row" style={{ flexDirection: "column", alignItems: "stretch", gap: 6 }}>
          <span className="term-note">
            The exact instructions sent to the model. Leave blank to use the built-in
            default (shown greyed below). Placeholders are filled in automatically:{" "}
            <code>{"{taxonomy}"}</code> (your tag list), <code>{"{new_tags}"}</code> (new-tag
            rules), <code>{"{rejected}"}</code> (tags you've rejected). Keep the JSON-shape
            line or suggestions won't parse.
          </span>
          <textarea
            className="tag-input"
            style={{ minHeight: 200, fontFamily: "monospace", fontSize: 12, resize: "vertical" }}
            placeholder={defaultPrompt || "(loading the default prompt…)"}
            value={values.prompt_template ?? ""}
            onChange={(e) => set("prompt_template", e.target.value)}
          />
          <div className="row">
            <button
              className="chip"
              onClick={() => set("prompt_template", defaultPrompt)}
              disabled={!defaultPrompt}
              title="Load the built-in prompt into the editor so you can tweak it"
            >
              Load default
            </button>
            <button
              className="chip"
              onClick={() => set("prompt_template", "")}
              disabled={!values.prompt_template}
              title="Clear — revert to the built-in default"
            >
              Reset to default
            </button>
          </div>
        </div>
      )}
      <div className="iptc-actions">
        <button className="chip chip-on" onClick={save}>
          Save AI settings
        </button>
        <span className="iptc-status">{status}</span>
      </div>
    </div>
  );
}

export const aiTaggingModule: ChairPhotoModule = {
  id: "ai",
  name: "AI Tagging",
  version: "0.1.0",
  description:
    "Suggest tags from your taxonomy (and propose new ones) using a vision model — local (Ollama) or cloud.",
  backendFeature: "ai",
  onLoad(api) {
    api.registerPanel({
      id: "ai-tags",
      label: "AI tags",
      slot: "inspector",
      render: () => <AiPanel api={api} />,
    });
    api.registerSettingsPanel(() => <AiSettings api={api} />);
  },
};
