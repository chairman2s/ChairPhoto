---
title: "Slideshow → movie"
description: "Render selected photos into an mp4 slideshow with ffmpeg."
tags:
  - chairphoto/module
  - chairphoto/render
aliases:
  - "Movie"
---

# Slideshow → movie

Select photos and render them into a slideshow **movie file** (`.mp4`). Built on
**ffmpeg** — an external binary, the same "shell out to a trusted tool" pattern used for
exiftool and Chrome — behind a `slideshow` Cargo feature, detected on PATH at runtime.

## Where it lives

Not a publish target, because it produces a *file*. The module registers a **toolbar
action**, "Make slideshow", enabled when at least two photos are selected. It opens a
settings dialog, then encodes in the **background** with a progress bar, and offers to
reveal or open the finished `.mp4`.

Each photo is rendered from its **Original** — a multi-photo selection has no single
active version.

## Pipeline

1. Render each selected photo to a temporary full-size JPEG through the normal export path
   (`resolve_originals` + `write_item_jpeg`), in the chosen order.
2. Build one ffmpeg invocation:
   - Per image: loop to a still, `scale=W:H:force_original_aspect_ratio=decrease` then
     `pad=W:H:(ow-iw)/2:(oh-ih)/2` — letterbox, never crop — and set `fps`. With Ken
     Burns on, `zoompan` applies a slow zoom over `duration × fps` frames.
   - Chain the clips with `xfade` (transition plus accumulating offsets) for crossfades;
     without transitions, `concat`.
   - Encode `libx264`, `-pix_fmt yuv420p`, `-movflags +faststart` for broad compatibility.
3. Parse `-progress pipe:` and emit `slideshow:progress` events carrying done/total.

The xfade + zoompan filtergraph for N inputs is the fiddly part. The offset/filtergraph
string builder is unit-tested, and the engine is verified against real ffmpeg and ffprobe:
a three-image render is probed for output existence, exact dimensions, and duration. Tests
that invoke ffmpeg skip gracefully when it is not on PATH.

## Options

`duration_per_photo` (seconds — the full clip length, transition overlap included),
`transition` on/off with `transition_duration` (clamped below `duration_per_photo` so every
clip keeps some non-overlapping visible time), `ken_burns` on/off, `fps` (default 30), and
an aspect/resolution preset: 16:9 1080p, 16:9 4K, 1:1 1080, or 9:16 1080×1920. Plus the
output folder, defaulting to `~/Videos` or `~/Pictures/Export`. It must be an absolute local
path (`~` expanded); a relative path or a URL is refused before the render is claimed, and
every path reaches ffmpeg as a `file:` URL, so it is never read as an option or a protocol.

The dialog also lets you drag-reorder the selected photos before rendering; the default
order is the current grid order.

## Implementation

- `crates/core/src/slideshow/mod.rs` — ffmpeg detection, filtergraph construction, and the
  run with progress parsing.
- `crates/core/src/app/slideshow.rs` — the render as a job (`JobRegistry::slideshow`):
  `claim_slideshow` resolves the Originals and claims the job under one catalog lock
  (optionally bound to a `CatalogIdentity`), and `SlideshowJob::run` renders the frames into
  a private cache directory (the app's cache dir, not `/tmp`, which can be a quota-limited
  tmpfs) and encodes. A newer render, Cancel (the job's own abort
  handle) or a catalog switch kills ffmpeg, removes the partial movie and answers
  "Slideshow cancelled". Missing ffmpeg is refused before anything is claimed.
- The GPUI Slideshow module (`crates/app/src/modules/slideshow/`) calls that job directly,
  off the UI thread; progress is cosmetic, the job's return value is the terminal result.

  ```
  claim_slideshow(state, expected: Option<CatalogIdentity>, photoIds, opts, destDir, ffmpeg)
    -> Result<SlideshowJob, String>
  event CoreEvent::SlideshowProgress { done, total, job }
  ```

- The dialog subscribes to the app model's events and matches
  `CoreEvent::SlideshowProgress`, filtering to its own job id so a stale or another
  dialog's job never moves its bar. Progress is nonessential: a lost or missed event still
  lets the encode finish, and the UI simply shows an indeterminate "Rendering…" instead of a
  determinate bar.

## Limits

- ffmpeg must be present at runtime. It is detected on PATH, with a clear error when
  missing.
- Encoding is CPU-heavy and can take a while for many photos or 4K output, hence the
  background job and progress bar. It never blocks the UI.

## Not included

**No background music.** Beyond being out of scope, audio carries **music-rights risk** — a
slideshow with copyrighted audio cannot be shared freely. ChairPhoto ships no music and
makes no licensing claim; any audio support would have to be user-supplied tracks that the
user owns or licenses.
