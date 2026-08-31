---
title: "Statistics"
description: "Read-only insights into the library: timeline, cameras, lenses, ratings, tags."
tags:
  - chairphoto/module
  - chairphoto/insights
aliases:
  - "Insights"
---

# Statistics

A full-window view of what is actually in your library: when you shoot, what you shoot with,
and which tags and ratings dominate. It is a reading tool, not an editing one — nothing here
changes a photo.

Enable **Statistics** in Preferences → Modules and it appears as its own main view, replacing
the grid while you are in it.

## What it shows

All of it comes from one backend call, so every panel describes the same set of photos:

| Panel | Source |
|---|---|
| **Totals** | photo count, and how many carry a capture time |
| **Timeline** | photos per month, from the first month with data to the last |
| **Hour of day** | when you shoot — the shape of a day's shooting |
| **Weekday** | which days you actually pick up a camera |
| **Top days** | your busiest individual shooting days |
| **Cameras / lenses** | bodies and glass by photo count |
| **Focal lengths** | the focal-length distribution across the set |
| **Ratings** | how the set breaks down by star rating |
| **ISO / aperture / shutter speed** | exposure-setting distributions, bucketed by stop |
| **Cull survival** | pick vs reject among decided photos; undecided count footnoted |
| **Quality hit rate** | share of rated photos at ≥4★; unrated count footnoted |
| **Keeper analysis** | both rates crossed with lens, camera, focal length, ISO, aperture, and shutter speed |
| **Top tags** | most-used tags in the set |
| **Invalid dates** | photos whose capture time could not be parsed |

**Invalid dates is the one to act on.** A photo with an unreadable capture time is missing from
the timeline, hour and weekday panels, so a surprising count there explains a timeline that
looks wrong.

## Keeper analysis semantics

Pick/reject and rating are deliberately independent axes — a 1★ pick is a photo you keep for
the memory but wouldn't showcase — so the two rates never share a definition or a denominator:

- **Cull survival** counts `pick_state = 'pick'` among *decided* photos (`pick_state != 'none'`).
  Rejected photos stay in the library and in these numbers; trashed and missing photos are
  excluded entirely (reject is a culling verdict, trash is deletion — they are different things).
- **Quality hit rate** counts photos rated ≥4★ among *rated* photos (`rating > 0`).
- Auto-stacked burst members that were never explicitly decided count as **undecided**, not
  rejected — collapsing a burst to its keeper writes stacking, not pick states.
- Shutter speeds are parsed from their EXIF text forms ("1/250", "0.5") to seconds and bucketed
  by stop; unparseable values fall out of the shutter panels.
- Rate rows with fewer than 20 decided (or rated) photos are dimmed as small samples, never
  hidden.
- Videos and other files without exposure EXIF simply fall out of the ISO/aperture/shutter
  panels; the per-panel totals say what each panel could see.

## Scoping the view

The statistics can describe the whole catalog or a slice of it. `catalog_stats` takes optional
`tagId`, `albumId` and `batchId` filters, so you can ask the same questions of one tag, one
album, or a single import batch — useful for "what did I actually shoot on that trip" without
re-filtering the grid.

## Where it lives

```
src/modules/plugins/statistics.tsx    the view, registered via registerMainView
src-tauri/src/commands/graph.rs:174   catalog_stats command
src-tauri/src/catalog/stats.rs:95     the queries
```

The module id is `statistics`. It is **frontend-only** — it declares no `backendFeature`, and
`catalog_stats` is registered unconditionally, so it is available in every build including
`--no-default-features`. There is no Cargo feature to enable.

## Limits

- Everything is computed live from the catalog on each open — one pass over the photos table
  plus a top-tags join, no cached snapshot. When the scope changes, the previous figures stay
  on screen until the new ones arrive. The `catalog_stats` operation in the performance
  harness (`docs/performance-harness.md`) guards this path against regressing to per-panel
  queries.
- Photos marked missing are excluded, so counts describe what the catalog can currently see.
- The panels are read-only. Clicking a bar does not filter the grid.
