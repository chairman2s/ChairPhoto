---
title: "Tag Graph"
description: "The tag vocabulary drawn as a force-directed co-occurrence graph."
tags:
  - chairphoto/module
  - chairphoto/tagging
  - chairphoto/insights
aliases:
  - "Graph view"
---

# Tag Graph

A force-directed picture of your tag vocabulary — which tags you actually use, and which ones
keep turning up together. It answers questions a tag tree cannot: where the vocabulary has
clustered, which branches are dead, and which pairs are effectively synonyms in practice.

Enable **Tag Graph** in Preferences → Modules and it appears as its own main view.

## What it draws

- **Nodes are tags**, sized by how many photos carry them. Only tags with at least one
  non-missing photo appear, so unused branches of the vocabulary stay out of the picture.
- **Edges are co-occurrence** — two tags are linked when they appear on the same photos, and
  the link strengthens with how often that happens.

The useful reading is the clustering. Tags that sit tightly together describe the same kind of
photo, which is a good signal that they belong on the same branch, or that one should be a
synonym rather than its own tag. Isolated nodes are the opposite — vocabulary you created once
and never reused.

Selecting a tag pulls up its photos through the normal `list_photos` path, so the graph is a way
into the library rather than a dead end.

## Where it lives

```
src/modules/plugins/tagGraph.tsx      the view, registered via registerMainView
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

- The graph is computed live on each open and is not cached; a large vocabulary takes a moment
  to settle into its layout.
- Photos marked missing are excluded, so the picture reflects what the catalog can currently
  reach.
- It is a view of the vocabulary, not an editor — reparenting and merging tags happen in the tag
  tree (see [taxonomy.md](taxonomy.md)).
