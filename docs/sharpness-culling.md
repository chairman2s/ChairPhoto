---
title: "Sharpness culling — out-of-focus detection"
description: "Score photos for focus and surface the soft ones for culling."
tags:
  - chairphoto/core
  - chairphoto/insights
aliases:
  - "Sharpness"
  - "Culling"
  - "Focus"
---

# Sharpness culling — out-of-focus detection

ChairPhoto scores every photo for focus and surfaces the result as a filterable facet, a
grid badge, and a sort order, so a soft frame can be culled quickly. It **flags, never
auto-rejects** — an intentional motion-blur pan is a keeper that every focus metric hates.

Everything runs locally: classical pixel math on cached previews, plus face boxes and
exiftool makernotes that the app already has. There is no cloud path and no opt-in dialog.

## Why a global score is not enough

The textbook measure — variance of the Laplacian over the whole image — measures *average*
sharpness. A tack-sharp bird on creamy bokeh scores **lower** than a boring
everything-in-focus snapshot, so a global score flags the best shallow-depth-of-field
keepers as soft. The scorer is therefore region-aware.

## How a photo is scored

Each photo picks the **best available** region source, and records which one it used:

1. **Face boxes** (`method='face'`) — with the `faces` feature compiled in and faces
   detected, sharpness is measured inside the face boxes. The sharpest box wins, so one
   soft face among sharp ones does not sink the score. A soft face on a sharp background
   is exactly the frame worth flagging.
2. **The autofocus point** (`method='afpoint'`) — where the camera recorded where it
   focused, sharpness is measured there: "did the AF land?", without a subject-detection
   model.
3. **Tiled maximum** (`method='tile'`) — the baseline. The grayscale image is cut into an
   **8×8 grid**, the Laplacian variance of each of the 64 tiles is computed
   independently, and the photo's score is the **~90th-percentile tile** — "is anything
   sharp anywhere?". The 90th percentile is deliberately not the maximum, which a single
   specular glint, hot pixel, or JPEG block could dominate.

Each tier only wins when it actually yields a score. An off-image face box or an
out-of-range AF point falls through to the next, so `sharpness_method` is honest about
what was measured. All three tiers use the **same** Laplacian-variance focus measure, so
their scores are directly comparable.

The chain lives in `sharpness_indexer::score_image_regions` (and `score_jpeg_regions`);
the pure region math is `sharpness_regions`, which is feature-independent and takes
already-extracted boxes or an already-parsed point. Without `--features faces`, face
boxes are always empty and the chain degrades to AF/tile with no code change.

### Reading the autofocus point

