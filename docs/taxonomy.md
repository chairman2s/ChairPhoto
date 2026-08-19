---
title: "Taxonomy — Model & Licensing"
description: "The controlled-vocabulary tag model, export keywords and the licensing position."
tags:
  - chairphoto/core
  - chairphoto/tagging
aliases:
  - "Tags"
  - "Vocabulary"
---

# Taxonomy — Model & Licensing

> ChairPhoto's tag system is a **controlled vocabulary / thesaurus** (the Getty
> model), not a flat folksonomy (Flickr). This doc describes how the vocabulary is
> structured and the licensing position.

## The model

Tags form a hierarchical controlled vocabulary. Each tag has:
- a **canonical name** + nested path (the "preferred term" + broader/narrower),
- **synonyms** (variant terms), each with an export flag,
- **translations** (per-language names),
- a **description** (definition),
- an **exportable** flag (`tags.exportable`): when off, the tag is *organizational* —
  it still groups photos in the library but is never emitted as an export keyword or as a
  segment of the `lr:hierarchicalSubject` path (its descendants still export). Set it in
  the tag editor ("Export" section). Mirrors the darktable `_`-prefix convention; the gate
  lives in `Catalog::export_labels` / `path_labels`.
- drag-and-drop reparenting.

This maps directly onto a classic thesaurus (preferred term, variant terms,
broader/narrower terms, definitions, multilingual) — which is the right structure for
curated, high-quality organisation and for sharing vocabularies.

## How the big platforms organise tags (research, 2026)

- **Flickr — folksonomy**: flat, free user tags; no controlled vocabulary; plus
  **machine tags** (`namespace:predicate=value`) for structured/external refs;
  organisation via albums/collections/groups separate from tags.
- **Getty — controlled vocabulary**: AAT/TGN/ULAN thesauri with preferred terms,
  variant terms, broader/narrower, **related** terms, definitions, **stable IDs**, and
  **domain separation** (AAT = things, TGN = places, ULAN = people).
- **Stock (Adobe/Shutterstock)**: controlled vocab needs *fewer* keywords because
  hierarchy + synonyms auto-expand; keyword **order** matters (primary subject first);
  workflow general → specific.

Sources: Getty intro to vocabularies; AAT (Wikipedia); Flickr machine tags
(code.flickr); Shutterstock/Adobe keywording guides.

## Stable identity and export expansion

- **Stable tag IDs.** Every tag has a stable **local** `uuid` (unique index, assigned on
  create, backfilled on migrate), so a user's own catalogs **merge by ID, not name** across
  machines (laptop↔desktop) — robust against renames and translations. The uuid identifies
  *your* tag, and its stability is load-bearing: the photo↔tag merge depends on it, so a
  tag's uuid is never rewritten once assigned.
