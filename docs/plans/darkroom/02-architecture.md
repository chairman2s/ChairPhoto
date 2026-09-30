# Architecture: Darkroom

## Fit

The Darkroom **evolves the existing Basic Editor module in place** — same module id
(`basic-editor`), same registered edit renderer (the loupe contract), same edit record,
same versions. It is a new *surface* over the existing engine, not a new pipeline:

- `src/components/EditorView.tsx` (the current Develop tab, 1.3k lines) is **succeeded** by
  a Darkroom view composed of: stage (print + tone strip + actions), proof sheet, duel,
  and the classic slider rail (which reuses the existing slider/crop/perspective controls).
- `crates/core/src/plugins/edit/` (render engine) gains one pipeline stage (zone curve) and
  two small helpers (zone masses, auto-tone suggestion). Order and record stay canonical.
- The pop-out loupe is the print with **zero new protocol**: `loupe:photo` already carries
  `{photoId, editJson}` — the Darkroom broadcasts its working edit (throttled) and the
  loupe renders it exactly as it renders a version today.
- Presets (`src/modules/presets.ts`) become the seed data for proof-sheet spreads.
- Versions (`photo_versions` + CRUD commands) are the fork/save substrate, unchanged.

## Endpoints

All Tauri commands, gated behind the `edit` Cargo feature like their siblings:

- `render_edit(photo_id, edit_json, max_edge, hi_res)` — **existing, unchanged**: live
  preview renders and the loupe print.
- `render_edit_batch(photo_id, edit_jsons[], max_edge)` — **existing, unchanged**: the
  proof sheet (12 records, proxy decoded once) and duel rounds (2 records). Its
  shared-downscale precondition (records don't crop differently) holds for both.
- `edit_zone_masses(photo_id, edit_json) -> [f32; 8]` — **new**: share of pixels per EV
  zone of the *rendered working state* (a coarsened `luma_histogram` — the sampling code
  exists in `export/mod.rs`, gets promoted into the edit plugin). Drives the tone strip's
  fill heights; debounced with the preview render.
- `suggest_auto_tone(photo_id) -> String` — **new**: a classical (histogram-stretch based,
  no ML) edit-json *fragment* for the proof sheet's "Auto" cells. The learned-model
  version (Deep Guided Exposure Correction, see 00-status) is a post-v1 drop-in behind
  the same command.

## Data

- **No new tables.** Versions, settings, LUTs all exist.
- **Edit record schema v2** (serde-defaulted; v1 records parse and render bit-identically):
  `"zones": [f32; 8]` — per-EV-zone exposure offsets from the tone strip, rendered as one
  smooth luma-keyed gain curve. Everything else in the record is unchanged.
- Settings keys (namespaced like the module's existing ones): `basic-editor.proofSpread`
  (which looks get dealt), `basic-editor.duelDims` (round order), `basic-editor.printOnLoupe`.

## Flow

Open a keeper in the Darkroom → working state = active version's record (or as-shot):

1. **Proof sheet**: TS builds ~12 candidate records = `suggest_auto_tone` fragment ×
   looks (presets/films/B&W) + "as shot" → one `render_edit_batch` → click adopts that
   record as the working state.
2. **Tone strip**: `edit_zone_masses(working)` fills the strip; dragging a zone mutates
   `zones[i]` → debounced `render_edit` (stage) + throttled `broadcastPhoto(photoId,
   working)` (loupe print) + refreshed masses.
3. **Duel**: TS generates A/B variants of the working state for the round's dimension →
   `render_edit_batch` of 2 → the pick becomes the working state; "keep both" calls
   `create_version` + `set_version_edit` for the loser.
4. **Save**: `set_version_edit` on the active version (or `create_version` first) —
   auto-save semantics identical to the current Develop tab.

Renders stay on the proxy tier for interaction (the existing performance contract);
export is untouched (full-res path already renders any record, and serde-defaulting
means v2 records with zones export correctly once the engine stage exists).

## External

None. Everything renders locally; no new dependencies (the zone curve is a few dozen
lines against the existing `image`-crate pipeline).
