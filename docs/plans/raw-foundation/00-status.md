# Status: RAW foundation (Develop renders the real file)

- Gate 1 — Product: APPROVED 2026-09-10 (drafted 2026-09-09, revised after the assumptions review)
- Gate 2 — Architecture: APPROVED 2026-09-10
- Gate 3 — Program Design: APPROVED 2026-09-10
- Gate 4 — Slice plan: APPROVED 2026-09-10

## Slices
- [x] Slice 1 — tracer bullet: vendored LibRaw builds with the app; `raw_probe`; the source badge (2026-09-10; seen on screen on the A7R VI). Finding: Sony *Lossless* Compressed RAW 2 files store a 10240×7168 raster (73.4 MP) padded around the 9984×6656 picture; the badge and any size the engine reports must use the camera's visible rectangle (`raw_inset_crops[0]`), which is also what `crop_to_inset` trims the export to.
- [x] Slice 2 — the working image renders (engine 2 behind `develop.rawEngine`) (2026-09-19;
  seen on screen on the A7R VI: badge `RAW · 16-bit · 66.5 MP`, brightness matching the
  preview at the 1.4 EV baseline, and Exposure −1 pulling the whites to a clean grey with
  no banding in the dark reds). Landed: `raw::decode_linear`
  (16-bit linear, honest clipping, abortable), `develop::{ResidentSet, session}` with the
  `develop` job family, `RenderSource`/`SourceToken` and the `s=` token on `edit://`,
  `render_edit_batch`/`edit_zone_masses` with a `source`, the engine id on the record
  (engine 1 byte-identical with the field present), `apply_look_with` (engine 2 shares the
  display-domain look), `linear.rs` (exposure/WB in light, sRGB or soft-shoulder transform,
  `BASELINE_EV = 0.5` provisional, clip mask), geometry on f32 images, export dispatch for
  engine-2 records, the Preferences toggle, and the Darkroom wiring (open/close, event,
  token in every render URL, engine-2 stamp on saves).
  **Measurements on the A7R VI (`_DSC8120.ARW`, 9984×6656):** full linear decode 2.59 s in
  release (OpenMP, 24 threads; 5.6 s single-threaded before), **14.5 s in the debug
  profile `tauri dev` runs** — the on-screen swap will look slow in dev and normal in a
  release build; mean linear sample 17847/65535 (27 % of sensor white — no auto-bright).
  Headroom: locked by `engine2_renders_the_working_image_and_recovers_headroom` on a
  synthetic image (a patch 1.4× above white returns at −1.5 EV; engine 1 on the 8-bit
  rendering cannot).
  **On-screen finding (first open, `_DSC8291.ARW`):** the RAW rendered nearly white. Cause,
  found by `develop::tests::fixture_working_image_renders_near_the_camera_preview` printing
  every stage's mean: the `image` crate's `thumbnail`/`resize` are **wrong on f32 buffers**
  (mean 0.08 → 0.58, values up to 1.5 from a source capped at 1.0). Fixed with
  `linear::downscale_linear`, an exact area average; the linear path never calls the
  crate's resampler now. **Distance from the camera JPEG (same photo):** preview mean sRGB
  105.6; the linear decode at baseline 0 EV → 66.8, +0.5 → 79.4, +1.0 → 93.8, +1.5 → 109.9,
  so `BASELINE_EV` is set to **1.4** (from 0.5). The `Soft` shoulder changes nothing at
  these means (it acts only near white); its default stays open until judged by eye.
  **Still to judge by eye:** the user's first side-by-side reads the RAW as slightly lighter
  and less saturated in the oranges and a touch flatter overall (the camera's tone curve and
  picture style, which a plain decode does not carry) — whether to match the camera by
  default or keep the honest decode is the user's call and stays open. Headroom has not yet
  been seen on a *clipped* frame: the poster test shot had nothing blown, so the preview at
  −1 EV would have looked the same; try a blown sky or daylight behind a subject.
  Known, deliberate gaps for later slices: the loupe print and the proof sheet/duels still
  render engine 1 from the preview (an engine-2 broadcast makes the loupe fall back to the
  unedited preview); neighbours are accepted by `develop_open` but not preloaded (slice 4).
- [ ] Slice 3 — ownership and cleanup, forced
- [ ] Slice 4 — the `.rawf` decode cache + neighbour preload
- [ ] Slice 5 — proof sheet, duels, masses, loupe, clipping overlay on the working image
- [ ] Slice 6 — export is the view (tone match gone for engine 2; exact parity)
- [ ] Slice 7 — engine-1 versions kept honest; fork into the new engine
- [ ] Slice 8 — the swap: `develop.rawEngine` default on, docs, licensing
- [ ] Slice 9 — Kelvin white balance (first follow-on)

## Notes for a fresh session
- Origin: the GPU-smoothness work on `feature/darkroom-gpu` (see
  `docs/plans/darkroom/00-status.md` § Follow-on) made the drag fast, and in doing so made
  the real problem visible: everything interactive in Develop renders from the camera's
  embedded JPEG, so the image being judged is not the image that gets exported. Export
  decodes the RAW and *tone-matches itself to the preview* to hide the gap.
