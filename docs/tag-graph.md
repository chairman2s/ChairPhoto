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
  one for the inspector; **select a community** to light up all of its edges.
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
src/modules/plugins/tagGraph.tsx      the view, registered via registerMainView
src/modules/plugins/tagGraphBundle.ts  the radial layout — pure, unit-tested
src/modules/plugins/tagGraph.css
src-tauri/src/commands/graph.rs:66    library_graph — nodes and edges
src-tauri/src/catalog/mod.rs:1841     the queries
```

The module id is `tag-graph`. It is **frontend-only** — no `backendFeature`, and `library_graph`
is registered unconditionally, so it is available in every build including
`--no-default-features`.

`commands/graph.rs` also exposes `photo_tag_graph`, the photo↔tag bipartite graph, for views that
want individual photos as nodes rather than the tag-level projection this module draws.

## Limits

- The graph is computed live on each open and is not cached. The bipartite Photo ↔ tag mode
  keeps only the 1,500 most-tagged photos so its force layout stays animatable.
- Photos marked missing are excluded, so the picture reflects what the catalog can currently
  reach.
- It is a view of the vocabulary, not an editor — reparenting and merging tags happen in the tag
  tree (see [taxonomy.md](taxonomy.md)).