- **Hierarchical export expansion** — `Catalog::export_labels_with_ancestors`
  returns a tag's export labels plus all ancestors' (deduped); `assemble_export_keywords`
  builds the per-photo keyword set (flat `dc:subject` = every assigned tag + ancestors +
  per-language synonyms; `lr:hierarchicalSubject` = each tag's pipe-joined path), and the
  exporter emits it into the Hand-off sidecar via the merge-safe `xmp::write_keywords`.
  Keywords are ordered by **specificity**: most-specific assigned tag first, leaf
  before ancestors — the stock-submission convention.

## Tag maintenance (A5)

A vocabulary of a few hundred roots needs surgery occasionally, and the operations that
reshape it used to be hand-written SQL against a live catalog. They now live in
`catalog/tag_maintenance.rs`, exposed through `commands/tags.rs`.

**They are pure catalog transactions.** `xmp::write_keywords` has exactly one caller — the
export destination — so there is no in-library sidecar to rewrite, no job family, no
resumability. Every function takes a `&Connection` and never commits: the *caller* owns the
transaction, which is what makes `dry_run` trustworthy. A preview is the real mutation rolled
back, not a second implementation that can drift from the one that writes.

### The operations

| Operation | Where |
|---|---|
| **Merge** tags into one | `merge_tags` — new |
| **Split** a tag by moving selected photos to another | `split_tag` — new |
| **Find orphans** (tags holding no photos anywhere below them) | `find_orphan_tags` — new |
| **Find near-duplicates** | `find_similar_tags` — new |
| **Rename a subtree** | `Catalog::rename_tag` — already rewrites every descendant's path |
| **Promote to a top-level axis** | `Catalog::move_tag(id, None)`; the tag tree's Move dialog calls this "↑ Top level" |

The last two were on the A5 wish list and turned out to already exist. Note the vocabulary:
promoting a tag makes it a top-level **axis** (People, Place, Brand), *not* a "facet" —
facets are the EXIF-derived internal filters in `catalog/facets.rs`, are never exported, and
are not tags at all.

### Why a merge is not `UPDATE photo_tags SET tag_id`

Each of these is handled, and each has a test:

- **`tags.parent_id` cascades on delete.** Children are reparented onto the target *before*
  the source row goes, or deleting the source takes its whole subtree with it.
- **`full_path_norm` is UNIQUE.** Descendant paths are recomputed and every collision found
  before the first write, then refused by name. Two sources whose children would land on one
  path are refused the same way. Merging `A` into `B` when both own a child called `x` is a
  refusal, not a recursive merge — merge that pair first.
- **`photo_tags` PK `(photo_id, tag_id)`.** A photo carrying both collapses to one row
  keeping the **earlier** `created_at` — when the user first said this about this photo.
- **Smart album rules store tag ids.** `rule_json` conditions are rewritten and the albums
  named in the report; a rule that will not parse is left exactly as found.
- **Auto-tags recompute membership.** Merging *into* an auto-tag is refused (the engine
  deletes and re-derives its assignments, so the merge would silently undo itself); merging
  one *away* warns that the engine re-creates it by path, empty.
- **Tags survive in bundles.** See the tombstone below.
- **Plugins hold tag references.** See ownership below.

`tag_synonyms.synonym_norm` is UNIQUE **catalog-wide**, so two tags can never hold the same
normalized synonym and moving `tag_id` cannot collide. (The A5 design predicted a hazard the
schema rules out.) `tag_terms` is unique per `(tag, text, language)`, so those genuinely can
collide: they are skipped and **named** in the report.

### The tombstone — what makes a merge permanent

`tags.uuid` is how shared taxonomies merge by identity rather than name, and that cuts both
ways: `catalog/merge.rs` matches an incoming bundle tag **by uuid first**, so importing a
bundle exported before a merge would re-create the tag that was merged away, quietly undoing
the reorg.

`tag_aliases (dead_uuid, target_tag_id, dead_path, merged_at)` records where each merged-away
uuid went, and the bundle importer consults it before creating anything. A merge that
repoints an earlier merge's target rewrites the older rows too, so a chain never strands a
uuid. Without this, A5 would be cosmetic — it would survive exactly until the next bundle
import.

### Plugin state, and who moves it

Core owns no knowledge of `faces__*` or `smarttags__*`. Each plugin exposes its own repoint
(`faces::store::repoint_person_tags`, `smarttags::classifier::forget_merged_tag_paths`) and
`commands/tags.rs` composes them into the merge's single transaction.

**Plugins move first, and the order is load-bearing.** `faces__faces.person_tag_id` is
`ON DELETE SET NULL`, so the moment core deletes a source tag the cascade un-names every face
that pointed at it — a repoint running afterwards finds nothing and reports a truthful-looking
zero while the data is being lost. `faces__rejections.person_tag_id` has no FK at all and
would simply dangle. Smart Tagging keys on the tag *path*, which likewise exists only while
the tag does.

A plugin failure **fails the whole merge**, inverting the usual "a missing optional capability
degrades only the cosmetic behaviour" rule: faces detached from the person they belong to is
data loss, not a missing nicety.

Report counters are three-valued for plugin state: `None` = that plugin is compiled out and
nothing was checked; `Some(0)` = checked, nothing there.

### Finding duplicates

`find_similar_tags` compares **leaf names** (so `Place/Bergen` and `People/Bergen` surface as
a pair worth a glance) with a length prefilter, and reports per-pair co-occurrence — the
signal that separates a typo from two real concepts. Co-occurrence is *not* used to find
pairs: that needs an all-pairs self-join over `photo_tags`, tens of millions of rows on a
six-figure catalog. The documented cost is that two duplicates with unlike names (`Bike`,
`Velocipede`) will not appear.

## Faceted axes (insight from photo contests)

Photo contests (Sony WPA, Nat Geo, National Wildlife, Nature Photography Contest, NY
Photography Awards, MonoVision) organise along **three different axes**, not one:

1. **Genre/style** — Portrait, Landscape, Street, Architecture, Wildlife, Travel, Still
   Life, Creative/Abstract, Documentary, Sports, Food, Lifestyle. Very consistent — a
   standard genre vocabulary.
2. **Treatment/colour** — Black & White / Monochrome is orthogonal to genre (MonoVision
   has the same genres *inside* a B&W contest). NOT a genre.
3. **Capture technique / subject domain** — Aerial/Drone, Mobile, Macro, Astro/Night,
   Underwater; subject splits (Birds/Mammals/Insects).

Design consequence — keep these on the *right* axis:
- **Genre → tag taxonomy.** The common contest genres make a **safe-to-ship starter
  vocabulary** ("Photography Genres") — generic terms, not copyrightable.
- **Colour/treatment (B&W) → an AUTO-TAG, not a smart album, not a hand tag.** Decided:
  monochrome must be exportable (`#bnw #blackandwhite #monochrome`), and only a *tag*
  flows through the export pipeline — a pure facet stays internal. So make it an
  **auto-tag**: a real tag (lives in the tree, has synonyms/hashtags + translations,
  filters, exports) whose **assignment is computed by a rule, not done by hand**.
  - Concept: an auto-tag = derived/system-managed tag. Applied automatically (at scan,
    and re-applied when edits change) and kept in sync; otherwise identical to a normal
    tag. Mark/group as system-managed (e.g. a `Treatment` branch) so it isn't clutter.
  - Because it's a tag, **filtering by it inside any album/tag just works** (the tag
    filter ANDs with the current view) AND it **exports** with its hashtags. Both the
    "narrow the current album to B&W" need and the "share #bnw" need are met.
  - **Monochrome rule**: **pixel-derived** — `photos.is_grayscale`, computed by
    sampling the embedded preview's chroma during caching. Camera "B&W" flags proved
    unreliable (the Sony A7R VI writes a stale `CreativeStyle=B&W` on *every* frame), so
    we detect B&W from the actual image, not metadata. A B&W *edit* will set the flag
    later too (RAW is colour — "make it B&W" is a develop function, see plugin-system.md).
  - **The rule engine**: the auto-tag engine (`catalog/autotags.rs`) is data-driven —
    each rule is `(rule key, tag path, export hashtags, a match SELECT)`. Built-in rules:
    `monochrome` (`Treatment/Black & White`), `long-exposure` (`Technique/Long Exposure`,
    shutter ≥ 1 s; `#longexposure …`), `panorama` (`Technique/Panorama`, long side ≥ 2× short
    side; `#panorama #pano`). Add a rule by appending one entry to `auto_tag_rules()`.
  - **Facets vs auto-tags**: want it shared/exported → **auto-tag**; purely-internal
    filtering you'd never share (has-GPS, shot-on-mobile, drone) → **facet**.
  - **Filter bar**: the catalog ANDs culling + tag + album in
    `list_photos`; the `FilterBar` component surfaces the active scope as removable
    chips.
  - **Facets**: derived, internal-only filters computed from EXIF —
    `has-gps`, `mobile`, `drone` (`catalog/facets.rs`). They AND into `list_photos`
    and appear in the filter bar as add/remove chips, but are **never exported**
    (no tag, no XMP). Add a facet by appending one `(key, label, SQL predicate)` row.
