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

Editing arrives as a full-window **Develop** tab rather than a modal: the **Darkroom**
(docs/plans/darkroom — since its slice 8 the only Develop surface; the classic editor view
is retired). A version bar, the tone strip (the histogram as a control), the proof sheet and
duels, the preset browser, crop with social aspect presets and Free (drag to move, resize
from the corners, live pixel-size readout), composition overlays (none, thirds, phi grid,
golden spiral — remembered in `editor.crop_overlay`), tone sliders for EV, contrast,
highlights, shadows and white balance in Kelvin or relative (double-click any slider to
reset), a filmstrip, the pop-out loupe print, and autosave with history.

A photo can carry **multiple versions** — several crops, or the same frame at different
exposures — each independently editable and exportable.

**Packaging.** The render engine is gated behind the `edit` Cargo feature, while the loupe's
render hook belongs to the bundled Basic Editor module. Disable the module and the loupe falls
back to the original image with the Develop and Edit entry points hidden. The RAW decode —
Develop's working image and every engine-2 render and export — sits behind the `raw`
feature via the vendored LibRaw; without it Develop works on the camera preview.

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
    updated_at INTEGER NOT NULL,
    changed_seq INTEGER NOT NULL DEFAULT 0  -- settings-write order: the automatic face (#252)
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

### History and autosave (the Darkroom)

The Darkroom saves every change to the **active version** as it goes — settings only, never
pixels, never the original or its sidecar (user decision 2026-09-24, replacing the slice-7
sandbox). A photo with no version gets "Version N" on its first change; "Original" on the
shelf shows the unedited file, and changing anything there starts a new version. "+ New
version" copies the current settings into a new version and continues there.

Each settled change (0.6 s of quiet) is a **history step** on that version, named after what
changed ("Exposure +0.50", "Crop 4:5", "Proof: Portra", "Reset"). The same control still
moving within four seconds amends its step rather than adding one, so a keyboard nudge or a
second drag is one step. Step 0, "Before", holds what the version had when its history
began, so the first change is always undoable.

The History panel (top of the rail) lists the steps newest first; clicking one — or Ctrl+Z /
Ctrl+Shift+Z / Ctrl+Y — makes it current and saves its settings back into the version. The
steps after it stay until the next change, which **replaces** them (a list, not a tree). At
most 200 steps per version are kept. Pending changes are saved before a step, a version
switch, or leaving the photo.

**Filmstrip.** The Darkroom's bottom strip shows the Library's photos in their current
order and filter, the one being developed centred. Click a frame, or ← / →, to move on
(the arrows are left alone while a slider, field or the proof sheet/duel has them). Moving
on saves first. With the RAW engine on, the next and previous photos are already decoded
in memory (docs/plans/raw-foundation, slice 4), so a step shows the RAW at once.

**Cover (the Library face).** A photo's face in the Library grid, the Bench and the
filmstrip is the look of its **most recently changed version** — the one whose settings were
written last: a settled edit, a proof adopted, a history step (undo/redo included), a new or
duplicated version, a version merged in from a bundle. Opening or renaming a version is not a
change. With no versions it is the original (owner decision, #252). "☆ Use as cover" on the
Darkroom bar **pins** what is shown as the face — the version being edited, or, with
"Original" chosen on the shelf, the untouched original — and it stays the face whatever is
edited later; "★ Cover" unpins it, and the face follows the latest change again. The bar says
"Face: latest edit" while nothing is pinned, and the shelf stars the pinned chip. Deleting
the face's version falls back to the next most recently changed version, else the original;
a pin on a deleted version is lifted.

The original is untouched: the grid thumbnail is rendered from the version's settings at
512 px on a worker (`plugins/edit/cover.rs`, with the RAW engine when the version uses it) and
cached under `<cache>/chairphoto/cover512v2/`, keyed by file and settings. If that render
fails the plain thumbnail is shown; the original's own thumbnail is still kept as the
offline fallback. The photo row carries a face token, `"<version>:<rev>"` (none for the
original), which the views ask for the thumbnail under; `rev` rises on every change of the
face — it moving to another version, the face version's settings changing, a pin or unpin, a
deletion — so no view shows a cached stale face. Only settled writes count, so a slider drag
renders no faces; the Library's rows (and so the faces) are re-read when Develop is left
and after a pin or a version operation. When the filmstrip steps on from a photo whose
changes were saved, only that photo's face is read again (`ShellState::refresh_face`), not
the whole library. The original's offline fallback thumbnail is refreshed on the face path
as on the plain one, so a rotation change does not leave it stale.

Stored in the core table `photo_cover` (one row per photo): `version_id` is the face itself,
kept current in the transaction of every write that can move it (`catalog::edits`,
`settings_written` / `refresh_face`), `pin` says how it is chosen (0 automatic, 1 the
version, 2 the original), and `rev` counts. Versions order by `photo_versions.changed_seq`,
which each settings write sets one past the photo's highest. Covers set before #252 migrate
as pinned. Local to this catalog like the history: catalog merge and bundle export do not
carry it.

Storage: core tables `photo_version_history` and `photo_version_history_head`
(`catalog/schema.rs`), both cascading with their version. They are local to the catalog:
catalog merge and bundle export carry versions but not their history. Every write that
changes a version's settings — a save, a commit, a step — refreshes the photo's monochrome
flag and auto-tag the same way (`commands::editing::write_version_then_refresh_monochrome`).

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
`basic-editor.presets`. Presets are look-only — never crop/straighten. The Darkroom's preset
browser (`src/components/PresetBrowser.tsx`) shows the current photo rendered per preset, each
card an `edit://` render from the Darkroom's own source and engine; "☆ Save as preset" on
the bar saves the current look (`lookOnly`), and the browser renames and deletes user
presets.

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

- **What Develop renders from.** A RAW the bundled decoder supports is developed from the
  RAW itself (engine 2, below) — the default since the swap (docs/plans/raw-foundation,
  slice 8; the `develop.rawEngine` setting is gone). The camera's embedded **preview proxy**
  is what the stage shows for the moment the RAW is being prepared, and what engine 1
  renders from: JPEG-only photos, RAWs the decoder does not support yet (the bar says so and
  names the camera), and versions saved on engine 1.
- **Two caches make the drag cheap.** The proxy JPEG is decoded once (a one-slot cache
  keyed by the bytes' fingerprint), and `render_proxy` keeps the **framed base** — the
  proxy after perspective → straighten → crop → downscale, before the look — keyed by
  (proxy fingerprint, geometry, edge), four entries, least recently used out. A look-only
  slider frame therefore pays the look and the encode and nothing else; a geometry change
  is a miss and re-frames. Both caches are byte-identical to the uncached path (locked by
  tests in `plugins/edit`).
- **Transport — in-process, no encode, no protocol.** Every render — the Darkroom stage, the
  loupe's active version, the Duel's two variants, the Proof sheet and the preset browser —
  is an `EditJob { photo_id, edit_json, max_edge, hi_res, base_only, source, clip,
  catalog }` (`crates/core/src/image_pool.rs`) submitted to the same bounded LIFO image
  pool as thumbnails/previews/zoom, under `JobKey::Edit`: the newest job renders first and
  identical jobs coalesce into one render, which is what makes slider spam safe.
  `media::render_edit_image` runs it and hands GPUI the result directly as a BGRA
  `RenderImage` texture — no JPEG/PNG encode, no URL, no IPC, so there is nothing to cache or
  bust: a regenerated proxy or re-imported LUT is simply a new job. `source` names the working
  image to render from (the camera preview, or a resident RAW by token), `clip: true` asks for
  the sensor-clipping overlay instead of the render, and `base_only: true` renders the
  geometry only (perspective → straighten, no crop, no look). `hi_res: true` renders from the
  native-size zoom tier instead of the 2048 px proxy. Every job carries the `CatalogIdentity`
  its photo id was read from (#251) — the Darkroom's open (`OpenPhoto::from`) for the stage,
  the Duel, the Proof sheet and the loupe print; the shell's rows (`rows_from`) for the
  loupe's active version — so a request from before a catalog switch can never merge into,
  or be handed to, one made after. The worker renders a job only while that catalog is still
  open, checked under the catalog lock together with the photo's read (`with_catalog_as`'s
  check): a switch publishes the new catalog before `catalog:switched` reaches the UI, and a
  job reaching a worker in that window answers `CATALOG_CHANGED` and renders nothing, never
  the new catalog's photo of that id. The front end drops that answer — the stage like a
  cancellation, `EditRenders` as `RenderState::Stale` (nothing drawn, no failure, not asked
  again). An engine-2 render that reuses a resident decode, found by photo id alone, checks
  again after finding it. One render path (`media::render_edit_image`) serves every
  caller, engine-1 and engine-2 alike: it branches internally on the record's engine and on
  whether a resident RAW working image exists for the job's source token.
- **Loupe:** shows the active version's render when the module is enabled (`renderForLoupe`):
  an engine-2 version is the RAW through its pipeline at 2560 px, full size to zoom — from
  the Darkroom's own working image while it prints there, else from an offline load — and
  an engine-1 version renders as it always has; otherwise the unedited preview.
- **Edited export.** "Show off" (JPEG) renders each chosen version at full resolution: an
  engine-2 version from its working image (below — it *is* the view); an engine-1 version
  from a LibRaw decode tone-matched to the camera preview it was judged on, or from the
  original for JPEG-only photos. Per-version filenames (`<stem> - <version>.jpg`),
  collision-safe.
- **Hand-off export (RAW + XMP) stays unedited** — you're giving the RAW to another editor;
  crop/exposure are not written into the sidecar (merge-safe invariant).

### Two engines, one record shape (docs/plans/raw-foundation)

Every record carries an **engine id** — absent or `1`: the pipeline above, on the camera's
embedded preview, rendered exactly as it always was; `2`: the scene-linear pipeline on the
RAW **working image**. A version means one thing forever: the Darkroom never reinterprets
an engine-1 record as engine 2 (an EV of +1 on gamma pixels is not the same picture as +1
in linear light). New edits of a supported RAW are engine 2. A saved engine-1 version keeps
rendering on engine 1 even with the RAW open, and the bar offers **Develop with the new
engine**, which forks "<name> (RAW)" with the framing copied and tone and look reset.

- **Working image.** Opening a photo in the Darkroom claims the `develop` job family and
  decodes the RAW on its own thread (`raw::decode_linear`: 16-bit, gamma 1.0, sRGB/Rec.709
  primaries, as-shot white balance, no auto-brightening, highlights clipped at sensor white,
  the camera's visible rectangle) into an f32 image held by `develop::ResidentSet`. A photo
  switch, leaving Develop, or a catalog switch trips the claim and releases it (the switch releases in its detach phase, with the slot); a
  decode that finishes after a newer claim removes only its own image. The resident set's
  lock is a leaf in the `commands::jobs` lock order. Each of those transitions is forced in
  `develop::session::tests` and `commands::jobs::tests`. The event
  `develop:source` carries the state — `preview` (preparing), `raw` with the **token**
  `w:<photo>:<generation>`, `unsupported` with the camera, `jpeg`, `nodecoder`.
- **Decoder.** LibRaw, vendored as a pinned submodule and compiled in (`raw` feature; LGPL-2.1,
  see `MODULE_LICENSING.md`). Every call runs under the crash marker: a file that took the
  process down twice is skipped and Develop stays on its preview, saying so. A camera
  newer than the decoder is *RAW not supported yet* on the bar, never a silent fallback.
- **Caches, each bounded.** The `.rawf` **decode cache** keeps each decoded RAW on disk
  (uncompressed, keyed by file size, mtime and decoder version; Preferences › Darkroom sets
  its size, default 20 GB, oldest first out), so a second open is a read. The **resident
  set** holds the open photo and its preloaded neighbours in memory (4 GB budget; the open
  photo is never evicted for a neighbour). One **offline** image (`develop::offline`) serves
  engine-2 renders outside Develop — the Library loupe, cover thumbnails, exports — loaded
  one at a time and kept 60 s. The **framed-base cache** keeps four geometry-applied bases,
  linear ones only up to 2560 px. **Cover** thumbnails cache on disk per file and look.
- **Camera match.** When a working image is prepared it is measured against its own camera
  JPEG (`linear::camera_match_ev`), and a new engine-2 record stores that offset as
  `cameraEv`: the extended-low-ISO pull, a body's metering bias, and a global share of DRO
  that one fixed curve cannot carry.
- **Sensor clipping.** *◩ Clipping* on the bar overlays, in magenta, where the RAW itself
  is clipped (`edit://…&k=1`) — the only white no slider can bring back.
- **Source on every render.** `edit://…&s=<token>`, `render_edit_batch` and
  `edit_zone_masses` name the pixels they render from. A token that is no longer resident is
  a 404, never a fallback to the preview; engine 2 refuses a preview source and engine 1
  refuses the working image (`plugins/edit/source.rs`, `RenderSource`).
- **Engine 2's pipeline** (`plugins/edit/linear.rs`): exposure and white balance as
  multiplications of linear light (values above white survive), then one display
  transform (`display`: absent/`"srgb"`, `"soft"` with a highlight shoulder, or
  `"camera"`, the per-channel curve fitted to the camera's own JPEGs, or `"camera.2"`,
  that curve after a fitted camera colour matrix, which new engine-2 records get by
  default; a record keeps the transform it was saved with) with a
  `BASELINE_EV` lift plus the record's `cameraEv` — the offset that matched this photo's
  camera JPEG, measured when the RAW was prepared and stamped on a new engine-2 record —
  so as-shot lands at the camera JPEG's brightness, then the *same*
  display-domain look as engine 1 — zones, region sliders, contrast, saturation, and the
  finish (B&W, LUT, split, fade, vignette, grain) — so presets mean the same on both.
  Geometry (perspective → straighten → crop → downscale) runs on the f32 image; the
  framed-base cache keys on the token instead of the JPEG fingerprint.
- **White balance on engine 2** is a tagged meaning on `tone.wb`: `mode` absent or
  `"relative"` (warmer/cooler than as-shot, the same gentle gains as engine 1) or
  `"kelvin"` — the scene's light, `kelvin` plus `tint` (+100 = one stop less green),
  rendered as a change of balance in the camera's own space
  (`rgb_cam · diag(b / as-shot) · rgb_cam⁻¹`, green held). The balance for a stated light
  comes from the **camera's own white-balance table** where the file has one (LibRaw's
  `WBCT_Coeffs`, interpolated in mireds), else from the decoder's daylight multipliers and
  matrix. The as-shot light is solved the same way, so rendering it is exactly the picture
  as shot, and a blank white balance and "as shot" are the same record. The Darkroom's
  rail shows Kelvin (slider 2000–12000 K, log; double-click = as shot) or the relative
  pair, switched by *K/±*; which one a fresh RAW edit shows is Preferences › Darkroom
  (`develop.wbSlider`, Kelvin by default). The proof sheet's warm/cool cells and the
  duel's warmth round step stated light in mireds on the RAW. A camera with neither table
  nor daylight white refuses Kelvin with a clear error, never a silent relative.
- **Export of an engine-2 version** renders the same working image the view does — the
  Develop session's, or one bounded load from the `.rawf` cache or the decoder
  (`develop::offline`) — through the same pipeline at full size; the tone-matching step
  belongs to engine 1 only. At 100 % the export equals the view byte for byte
  (`export_at_full_size_is_the_view_at_full_size`). Every engine-2 export is also checked
  against the view at Fit (`plugins::edit::parity`: the view's 512 px render against the
  export scaled to it, mean |Δ| ≤ 6 levels — above what resampling order alone produces on
  a detailed frame, below any other-source or other-pipeline mismatch) and tallied per
  catalog in `metrics.exportParity`; Preferences › Darkroom shows the count — the product's
  success metric, "0 exports that differ from the view".

### Measuring the render path

Every stage of a render can be timed without a profiler: `CHAIRPHOTO_EDIT_TIMING=1` makes
the backend print one `[edit-timing]` line per render (stages, total, and the build profile —
a debug `cargo run` without `--release` runs the engine unoptimized, so its numbers are not
release numbers), and the Darkroom's Preferences toggle "Log render timings to the console"
adds the GPUI half: submit-to-paint and drag cadence per frame, summarized every 2 s and
persisted under `editor.renderTiming.lastSummary`. The ignored bench
`plugins::edit::bench::render_stage_timings` gives the same stages in isolation; see
`docs/performance-harness.md` § Edit render bench.

