---
title: "Smart albums"
description: "Saved rules that resolve to a photo set, evaluated live."
tags:
  - chairphoto/core
  - chairphoto/library
---

# Smart albums

A **smart album** is a saved, named **rule** that resolves to a photo set — the dynamic
counterpart to a manual album. It appears in the left sidebar next to manual albums;
selecting it filters the grid to the photos matching its rule.

Membership is evaluated **live**: the rule is translated to a SQL `WHERE` clause and
ANDed into `list_photos` on every view, so it is always current. There is no membership
table, no rebuild, and no staleness. Conditions run on promoted, indexed `photos` columns,
so it stays fast on large libraries.

## Rule JSON

The rule is stored as a JSON string in `smart_albums.rule_json`, opaque to the schema and
interpreted by the `rule_to_sql` translator — the same arrangement as
`photo_edits.edit_json`.

```jsonc
{
  "match": "all",            // AND across all conditions
  "conditions": [
    { "field": "rating",        "op": "gte",     "value": 4 },
    { "field": "camera_model",  "op": "is",      "value": "ILCE-7RM6" },
    { "field": "lens",          "op": "contains","value": "24-70" },
    { "field": "iso",           "op": "lte",     "value": 400 },
    { "field": "capture_time",  "op": "between", "value": ["2026-06-01", "2026-07-01"] },
    { "field": "pick_state",    "op": "is",      "value": "pick" },
    { "field": "tag",           "op": "under",   "value": 42 },     // tag id, incl. descendants
    { "field": "batch",         "op": "is",      "value": 7 },      // import_batch_id
    { "field": "flag",          "op": "is",      "value": "has-gps" }
  ]
}
```

Conditions are a flat AND list. The `match` key is always `"all"`; the schema leaves room
for nested AND/OR, which is not implemented.

### Field / operator matrix

| field (group)                     | column / source                  | ops |
|-----------------------------------|----------------------------------|-----|
| `rating` (culling)                | `p.rating` (int 0–5)             | `eq` `gte` `lte` `between` |
| `color_label` (culling)           | `p.color_label` (text)           | `is` `isNot` `isSet` |
| `pick_state` (culling)            | `p.pick_state` (none/pick/reject)| `is` `isNot` |
| `camera_make` `camera_model` `lens` `shutter_speed` (capture, text) | `p.<col>` | `is` `contains` |
| `iso` (capture, int)              | `p.iso`                          | `eq` `gte` `lte` `between` |
| `aperture` `focal_length` (capture, real) | `p.<col>`                | `eq` `gte` `lte` `between` |
| `capture_time` (date)             | `p.capture_time` (ISO text)      | `before` `after` `between` |
| `tag` (tags)                      | `photo_tags` + `descendant_tag_ids` | `under` (incl. descendants) `is` (exact) |
| `batch` (batch)                   | `p.import_batch_id`              | `is` |
| `flag` (flags)                    | derived predicate                | `is` — values: `has-gps`, `monochrome` (`p.is_grayscale=1`), `is-raw` (extension set) |

Text `contains` becomes a case-insensitive `LIKE '%v%'`. `capture_time` compares ISO
strings, which sorts correctly for ISO 8601. A `tag`/`under` condition expands via
`descendant_tag_ids` into an `EXISTS(… pt.tag_id IN (…))`, so multiple tag conditions AND
correctly. Every value is a **bound parameter** — no string interpolation anywhere. An
unknown field or operator is a validation error, never silently ignored.

## Behaviour

- An **empty rule** (no conditions) matches all photos; the builder shows a hint.
- Selecting a smart album sets it as the active filter and clears any active
  tag/album/batch — it *is* the filter. The culling chips (all/pick/…) still AND on top.
- Ordering is capture-time; smart albums have no manual member order.

## Where it lives

```sql
smart_albums(id, uuid, name, rule_json, position, created_at, updated_at)
```

No `smart_album_photos` table — membership is the query result.

- `catalog/smart_albums.rs` — CRUD plus the `rule_to_sql` translator with its field/op
  allowlist, unit-tested per field and operator.
- `list_photos` takes an optional `smart_album_id`; when set, the album's rule is
  translated and ANDed into the existing WHERE builder.
- Commands: `list_smart_albums`, `create_smart_album`, `rename_smart_album`,
  `set_smart_album_rule`, `delete_smart_album`, `reorder_smart_albums`, and
  `smart_album_count(ruleJson)` — a `COUNT(*)` through the translator that drives the
  builder's live match count.

```
list_smart_albums() -> [{ id, uuid, name, ruleJson, photoCount }]
create_smart_album(name, ruleJson) -> id
smart_album_count(ruleJson) -> number
list_photos(..., smartAlbumId?)
```

- Frontend: `components/SmartAlbumsPanel.tsx` is the sidebar section (list with counts,
  select-to-filter, rename, delete, new, edit rule); `components/SmartAlbumEditor.tsx` is
  the modal rule builder — grouped field picker, an operator picker driven by the field's
  type, a value input per type (number, text, enum dropdown, date range, tag picker
  reusing the tag tree, batch dropdown, flag dropdown) — with the live match count,
  round-tripping the Rule JSON.