- User direction (2026-09-09), in their words: "In developer mode we should always work
  with the best possible quality. I can't adjust things on an image that doesn't really
  look like the result." "I don't like the thought of compromise on the quality. I don't
  think memory is a big problem on a modern computer so long as we are good to clean it
  up when we are finished." Library stays on proxies; Develop does not.
- Ruled out by that direction: half-size demosaic for interaction, 8-bit decode as a
  stepping stone, any quality setting. Rendering at screen size *from* the full-quality
  image is not a compromise (the screen cannot show more) and stays.
- Sizing case: the user's Sony ILCE-7RM6 — 14-bit Compressed RAW 2, 10016×6672 (67 MP),
  44 MB files; embedded JPEGs 1616×1080 and full-size. A float RGB working image is
  ~800 MB; this machine has 64 GB. Most of the library is 20 MP Olympus/Canon.
- Non-negotiables inherited from AGENTS.md and the Darkroom: originals read-only; old edit
  records keep rendering exactly as before (a new engine must be dispatched by an
  explicit engine identifier, never by reinterpreting EV/WB in linear light); decoded
  images are owned by the current photo/session and released on switch or exit.
- Reference points read this session: Lightroom's Develop never uses pre-rendered
  previews and renders from the RAW through the full pipeline; RapidRAW (source read
  2026-09-09) decodes to linear f32 once per open, caches a geometry-keyed preview base,
  renders the look on the GPU, drag at JPEG q75 / settled at q94, no CPU fallback.
- Assumptions review (2026-09-10), findings that bind later gates:
  1. **The user's newest camera does not decode with any installed decoder.** ILCE-7RM6
     "Compressed RAW 2" (736 photos): LibRaw 0.22.2 (what the `raw` feature links) says
     unsupported, darktable 5.6.1's rawspeed says "Unsupported compression 32766",
     RawTherapee 5.13 fails to load. Today's export for that camera therefore already falls
     back to the embedded JPEG silently. **LibRaw master (commit dde798d, 2026-09-09) decodes
     it**: 6672×10016, 16-bit linear, 5.6 s wall clock, 401 MB TIFF. The A7 IV decodes with
     the packaged library in ~1 s (33 MP). Gate 2 must decide on a vendored, pinned LibRaw
     snapshot built with the app (LGPL route), and the product carries an honest
     "RAW not supported yet" state.
  2. Preview = export is exact only at 100%; Fit is a resample, and resolution-dependent
     effects must live in normalized image space (grain already does). Metric reworded.
  3. Decoding the RAW yields *a* rendering, not "correct" colour: the first render will look
     flatter/cooler than Sony's JPEG. Rendering transform and camera characterisation are
     Gate 2 decisions to judge on real photos, and the biggest quality risk of the project.
  4. The preview→RAW swap on open is a visible jump by design, and a 67 MP decode is ~5.6 s.
     Gate 2 must choose between a persistent on-disk cache of the linear decode (keyed by
     file + decoder version), a transient low-res RAW placeholder (not a quality compromise:
     it lasts a moment, like a drag frame), or both. Neighbour preload alone does not cover
     the first open.
  5. Memory: the decoded image fits (64 GB here), but float intermediates and GPU textures
     (536 MB per 67 MP RGBA16F) multiply it; per-stage bounding (tiles/regions) is a design
     constraint, not a quality one. Cleanup on switch/exit is necessary, not sufficient.
  6. Known limit, out of scope: WebKitGTK is not colour-managed, so preview = export holds
     for the file, not for a wide-gamut screen.
- Gate 3 decisions with the user (2026-09-10): white balance mode lives on the record
  (`relative` ships now, `kelvin` parsed now and rendered in a later slice; a preference may
  only choose which slider new edits show — never how a saved version renders; presets carry
  the mode their author meant, and Kelvin is preferred for scene-light presets because
  relative is a look that changes meaning from scene to scene). Highlights: honest clipping
  at sensor white first, measured on the user's clipped-sky keepers in slice 2; unclip modes
  are the fallback and would regenerate the decode cache.
- Research notes with the engine options and their licences: `agent-notes/darkroom-research/02-raw-engine-research.md` (untracked).
- Branch: `feature/raw-foundation` off `feature/darkroom-gpu` (decided at Gate 2 draft);
  the transport/cache/timing work carries over unchanged.
- Gate 2 draft findings (2026-09-10): LibRaw's `Makefile.dist` builds with `USE_ZLIB` only
  and no OpenMP by default, so the 5.6 s decode was single-threaded; the vendored build
  turns OpenMP on. `libraw_version()` exists in the bindings for the cache key. The job
  registry's `JobFamily::begin` (catalog → abort → slot) is the ownership primitive the
  `develop` family reuses. `parseEdit` round-trips unknown fields, so an engine id survives
  the frontend. The preview cache already keys on path+mtime+size (`cache_path_for`).
