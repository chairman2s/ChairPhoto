---
title: "Map & geo-tagging"
description: "Plot photos by GPS, auto-tag them with geofences, and reverse-geocode locations."
tags:
  - chairphoto/module
  - chairphoto/tagging
aliases:
  - "Map"
  - "Geofences"
  - "Geotagging"
---

# Map & geo-tagging

An optional module (heavy: a map library plus tiles) that does two things with a photo's
GPS coordinates, which the scanner already reads from EXIF into `gps_latitude` /
`gps_longitude`:

1. **Map view** — plot photos on a map, clustered; click a marker to select and inspect.
2. **Geofence auto-tagging** — draw named areas on the map, each bound to a hierarchical
   place tag (e.g. `Places/Vestfold/Tønsberg/Brygga`); photos whose GPS falls inside a
   fence get that place tag.

Place tags created this way are **normal hierarchical tags** — they filter, export, and
live in the taxonomy like any other, per [taxonomy.md](taxonomy.md).

## Geofences

**Shape.** A freeform polygon, tested with `point_in_polygon` (ray casting, even-odd rule)
in `src-tauri/src/plugins/map/mod.rs`, covered by tests for vertices, edges, concave
polygons, and degenerate cases. Pure Rust, no extra dependencies.

**Storage.** The plugin-owned table `map__fences(id, name, tag_path, polygon, created_at)`,
with the polygon stored as JSON `[[lat,lng],…]` — the `<plugin>__` prefix marks the table
as owned by the module.

**When tags are applied.** GPS never changes, so nothing is continuously recomputed:

- On **import**, photos with GPS inside a fence get the place tag as a normal assignment.
  The scanner calls `apply_fences_to_photo` for new photos.
- Adding or editing a fence gives you an explicit **Apply** action that re-scans and
  backfills matches — `apply_fence(fence_id)` for one, `apply_all_fences()` for all.
  Re-applying is idempotent.
- Assignments are **editable and never auto-removed**. You can hand-correct drift, or add
  the tag to a photo that has no GPS at all.

Because the tag is seeded and then owned by you, geofence place tags are **not** marked
`auto_rule` — unlike the monochrome auto-tag, which is continuously rebuilt.

Tags are created through the normal `create_tag` (which builds the hierarchy) and
`assign_tag` paths. Commands: `list_fences`, `create_fence`, `update_fence`,
`delete_fence`, `apply_fence`, `apply_all_fences`, and `map_photo_points` — a lightweight
`(id, lat, lng)` query that feeds the map view.

## Map view

`src/modules/plugins/map.tsx` registers a full-surface main view (`registerMainView`)
rendering **Leaflet** over **OpenStreetMap** raster tiles, with clustered markers from
`map_photo_points` and click-to-select. The tile source URL is a setting, so you can point
it elsewhere.

Polygons are drawn on the map by clicking to add vertices and double-clicking to close,
and can be edited or deleted. A fence list overlay shows each fence's name and bound tag
path, with per-fence **Apply** and **Apply all** reporting a result count. The fence editor
requires both a name and a tag path. When no photo in the catalog has GPS, the view shows
an empty state explaining that GPS is read from EXIF during the scan.

Leaflet (BSD-2) and MapLibre (BSD-3) are permissive; OpenStreetMap tiles are free under
ODbL with **attribution** and a usage policy — fine for personal desktop use, with a custom
tile source available for heavy use.

## Reverse-geocoding

Geofences handle the fine personal spots a geocoder will never know. Reverse-geocoding
handles the broad administrative areas: a GPS coordinate becomes coarse country / state /
city via OSM Nominatim, filling **empty** IPTC location fields.

`src-tauri/src/plugins/map/geocode.rs` performs the lookup at coarse zoom and caches it in
`map__geocode_cache`, keyed on lat/lng rounded to ~1 km (0.01° ≈ 1.1 km at the equator), so
nearby photos share a single network call. The cache stores city, state, country, and
country code with a timestamp; a cache hit makes no HTTP request. Tests run against a
mocked HTTP server — never live Nominatim.

Three commands:

| command | what it does |
|---------|--------------|
| `reverse_geocode_photo(photo_id)` | Look up and return the location, or `null` when the photo has no GPS. Serves the cache when the ~1 km cell is already known. Writes nothing. |
| `geocode_to_iptc(photo_id)` | Fill the photo's **empty** `iptc_city` / `iptc_state` / `iptc_country` / `iptc_country_code`. Returns whether anything was filled. |
| `geocode_all_to_iptc()` | The same fill across the library, emitting `geocode:progress` events. Returns a summary of totals, filled, and skipped. |

Fields that already hold a value are **never overwritten** — this fills blanks, it does not
replace. The write path is `set_iptc` + `xmp::write_iptc`, identical to a manual IPTC save,
so XMP sidecars stay merge-safe. The single-photo path uses a TOCTOU-safe three-step
pattern (read GPS and check cache, async HTTP, store result) so it never blocks the UI
thread. The inspector exposes "Geocode location" for one photo and "Geocode all with GPS"
for the batch, with a progress bar and a summary.

### Nominatim usage policy

[Nominatim's terms](https://operations.osmfoundation.org/policies/nominatim/) require a
meaningful User-Agent and at most one request per second. Both are enforced in code, with
no configuration needed:

- ChairPhoto sends `User-Agent: ChairPhoto/0.1 (photo-organizer;
  https://github.com/chairphoto/chairphoto)`.
- A global rate limiter holds a mutex across the sleep, so concurrent callers cannot race
  past the ≤1 req/s limit. The limiter is a global static, so the single-photo and batch
  commands share one budget.

### Self-hosting

You can run your own [Nominatim](https://nominatim.org/release-docs/latest/admin/Installation/)
and point ChairPhoto at it with the catalog setting `geocode.endpoint`:

```sql
UPDATE settings SET value = 'https://my-nominatim-instance:8080' WHERE key = 'geocode.endpoint';
```

The ≤1 req/s limit and the User-Agent apply to self-hosted instances too — self-hosting
does not bypass the throttle.
