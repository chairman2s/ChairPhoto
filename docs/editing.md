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
  "straighten": 1.5,    // degrees; rotate about the centre to level the frame
  "perspective": {      // four-corner keystone correction; absent = geometry untouched
    "tl": [0.098, 0.171], "tr": [0.853, 0.106],   // the SUBJECT's corners, as fractions
    "br": [0.878, 0.900], "bl": [0.083, 0.921],   // of the source (same units as crop)
    "aspect": 0.79      // optional output width/height; absent = mean edge lengths
  },
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
a simple editor; true raw-domain WB is a later, RAW-pipeline concern. Crop is axis-aligned;
tilt is `straighten`'s job and off-axis shots are `perspective`'s.

### Geometry, and why its order is fixed

Three stages run before the look, always in this order — `perspective` → `straighten` →
`crop` — because each redefines the frame the next one measures against:

- **`perspective`** maps the named quadrilateral back onto a rectangle, undoing the
  keystone of a picture or document photographed off-axis. It runs first and it changes the
  canvas size, so `straighten`'s centre and `crop`'s fractions refer to the *rectified*
  image, not the original. The engine solves the 8-DOF homography from the output rectangle
  onto the quad and inverse-samples it bilinearly.
- **`straighten`** rotates about the centre. The UI pairs it with an inscribed crop so the
  rotation's black corners stay out of frame.
- **`crop`** is axis-aligned, in fractions of whatever the two stages above produced.

Two guards are load-bearing rather than defensive habit. A degenerate quad — collinear or
coincident corners — has no well-defined rectification, so it leaves the geometry alone
instead of failing the render, the same contract a missing LUT gets. And the output edge is
capped at 4× the source's longest edge: the quad is user-supplied data, and without a cap a
mis-dragged handle could ask for a multi-gigapixel canvas.

`perspective.aspect` is optional because the engine cannot recover the subject's true
ratio on its own — that needs the camera's focal length, and the engine is handed a JPEG,
never EXIF. Absent, it derives the ratio from the quad's mean edge lengths, which is right
for a roughly square-on shot; the UI writes an explicit value when it can do better.

In the UI (Develop → Crop & Rotate → Perspective) the four corners are dragged onto the
subject. While the handles are up the preview renders *without* the warp: the handles are
aimed at the original's corners, so rectifying underneath them would move the target.
Corner auto-detection is not implemented — the handles are placed by hand.

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
  geometry (perspective → straighten → crop) + tone (EV→linear gain, contrast,
  highlights/shadows, WB) using the `image` crate.
- **Module UI (TS):** crop overlay with the aspect presets, the perspective corner handles,
  tone sliders, the Versions panel, live preview, and `registerEditRenderer(...)` so the
  loupe shows the edited result.

## Rendering & export

- **Live preview:** render the cached **preview proxy** (embedded JPEG, ~fast) as sliders/crop
  change (debounced). Proxy quality is fine for judging an edit.
