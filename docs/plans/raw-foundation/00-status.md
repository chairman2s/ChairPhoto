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
  **Camera look — DECIDED with the user (2026-09-24): the default is closer to the camera's
  picture style.** The user's first side-by-side read the RAW as slightly lighter and less
  saturated in the oranges and a touch flatter overall. New engine-2 records carried
  `display: "camera"` (now `camera.2`, below), a per-channel tone curve
  (`linear::CAMERA_CURVE`) fitted by the ignored
  `develop::camera_fit::fit_camera_transform` against the embedded camera JPEGs of
  the five Sony ARWs in the agent library (A7 IV Standard, A7R VI Vivid, DRO Auto). Mean
  |Δ| to the camera JPEG, centre 80 %, levels of 255: plain sRGB 10.1 → camera 3.6
  (per photo 2.5–4.3). The curve alone brings the colour up to the camera's (median chroma
  on the Vivid frame 58 → 75, camera 75), so there is no saturation step. Records without
  the field, saved before, stay sRGB. Left out of the fit: two frames at **extended low
  ISO** (50 on the A7 IV, 80 on the A7R VI, below base 100), which the camera exposes
  brighter and pulls down in its own processing, and `_DSC7602.dng`, whose camera preview
  sits darker for a reason not found (its `BaselineExposure` is +0.35, the wrong sign).
  **Per-photo camera match (2026-09-24):** those are brightness the global curve cannot
  carry, so each working image is now also measured against its own camera JPEG when it
  is prepared (`linear::camera_match_ev`: the EV offset minimizing mean |Δ| of luma, centre
  80 %), and a new engine-2 record stores it as `cameraEv`, added to the baseline lift —
  decision 4's per-camera baseline, made per photo. It is not an ISO rule: the ISO 50 and
  ISO 80 frames both need about −1.6 EV. Offsets on the corpus: the five fitted frames
  −0.10..+0.04, the low-ISO frames −1.60 and −1.57, the DNG −0.64. Mean |Δ| over all
  eight: camera curve alone 18.4 → with the match 3.7 (worst 6.2, the DNG). Cost per
  prepared image in the debug build ~120–200 ms (the linear downscale most of it), on
  the decode thread before the image is announced. Seen on screen: the ISO 50 frame's
  stage mean RGB (93, 89, 85) against the camera JPEG's (93, 89, 84), 60 levels brighter
  before; a Contrast nudge saved `"cameraEv":-1.6`. DRO is local tone mapping; a global
  curve and offset approximate it. **Hue (2026-09-24):** the camera rendered blues more
  violet than the decode's matrix (the Vivid frame). New records now get `camera.2`: a
  3×3 matrix in linear light (`linear::CAMERA_MATRIX`, rows summing to 1) before the same
  curve, fitted on all eight frames after each one's camera match. Shipped path (matrix +
  curve + match), mean |Δ| per frame: 2.3, 3.2, 2.0, 3.8, 3.9, **2.2** (the blue Vivid
  frame, 4.6 before), 3.1, 6.7 (the DNG, 6.1 before); mean 3.7 → 3.4. One blue scene
  carries most of that evidence, and the Standard and Vivid frames alone fit different
  matrices — `camera.2` is the compromise; a per-style or per-photo colour match is the
  lever if it is not enough. `camera` records keep the curve alone. **Headroom seen on
  a clipped frame (2026-09-19):** a sunlit portrait against sky whose camera preview had
  pixels in the whites bin and a featureless cloud bank; the RAW at −1 EV shows the clouds'
  shape and shading, keeps the sky blue instead of grey, and empties the whites bin —
  the preview at −1 EV can only turn that cloud into a flat grey patch.
  Known, deliberate gaps for later slices: the loupe print and the proof sheet/duels still
  render engine 1 from the preview (an engine-2 broadcast makes the loupe fall back to the
  unedited preview); neighbours are accepted by `develop_open` but not preloaded (slice 4).
