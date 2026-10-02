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
in `crates/core/src/plugins/map/mod.rs`, covered by tests for vertices, edges, concave
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

### The GPUI map (`crates/app/src/modules/map/`)

The GPUI port draws the map itself — no map library (`docs/plans/gpui/map.md`). The
headless half lives in the core under `plugins::map`: `tiles::math` (Web Mercator, fractional
zoom, the visible tile grid, fit-to-points), `tiles::source` (the tile URL), `tiles::cache`
and `tiles::fetch` (below), and `cluster` (grid clustering, 60 px). The module paints tiles,
fences and markers on one canvas, with the fence list, filmstrip, status bar and fence editor
as overlays, and contributes the same settings and inspector "Geocode" panels as React.

**Tiles need the user's yes, per host** (decision #118). The first time the map opens with a
tile host it has no answer for, a card asks whether to load tiles from that host and says
what a tile request reveals (the IP address and roughly where the photos are). The answer is
a per-machine preference, `map.tileHosts` in `machine-prefs.json` (`{"tile.openstreetmap.org":
true}`): a tile request reveals this computer's address whichever catalog is open, so another
catalog does not ask again. It is changeable in Preferences → Map ("Block", "Ask again") or
from the status bar's "Map tiles off" chip. Until a host is allowed nothing is fetched;
markers and fences show on a plain background with a graticule. A new tile URL on another
host asks again. The consent host is the host a filled-in tile URL goes to, and a
template with a placeholder in its host is refused. A tile server's redirect is followed
only to its own host or another allowed one; anywhere else fails the tile without
contacting that host. The tile URL itself stays the catalog setting `map.tileUrl`: consent is
keyed by the host it names, so it need not move. Answers the first port stored per catalog
(the module setting `map.tileHosts`) move to the machine on that catalog's first read —
only allowed/denied entries; where they disagree with the machine's or another catalog's,
denied wins — and the catalog's copy is then emptied, so a later Allow is not undone.
Reverse geocoding stays user-initiated per click, as before.

**OSM tile policy.** The default URL is the policy's exact
`https://tile.openstreetmap.org/{z}/{x}/{y}.png`; React's stored `{s}.` default reads as it,
and `{s}` is never used (dropped on OSM's host, `a` elsewhere). Every request carries
`plugins::map::USER_AGENT`. Tiles are cached on disk under
`$XDG_CACHE_HOME/chairphoto/tiles/` (512 MiB cap, least recently used evicted), fresh for
`max(max-age, 7 days)`, then revalidated with `If-None-Match`/`If-Modified-Since`; a stale
tile is shown when revalidation fails. A 2xx answer is cached only if it decodes as an
image, and a body over 2 MiB fails the tile; a failed tile is asked for again after a
backoff that doubles from 2 s up to 2 minutes while it stays in view. Only the tiles intersecting the view load, at most
four requests at a time, and a load that leaves the view before its request starts is
cancelled. Decoded tiles are GPU textures in a least-recently-used set of 256; a tile on
screen is never evicted, so a 4K canvas showing ~300 tiles holds them all rather than
reloading its own tiles in a loop. The attribution is always on the status bar while tiles
show.

**Measuring.** `CHAIRPHOTO_MAP_TIMING=1` logs the map's render and paint CPU time per frame
(p50/p95/max every 120 frames); `cargo run --release -p chairphoto-app --example map_bench`
times clustering and the tile grid on synthetic points.

## Reverse-geocoding

Geofences handle the fine personal spots a geocoder will never know. Reverse-geocoding
handles the broad administrative areas: a GPS coordinate becomes coarse country / state /
city via OSM Nominatim, filling **empty** IPTC location fields.

`crates/core/src/plugins/map/geocode.rs` performs the lookup at coarse zoom and caches it in
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
so XMP sidecars stay merge-safe: `write_iptc` is given the IPTC before and after the fill and
writes only the fields that changed, so a creator, rights, caption or title another tool put
in the sidecar — which the catalog never imported — survives a geocode (#144). Both fills are bound to the catalog they read the photo
from (`CatalogIdentity`): the cache, the row, the sidecar path and the IPTC written into it
all come from that catalog, and a catalog switch during the Nominatim call makes the next
step fail closed with `CATALOG_CHANGED` instead of writing the new catalog's row (whose ids
collide) or the old catalog's sidecar. The GPUI module binds its fence writes the same way,
to the catalog the fences were read from. In the GPUI module, Geocode all is an owned job
(`geocode_all_to_iptc_with`): at most one run; Cancel, a catalog switch or disabling the
module sets its abort flag and aborts its task (dropping a pending Nominatim request), and
its progress and result land only while it is still the current run. The single-photo path uses a TOCTOU-safe three-step
pattern (read GPS and check cache, async HTTP, store result) so it never blocks the UI
thread. The inspector exposes "Geocode location" for one photo and "Geocode all with GPS"
for the batch, with a progress bar and a summary.

### Nominatim usage policy

[Nominatim's terms](https://operations.osmfoundation.org/policies/nominatim/) require a
meaningful User-Agent and at most one request per second. Both are enforced in code, with
no configuration needed:

- ChairPhoto sends `User-Agent: ChairPhoto (photo organizer;
  +https://github.com/chairman2s/ChairPhoto)` (`plugins::map::USER_AGENT`, which the map
  tiles send too).
- A global rate limiter holds a mutex across the sleep, so concurrent callers cannot race
  past the ≤1 req/s limit. The limiter is a global static, so the single-photo and batch
  commands share one budget.
- Each request gives up after 20 s (`NOMINATIM_TIMEOUT`), so a server that never answers
  cannot hold a geocode forever.

### Self-hosting

You can run your own [Nominatim](https://nominatim.org/release-docs/latest/admin/Installation/)
and point ChairPhoto at it with the catalog setting `geocode.endpoint`:

```sql
UPDATE settings SET value = 'https://my-nominatim-instance:8080' WHERE key = 'geocode.endpoint';
```

The ≤1 req/s limit and the User-Agent apply to self-hosted instances too — self-hosting
does not bypass the throttle.