- **Two caches make the drag cheap.** The proxy JPEG is decoded once (a one-slot cache
  keyed by the bytes' fingerprint), and `render_proxy` keeps the **framed base** — the
  proxy after perspective → straighten → crop → downscale, before the look — keyed by
  (proxy fingerprint, geometry, edge), four entries, least recently used out. A look-only
  slider frame therefore pays the look and the encode and nothing else; a geometry change
  is a miss and re-frames. Both caches are byte-identical to the uncached path (locked by
  tests in `plugins/edit`).
- **Transport — the Darkroom stage is a native URL.** The stage `<img>` loads
  `edit://<photoId>?r=<base64url(record)>&m=<maxEdge>[&b=1][&hi=1][&v=bust]`
  (`editRenderUrl` in `src/modules/api.ts`; `protocol::handle_edit_request` and
  `commands::editing::render_edit_bytes` in Rust), served through the same bounded LIFO
  image pool as `thumb://`/`preview://`/`zoom://`: the newest URL renders first and
  identical URLs coalesce into one render, which is what makes slider spam safe. Responses
  are `Cache-Control: no-store` — rendering, not fetching, is the cost, and a regenerated
  proxy or re-imported LUT must never show stale pixels. `b=1` renders the geometry only
  (perspective → straighten, no crop, no look) as lossless PNG: the base the GL drag tier
  will shade. The `render_edit` / `render_edit_batch` commands still return base64 data
  URLs for their remaining callers (the loupe window's renderer, the proof sheet, duels,
  the preset browser, the legacy Develop view) — listed as follow-ups in
  `docs/plans/darkroom/00-status.md`, not a transport the Darkroom stage uses.
- **Loupe:** shows the active version's render when the module is enabled; otherwise the
  unedited preview (the core edit contract already falls back).
- **Edited export — decided: render from a full RAW decode.** "Show off" (JPEG) renders each
  chosen version from the **full-resolution source**: a decoded RAW for RAW originals, or the
  original JPEG for JPEG-only photos. This gates *edited RAW export* on a RAW decoder that
  doesn't exist yet (see Phase 3) — JPEG-only originals can export edited immediately.
  Per-version filenames (`<stem> - <version>.jpg`), collision-safe.
- **Hand-off export (RAW + XMP) stays unedited** — you're giving the RAW to another editor;
  crop/exposure are not written into the sidecar (merge-safe invariant).

### Two engines, one record shape (docs/plans/raw-foundation)

Every record carries an **engine id** — absent or `1`: the pipeline above, on the camera's
embedded preview, rendered exactly as it always was; `2`: the scene-linear pipeline on the
RAW **working image**. A version means one thing forever: the Darkroom never reinterprets
an engine-1 record as engine 2 (an EV of +1 on gamma pixels is not the same picture as +1
in linear light). Engine 2 is behind the `develop.rawEngine` setting (Preferences →
Darkroom) until the swap slice makes it the default.

- **Working image.** Opening a photo in the Darkroom claims the `develop` job family and
  decodes the RAW on its own thread (`raw::decode_linear`: 16-bit, gamma 1.0, sRGB/Rec.709
  primaries, as-shot white balance, no auto-brightening, highlights clipped at sensor white,
  the camera's visible rectangle) into an f32 image held by `develop::ResidentSet`. A photo
  switch, leaving Develop, or a catalog switch trips the claim and releases it. The event
  `develop:source` carries the state — `preview` (preparing), `raw` with the **token**
  `w:<photo>:<generation>`, `unsupported` with the camera, `jpeg`, `nodecoder`.
- **Source on every render.** `edit://…&s=<token>`, `render_edit_batch` and
  `edit_zone_masses` name the pixels they render from. A token that is no longer resident is
  a 404, never a fallback to the preview; engine 2 refuses a preview source and engine 1
  refuses the working image (`plugins/edit/source.rs`, `RenderSource`).
- **Engine 2's pipeline** (`plugins/edit/linear.rs`): exposure and white balance as
  multiplications of linear light (values above white survive), then one display
  transform (`display`: absent/`"srgb"`, or `"soft"` with a highlight shoulder) with a
  provisional `BASELINE_EV` lift so as-shot lands near the camera JPEG, then the *same*
  display-domain look as engine 1 — zones, region sliders, contrast, saturation, and the
  finish (B&W, LUT, split, fade, vignette, grain) — so presets mean the same on both.
  Geometry (perspective → straighten → crop → downscale) runs on the f32 image; the
  framed-base cache keys on the token instead of the JPEG fingerprint.
- **White balance on engine 2** is a tagged meaning on `tone.wb`: `mode` absent or
  `"relative"` (warmer/cooler than as-shot, the same gentle gains as engine 1) or
  `"kelvin"` (a scene light; parsed now, rendered in the Kelvin slice — refused with a clear
  error until then, never silently treated as relative).
- **Export of an engine-2 version** decodes the same working image and renders it through
  the same pipeline at full size; the tone-matching step belongs to engine 1 only.

### Measuring the render path

Every stage of a render can be timed without a profiler: `CHAIRPHOTO_EDIT_TIMING=1` makes
the backend print one `[edit-timing]` line per render (stages, total, and the build profile —
`tauri dev` runs the engine unoptimized, so its numbers are not release numbers), and the
Darkroom's Preferences toggle "Log render timings to the console" adds the frontend half:
IPC round trip, resolve-to-paint, and drag cadence per frame, summarized every 2 s and
persisted under `editor.renderTiming.lastSummary`. The ignored bench
`plugins::edit::bench::render_stage_timings` gives the same stages in isolation; see
`docs/performance-harness.md` § Edit render bench.

