---
title: "Obsidian notes"
description: "Companion notes for a photo or a tag in an Obsidian vault, linked both ways."
tags:
  - chairphoto/module
  - chairphoto/integration
aliases:
  - "Vault notes"
---

# Obsidian notes

Keep a companion note in an [Obsidian](https://obsidian.md) vault for a **photo** or for a
**tag**, linked in both directions: ChairPhoto opens `obsidian://` to create or open the
note, and the note carries a `chairphoto://` link that jumps back to the photo or tag.

The module is **frontend-only** — there is no Cargo feature and no backend. Enable it in
**Preferences → Modules**.

ChairPhoto never writes into your vault. The note is created by **Obsidian itself** through
its [URI scheme](https://obsidian.md/help/Extending+Obsidian/Obsidian+URI); ChairPhoto only
hands over the file path and the initial contents. No image is exported.

## Setup

The module's Preferences tab takes two values:

| setting | meaning |
|---------|---------|
| **Vault name** | Your vault's name as Obsidian knows it — not a path. |
| **Notes folder** | Folder inside the vault for the notes. Defaults to `ChairPhoto`. |

Creating a note without a vault name set shows a reminder rather than failing silently.

## Photo notes

The inspector gains a **Note** panel. **Create note in Obsidian** opens Obsidian with a new
note prefilled; once one exists the panel offers **Open note** and **Forget**.

The note is named from the capture date, the filename stem, and the first 8 characters of
the photo's UUID — readable, and collision-proof across cards that reuse `DSC` numbers:

```
ChairPhoto/2026-07-04 _81A8352 3f9c1a7e
```

Its frontmatter carries what the catalog knows, omitting fields the photo does not have:

```yaml
---
type: ChairPhoto
chairphoto: 3f9c1a7e-…          # the photo's stable UUID
photo: _81A8352.ARW
captured: 2026-07-04T09:30:00
camera: ILCE-7RM6
lens: FE 24-70mm F2.8 GM
exposure: f/2.8 · 1/500s · ISO 400
rating: 4                        # omitted when unrated
tags:
  - Transportation/Cars/Coupé    # up to 20
---

[Open in ChairPhoto](chairphoto://…) · [Open in loupe](chairphoto://…/loupe)
```

## Tag notes

The tag editor gains an **Obsidian note** section, for writing down what a tag actually
means — the kind of definition that does not belong in the tag's short description.

Tag notes live under `<folder>/Tags/`, named from the tag's leaf name plus its UUID prefix,
so two tags called `Bridge` under different parents never collide.

```yaml
---
type: ChairPhotoTag
chairphoto-tag: 8b2e…            # the tag's stable UUID
tag: Places/Vestfold/Tønsberg
aliases:                          # every taxonomy term: translations + synonyms
  - Tønsberg
  - Tunsberg
tags:
  - Places/Vestfold/Tønsberg
---

[Show photos in ChairPhoto](chairphoto://tag/8b2e…)
```

The **aliases** come from the tag's translations and synonyms, so the note is findable in
Obsidian by any term your taxonomy knows — see [taxonomy.md](taxonomy.md). The tag's
description, if it has one, becomes the note body.

## Tag names become Obsidian tags

Obsidian tags cannot contain spaces, so each path segment is CamelCased while the `/`
hierarchy is preserved — Obsidian nests tags on `/` the same way ChairPhoto does:

```
Street Photography/Old Town   →   StreetPhotography/OldTown
```

## Linking back

`chairphoto://` is a real OS-level scheme, registered in `tauri.conf.json` and delivered by
`tauri-plugin-deep-link`, with `tauri-plugin-single-instance` forwarding a second launch's
URL into the running app rather than starting a new one. Three targets:

| link | opens |
|------|-------|
| `chairphoto://<photo-uuid>` | the library grid, with that photo selected |
| `chairphoto://<photo-uuid>/loupe` | the loupe |
| `chairphoto://<photo-uuid>/develop` | the Develop editor |
| `chairphoto://tag/<tag-uuid>` | the library filtered to that tag |

The created note only writes the first two, but all four are accepted, so you can hand-write
a `/develop` link in a note. Both two- and three-slash forms parse (`chairphoto://…` and
`chairphoto:///…`), and the scheme is case-insensitive.

Because every link keys on a **UUID rather than a name or path**, it survives renaming a
tag, re-parenting it, moving the photo on disk, or merging catalogs across machines.

## Where it lives

`src/modules/plugins/obsidian.tsx`. The module registers an inspector panel, a tag-editor
panel, and a settings panel, and reaches the OS only through `openExternal`.

The photo↔note mapping is stored in the module's own settings, keyed by the subject's UUID
— `obsidian.note.<photo-uuid>` and `obsidian.tagnote.<tag-uuid>` after the host namespaces
them — holding the vault, the vault-relative file path, and a creation timestamp.

## Limits

- **Obsidian must be installed**, and the vault name must match exactly. ChairPhoto cannot
  discover vaults; it only hands a URI to the OS.
- The initial note contents are URL-encoded into the `obsidian://new` URI, so they are kept
  compact. Photo tags and tag aliases are capped at 20 each.
- One note per photo and one per tag.
- **Forget** removes ChairPhoto's link only. The note stays in your vault, and ChairPhoto
  never deletes or edits vault files — after creation the note is entirely yours.
