---
title: "My Tag Taxonomy — foundation & conventions"
description: "One photographer's tag conventions, as a worked example of the taxonomy model."
tags:
  - chairphoto/example
  - chairphoto/tagging
aliases:
  - "Tag conventions"
---

# My Tag Taxonomy — foundation & conventions

The personal rules this library's tags follow. Tag *against* this, and keep it consistent
— a good foundation is cheaper than re-tagging thousands of photos later. See
[taxonomy.md](taxonomy.md) for how the underlying system (hierarchy, synonyms, export
flag, hierarchical expansion) works.

## The seven rules

1. **Facets are separate branches.** Orthogonal dimensions never share a path:
   - *What it is* (the subject/object): `Transportation`, `Nature`, `Architecture`, …
   - *Where* (location): `Places/Norway/Vestfold/Tønsberg`
   - *When / occasion*: `Event/Festival`
   - *Who made it* (brand/model): `Manufacturer/BMW/M2`
   - *Who's in it* (people): `People/…`

   Don't cram one facet into another. A BMW M2 is *made by* BMW **and** *is a* car — two
   tags from two branches, not `Manufacturer/Cars/BMW/M2`.

2. **One scheme, held consistently.** Flat domains (what we use) — **not** a half-faceted
   mix. Avoid wrapping some content under `Subject/` while `Places`, `Event`, … stay
   top-level. If you ever switch to explicit facets, convert *every* root.

3. **is-a nesting only.** A child must be *a kind of* its parent. "M2 is a BMW" ✓;
   "BMW is a Car" ✗. If it fails the is-a test, it belongs in a different branch. (Naming
   the grouping node so is-a holds is fine: "Saga is a *Boat maker* is a *Manufacturer*".)

4. **Tag the leaf; ancestors are implied.** Assign the most specific tag only — the
   hierarchy expands ancestors on filter, count, and export. The app auto-prunes redundant
   parents on assign/paste, so a photo carries leaves only.

5. **Scaffolding ≠ content → mark it non-exportable.** Grouping roots that aren't useful
   hashtags (`Manufacturer`, any facet root, sometimes `Transportation`/`Places`) get the
   tag editor's **Organizational — don't export** flag. The leaves still export
   (`#bmw`, `#boat`, `#tønsberg`).

6. **Identity over labels.** Rename freely — a tag's stability is its structure + UUID,
   not its text. Use synonyms / translations / export terms for label and hashtag variants
   (model `315` → export `#saga315`; `M2` → `#bmwm2`).

7. **Start shallow, deepen on demand.** Don't pre-build a giant tree. Add a branch when a
   photo actually needs it. Speculative depth you never use is just friction.

## Top-level skeleton (flat domains)

Each is a real category; each *leaf* is a usable hashtag. `*` = non-exportable scaffolding.

```
Nature           Transportation       Architecture        People
Places           Event                Manufacturer*       Activity / Concept
Landmark
```

## Don't invent from zero

Adopt a battle-tested photographer's controlled vocabulary as the skeleton and prune/extend
it — it encodes thousands of these decisions already made well:
- **Getty AAT** (the thesaurus model [taxonomy.md](taxonomy.md) follows)
- David Riecks' **Controlled Vocabulary Keyword Catalog**
- Lightroom keyword catalogs
