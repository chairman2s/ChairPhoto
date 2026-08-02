---
title: "Collage"
description: "Composite selected photos into one image on an interactive freeform canvas."
tags:
  - chairphoto/module
  - chairphoto/render
aliases:
  - "Mosaic"
---

# Collage

Select photos and composite them into a single image (`.jpg`/`.png`). Pure Rust using the
**`image` crate**, already a core dependency — no external binary, fully testable in dev.
Behind a `collage` Cargo feature, which is a compile gate rather than an extra dependency.

Enable the module in **Preferences → Modules**; the "Make collage" toolbar action then
becomes available when at least two photos are selected. It is not a publish target,
because it produces a *file*.

## The canvas

The dialog is an **interactive canvas**:

- **Drag** to move a tile, **corner-drag** to freely resize it to any shape — the photo
  cover-fills the cell.
- **Shift-drag** repositions the photo inside its frame, **scroll** zooms it within the
  frame.
- Tiles may **overlap**, with Front/Back z-order.

A **Template** dropdown (Grid, Columns, Rows, and Feature + column/strip on any side —
left, right, top, bottom; see `collageTemplates.ts`) lays the photos into cells. Applying a
template enters **Locked layout**: slots are fixed, and dragging a photo onto another
**swaps them** — drop onto the feature cell, which is always cell 0, to choose the feature.
Shift-drag and scroll still pan and zoom within a slot. Unchecking **Lock layout** returns
to free move/resize. **Auto-arrange** seeds the justified mosaic (unlocked), and the canvas
auto-arranges when it opens.

Each tile is a normalized `Placement { photoId, x, y, w, h, z }` in 0–1 canvas coordinates.
The canvas *is* the live preview — client-side positioned thumbnails, so there is no
per-drag server round-trip.

Tiles **cover-fill** their cell at a per-tile zoom and focal offset (`resize_cover_offset`
with `Placement.zoom/ox/oy`). At zoom 1 with a cell matching the photo's aspect you see the
whole photo uncropped; a differently-shaped cell or zoom > 1 crops to fill and is pannable
via the offset. The canvas mirrors this exactly with a sized, offset `<img>`, so what you
see matches the export. Decodes go through `decode_upright`, baking in EXIF orientation, so
portrait and rotated photos render the right way up.

## Saving

**Save to** is either a **folder** or the **library**:

- Folder → `make_collage_freeform` writes the image where you choose.
- Library → `save_collage_to_catalog` writes it under `<library root>/Collages/` and
  indexes it via `scanner::index_generated_file` (UUID, sidecar, metadata, no import
  batch), so it appears as a normal catalog photo.

A collage saved to the library is stamped with the **current time as its capture date** —
it has no EXIF date, which would otherwise sort it to the bottom of a date-ordered library
— and auto-tagged `Collage/<kind>` (Grid, Columns, Rows, Feature-left, Feature-right,
Feature-top, Feature-bottom, Mosaic, or Freeform). Both the `Collage` parent and the leaf
are marked **non-exportable**: they are organizational only and are never emitted as
keywords on export or publish.

## Layout algorithm (justified rows)

Auto-arrange uses a justified mosaic — tiles keep their aspect ratios, packed into
full-width rows of balanced height, the algorithm familiar from Flickr and Google Photos.
It suits mixed portrait/landscape sets. For a fixed output **width** `W`, gap `g`, and
target row height `Hₜ`:

1. Each photo has aspect `aᵢ = w/h`. Greedily add photos to the current row until the
   summed scaled width `Σ(aᵢ·Hₜ) + gaps` would exceed `W`.
2. Scale that row to fit `W` exactly: row height `H = (W − gaps) / Σaᵢ`, each tile width
   `= aᵢ·H`. (Cover mode normalizes `aᵢ` toward the row's median aspect first.)
3. Stack rows, the last one left-justified at roughly `Hₜ`. Total canvas height is the sum
   of row heights plus gaps.

**Output aspect ratio.** With **Free**, height is whatever step 3 yields and grows with the
number of rows. With a **fixed** aspect `r` the canvas is exactly `W × (W/r)`: a smaller
target row height packs more, shorter rows and a larger one packs fewer, taller rows, so
total height is monotonic in `Hₜ` — the solver **binary-searches `Hₜ`** until total height
approximates `W/r`, which takes a few iterations. Any small residual, such as a short last
row, is centered and the remainder becomes the background mat; **Cover** can take up the
slack by nudging tile crops. The fixed presets cover the social formats (1:1, 4:5, 9:16, …).

Compositing then creates a `W × height` canvas filled with the background color and, for
each tile, decodes the source, resizes it (Lanczos) to the tile rect, optionally applies
rounded-corner alpha and a border, and overlays it. It is memory-bounded: tiles render from
each photo's cached embedded preview (`thumbnails::zoom_bytes`) rather than a full RAW
decode, which is plenty for a collage.

### The fit toggle

A justified mosaic is aspect-preserving by nature — that is the point, every photo shown
whole — so **Contain** is the default. **Cover** is a secondary mode that lightly crops
each photo toward a common aspect before packing, trading a little cropping for a more
uniform tile look.

## Options

`width` (output px; presets 1080/2048/4096), `aspect` (Free, 1:1, 4:5, 5:4, 3:2, 2:3, 16:9,
9:16, or custom W:H), `row_height` target (the seed for the binary search when the aspect is
fixed), `gap` (px), `background` color including white/black/transparent, `fit`
(contain | cover), `border_width`, `corner_radius`, `format` (JPEG | PNG), and the output
folder.

Rounded corners and transparency need **PNG**; JPEG flattens onto the background color
because it has no alpha, and the dialog nudges you toward PNG when that matters.

## Implementation

Backend `collage::{compose_freeform, resize_cover_offset}` with commands
`collage_auto_arrange` (async, upright-aspect layout), `make_collage_freeform`, and
`save_collage_to_catalog`. Frontend `CollageDialog` plus `collageTemplates.ts`.

`CollageDialog.tsx` owns the module's backend surface: private
`CollageFormat`/`CollageOptions`/`Placement`/`FreeformOptions` DTOs and wrappers over
`ChairPhotoAPI.invoke`, not core `api.ts` wrappers, per the module isolation rule.

An earlier iteration of the dialog showed a static server-rendered preview; the canvas
replaced it. Its `make_collage` and `collage_preview` Rust commands remain defined and
registered, but the frontend no longer calls them, and their `api.ts` wrappers are gone.
The justified `layout()` underneath still backs Auto-arrange.

## Limits

- No tile rotation.
- Cells can overlap and bleed off the canvas, where they are clipped.
- Compositing many large tiles is CPU and RAM work, so it runs off the UI thread;
  rendering from previews keeps it light, and the output width is bounded.