- **Capture technique → mostly EXIF-derived + smart albums.** Mobile vs camera
  (Make/Model), drone (model), macro (lens/focal heuristics), astro/night (long
  exposure + high ISO + night capture). Feed smart albums, not the tag tree.
- **"Contest entry" needs no new primitive** — it's an **album** of photos classified by
  the genre vocabulary (+ e.g. a B&W smart album).

Do NOT push colour/technique into the tag hierarchy (that's the folksonomy mistake).

## Hashtags & variant terms (guidance)

Social hashtag lists (e.g. street photography) mix three kinds of thing — treat each
differently; do NOT dump them all into one bucket:

1. **Variant forms of one concept** (`#streetphotography #streetphoto #streetphotographer
   #streettogs #streetlife`) → **synonyms** of a single canonical tag
   (`Street Photography`). They aren't dictionary synonyms, but they ARE controlled-
   vocabulary *variant terms* (Getty "Used For"). Tag the concept once; emit the chosen
   synonyms on export (per-synonym export flag; per-language, e.g. nl `#straatfotografie`).
   This is the key win over folksonomy: one clean concept, many export aliases.
2. **Distinct sub-concepts** (Candid, Urban, Architecture, Night, Documentary, Film) →
   **their own tags**, not synonyms (they apply beyond the parent genre). Colour
   (B&W/Colour) is the colour **attribute** (auto-detect → smart album), not a synonym.
3. **Community / reach / collective hashtags** (`#streetdreamsmag #lensculturestreets
   #magnumphotos #ig_street #everybodystreet`) → NOT synonyms and NOT content tags;
   they're publish-time *distribution* labels. Keep them out of the content taxonomy;
   put them in a **hashtag bundle** (reuse tag groups, or a dedicated export hashtag
   set) applied at social-export time.

Export payoff: posting a photo emits the concept's export-flagged synonyms **+** a
chosen reach-hashtag bundle, trimmed to the platform's tag limit.

## Make / model identity (vehicles — and the general pattern)

How to model "this is a **BMW M2**" so it publishes correctly (the driving goal: right
hashtags on Instagram/social) **and** browses well. Decided with the owner.

**It must be TAGS, not metadata.** Everything that produces publishable output or lets you
browse runs off the **tag tree**: hashtags (`assemble_hashtag_bundle`), keywords/XMP
(`assemble_export_keywords`), and filtering (`list_photos` filters by tag/album/batch/facet
only — there is **no** filter-by-metadata-value). So make/model in a metadata field would be
invisible to hashtags *and* unbrowsable. An "Instagram module" converts a photo's **tags**
to hashtags, never metadata. (This is the same reason B&W is an auto-*tag*, not a facet.)

**Two axes, not one nested tree** — keep type and make/model on separate axes (domain
separation, the same principle as genre vs treatment above):
- **Type** (what it *is*, clean broader/narrower): `Transportation/Cars`,
  `Transportation/Motorcycles` — like the existing `Transportation/Watercraft/Ferry`.
  Gives `#car`/`#motorcycle` and "show all cars". **Body style** is the natural deeper
  level on this same axis (it's still an is-a): `Transportation/Cars/Coupé`,
  `…/Cars/Sedan`, `…/Cars/Cabriolet`, `…/Cars/SUV`. A coupé *is* a car, so this is clean,
  browses ("all coupés" across makes), and emits `#coupe`.
- **Make / model** (who *made* it): `Manufacturer/<make>/<model>`, e.g.
  `Manufacturer/BMW/M2`. A model number is manufacturer-scoped, so model-under-make is a
  consistent "made-by" hierarchy. One BMW node serves "show all BMW" across cars **and**
  motorcycles. Keep the **model** here a clean designation (`M2`); the **body style**
  (Coupé) goes on the type axis above, not baked into the model node.

A BMW M2 Coupé gets all of: `Transportation/Cars/Coupé` (type + body style) **and**
`Manufacturer/BMW/M2` (make/model). This makes "all cars", "all BMW", and "BMW ∩
Motorcycles" all work — which a single `Transportation/Cars/BMW/M2` tree breaks (it spawns
a *second*, unrelated BMW node the day you add a BMW motorcycle) and it also reads as the
false is-a "BMW is a kind of car". Avoid the nested form.

**Hashtags come from a tag's own labels, NOT its ancestors.** `assemble_hashtag_bundle`
emits each member tag's canonical name **+ export-flagged synonyms** only — it does *not*
walk ancestors (ancestor expansion lives in `export_labels_with_ancestors`, used by keyword
export, not hashtags). So:
- The lever for good hashtags is **synonyms on the tag**, not tree depth. Tree shape above
  is chosen for *browsing*; it barely affects hashtag output.
- Create the `M2` tag with export synonyms like `BMW M2`, `BMW`, `M Power` → `#bmwm2`,
  `#bmw`, `#mpower`. Without them an `M2` tag alone only yields `#m2`.
- The `Manufacturer` **root should be marked non-exporting** (per-term export flag) so it
  never leaks `#manufacturer`.

This keeps subject and maker/model on separate axes, and generalises beyond vehicles to
any "subject + maker/model" pair.

## Quick-tag groups and hashtag bundles

- **"Recently used" auto group** — a virtual quick-tag group (sentinel id, not stored) of
  the tags you most recently applied **by hand**, newest first. Backed by a `last_used_at`
  column on `tags` (schema v13) bumped inside `assign_tag` (the manual path: quick-tag,
  inspector, AI-accept) on every apply, including re-applies. The auto-tag engine writes
  `photo_tags` directly and bypasses `assign_tag`, so machine tags never appear here — this
  is why we track `last_used_at` rather than `MAX(photo_tags.created_at)`, which the auto-tag
  rebuild re-stamps to `now()` every scan. `Catalog::recently_used_tags(limit)` +
  `recently_used_tags` command; surfaced as the first chip in the QuickTagBar.
- **Social hashtag bundles on export** — `Catalog::assemble_hashtag_bundle(group, limit)`
  renders a tag group's member export-labels to deduped `#hashtags` (Unicode-safe,
  capped to a platform limit). Core export-to-disk emits them BOTH as `hashtags.txt`
  and a copyable field in the Export dialog. Per-platform destinations (Flickr/
  Instagram/SmugMug) are **module** territory — each may format/limit tags differently.