- [x] Slice 3 — ownership and cleanup, forced (2026-09-19). The open is now a `claim` step
  and the worker's tail a `publish` step, both plain functions the tests drive by hand.
  Fixed on the way: a catalog switch tripped the claim and cleared the slot but **left the
  working image resident** (800 MB nothing owned, reachable by a token whose photo id means
  another photo in the next catalog) — `DetachGuards::trip_and_clear_all` now releases it;
  a superseded decode cleared *every* image, so a slow decode finishing after a fast one
  would have dropped the newer photo's image — it removes only its own token; an open with
  the engine switched off released nothing — it trips and releases like any other open.
  Forced tests: `open_for_another_photo_trips_the_previous_claim`,
  `a_superseded_decode_releases_only_its_own_image`,
  `close_releases_the_image_and_the_stale_token_names_nothing`,
  `reopening_the_resident_photo_answers_without_a_new_claim`,
  `an_open_that_prepares_nothing_still_releases_the_previous_image`,
  `jobs::a_switch_releases_the_develop_working_image`,
  `editing::a_stale_working_token_is_an_error_not_other_pixels`; the first two and the
  switch test were checked to fail against the mutated code. The resident set's lock is
  documented as a leaf of the `commands::jobs` lock order. RSS before/after a quit from the
  Darkroom: pending the measurement below.
- [x] Slice 4 — the `.rawf` decode cache + neighbour preload (2026-09-24; seen on screen on
  the isolated catalog's Sony files). Landed: `develop/cache.rs` (16-bit linear, fixed header
  carrying the full key so a hash collision is a miss, temp-then-rename writes, a hit touches
  mtime so trimming is LRU, directories keyed by decoder version with old ones deleted by the
  trim); the worker loads cache → else LibRaw (under its crash marker) → writes and trims;
  after publishing the current photo it preloads N+1 then N−1 into the same claim's memory
  budget without announcing them; a new claim keeps only the new photo and its neighbours,
  re-keyed to itself (old tokens 404), so a step to a preloaded neighbour is adopted at once;
  Preferences → Darkroom gains the cache size (`develop.decodeCacheGb`, default 20), its
  current use with a Clear button, and `develop.preloadNeighbours` (default on).
  **Measured.** Bench `develop::cache::tests::bench_cache_hit` (6656×9984, the A7R VI
  picture): write 0.19 s, read 0.09 s, to working image 0.39 s release / 0.59 s debug. On
  screen (debug, 32.7 MP Sony): first open 12.2 s decode; the same photo from the cache
  0.31 s; neighbours from the cache 0.46–0.67 s. The first cut read pixels with a per-value
  loop — 11.6 s at opt-level 0, slower than the decode it replaced in every `tauri dev`
  session — fixed by reading and writing the buffer's own bytes on little-endian hosts
  (`pixels_are_stored_little_endian` pins the file format), and the u16→f32 conversion now
  runs across cores.
  **Fixed on the way (slice 3):** the worker cleared the session's status slot after a
  *successful* publish, so reopening a still-resident photo found no record, released it and
  decoded again. The slot now describes the open session and is cleared only on failure;
  readers still check the resident set, so after a close it reads "not resident", never
  "preparing" (`the_session_outlives_its_worker_but_not_its_image`). The old tests never ran
  the worker's final step, which is why they passed.
  **Stepping — settled (2026-09-24):** the Darkroom autosaves into the active version and
  has a filmstrip (docs/editing.md § History and autosave), so moving to the next photo
  saves first and adopts the preloaded neighbour from memory.
  Not measured: whether a background neighbour decode (OpenMP, all cores) makes slider drags
  stutter; the plan's fallback is a lower thread priority or the setting.