Sony ARW makernotes record the AF point, extracted with exiftool (already a scan
dependency). The dependable field is **`MakerNotes:FocusLocation`**, four integers —
**`"imgW imgH afX afY"`** — giving the AF point in **unrotated sensor** pixel coordinates
alongside the sensor's own dimensions. A centered AF point reads as exactly `imgW/2
imgH/2` (e.g. `9984 6656 4992 3328`).

Two details matter:

- **Orientation.** `imgW imgH` is always the *unrotated* sensor frame, even for a portrait
  shot with `Orientation = Rotate 90 CW`, while the cached preview is fully oriented. The
  parser normalizes in the sensor frame, then rotates the normalized point by the EXIF
  `Orientation` code to land it on the oriented preview.
- **The `-fast2` interaction.** The main scan's exiftool pass uses `-fast2` for a
  substantial NAS speedup, and that stops *before* the MakerNotes — so it does not return
  `FocusLocation`. A separate targeted pass (`-FocusLocation -Orientation -n`, RAW files
  only, two tags) folds the AF point and orientation into the same `photo_metadata` write,
  leaving the fast bulk pass untouched. See `metadata::enrich_af_points`.

The alternative fields are not usable: `FocalPlaneAFPoint*` are tiny grid coordinates on a
0–640 scale rather than image pixels and are often `n/a` or `0`, and
`AFPointSelected`/`AFPointsUsed` read `n/a`/`(none)`.

## Absolute thresholds versus burst-relative ranking

A foggy landscape is legitimately low-contrast; a macro shot is sharp in a sliver. The
catalog-wide `soft` facet therefore uses a **conservative** threshold, read from the
`sharpness.soft_threshold` setting (`catalog/facets.rs`); false negatives are acceptable,
false positives are not. The facet fires on `photos.sharpness < threshold AND sharpness IS
NOT NULL`.

The sharper signal is **relative ranking inside a burst cluster** — same scene, same
subject. A frame scoring below `cluster_median × 0.60` (the default
`BURST_SOFT_THRESHOLD_DEFAULT`, settable as `sharpness.burst_soft_threshold`) is almost
certainly a missed frame and is flagged `soft-in-burst`; the best frame of the cluster is
flagged `sharpest-of-burst`. Flags are written to `photos.burst_flag` in a single
transaction via `Catalog::set_burst_flags`.

The rule itself is `burst::flag_cluster`, a pure function over one cluster. It returns a
verdict per member **and the numbers behind it** — median, cutoff, sharpest frame, scored
count — because both callers need the derivation: `analyze_burst_sharpness` to persist the
flag, and `explain_photo_signals` to explain it. Keeping one implementation is what stops
a badge and its explanation from disagreeing.

## Explaining a flag

A badge is a verdict with its reasoning discarded. `explain_photo_signals`
(`commands/culling.rs`) puts the reasoning back for one photo, and the inspector's
**Culling signals** section renders it: the cluster with each frame's score, rating and
dHash distance from the subject, the median and the cutoff it implies, this frame's rank
among the scored, and the absolute score against `sharpness.soft_threshold`.

Two properties of that surface are load-bearing rather than cosmetic:

- **It recomputes; it does not read the cluster back.** No cluster is stored — a run
  flags whatever photo set it was handed. `catalog::culling::burst_neighbourhood`
  rebuilds the time run around the photo (walking outward while each step stays inside
  `ai.burst_time_gap_secs`, over the photos the grid lists: present, not stacked), and the
  real `group_into_clusters` splits it on visual similarity again.
- **Disagreement and incompleteness are reported, not smoothed.** When the fresh verdict
  differs from `photos.burst_flag`, both are shown and the badge is marked stale — the
  stored flag came from a run over different neighbours, and that is the useful answer.
  When the run is longer than one lookup can cover, the cluster is marked truncated so its
  size, rank and median read as lower bounds.

The command never writes the fresh verdict back. Repairing a badge as a side effect of
looking at it would hide that the last analysis run is stale, and would make opening an
inspector section a catalog mutation. Re-running burst analysis is the way to refresh a
flag, and it stays an explicit action.

## Resolution and scheduling

A 256 px thumbnail cannot show micro-blur — a back-focused eye looks fine at that size.
Scoring runs on the ~1024–2048 px cached preview, which makes it an asynchronous
background index job: resumable queue, progress events, abort-safe, the same shape as face
detection and pHash. New imports are scored when their preview is generated. Scoring never
runs on the UI thread.

## Storage and surfacing

- `photos.sharpness` (REAL), `photos.sharpness_method` (TEXT) and `photos.burst_flag`
  (TEXT) are core columns, not a plugin table. All are nullable — NULL means "not yet
  scored". Storing the method lets the UI qualify the badge and lets thresholds differ per
  method.
- The `soft` facet appears once at least one photo has been scored; `soft-in-burst` and
  `sharpest-of-burst` appear once at least one burst has been flagged. All three compose
  with the rest of the filter bar.
- The grid shows a subtle tile badge, and the inspector's Culling signals section shows
  what the badge was derived from. Tile and loupe tooltips describe the comparison rather
  than quoting a threshold: the fraction is a setting, so a hardcoded "60%" in a tooltip
  is wrong the moment it is changed.
- Two sort orders are available: `sharpness_asc`, which puts the least sharp — and so most
  suspect — frames up front for culling, and `sharpness_desc`. Unscored photos sort after
  scored ones in both.

## Not included

- **No cloud path.** The computation takes milliseconds; uploading to outsource it would
  be absurd and would break "nothing ever leaves home" for zero benefit.
- **No auto-reject or auto-rating** from the score — facets and badges only.
- **No aesthetic scoring.** This measures focus, nothing else.
- **No blink or gaze verdict** in the signals panel. The face pipeline persists the
  5-point ArcFace template — one centre per eye — from which eye-openness cannot be
  derived at any confidence. That needs its own pinned model; until then an empty row
  promising it would be worse than its absence.