## Licensing position (checked)

No conflict. Reasoning:

- We are borrowing **structural/design concepts** (thesaurus shape: preferred terms,
  synonyms, broader/narrower, related terms, hierarchy; Flickr's machine-tag namespacing
  convention; Lightroom's "contained keywords" export idea). **Ideas, methods, and data
  structures are not copyrightable** — using the design pattern is free and clear.
- We are **not copying anyone's vocabulary content** (term lists, hierarchies,
  definitions). ChairPhoto ships no vocabulary content — the tags are the user's own.
- **IPTC** is an open standard we implement; **stock keywording practices** are general
  know-how. No issue.

A standing rule about content rather than architecture: any **starter or community
vocabulary** shipped with ChairPhoto must come from **openly licensed** data, with
attribution:
- **Getty Vocabularies** (AAT/TGN/ULAN) are released under **ODC-By 1.0** — free to use,
  adapt, and redistribute **with attribution** to the J. Paul Getty Trust (cite the
  vocabulary + contributors/sources). So Getty data is even usable directly if wanted.
- Other open sources: **Wikidata** (CC0), iNaturalist taxonomy, etc.
- Do **not** scrape proprietary keyword lists (e.g. a stock agency's controlled vocab).

Source: Getty Vocabularies "Obtain"/LOD pages (ODC-By 1.0).

