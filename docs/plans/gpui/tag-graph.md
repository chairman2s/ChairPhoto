# Tag graph on GPUI: layout and drawing

Status: research for wayfinder ticket #117 (map #92). Implemented, Communities only, by
#121 on `wf/tag-graph`: `crates/model/src/tag_graph/` (layout, scene, labels, session) and
`crates/app/src/modules/tag_graph/` (view, tiny-skia raster, canvas). The owner's decisions
in #120 apply: Photo ↔ tag dropped, horizontal labels, base edges soft while zooming. One
detail differs from the plan below: the raster strips stroke 8 overlap rows each, because
tiny-skia anti-aliases a pixmap's first rows differently (seams otherwise).
Measurements: `cargo run --release -p chairphoto-app --example tag_graph_bench`.
Base: `feature/gpui` at `ebe3259`. Toolchain: gpui-pre 0.3.7 (with gpui-pre-wgpu 0.3.7) and
rustc 1.98.1, measured 2026-09-30.

## The question rests on a wrong premise

The ticket asks what should replace d3-force (many-body, link, center, collide) for
`tagGraph.tsx`. Since `1a2f3a0` ("graph: radial edge bundling replaces the force-directed
community view"), the **main Communities view uses no physics at all**. It is a
deterministic radial hierarchical edge bundling (Holten 2006) built by
`tagGraphBundle.ts::buildBundleLayout` (`src/modules/plugins/tagGraphBundle.ts:110-272`),
and `docs/tag-graph.md` says as much: "The layout is deterministic — no physics, no settling".

d3-force is still used in one place: the **legacy "Photo ↔ tag" bipartite mode**
(`tagGraph.tsx:38`, `:433-480`), which caps its input at the 1,500 most-tagged photos
(`BIPARTITE_PHOTO_CAP`, `tagGraph.tsx:119`, `:344-346`).

So the port has two separate problems:

1. **Communities (the default view).** Port the bundle layout, which is pure geometry, and
   work out how to paint about 8,600 bundled curves. The painting turns out to be the hard
   part (see [Drawing](#drawing-on-gpui-pre-037)).
2. **Photo ↔ tag.** Decide whether to keep the mode. If it stays, it needs a d3-compatible
   force simulation.

## Today's implementation

### Scale it is designed for

- Communities: **1,400 tags and 8,600 co-occurrence edges**. These are the numbers in the
  `1a2f3a0` commit message, the only recorded measurement. `docs/tag-graph.md` gives no
  counts. The same commit and `tagGraph.tsx:186-188` say a 1,400-node force layout became a
  "hairball". Cameras add one node per camera model, plus camera↔tag edges
  (`catalog/mod.rs:1954-2013`).
- Photo ↔ tag: at most **1,500 photos** plus the tags they use
  (`tagGraph.tsx:344-349`; `docs/tag-graph.md` "Limits"). `photo_tag_graph`
  (`catalog/mod.rs:2018-2051`) returns every tagged photo, and the frontend truncates the list.
- Data comes from `library_graph` and `photo_tag_graph` (`src-tauri/src/commands/graph.rs:29-109`),
  which return JSON. It is recomputed on every open and never cached.

### Forces and parameters (Photo ↔ tag only, `tagGraph.tsx:441-456`)

| d3 setting | Value | Source |
|---|---|---|
| `alphaDecay` | `0.04` (d3 default ≈ 0.0228); `alphaMin` 0.001 → 170 ticks to settle | `:444` |
| `velocityDecay` | d3 default 0.4 | not set |
| `forceLink` | `.distance(28).strength(0.4)`, id accessor `d.id` | `:445-451` |
| `forceManyBody` | `.strength(-60)`; theta, distanceMin and distanceMax at the d3 defaults | `:452` |
| `forceCollide` | `.radius(d => d.r + 2)`, strength 1, 1 iteration | `:453` |
| `forceCenter` | `(0, 0)` | `:454` |
| `forceX` / `forceY` | `(0)`, `.strength(0.04)` | `:455-456` |
| node radius | tags `4 + √count·2.2` (`:111`); photos `PHOTO_R = 13` (`:115`) | |
| restart | freeze → `sim.stop()`; unfreeze → `alpha(0.3).restart()` | `:474-480` |
| drag | `alphaTarget(0.3).restart()`, pins `fx`/`fy` while moving, `alphaTarget(0)` and unpins on release | `:833-859` |

d3 runs the forces in insertion order: link, charge, collide, center, x, y.

### Communities layout (`tagGraphBundle.ts`)

- It builds a trie over tag paths (`:113-125`). Cameras hang under `CAMERA_GROUP` (`:56`, `:107`).
- A tag that has photos and children gets a "self" leaf (`:127-142`).
- Subtree totals and heights are computed, and children are sorted by total (`:144-165`).
- Ring slots follow a depth-first walk, with `SIBLING_GAP = 0.35` and `GROUP_GAP = 2.5` slot
  widths between runs (`:75-77`, `:167-203`). The ring starts at 12 o'clock.
- Internal nodes use `d3.cluster` radii by height (`:205-220`). Each top-level group gets an
  arc (`:222-234`). Edges follow a lowest-common-ancestor polyline (`:239-263`).
- `bundlePath` (`:293-349`) is d3-shape's `curveBundle` (β = `BUNDLE_BETA = 0.85`,
  `tagGraph.tsx:129`) feeding `curveBasis`. It emits one moveTo, one lineTo and a run of cubic
  Béziers per edge.
- The file is covered by 18 unit tests (`src/modules/plugins/__tests__/tagGraphBundle.test.ts`).
- Geometry constants: `RING_R = 400` graph units, `LABEL_EXTENT = 100` px,
  `LABEL_MAX_CHARS = 16` (`tagGraph.tsx:126-128`).

### Painting (`tagGraph.tsx`)

- There is one `<canvas>`. Refs plus a dirty flag drive one requestAnimationFrame loop
  (`:1395-1407`), and a ResizeObserver marks the canvas dirty (`:1409-1417`).
- **Bundle mode** (`drawBundle`, `:975-1194`):
  - The un-highlighted edges are cached as one `Path2D` per (colour, alpha step), rebuilt only
    when the layout, link set or isolation changes (`:993-1028`).
  - Alpha is `0.05 + 0.35·log1p(w)/log1p(max)`, rounded to steps of 1/50. The colour is the
    community colour of the heavier end, or amber for camera edges (`:1000-1019`).
  - Strokes are `1/k` wide, so they stay 1 screen pixel at any zoom (`:1029-1036`).
  - When something is in focus, base edges drop to ×0.2 and the lit edges are redrawn 1.5 px
    wide at 0.85 alpha, coloured by the far end (`:1031`, `:1037-1062`).
  - Chrome is drawn in screen space: group arcs 3 px wide at `R+6` (`:1072-1079`); dots of
    radius `1.5 + 4.5·√(count/max)` (`:1086-1106`).
  - Labels are placed greedily by count, 12 px apart in angle, at most 500
    (`:1109-1135`). The hovered or selected label is forced, and its neighbours are added
    where they fit (`:1131-1135`). Each label is **rotated along its radius and stroked with
    a halo** (`strokeText` + `fillText`, `:1137-1158`).
  - Group names are rotated along their arcs (`:1160-1180`). A tooltip box is drawn next to
    the hovered node (`drawTooltip`, `:943-970`, `:1183-1193`).
- **Force mode** (`:1200-1379`, after the bundle hand-off at `:1231`): links are bucketed by (colour, width, dash, alpha) with
  viewport culling. Hierarchy links are dashed `[3,3]`. Photo thumbnails come through
  `convertFileSrc(…, "thumb")` and are clipped to circles (`:1316-1335`). Labels appear only
  when `k·11 ≥ 6` (`:1303`, `:1357`).

### Interactions

| Interaction | Behaviour | file:line |
|---|---|---|
| Wheel zoom | ×1.15 per notch about the cursor, k clamped to [0.05, 6]. The listener is non-passive so it can `preventDefault` | `tagGraph.tsx:805-822` |
| Zoom buttons | −/＋ ×1.2 about the centre | `:741-750`, `:1590-1595` |
| Fit / Re-center | Fit: the ring plus the label band, or the node bbox with 48 px padding in force mode. Re-center: `{k:1,x:0,y:0}` | `:752-798`, `:800-803`, `:1597-1602` |
| Auto-fit | Once per graph, when the ring first has nodes | `:1386-1393` |
| Pan | Pointerdown on empty canvas deselects, then drags the view (window-level move/up listeners) | `:861-877` |
| Select (bundle) | Pointerdown on a ring slot selects it immediately. No drag | `:827-832` |
| Drag (force) | Drags a node by pinning fx/fy. Movement under 4 px counts as a click and selects. When frozen, the node moves directly | `:826`, `:833-860` |
| Hover | Hit test and cursor change (pointer or grab). Hover lights the node's edges and dims the rest | `:879-887` |
| Pointer leave | Clears hover | `:889-894` |
| Hit test (bundle) | By angle: within the ring tolerance `8/k`, or in the label band only for slots that carry a label | `:704-728`; label slots from `labelledRef` (`:216`) |
| Hit test (force) | The topmost circle under the pointer, radius `r+2` | `:729-737` |
| Keyboard | Esc deselects. With nothing selected it climbs one branch level | `:906-918` |
| Left panel | Breadcrumbs (`:1423-1443`); node-type toggles (`:1445-1475`); communities list, which toggles the active community (`:1476-1494`); link-strength slider 0–20 (`:1495-1513`); Freeze and Thumbnails switches in force mode (`:1516-1540`); mode segmented control (`:1542-1557`) | |
| Inspector | CONNECTED chips select a node; Filter → `api.filterByTag`; Isolate toggles the neighbourhood; Open loupe; Focus on branch (`:1614-1643`). The community card offers the same actions (`:1645-1663`) | |
| Loupe mirror | `api.showInLoupe(card)`, which follows the selection and is released on unmount | `:612-661` |
| Top photos | `list_photos` with `{tagId, window:{0,6}}` | `:663-684` |

## Force-layout options (only needed for Photo ↔ tag)

| Crate | Version / date | Licence | Algorithm | Fit |
|---|---|---|---|---|
| **fjadra** | 0.2.1, 2024-12-11. Last code commit 2024-12-30; the repo was pushed 2026-03-07 | MIT OR Apache-2.0 | A port of d3-force: Link, ManyBody (Barnes–Hut quadtree), Collide, Center, PositionX/Y. It keeps d3's defaults (alpha_min 0.001, decay `1-0.001^(1/300)`, velocity factor 0.6, phyllotaxis initial positions; `simulation.rs:33-45`, `:84-91`). Used by Rerun, 1.28M downloads | **Covers all six forces with every parameter we set.** Gaps below |
| force_graph | 0.4.0, 2025-11-21 | MIT | Its own spring/charge model with fixed parameters (`lib.rs:80-98`). O(n²) pairwise repulsion (`lib.rs:226-234`) | Not d3 semantics, so today's look would be lost. Too slow at about 2,900 nodes (inferred from the O(n²) loop, not measured) |
| fdg-sim | 0.9.1, 2022-12-17 | MIT | Fruchterman–Reingold, O(n²) repulsion (`force/fruchterman_reingold.rs:119-139`) | Unmaintained since 2022. Not d3 |
| forceatlas2 | 0.8.0, 2025-10-12 | **AGPL-3.0-only** | ForceAtlas2 | Different algorithm, and its licence would put AGPL terms on the GPL-3.0-only app. Rejected |
| Port only what we use | | ours | The d3-force link, many-body, collide and center/x/y, plus the d3-quadtree parts they need | About 700–900 lines (estimate), mostly the quadtree and Barnes–Hut. Reimplements what fjadra already does |

### fjadra gaps, checked in `~/.cargo/registry/src/*/fjadra-0.2.1`

- **No runtime mutation.** `Simulation.particles` is private (`force/simulation.rs:117-126`).
  The only accessors are `positions()` (`:199`) and `set_alpha()` (`:236`), and there is no
  `set_alpha_target`. d3's drag protocol (pin `fx`/`fy`, `alphaTarget(0.3)`) therefore cannot
  be expressed. Upstream issue grtlr/fjadra#6 ("Ability to mutate node graph of simulation?")
  is still open. The fix is small (make `particles_mut()` public and add `set_alpha_target`,
  roughly 20–40 lines) but needs a fork or an upstream PR.
- **Force order is alphabetical.** Forces live in a `BTreeMap<String, Force>` (`:124`), so they
  run in name order, not insertion order. The workaround is to name them
  `0link, 1charge, 2collide, 3center, 4x, 5y`, as the benchmark does.
- Minor: `Particle::with_fixed_y` sets `fx` instead of `fy` (`force/particle.rs:51-54`). We do
  not use it. Upstream issue #5 tracks non-determinism, which does not matter here because d3
  is seeded randomly too.

## Measurements

These come from a throwaway crate outside the repository
(`/tmp/claude-1000/-home-chairman-Projects-chairphoto/40e120f2-5cd5-445c-bab3-5199e6aa85cb/scratchpad/forcebench`, not committed and ephemeral: `cargo build --release`, then `./target/release/forcebench`) plus a
Node baseline (`node d3bench.mjs`) that uses the repo's own `node_modules/d3-force` 3.0.0 on
**the same generated graphs**. The machine is a Ryzen 9 3900X. The load average was about 15
during the later runs (other agents were building), so each figure is the **range over runs**.

**The graphs are synthetic.** Photos get 1–15 tags each (mean 8), weighted toward popular
tags. Bundles use a 12 × 6 × ~20 hierarchy with 1,400 leaves and 8,556 edges, 60% of them
inside one family. I could not read the real catalog in this session (access to the
application-data database was denied), so none of this is real-library data.

### Force simulation (Photo ↔ tag config, until alpha < alphaMin = 170 ticks)

| Graph | fjadra 0.2.1 (release) | d3-force 3.0.0 (Node 25) |
|---|---|---|
| 500 photos → 900 nodes / 3,953 links | 4.1–6.0 ms/tick (166–245 ticks/s); settle 0.7–1.0 s | 6.0–6.1 ms/tick (163–166 ticks/s); 1.0 s |
| **1,500 photos (the cap) → 2,893 nodes / 11,908 links** | **15.8–24.7 ms/tick (40–63 ticks/s); settle 2.7–4.2 s** | 23.9–26.0 ms/tick (38–42 ticks/s); 4.1–4.4 s |
| 3,000 photos → 4,400 nodes / 23,625 links | 26.9–39.1 ms/tick (26–37 ticks/s) | not run |

- **Same look (a coarse check):** the final x-extent is 2,957 for fjadra and 2,940 for d3 at
  1,500 photos, and 1,090 against 1,110 at 500. I did not compare pictures.
- Measured with fjadra on the 1,500 graph, 170 ticks: removing collide saves 0.8–1.0 s of
  2.7–3.2 s, and removing link saves 0.8–1.2 s. Removing charge changes nothing (3.1–2.8 s),
  presumably because collide then does more work on clumped nodes. That explanation is an
  inference.
- Inference: at the cap a tick costs more than the 16.7 ms frame budget in either engine.
  Today d3-timer ticks on the UI thread and drops frames. In GPUI the simulation must tick
  on the background executor (`App::background_executor`, `gpui-pre src/app.rs:300`) and
  publish position snapshots. The graph would then settle in about 3–4 s, as it does today.

### Communities edges: GPUI path tessellation versus CPU raster

`PathBuilder::build` does lyon stroke tessellation into `VertexBuffers<_, u16>`
(`gpui-pre src/path_builder.rs:244-319`). I ran the same tessellation over 8,556 bundled
curves, using a Rust transcription of `bundlePath`, 1 px strokes, and 96 (colour, alpha)
buckets as today.

| Ring radius on screen | Tolerance | Vertices / triangles | Buckets over the u16 index limit | CPU time |
|---|---|---|---|---|
| 400 px | 0.1 (the default) | 1.04 M / 1.03 M | 0 / 96 | 43–63 ms |
| 600 px | 0.1 | 0.99 M / 0.98 M | **4 / 96** (tessellation error) | 49–69 ms |
| 1,200 px | 0.1 | 1.21 M / 1.20 M | **6 / 96** | 63–181 ms |
| 600 px | 0.5 | 0.63 M / 0.61 M | 0 / 96 | 24–38 ms |

Tessellating one path per edge costs 3–16 µs per edge.

**Computed, not measured:** GPUI turns every triangle into three unindexed vertices
(`Path::push_triangle`, `gpui-pre src/scene.rs:876-910`; `build_path`,
`path_builder.rs:322-345`). The renderer then uploads one 104-byte
`PathRasterizationVertex` per vertex **every frame** (`gpui-pre-wgpu src/wgpu_renderer.rs:107-112`,
`:1836-1860`; the size is summed from the `repr(C)` fields, with `Background` 72 bytes from
`color.rs:779-787`). That is:

- about 190 MB per frame at tolerance 0.5;
- about 300–370 MB per frame at 0.1, which exceeds `MAX_INSTANCE_BUFFER_SIZE = 256 MiB`
  (`wgpu_renderer.rs:19`).

Stroke widths are baked into the triangles, so every zoom step also means re-tessellating
(24–181 ms). **Stroking the whole edge set as GPUI paths is not viable** at the designed
scale.

CPU rasterisation with tiny-skia 0.11.4 (already in the GPUI tree through resvg:
`feature/gpui-spike:crates/spike/Cargo.lock`), with 1 px anti-aliased strokes and all 96 buckets:

| Pixmap | 1 thread | 12 horizontal strips (scoped threads) |
|---|---|---|
| 1,100² | 382–722 ms | 202 ms |
| **1,500²** | 549–1,122 ms | **126–347 ms** |
| 2,700² | 1,442–2,642 ms | 220 ms |

Building the tiny-skia paths takes 4–7 ms. Hairline mode (width 0) is no faster (556–1,233 ms).

## Drawing on gpui-pre 0.3.7

What the API offers (source: `~/.cargo/registry/src/*/gpui-pre-0.3.7`):

- **`canvas(prepaint, paint)`** (`src/elements/canvas.rs:10-19`) gives a closure with
  `Bounds<Pixels>`. It is `Styled` (`:91-95`), but its paint does **not** apply
  `style.opacity` (`:84-88`). Element opacity is applied only by `div` (`src/elements/div.rs:2538`),
  and `Window::with_element_opacity` is `pub(crate)` (`src/window.rs:4030`).
- **Paths:** `PathBuilder::stroke(width)` and `::fill()` (`src/path_builder.rs:88-98`),
  `dash_array` (`:108`), `move_to`, `line_to`, `curve_to` (quadratic), `cubic_bezier_to`,
  `arc_to` and `add_polygon` (`:125-195`), transforms (`:205-240`), and `build()` (`:244`).
  Paint with `Window::paint_path(path, color)` (`src/window.rs:4573-4586`). Each path takes
  one colour. There is no per-path alpha other than the colour's own alpha.
- **Quads:** `Window::paint_quad` (`src/window.rs:4502`), with corner radii for round dots.
- **Images:** `Window::paint_image(bounds, image_bounds, corners, Arc<RenderImage>, frame, grayscale)`
  (`src/window.rs:4871`) draws a BGRA `RenderImage::new(frames)` (`src/assets.rs:43-67`)
  scaled to any bounds and respects element opacity. `Window::drop_image`
  (`src/window.rs:4989`) evicts it from the atlas.
- **Text:** `WindowTextSystem::shape_line(text, font_size, runs, force_width) -> ShapedLine`
  (`src/text_system.rs:638`) and `ShapedLine::paint(origin, line_height, align, align_width, …)`
  (`src/text_system/line.rs:108`). `Window::paint_glyph` (`src/window.rs:4639-4712`) always
  inserts sprites with `TransformationMatrix::unit()` (`:4692-4710`). **Text cannot be
  rotated** through the public API, and there is no stroked text, so no `strokeText` halo.
  Only `svg()` takes a transformation (`src/elements/svg.rs:64-67`).
- **Input:**
  - Element listeners: `on_mouse_down` (`div.rs:869`), `on_mouse_move` (`:1000`),
    `on_mouse_up` / `on_mouse_up_out` (`:924`, `:987`), `capture_any_mouse_up` (`:937`),
    `on_scroll_wheel` (`:1054`), `on_pinch` (`:1066`), `on_key_down` (`:1127`),
    `track_focus` (`:788`).
  - Raw listeners: `Window::on_mouse_event` (`window.rs:5246`) and `insert_hitbox`
    (`:5109`), with `set_cursor_style(style, &hitbox)` (`:3945`).
  - Event types: `MouseDownEvent.click_count` (`interactive.rs:148-159`),
    `MouseMoveEvent.pressed_button` (`:494-499`), which lets a drag continue outside the
    element; `ScrollWheelEvent` / `ScrollDelta` (`:522`, `:554`) with `pixel_delta(line_height)`
    (`:614`); `MouseExitEvent` (`:666`) for pointer-leave. GPUI needs no `passive: false`
    workaround.
- **Frames:** `Window::request_animation_frame` (`window.rs:2622`) and `on_next_frame`
  (`:2602`). Unfocused windows are throttled by `inactive_frame_interval`, which defaults to
  33 ms (`src/platform.rs:2213`, `:2359`). A settling simulation in an unfocused window
  needs this set to `None`, as map #92 notes.

## Recommendation

**Use no force-layout crate for the main view. Port the deterministic bundle layout, paint
its edge layer as a raster produced on a background thread, and draw everything interactive
live.** Photo ↔ tag is a separate decision for the owner (see Risks). If that mode is kept,
use **fjadra 0.2.1** with a small mutation patch rather than porting d3-force ourselves.

1. **`bundle.rs`** — port `tagGraphBundle.ts` one-to-one: the trie, self leaves, ring slots,
   cluster radii, LCA paths, and `bundle_path` into a small `PathSink` trait. Port the
   18 tests. It is pure, so it can live in the Tag graph module crate and be tested
   without GPUI. The benchmark's `bundle_path` transcription is about 45 lines.
2. **Edge layer.** Build tiny-skia paths per (colour, alpha) bucket when the layout, link
   threshold, isolation or view size changes. Rasterise them over N strips on the background
   executor into a `RenderImage` at the current zoom, and paint it with `paint_image`.
   - **During a wheel or pan gesture**, repaint the cached image scaled and translated
     (a GPU-sampled `paint_image` into transformed bounds), then re-rasterise when the
     gesture settles (debounced; 126–347 ms measured). The result is briefly soft
     while zooming and crisp afterwards. That is a visible difference from today's
     per-frame re-stroke, and it has to be accepted.
   - **Focus dimming (×0.2):** put the edge image in its own `div().opacity(…)` layer
     beneath the chrome canvas, since `canvas` ignores opacity.
3. **Live layer** (`canvas`). Draw lit edges for hover, selection or community as per-edge
   `PathBuilder::stroke(px(1.5))` paths. Typical degrees are tens to hundreds of edges at
   3–16 µs each (the per-edge tessellation above), which fits the frame budget. Draw the group
   arcs with `arc_to`, the dots as round `paint_quad`s, and the labels with `shape_line`,
   cached per label text.
4. **Labels: horizontal, not radial.** gpui-pre 0.3.7 cannot rotate text. Anchor each label
   at its slot, left-aligned on the right half of the ring and right-aligned on the left
   half. Keep the greedy by-count placement, but test for vertical overlap on each side
   instead of the angular gap. Replace the halo with a canvas-coloured rounded quad behind
   the text. Group names become horizontal labels at the arc midpoints. **This is a visible
   design change and needs the owner's approval.**
5. **Interaction.** Hit testing stays the same pure angle maths (portable from `hitTest`),
   using one hitbox for the whole canvas. Map wheel → `ScrollWheelEvent`, drag →
   `MouseMoveEvent.pressed_button`, leave → `MouseExitEvent`, and Esc → `on_key_down`
   with `track_focus`. The panels, inspector, community card and loupe card become
   gpui-component widgets through the Module trait's main-view contribution.
6. **Photo ↔ tag, if kept.** Use fjadra with forces named in d3 order. Tick it on the
   background executor with Arc position snapshots, `cx.notify()` per batch and
   `request_animation_frame`, and set `inactive_frame_interval: None` while it settles.
   Drag and freeze need the fjadra patch (public `particles_mut`, `set_alpha_target`),
   carried as a `[patch.crates-io]` fork until upstream (#6) takes it. Thumbnails come from
   the thumbnail cache as images clipped with rounded corners. Keep the 1,500-photo cap:
   at 16–25 ms per tick, settling takes about 3–4 s, as it does today.

### Size estimate (inference)

| Piece | Rust lines |
|---|---|
| `bundle.rs` layout, spline and ported tests | ~550 |
| Edge-layer raster worker, bucket cache and invalidation | ~250 |
| Canvas painter (live edges, arcs, dots, labels, tooltip), hit test, pan/zoom/keys | ~700 |
| Left panel, inspector, community card, loupe card (gpui-component) | ~600 |
| **Communities total** | **~2,100** |
| Photo ↔ tag if kept: fjadra driver, drag/freeze, painter, thumbnails, fjadra patch | +~550 |

For comparison, today's code is 1,970 lines of TSX, 349 of TS and 524 of CSS.

## Risks and open questions

- **Owner decisions:**
  - Does the legacy Photo ↔ tag mode survive the port? It is labelled "legacy view"
    (`tagGraph.tsx:38`) and exists only because of the old force view.
  - Are horizontal labels acceptable in place of radial ones?
  - Is a briefly soft edge layer acceptable during zoom?
- **Unmeasured:**
  - GPU cost of `paint_image` for a full-window edge raster, and of per-edge live paths, in a
    real GPUI window on the RTX 3080 / EIZO (29.9 Hz). The spike harness on
    `feature/gpui-spike` could measure both.
  - Rasterisation time on the real library. All the numbers here are synthetic, taken under
    a load average of about 15.
- The u16 index ceiling (`path_builder.rs:262`, `:308`) makes any single `PathBuilder` of
  more than about 65k vertices fail. Live paths must stay per edge or small per bucket.
- fjadra has had no code commits since 2024-12, and our drag needs a fork patch. If upstream
  stays silent, vendoring its ~2.8k MIT/Apache lines (or ~800 lines ported from d3) is the
  fallback. A port would also bring back insertion-order forces.
- Radial labels could come back later if GPUI gains a glyph transform. `MonochromeSprite`
  already carries a `transformation` field (`scene.rs:711-719`), but `paint_glyph` does not
  expose it.

## Proposed next ticket

**"Port the Tag graph module to GPUI"**. The question is: can the ported bundle view (a
tiny-skia edge raster plus live highlight paths and horizontal labels) hold the 33.4 ms
EIZO frame budget for hover and wheel zoom at the real library's tag count? Also, which
of Photo ↔ tag, radial labels and crisp-while-zooming does the owner give up?
