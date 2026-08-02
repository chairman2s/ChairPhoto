---
title: "Editing"
description: "Non-destructive crop, tone, film looks and LUTs, saved as named versions."
tags:
  - chairphoto/module
  - chairphoto/render
aliases:
  - "Versions"
  - "Develop"
  - "LUTs"
---

# Editing

A non-destructive editor for crop, tone and film looks. Your original RAW or JPEG is never
modified — that is a binding architecture invariant, not a policy this module chose.

Editing arrives as a full-window **Develop** tab rather than a modal, following the darkroom
metaphor: a version bar, crop with social aspect presets and Free (drag to move, resize from the
corners, live pixel-size readout), composition overlays (none, thirds, phi grid, golden spiral —
remembered in `editor.crop_overlay`), tone sliders for EV, contrast, highlights, shadows and
white balance (double-click any slider to reset), a live proxy preview, and auto-save to the
active version.

A photo can carry **multiple versions** — several crops, or the same frame at different
exposures — each independently editable and exportable.

**Packaging.** The render engine is gated behind the `edit` Cargo feature, while the loupe's
render hook belongs to the bundled Basic Editor module. Disable the module and the loupe falls
back to the original image with the Develop and Edit entry points hidden. Full-resolution RAW
decode for export sits behind the `raw` feature via LibRaw.

## Goal & requirements

A simple, non-destructive editor for the social/export workflow:
- **Crop** with predefined **social aspect-ratio presets** (Instagram, TikTok, Snapchat,
  Facebook, etc.) plus free/original.
- **Exposure / tone** control.
- Delivered as a **module** (not core), mirroring AI tagging.
- **NEVER changes the original** RAW/JPEG (binding invariant).
- **Multiple versions per photo** — e.g. several crops, or the same shot at different
  exposures — each independently editable and exportable.

## Non-destructive guarantee (binding)

This is already how chairphoto works and the editor must not break it:
- The scanner and the path **resolver** only ever *read* photo files; nothing writes into
  the user's photo folders (AGENTS.md).
- Edits live as **JSON in the catalog**, never in the photo file. We do **not** write
  crop/exposure into the `.xmp` develop fields — that namespace belongs to darktable/RawTherapee
  and the merge-safe XMP invariant forbids clobbering it.
- Producing an edited image always writes a **new file** (export) or renders to memory
  (preview). The original is input-only.
- A test will assert that creating/rendering versions never opens an original for writing.

## Versions model (core)

Today the edit record is one-per-photo (`photo_edits`, `photo_id PRIMARY KEY`). Multiple
versions need a new **core** table (data only — no processing — so versions survive even if
the editing module is disabled, like `photo_edits` does):

```sql
CREATE TABLE photo_versions (
    id         INTEGER PRIMARY KEY,
    photo_id   INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,            -- "Instagram square", "Bright", …
    edit_json  TEXT NOT NULL,            -- crop + tone for THIS version (shape below)
    position   INTEGER NOT NULL,         -- display order
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
```

- The **original is the implicit base** (unedited); named versions are derivatives that all
  reference the *same* original file via the resolver and render by applying their `edit_json`.
- CRUD: create / rename / delete / **duplicate** (clone a version to tweak) / reorder.
- The single `photo_edits` record is subsumed by versions.

### UX

- A **"Versions" panel in the inspector** for the selected photo: list, add, rename,
  duplicate, delete, pick the active version to edit/preview.
- The **grid** shows the master thumbnail with a small **"N versions" badge** — no extra
  tiles, no stacks. Versions are chosen at export time.

## Edit record shape (resolution-independent)

```jsonc
{
  "version": 1,         // edit_json schema version — lets the render engine apply the
  // correct interpretation/defaults per record if the tone set is extended.
  // Records are canonical (never baked), so old versions must keep rendering.
  "crop": { "x": 0.10, "y": 0.0, "w": 0.80, "h": 1.0, "aspect": "1:1" },
  // crop is FRACTIONS (0–1) of the original, so one record works on the small preview
  // proxy (live editing) and the full-res source (export) alike.
  "tone": {
    "ev": 0.5,          // exposure in stops
    "contrast": 0.0,    // -1..1
    "highlights": 0.0,  // -1..1 (recover / boost)
    "shadows": 0.0,     // -1..1 (lift / crush)
    "wb": { "temp": 0, "tint": 0 }  // relative offsets, applied post-decode in RGB
  }
}
```

