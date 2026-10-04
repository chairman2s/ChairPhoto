---
title: "Tag Graph"
description: "The tag vocabulary drawn as a radial edge-bundled co-occurrence graph."
tags:
  - chairphoto/module
  - chairphoto/tagging
  - chairphoto/insights
aliases:
  - "Graph view"
---

# Tag Graph

A radial picture of your tag vocabulary — which tags you actually use, and which ones keep
turning up together. It answers questions a tag tree cannot: which families lean on each other,
which branches are dead, and which pairs are effectively synonyms in practice.

Enable **Tag Graph** in Preferences → Modules and it appears as its own main view.

## What it draws

Radial hierarchical edge bundling (Holten 2006), drawn on a canvas:

- **Tags sit on a ring**, one slot each, ordered by a depth-first walk of the tag hierarchy so
  children sit beside their parent and each top-level community is a contiguous arc, sized by
  its photo count and marked with a coloured band. Cameras get an arc of their own. Only tags
  with at least one non-missing photo appear, so unused branches stay out of the picture.
- **Edges are co-occurrence** — two tags are linked when they appear on the same photos, with
  opacity following how often. Each edge is routed through the hierarchy (up to the lowest
  common ancestor and down again) and smoothed, so all the edges between two families share a
  path and read as one bundle.
- **Nothing is drawn until a node type is switched on.** The view opens empty; toggle Tags
  and/or Cameras to draw them.
- **Hover a tag** to light up everything it appears with, coloured by the far end; **select**
  one for the inspector; **select a community** on the left to light up all of its edges and
  put the family itself in the inspector — photos, its top tags, and the same focus / filter /
  loupe actions a tag gets. That works for a family whose top tag has no photos of its own
  (such a tag is not on the ring, but the vocabulary still knows its id).
- **Focus on this branch** (in the inspector, for a tag with children) redraws the ring for
  just that tag's subtree: its direct children become the arcs, and the tag itself keeps a
  slot under its own name. Cameras stay, showing what shot that branch. A breadcrumb in the
  left panel climbs back out; Esc with nothing selected goes up one level.
- **Open loupe window** (in the inspector) mirrors the selected node to the pop-out loupe —
  its numbers, what it connects to, and a wall of its photos, paged — so the inspector can
  live on a second screen. It follows the selection from then on, and hands the loupe back
  to the photo selection when you leave the graph.

The layout is deterministic — no physics, no settling — so the same library always looks the
same, and the picture is ready as soon as the data arrives.

The useful reading is the bundles. A thick bundle between two arcs is two families that describe
the same photos, which is a good signal that they belong on one branch, or that one tag should be
a synonym rather than its own. A slot with no edges is the opposite — vocabulary you created once
and never reused.

Selecting a tag pulls up its photos through the normal `list_photos` path, so the graph is a way
into the library rather than a dead end.

## Where it lives

```
crates/core/src/catalog/mod.rs       Catalog::library_graph / Catalog::photo_tag_graph — the queries
crates/model/src/tag_graph/          the radial layout (bundle.rs, graph.rs) — pure, unit-tested
crates/app/src/modules/tag_graph/    the module, its view, and the tiny-skia raster
```

The module id is `tag-graph`. It is **app-only** — no `backend_feature`, and `library_graph`
is defined unconditionally in core, so it is available in every build including
`--no-default-features`.

`Catalog::photo_tag_graph` is the photo↔tag bipartite graph, for a caller that wants individual
photos as nodes rather than the tag-level projection this module draws; no current caller
builds that view (the bipartite mode was dropped at the GPUI port, below).

## Limits

- The graph is computed live on each open and is not cached.
- Photos marked missing are excluded, so the picture reflects what the catalog can currently
  reach.
- **Communities only.** The GPUI port dropped the legacy bipartite Photo ↔ tag view (an
  owner decision, docs/plans/gpui/tag-graph.md); `Catalog::photo_tag_graph` still exists for
  it but has no current caller.
- It is a view of the vocabulary, not an editor — reparenting and merging tags happen in the tag
  tree (see [taxonomy.md](taxonomy.md)).
