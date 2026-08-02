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
| **Top tags** | most-used tags in the set |
| **Invalid dates** | photos whose capture time could not be parsed |

**Invalid dates is the one to act on.** A photo with an unreadable capture time is missing from
the timeline, hour and weekday panels, so a surprising count there explains a timeline that
looks wrong.

## Scoping the view

The statistics can describe the whole catalog or a slice of it. `catalog_stats` takes optional
`tagId`, `albumId` and `batchId` filters, so you can ask the same questions of one tag, one
album, or a single import batch — useful for "what did I actually shoot on that trip" without
re-filtering the grid.

## Where it lives

```
src/modules/plugins/statistics.tsx    the view, registered via registerMainView
src-tauri/src/commands/graph.rs:140   catalog_stats command
src-tauri/src/catalog/stats.rs:56     the queries
```

The module id is `statistics`. It is **frontend-only** — it declares no `backendFeature`, and
`catalog_stats` is registered unconditionally, so it is available in every build including
`--no-default-features`. There is no Cargo feature to enable.

## Limits

- Everything is computed live from the catalog on each open. There is no cached snapshot, so a
  very large library takes a moment to render.
- Photos marked missing are excluded, so counts describe what the catalog can currently see.
- The panels are read-only. Clicking a bar does not filter the grid.