- [x] Slice 5 — every surface on the same source (2026-09-24). The proof sheet and duels
  load `edit://` URLs with the stamped record and the session token (they had been engine-1
  base64 renders of the preview), and the Auto proof's analysis reads the working image
  (`suggest_auto_tone` with `source`/`base_json`); the masses already did. The loupe print
  carries the token (`broadcastPrint`), and `renderForLoupe` renders any engine-2 record as
  `edit://` at 2560 px (full size to zoom) in both loupes. Outside Develop an engine-2
  record renders from `develop::offline::working_image_for` — the session's image when it
  holds the photo, else one serialized load kept 60 s — which the cover renderer shares.
  The framed-base cache keeps linear bases only up to 2560 px. The **sensor-clipping
  overlay**: `edit://…&k=1` answers a transparent PNG of the stage's geometry and size,
  magenta where any channel is at sensor white (`CLIP_AT` 0.999 after the area downscale),
  shown by the *◩ Clipping* toggle on the RAW. On the corpus: `DSC07441` (the sky through
  the hole, white in the camera JPEG) 0 % clipped; `_DSC7742` (sunset) 0.81 % at full
  resolution, 0.57 % marked at 1400 px; a −3 EV pull marks the same pixels. Seen on
  screen: proofs and duel on the RAW. Not seen on screen: the pop-out loupe and the
  clipping layer (the agent's monitor was switched away when the loupe opened) — covered by
  tests only.
- [x] Slice 6 — export is the view (2026-09-24). An engine-2 export loads the working image
  through `develop::offline` (the session's, the `.rawf` cache, or the decoder — it had
  decoded afresh each time) and renders it with `render_image_opts` at full size; no tone
  match on that path. Exact equality at 100 % is locked on four records (tone, camera
  match, geometry + grain + vignette, B&W + zones). The in-app check at Fit
  (`plugins::edit::parity`) tallies every engine-2 export into `metrics.exportParity`,
  shown in Preferences. Its tolerance was measured, not assumed: resampling order alone
  gives 0.07–1.32 levels on the seven ARWs, 1.79–2.67 with grain, 3.60–5.28 on the
  detailed DNG (linear-light averaging only moved it), so the bar is 6 — it catches an
  export from another source or pipeline (8–60 levels), not subtle drift; exactness is the
  100 % test's job. On a real A7R VI frame the export came out 6656×9984 and the tally
  read 1 checked, 0 differing. Not seen on screen: the Export dialog run (monitor
  unavailable).
- [x] Slice 7 — engine-1 versions kept honest (2026-09-24). A saved version without the
  engine-2 stamp that holds anything (`isEngine1Version`) now stays on engine 1 in the
  Darkroom even with the RAW resident: no token in its URLs, no engine-2 stamp on its saves
  (before this, the stamp rewrote it as engine 2 on the next autosave). The bar says
  *camera preview · this version's engine* and offers **Develop with the new engine**,
  which forks "<name> (RAW)" with the framing copied, tone and look reset, `camera.2` and
  this frame's camera match (`forLinearEngine`), and switches to it; the engine-1 version
  is untouched. A blank `{}` version may start on either engine. Presets are look-only and
  render on engine 2 through the shared finish (fade, vignette, grain, B&W, zones are in
  the full-size parity test). Known gap: a change made while the RAW is still preparing
  saves an engine-1 record, and once the RAW is resident the next save stamps it engine 2
  — the lock is taken when a version is loaded, not mid-session. Not seen on screen
  (monitor unavailable); covered by tests.
- [x] Slice 8 — the swap (2026-09-24). The RAW engine is no longer a setting: every
  supported RAW opened in the Darkroom prepares its working image (`raw_engine_enabled`
  and the "switched off" branch are gone, with their test half), the Preferences checkbox
  is replaced by a line saying what Develop renders from, and the decode-cache settings
  and the export-parity count show unconditionally. `docs/editing.md` describes the
  sources, both engines, the decoder and its crash marker, every cache and its bound, the
  camera match and the clipping overlay as they are. **Licensing corrected:**
  `MODULE_LICENSING.md` had ChairPhoto taking LibRaw under CDDL-1.0, which the FSF lists
  as GPL-incompatible; it now names LibRaw's LGPL-2.1 option (section 3 permits GPL v2 or
  later, so GPL-3.0-only fits), and the package installs LibRaw's `COPYRIGHT` and
  `LICENSE.LGPL` (commit 7012115). The Darkroom itself is still behind its own
  early-preview switch (`editor.darkroom`, docs/plans/darkroom — that plan's slice 8).
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
