# GPUI port: slippy map for the Map module

Research answer for wayfinder ticket #116 ("Slippy map for the Map module"), part of the
GPUI rewrite map #92. Question: what replaces Leaflet + leaflet.markercluster once the UI
is GPUI (gpui-kit / gpui-component 0.7 on `gpui-pre =0.3.7`), covering licence, the OSM
tile-usage policy, and how today's opt-in and privacy gating carry over.

Researched 2026-09-30 against `feature/gpui` at `ebe3259`. File:line references are to
that tree unless they name a crate in `~/.cargo/registry/src/index.crates.io-*/` or a
URL. Statements marked **(inference)** are reasoned, not observed.

**Built** (ticket #119, branch `wf/map-module`): as recommended below, with the owner's
privacy decision from #118 (ask per host on first open, remembered — first as the catalog
setting `map.tileHosts`, since the #119 review in this machine's preferences) instead of the `map.tiles.enabled` switch proposed under "Opt-in and privacy
in the port". What was built, and how to measure it: `docs/map-and-geotagging.md` § "The
GPUI map".

**GPUI rewrite: done.** React and the Tauri shell were removed at the cutover (#165); this
research and the module it describes answer only to the GPUI app from here on. The
`src-tauri`/webview references below are this document's research record of the pre-cutover
tree and are left as written.

## Recommendation

**Write our own map widget; use no map crate.** Split it in two:

1. **Headless core in the backend** (`crates/core/src/plugins/map/`, `map` feature, no
   GPUI dependency, unit-testable): Web Mercator tile math with fractional zoom, a
   policy-compliant tile fetcher (existing `reqwest`, fixed User-Agent, conditional
   revalidation), a disk tile cache, and grid clustering in world-pixel space.
2. **A GPUI `MapView`** in the Map module: one `canvas` that paints tiles as
   `RenderImage`s, clusters/markers as quads, and fences as `PathBuilder` paths, with
   pan (drag), wheel/pinch zoom around the cursor, click/double-click hit-testing, and
   gpui-component widgets for the fence list, fence editor, filmstrip and settings.

Estimated size: **about 2,000–2,500 lines of Rust including tests** (breakdown in
[Estimate](#estimate)), replacing `map.tsx` (1,371 lines) + `map.css` (556 lines).

Key reasons:

- No usable crate exists. The two maintained Rust slippy-map widgets are hard-wired to
  other UI stacks: `walkers` depends non-optionally on `egui`/`egui_extras`/`emath`, and
  `galileo` on `wgpu` + `winit` (crates.io dependency lists, below). Neither exposes a
  headless tile engine we could drive from a GPUI `canvas`.
- The parts we would reuse are small: tile math is ~80 lines, and the tile-math crates
  are tiny or unsuitable (`slippy-map-tiles` is **GPL-3.0** and last released 2020).
- Today's map is *not* policy-compliant (see [OSM tile policy](#osm-tile-usage-policy-and-todays-compliance));
  owning the fetcher is the simplest way to fix identification and caching, which a
  webview `<img>` never let us control.
- Every drawing and input primitive we need is already in `gpui-pre 0.3.7` (verified,
  [What it needs from GPUI](#what-it-needs-from-gpui)).
- Clustering needs are modest (parity = markercluster's 60 px radius, click opens the
  members in a filmstrip). Grid clustering is ~100–150 lines; `supercluster` (MIT) is a
  fallback if grid artefacts are disliked.

## What the Map module does today

### Surface and data

| Feature | Where |
|---|---|
| Optional module, `backendFeature: "map"`, registers a full-surface main view, a settings panel and an inspector "Geocode" panel | `src/modules/plugins/map.tsx:1315-1370` |
| Modules start disabled; enabled set persisted in setting `modules.enabled` | `src/modules/host.ts:1027`, `:1059-1062`, `:1312` |
| Module settings are namespaced by module id (`tileUrl` → `map.tileUrl`) | `src/modules/host.ts:962`; `docs/module-capabilities.md:196` |
| `map` cargo feature is on by default and pulls in `reqwest` | `src-tauri/Cargo.toml:22`, `:49-52`, `:141` |
| Points: `map_photo_points` → `(id, lat, lng)` from `photos_visible` where both GPS columns are non-null | `crates/core/src/plugins/map/mod.rs:162-189`; command `src-tauri/src/commands/map.rs:103-117` |
| Empty state when no photo has GPS; loading/error overlays; status strip with count | `map.tsx:973-1016` |

### Tiles

| Feature | Where |
|---|---|
| Default URL `https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png` (subdomain form) | `map.tsx:88-90` |
| URL is a user setting, editable with Save / Reset to default; hint mentions heavy-use alternatives | `map.tsx:1092-1209` |
| Leaflet `tileLayer(tileUrl, { attribution, maxZoom: 19 })`, swapped when the URL changes | `map.tsx:521-524`, `:553-563` |
| Initial view `center [20,0], zoom 2`, zoom control on | `map.tsx:514-518` |
| Fit to data extent on load: `fitBounds(pad 0.05, maxZoom 12)` | `map.tsx:605-613` |
| Attribution: Leaflet control **and** a status-strip "Tiles © OpenStreetMap contributors" | `map.tsx:91-92`, `:1013-1015` |
| Tiles are fetched by the webview as `<img>`; CSP `img-src` allows any `https:` so the URL can be user-set | `src-tauri/tauri.conf.json:29`; `docs/module-capabilities.md:196` |
| Leaflet 1.9.4 defaults: `crossOrigin: false`, `referrerPolicy: false` (no attribute set) | `node_modules/leaflet/src/layer/tile/TileLayer.js:76-87` (main checkout) |
| Zoom snaps to integers (Leaflet default `zoomSnap: 1`; not overridden in `map.tsx:514-518`) **(inference from Leaflet defaults; not checked in the running app)** | — |

### Clustering and selection

| Feature | Where |
|---|---|
| `markerClusterGroup({ maxClusterRadius: 60, spiderfyOnMaxZoom: true, showCoverageOnHover: false, animate: false, zoomToBoundsOnClick: false })` | `map.tsx:531-539` |
| Pin-shaped SVG marker, blue `#3b82f6` | `map.tsx:113-130` |
| Default cluster bubbles in three size classes (<10, <100, ≥100) | `node_modules/leaflet.markercluster/src/MarkerClusterGroup.js:821-827` (1.5.3) |
| Marker click → `selectPhotoSilent(id)` + filmstrip of one | `map.tsx:579-582` |
| Cluster click → all leaf ids → select first silently + filmstrip of all | `map.tsx:592-603` |
| `cluster.off("clusterclick")` removes **every** clusterclick handler, including markercluster's own `_zoomOrSpiderfy` bound on add (`MarkerClusterGroup.js:844-846`), so spiderfy never runs **(inference from source; confirm in app)** | `map.tsx:592` |
| Filmstrip: bottom strip of thumbnails; click → `selectPhotoSilent`; "Show in Library" → `selectPhoto` (navigates); Esc / × closes | `map.tsx:349-416`, `:933-950` |

### Geofences

| Feature | Where |
|---|---|
| Fence list overlay: colour swatch, name, tag path, Apply / Edit / Delete, "Apply all", "+ Draw" | `map.tsx:235-327` |
| Seven-colour palette cycled by list index | `map.tsx:95-107` |
| Drawing: click adds vertex (dot marker + dashed preview polygon, fill 0.15); click within 16 px of first vertex (≥3 vertices) or double-click closes; double-click-zoom disabled while drawing; duplicate last vertex from dblclick dropped; <3 vertices cancels | `map.tsx:617-733`, `:816-830` |
| Fence editor dialog: name + tag path required, Enter/Esc | `map.tsx:152-217`, `:833-863` |
| Existing fences: polygon (weight 2, fill 0.15), click selects in list, draggable vertex handles with live preview, drag end → `update_fence` | `map.tsx:736-813`, `:1043-1088` |
| Delete via `window.confirm`; Apply / Apply all → count toast + `notifyChange` | `map.tsx:873-928` |
| Backend CRUD + `apply_fence` / `apply_all_fences`, table `map__fences`, polygon JSON `[[lat,lng],…]` | `crates/core/src/plugins/map/mod.rs:40-160`, `:199-260`; `src-tauri/src/commands/map.rs:13-102` |
| `point_in_polygon`: planar ray casting, boundary counts as inside | `crates/core/src/plugins/map/mod.rs:400-445` |
| `set_photo_gps` command exists (catalog + XMP + re-apply fences) but has **no** caller in `map.tsx`; only listed in core `api.ts` | `src-tauri/src/commands/map.rs:118-157`; `src/modules/api.ts:48` |

### Reverse geocoding (not map UI, but same module)

| Feature | Where |
|---|---|
| Nominatim `/reverse`, endpoint setting `geocode.endpoint`, app User-Agent, global ≤1 req/s limiter, ~1 km cache table | `crates/core/src/plugins/map/geocode.rs:24-74`, `:242-340` |
| User-initiated only: inspector "Geocode location" and settings "Geocode all with GPS" with progress | `map.tsx:1125-1160`, `:1263-1311` |

This is already Rust; the GPUI port only rewrites its two small UIs.

### Opt-in and privacy gating today

- **The only gate is enabling the Map module** (default disabled, `host.ts:1027`). Its
  description names OpenStreetMap (`map.tsx:1319-1320`), but there is no separate,
  network-specific consent: opening the Map view requests tiles immediately
  (`map.tsx:521-524`), and `fitBounds` then requests tiles around the user's photo
  locations (`map.tsx:605-613`).
- **(inference)** Tile requests disclose the client IP plus which areas are viewed —
  after fit-to-data, roughly where the user's photos were taken. No photo bytes leave,
  so AGENTS.md's "network transfer of photos requires an explicit, feature-specific
  opt-in" is not literally triggered, but "Nothing leaves the user's machine or home
  storage by default" is arguably met only because the module is off by default.
- Reverse geocoding is user-initiated per click (`map.tsx:1273-1290`, `:1125-1160`).

## OSM tile usage policy and today's compliance

Source: <https://operations.osmfoundation.org/policies/tiles/> (fetched 2026-09-30; the
page shows no revision date). Quotes are from that page.

| Requirement | Policy | Today | GPUI plan |
|---|---|---|---|
| URL | "Use exactly: `https://tile.openstreetmap.org/{z}/{x}/{y}.png`" — "Other subdomains or hostnames may be slower or withdrawn without notice." | **Non-compliant**: default uses `{s}.tile.openstreetmap.org` (`map.tsx:89-90`). | Default to the exact URL; migrate a stored value equal to the old default. |
| Identification (§3.1, §3.4) | "Send a clear, unique User-Agent string that names your app…"; generic library UAs "will be blocked". "Native apps usually do not have a referer, this is ok." | **Non-compliant (inference)**: tiles come from WebKitGTK `<img>`, so they carry the webview's browser UA. No UA override found in `src-tauri/src/lib.rs` or `tauri.conf.json` (grep for `user_agent`); Referer is whatever the `tauri://` origin yields (not observed). | Fixed UA, e.g. `ChairPhoto/<version> (+https://github.com/chairman2s/ChairPhoto)`. `api.fetch` already uses `ChairPhoto/<version>` (`src-tauri/src/commands/net.rs:330`). Note `geocode.rs:31` still points at a stale `github.com/chairphoto/chairphoto` URL — separate fix. |
| Caching | "Honour server caching headers" or "cache each tile for at least 7 days"; never send no-cache by default; "Use Conditional Requests using `If-None-Match` and `If-Modified-Since`"; keep "a sufficient local cache". | **Unverified**: only WebKit's HTTP cache, which we neither configure nor inspect. | Own disk cache: store body + ETag/Last-Modified + expiry; serve fresh hits offline; revalidate expired tiles conditionally; floor of 7 days. |
| Bulk / prefetch | Prohibits pre-seeding, archives, wide scans (esp. z≥14), "download for offline", and background jobs fetching tiles "a user is not currently viewing". "Offline use is not permitted on `tile.openstreetmap.org`." | Compliant (Leaflet loads only visible tiles plus its small edge buffer). | Fetch only tiles intersecting the viewport (at most a one-tile margin, **(inference)** within the policy's intent); cancel requests for tiles that leave the view; never prefetch other zooms. Showing already-cached tiles offline is repeat viewing, not an offline feature **(inference)**. |
| Protocols | Recommends HTTP/2 or HTTP/3; no numeric concurrency limit. | n/a | One shared `reqwest::Client` (HTTP/2 via rustls/ALPN), small concurrency cap (e.g. 4). |
| Attribution | "Show OpenStreetMap licence attribution clearly on the map"; not hidden behind toggles or off-screen. | Compliant (`map.tsx:522`, `:1013-1015`). | Always-visible overlay in a map corner, text per provider (OSM default). |
| Heavy use | "If you cannot meet these requirements, please use an alternative OSM-derived service … or run your own." | Custom tile URL setting (`map.tsx:1172-1190`). | Keep the setting; apply the same UA and caching to every host. |

## Options considered

Crate facts from the crates.io API on 2026-09-30 (`/api/v1/crates/<name>` and
`/<version>/dependencies`).

| Option | Version / licence / last release | Fit |
|---|---|---|
| [`walkers`](https://crates.io/crates/walkers) | 0.60.0, MIT, 2026-09-22, ~1.16 M downloads | Actively maintained, but non-optional deps `egui ^0.36`, `egui_extras`, `emath`, `ecolor`. It *is* an egui widget; its tile fetch/cache (`reqwest`, `http-cache-reqwest`, `lru`) is not published separately. **Reject**; worth reading as reference for caching/UA design. |
| [`galileo`](https://crates.io/crates/galileo) | 0.2.1, MIT OR Apache-2.0, 2025-07-11 | Full GIS renderer; non-optional `wgpu ^24`, `winit ^0.30`, `lyon`, `font-kit`, older `reqwest ^0.11`. Would add a second GPU stack beside GPUI's. **Reject.** |
| [`slippy-map-tiles`](https://crates.io/crates/slippy-map-tiles) | 0.16.0, **GPL-3.0**, 2020-08-14 | Tile naming only; licence incompatible with a permissive app, unmaintained. **Reject.** |
| [`webmercator_tiles`](https://crates.io/crates/webmercator_tiles) 1.0.0 (MIT, 2025-01), [`web-mercator`](https://crates.io/crates/web-mercator) 0.1.1 (MIT, 2026-07, 76 downloads), [`geonative-tile`](https://crates.io/crates/geonative-tile) 0.4.0 (MIT/Apache, 2026-06) | — | Headless tile math, but each replaces ~80 lines we can own and test; low adoption. **Optional**, not recommended. |
| [`supercluster`](https://crates.io/crates/supercluster) | 3.0.8, MIT, 2026-02-05; deps `geojson`, `thiserror`, `twox-hash` | Headless port of Mapbox supercluster; builder with `radius`, `extent`, `min_points`, `max_zoom` ([docs.rs](https://docs.rs/supercluster/latest/supercluster/)). Input via GeoJSON features. **Fallback** for nicer clusters; heavier than needed for parity. |
| [`geo`](https://crates.io/crates/geo) 0.33.1 / [`geo-types`](https://crates.io/crates/geo-types) 0.7.20 | MIT OR Apache-2.0, 2026 | Not needed: `point_in_polygon` already exists (`plugins/map/mod.rs:407`) and screen-space hit tests are a few lines. |
| gpui-component 0.7 | — | No map/tile component (`ls gpui-component-0.7.0/src`; grep for `slippy`/`mercator`/`tile_url` in gpui-component and gpui-kit found nothing). It has `chart`/`plot`, which are unrelated. |
| GPUI `img(uri)` for tiles | `gpui-pre-0.3.7/src/elements/img.rs:42-76` (URI → `Resource`), backed by `gpui-pre-http-client` | **Reject (inference)**: we could not set a policy UA per request, do conditional revalidation, or keep a disk cache we control. Not checked what UA gpui's HTTP client sends. |
| **Own implementation** | reuses `reqwest 0.12` (`Cargo.toml:141`), `image 0.25` (`Cargo.toml:124`, same major as gpui's `image 0.25.1`, `gpui-pre-0.3.7/Cargo.toml:348-349`) | **Recommended.** |

## Design sketch

### Headless core (`plugins/map/`, `map` feature)

- **`tiles::math`**: `LatLng ↔ world px` at fractional zoom `z` (world size `256·2^z`),
  latitude clamped to ±85.0511°; visible tile range for a viewport at
  `floor(z)` with the scale `2^(z - floor z)`; `fit_bounds(bounds, viewport, pad, max_zoom)`
  (parity with `map.tsx:609`). Pure functions, property tests (round-trip, antimeridian
  wrap of `x`).
- **`tiles::source`**: URL template with `{z}{x}{y}` (keep accepting `{s}`, mapping it
  to `a` for non-OSM hosts), max zoom (19), attribution text. Default the exact OSM URL.
- **`tiles::fetch`**: one `reqwest::Client` with the ChairPhoto UA, concurrency cap,
  conditional GET (`If-None-Match` / `If-Modified-Since`) on stale entries, never
  `Cache-Control: no-cache`. Returns bytes + validators + expiry
  (`max(server max-age/Expires, 7 days)`). Tests against a local mock server, like
  `geocode.rs:497-520`.
- **`tiles::cache`**: disk cache under the platform cache dir
  (`~/.cache/chairphoto/tiles/<host-hash>/<z>/<x>/<y>.png` + a sidecar with
  validators/expiry), size-capped LRU eviction. Not in the catalog: tiles are not
  catalog data and must not travel with bundles/merge **(inference; confirm with
  `docs/storage-and-import.md` owners)**.
- **`cluster`**: grid clustering in world-pixel space at `round(z)`: cell = 60 px
  (markercluster radius), bucket points by cell, cluster centroid = mean of members,
  members kept as ids for the filmstrip. Recompute on integer-zoom change on a
  background executor; O(n) per zoom. Swap for `supercluster` behind the same
  function if the grid look is unacceptable.
- `map_photo_points`, fence CRUD, apply and geocode code are unchanged.

### GPUI `MapView`

- State: `center: LatLng`, `zoom: f64` (fractional), viewport size, cluster set for
  the current integer zoom, tile LRU of `Arc<RenderImage>` keyed by `(z,x,y)`,
  in-flight tile tasks keyed the same way (dropped when out of view), fences, drawing
  state, filmstrip state.
- Paint order inside one `canvas`, clipped with `with_content_mask`: background →
  tiles (fallback: nearest cached parent tile scaled, so zooming never shows blanks) →
  fence fills/strokes → drawing preview (dashed) → vertex handles → clusters/markers →
  (outside canvas) overlays: fence list, attribution, status, filmstrip, dialogs.
- Input: drag on empty map pans; drag on a vertex handle edits the fence (live path,
  `update_fence` on mouse-up); wheel zooms about the cursor (`ScrollDelta::Lines` in
  steps, `Pixels` continuously for touchpads); pinch zooms; double-click zooms in unless
  drawing (then closes the polygon, as `map.tsx:718-724`); click hit-tests
  markers/clusters (screen-space radius), then fences (reuse `point_in_polygon` on
  unprojected coords), then adds a vertex when drawing; Esc closes the filmstrip /
  cancels drawing.
- Tile decode: PNG bytes → `image` → BGRA `Frame` → `RenderImage::new`, off the UI
  thread; evicted tiles released with `Window::drop_image` so the sprite atlas does not
  grow without bound.

### Opt-in and privacy in the port

Proposal (for the Map module port ticket to confirm with the owner):

- Enabling the Map module keeps granting local features only: points, clusters,
  fences, apply. **Tile loading gets its own explicit switch** (setting
  `map.tiles.enabled`, default off) shown in the Map view's empty-tiles state as
  "Load map tiles from `<host>`" with a one-line disclosure (IP and viewed areas are
  sent to the tile host). Without it the map draws markers/fences on a plain
  graticule. This matches AGENTS.md's "feature-specific opt-in" wording more closely
  than today's module-level gate.
- The switch is per host: changing the tile URL to a new host asks again.
  Loopback/LAN tile servers could be exempt **(open question)**.
- Reverse geocoding stays user-initiated per action; no change.
- Attribution overlay is always drawn when tiles are shown.

## What it needs from GPUI

All verified in `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/gpui-pre-0.3.7/`.

| Need | API | Where |
|---|---|---|
| Custom painting element | `canvas(prepaint, paint)` | `src/elements/canvas.rs:10-19` |
| Fence fill/stroke, dashed preview | `PathBuilder::fill()`, `::stroke(width)`, `.dash_array(&[Pixels])`, `.add_polygon(points, closed)`, `.move_to/.line_to/.close`, `.build()` | `src/path_builder.rs:88`, `:96`, `:108`, `:189`, `:125-131`, `:199`, `:244` |
| Paint a path | `Window::paint_path(path, color)` | `src/window.rs:4573` |
| Markers, cluster bubbles, vertex dots | `Window::paint_quad(PaintQuad)`, `fill(bounds, bg)` (rounded corners make circles) | `src/window.rs:4502`, `:7620` |
| Tiles as GPU images | `Window::paint_image(bounds, image_bounds, radii, Arc<RenderImage>, frame, grayscale)`; `RenderImage::new(frames)` (BGRA, `image::Frame`) | `src/window.rs:4871-4879`; `src/assets.rs:43-67` |
| Release evicted tiles from the atlas | `Window::drop_image(Arc<RenderImage>)` | `src/window.rs:4989` |
| Clip to the map bounds | `Window::with_content_mask` | `src/window.rs:3981` |
| Alternative: tiles as `img` elements | `ImageSource::Render(Arc<RenderImage>)` | `src/elements/img.rs:42-49` |
| Wheel zoom | `on_scroll_wheel`; `ScrollWheelEvent { position, delta, modifiers, touch_phase }`; `ScrollDelta::{Pixels, Lines}`; `pixel_delta(line_height)` | `src/elements/div.rs:390`; `src/interactive.rs:522-563`, `:614` |
| Pinch zoom | `on_pinch`, `PinchEvent`; emitted by the Linux Wayland and X11 clients | `src/elements/div.rs:405`; `src/interactive.rs:571`; `gpui-pre-linux-0.3.7/src/linux/{wayland,x11}/client.rs` (grep `PinchEvent`) |
| Pan drag, vertex drag | `on_mouse_down` / `on_mouse_move` / `on_mouse_up` (a hand-rolled drag; `on_drag` is for drag-and-drop payloads) | `src/elements/div.rs:126`, `:303`, `:210`, `:615` |
| Double-click | `MouseDownEvent::click_count` | `src/interactive.rs:148-159` |
| Smooth zoom / fling animation | `Window::request_animation_frame` | `src/window.rs:2622` |

Not verified (behaviour, not API): how fast `paint_image` is with ~40–80 tile sprites
per frame at the EIZO's 29.9 Hz budget, and whether `touch_phase`/`Pixels` deltas are
delivered for Linux touchpads the way the macOS path does. A first slice should measure
both.

## Estimate

Lines of Rust, including tests **(estimate, not measured)**:

| Piece | Lines |
|---|---|
| Tile math + fit-bounds + tests | 150–200 |
| Tile source / fetcher / disk cache + mock-server tests | 400–500 |
| Tile LRU, decode, atlas eviction, parent-tile fallback | 200–250 |
| Clustering + tests | 150–200 |
| `MapView` canvas paint + pan/zoom/hit-testing | 450–550 |
| Fence drawing and vertex editing | 250–300 |
| Overlays: fence list, editor dialog, filmstrip, status, attribution, empty state, settings, opt-in | 400–500 |
| **Total** | **~2,000–2,500** |

Roughly 3–5 sessions, split so the headless core (math, fetch, cache, cluster) lands and
is tested first, then a read-only map (tiles + clusters + filmstrip), then fences.

## Risks

- **Policy regressions are silent**: a wrong UA or missing cache gets the app blocked
  without notice (policy §3.4). Mitigate with a test that asserts the UA and conditional
  headers against the mock server.
- **Atlas growth**: forgetting `drop_image` on eviction leaks GPU memory as the user pans.
- **Input feel**: touchpad pixel scrolling and pinch on Linux are unverified; wheel-zoom
  step and anchoring need hands-on tuning in the real app (`chairphoto-app` skill).
- **Cluster parity**: grid clustering looks different from markercluster's distance
  clustering (boxy groups near cell edges). Acceptable for parity is a judgment call;
  `supercluster` is the fallback.
- **Opt-in change is user-visible**: existing users with the module enabled would see
  a "Load map tiles" prompt once. Needs the owner's agreement.
- **Adjacent defects found** (not part of this ticket): default tile URL uses the
  deprecated `{s}` subdomain form (`map.tsx:89-90`); Nominatim UA points at a stale
  repository URL (`geocode.rs:31`).

## Proposed follow-up ticket

**Title:** "Map module port: slippy map, tile cache, clustering and fences in GPUI"

**Question:** Given the own-widget design in `docs/plans/gpui/map.md`, should tile
loading get its own per-host opt-in (default off, markers on a plain background until
accepted), or stay gated only by enabling the Map module as today? The answer fixes the
first slice's scope and the settings schema (`map.tiles.enabled`, `map.tileUrl`
migration off the `{s}` URL).