Decided tone set: **EV + contrast + highlights/shadows + white balance**. WB here is a
post-decode RGB temperature/tint adjustment (approximate, not raw-domain WB) — adequate for
a simple editor; true raw-domain WB is a later, RAW-pipeline concern. Straighten/rotate is
not implemented; crop is axis-aligned.

### Film looks

The record grew optional **look** fields for develop presets (monochrome styles, film
simulations). All are serde-defaulted, so older records parse and render bit-identically:

```jsonc
{
  "bw":   { "enabled": true, "r": 0.9, "g": 0.15, "b": -0.05 },  // B&W channel mixer;
  // weights normalized by their sum (red-filter recipe shown). null/absent = colour.
  "split": { "shadow_hue": 35, "shadow_sat": 0.25,               // split toning; sepia/
             "highlight_hue": 45, "highlight_sat": 0.12,          // selenium = same hue
             "balance": 0 },                                      // both ends
  "grain": { "amount": 0.5, "size": 1.2, "seed": 0 },  // deterministic value noise in
  // normalized image space — preview and export show the SAME pattern; never time-seeded
  "fade": 0.2,        // 0..1 lifted matte blacks
  "vignette": -0.3,   // -1..1 (negative darkens corners)
  "lut": { "file": "kodak-2383.cube", "amount": 1 }  // .cube 3D LUT by BARE FILENAME,
  // resolved against <app data dir>/luts/ — portable; a missing file is non-fatal
}
```

Per-pixel processing order (fixed, so preview matches export): tone (EV/WB/regions/
contrast) → saturation/vibrance → **B&W mixer** → **LUT** (trilinear) → **split toning**
→ **fade** → **vignette** → **grain**. Implemented in `plugins/edit/look.rs`; the .cube
parser + mtime cache in `plugins/edit/cube.rs`; LUT files managed via
`list_luts`/`import_lut`/`delete_lut`.

**Develop presets** (`src/modules/presets.ts`): built-in library of parameter recipes
(monochrome filter styles, sepia/selenium, film stocks like Tri-X/Kodak Gold/Portra/
Ektachrome/Kodachrome/Velvia) + user presets saved under the settings key
`basic-editor.presets`. Presets are look-only — never crop/straighten. The preset browser
(`src/components/PresetBrowser.tsx`) shows the current photo rendered per preset via one
`render_edit_batch` call (proxy decoded once).

## Aspect-ratio presets (social), as data

A `(label, ratio, platform hint)` list, easy to extend:

| Label | Ratio | Where |
|-------|-------|-------|
| Original / Free | — | any |
| Square | 1:1 | Instagram, Facebook |
| Portrait | 4:5 | Instagram (max portrait), Facebook feed |
| Landscape | 1.91:1 | Instagram / Facebook link |
| Vertical | 9:16 | Reels, Stories, TikTok, Snapchat, Shorts, FB Stories |
| Wide | 16:9 | Facebook, video |
| 3:2 / 2:3, 4:3 / 3:4 | — | general |

Aspect ratio only. Optional **per-platform pixel resize** on export (e.g. 1080×1350) is not
implemented — the crop fixes shape, resize would fix pixels.

## Module split (mirrors AI tagging)

- **Core (Rust):** `photo_versions` table + CRUD/commands. No image processing.
- **Module engine (Rust, behind an `edit` Cargo feature so the backend still builds
  `--no-default-features`):** `render_edit(source, edit_json) → JPEG` — applies normalized
  crop + tone (EV→linear gain, contrast, highlights/shadows, WB) using the `image` crate.
- **Module UI (TS):** crop overlay with the aspect presets, tone sliders, the Versions
  panel, live preview, and `registerEditRenderer(...)` so the loupe shows the edited result.

## Rendering & export

- **Live preview:** render the cached **preview proxy** (embedded JPEG, ~fast) as sliders/crop
  change (debounced). Proxy quality is fine for judging an edit.
- **Loupe:** shows the active version's render when the module is enabled; otherwise the
  unedited preview (the core edit contract already falls back).
- **Edited export — decided: render from a full RAW decode.** "Show off" (JPEG) renders each
  chosen version from the **full-resolution source**: a decoded RAW for RAW originals, or the
  original JPEG for JPEG-only photos. This gates *edited RAW export* on a RAW decoder that
  doesn't exist yet (see Phase 3) — JPEG-only originals can export edited immediately.
  Per-version filenames (`<stem> - <version>.jpg`), collision-safe.
- **Hand-off export (RAW + XMP) stays unedited** — you're giving the RAW to another editor;
  crop/exposure are not written into the sidecar (merge-safe invariant).

