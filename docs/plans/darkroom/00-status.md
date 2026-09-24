# Status: Darkroom (the complete develop feature)

- Gate 1 — Product: APPROVED 2026-09-04
- Gate 2 — Architecture: APPROVED 2026-09-04
- Gate 3 — Program Design: APPROVED 2026-09-04
- Gate 4 — Slice plan: APPROVED 2026-09-05

## Slices
- [x] Slice 1 — tracer bullet: zones field end-to-end + bare DarkroomView behind `editor.darkroom` toggle (2026-09-05)
- [x] Slice 2 — zone curve + masses for real (engine + command + tests) (2026-09-05)
- [x] Slice 3 — live print on the pop-out loupe (2026-09-05; on-screen check pending — needs the second monitor)
- [x] Slice 4 — proof sheet (auto-tone + spread + adopt) (2026-09-05)
- [x] Slice 5 — duel refinement (+ per-pane version forking) (2026-09-05; DuelDim narrowed to the four numeric dims — "look" rounds deferred with the learned auto-tone)
- [x] Slice 6 — rail extraction from EditorView (no behaviour change) (2026-09-05; extracted into src/components/EditControls.tsx — EditStage/ToneRail/EffectsRail/GeometryRail; hands-on parity check of the classic Develop pending)
- [x] Slice 7 — version shelf & save-as-new-version (2026-09-05; user re-steer: the Darkroom is a sandbox — never overwrites its starting version; Save banks the settings as a NEW version). **Reversed 2026-09-24 by the user:** the Darkroom autosaves every change into the active version with a per-version history (undo/redo, a History panel) — see docs/editing.md § History and autosave; "Save as version" became "+ New version" (fork).
- [ ] Slice 8 — the swap: Darkroom becomes Develop

## Follow-on: GPU smoothness (branch `feature/darkroom-gpu`, plan 2026-09-06)

Direction (user, 2026-09-06): phased — a WebGL2 look shader as the drag tier now (the
settled Rust render stays the oracle), a native `wgpu` backend later, evidence-gated. The
survey and plan live in the session plan file; the increments are:

- [x] Gate 0 — WebGL probe (`GlSpike.tsx`, Preferences → Darkroom, behind the timing toggle).
  Result on the RTX 3080 / WebKitGTK 2.52.6 / DMABUF renderer disabled (the shipped
  default), two runs, identical: `{"context":true,"renderer":"Apple GPU" (masked),
  "maxTexture":32768,"max3d":16384,"frames":266,"cadence":{"p50":20,"p95":22},
  "finish":{"p50":0,"p95":1},"lost":false}` — a 1400 px look frame with eight 3D-LUT
  fetches and the grain hash completes in under a millisecond, at a 50 fps rAF cadence on
  a 120 Hz monitor. **Gate passed** (bar: finish p50 ≤ 16 ms). The alternative
  `WEBKIT_DISABLE_DMABUF_RENDERER=0 __NV_DISABLE_EXPLICIT_SYNC=1` row was not measured.
- [x] Increment 1 — instrumentation (`plugins/edit/timing.rs`, `bench.rs`, the frontend
  frame log). Baseline, `render_stage_timings`, synthetic 2048×1365 proxy, no LUT, N=10,
  medians in ms:

  | profile | edge | cache clone | downscale | look | encode jpeg q90 | encode png fast | base64 | render_image total | masses pass (1024) |
  |---|---|---|---|---|---|---|---|---|---|
  | debug (`tauri dev`) | 720 | 1.4 | 28.5 | 159.9 | 57.6 | 54.7 | 0.5 | 191.5 | 371.9 |
  | debug (`tauri dev`) | 1400 | 0.9 | 44.2 | 601.8 | 230.2 | 202.0 | 2.6 | 646.2 | |
  | release | 720 | 1.3 | 19.4 | 34.6 | 8.4 | 1.8 | 0.04 | 55.2 | 100.5 |
  | release | 1400 | 1.0 | 44.5 | 129.8 | 34.6 | 6.9 | 0.4 | 176.2 | |

  Reading: in the debug profile the look loop dominates everything (opt-level 0 — only the
  decoders are optimized in dev, `Cargo.toml` `[profile.dev.package.*]`), so the ~11 fps
  drag of 2fe085c was a debug-profile figure. In release the look loop still dominates —
  130 ms of the 176 ms settled frame, 35 of the 55 ms drag frame — with the `image`-crate
  downscale (19–44 ms) and the JPEG encode (8–35 ms) next; base64 is noise on the Rust side.
  Two consequences: the Phase 2 gate's criterion (1) (release look ≥ 25 ms at 1400 px) is
  met before any transport work, and the cheapest lever of all is not GPU at all — the loop
  was scalar and single-threaded on a 24-thread machine (survey row F). **Done 2026-09-09:**
  `apply_look` now runs over row chunks on rayon, byte-identical to the scalar loop (locked
  by `parallel_look_matches_the_sequential_loop`). Same bench, after:

  | profile | edge | look before → after | render_image total before → after | encode jpeg q90 | masses pass |
  |---|---|---|---|---|---|
  | debug | 720 | 159.9 → 12.5 | 191.5 → 42.6 | 58.0 | 371.9 → 76.1 |
  | debug | 1400 | 601.8 → 44.2 | 646.2 → 89.4 | 231.1 | |
  | release | 720 | 34.6 → 3.1 | 55.2 → 23.6 | 8.5 | 100.5 → 36.8 |
  | release | 1400 | 129.8 → 9.8 | 176.2 → 56.1 | 34.8 | |

  The look is no longer the bottleneck in either profile. What remains per frame is the
  `image`-crate downscale (20–45 ms, `thumbnail()` is single-threaded) and the JPEG
  encode (8–35 ms release, 58–231 ms debug). So the Phase 2 gate's criterion (1) is now
  **not** met (release look at 1400 px is 10 ms, under the 25 ms bar): a native GPU
  backend would not buy the drag anything the CPU cannot already do; the next levers are
  the downscale and the encode, or the GL tier, which skips both.

  The first on-screen drag after the rayon change (dev build, Exposure only, persisted
  summary) barely moved: request→paint p50 118 → 112 ms, 69% of frames superseded. An
  exposure-only record never had much look to parallelize; that frame was downscale +
  encode. **Done 2026-09-09, the framed-base cache:** `render_proxy` keeps the proxy after
  geometry + downscale, before the look, keyed by (proxy fingerprint, geometry, edge), 4
  entries LRU. A look-only frame now skips perspective/straighten/crop/`thumbnail()`.
  Bench, the cached drag path (`render_proxy_cached`, look + nothing else) vs the uncached
  render, ms:

  | profile | edge | render_image total | render_proxy (cache hit) | masses pass |
  |---|---|---|---|---|
  | debug | 720 | 42.1 | **12.4** | 76 → 43 |
  | debug | 1400 | 88.8 | **44.2** | |
  | release | 720 | 24.2 | **3.1** | 37 → 7 |
  | release | 1400 | 56.7 | **10.3** | |

  Per frame in release the Rust side is now ~3 ms + 8 ms JPEG encode at 720 px; the
  encode is the last CPU stage worth touching before the webview's own decode dominates.
  Frontend cadence (IPC + paint, `editor.renderTiming.lastSummary`): pending a manual drag
  with the toggle on — not taken in this session because the desktop was in use.
- [x] Increment 2 — native `edit://` transport for the darkroom stage (`protocol::handle_edit_request`, `editRenderUrl`; base64 gone from the drag path).
- [x] Increment 3 — WebGL2 look shader as the drag tier. **Decided against, 2026-09-23,
  not built.** Two problems, independent of each other, either one enough on its own:

  1. **Wrong layer for this app.** ChairPhoto is a native Tauri process with a webview
     for UI, not a website; the trusted render engine lives in Rust and preview/export
     must stay bit-identical to it (docs/editing.md). The original GPU research note
     said this before any code existed: "The browser/WebView GPU is only a display
     surface. The trusted renderer belongs in the native Rust process"
     (agent-notes/darkroom-research/05-gpu-rendering.md). A WebGL shader in the webview
     would have made the display layer do real rendering work — the thing that note
     ruled out.
  2. **It demonstrated real fragility.** The Gate 0 spike reproduced a genuine
     WebKitGTK/NVIDIA driver bug: a live WebGL2 context in the webview at app teardown
     segfaults the web process. Symbolized with debuginfod and reproduced standalone
     with `scripts/webgl-teardown-repro.py` — stack `WebKit::AuxiliaryProcess::terminate`
     → `WebProcess::stopRunLoop` → `PlatformDisplay::clearGLContexts` →
     `GLContext::~GLContext` (GLContext.cpp:335) → SIGSEGV in `libnvidia-eglcore`, under
     `WEBKIT_DISABLE_DMABUF_RENDERER=1` (what `lib.rs` sets on Linux) whenever a visible
     canvas holds a live context at teardown. That is exactly the kind of risk that
     comes from routing native work through browser rendering machinery instead of the
     engine process — evidence for (1), not just a bug to work around.

  `GlSpike.tsx` (the probe) and `scripts/webgl-teardown-repro.py` stay in the tree as
  the record of both findings; the probe is dead code behind a setting nothing turns on.
  Not deleted since nothing depends on it and it documents the decision better in place.
- [ ] Phase 2 gate — `wgpu` backend (native, Rust-side; the only GPU path this app's own
  architecture supports): build only on the exit criteria in the plan. Not currently
  met — after the rayon look loop and the framed-base cache the release-profile look is
  ~3 ms, far under the 25 ms bar, so the CPU path already clears what was being asked of
  a GPU. A stray, unmerged `wgpu` feasibility probe exists on `feature/darkroom-develop`
  (commit 65c7fe9, `src-tauri/src/bin/gpu_probe.rs`) from a parallel exploration; it was
  never integrated and this decision does not depend on it.
  **Research (2026-09-24):** what such a backend needs — verified, cited, and checked
  against the local spikes — is in `agent-notes/darkroom-research/15-gpu-backend-research.md`.
  Headlines: bit-exact GPU/CPU parity is not obtainable through wgpu/WGSL (export stays
  CPU; parity is a tolerance test); a preview-only backend needs no tiling; darktable's
  VRAM formula gives ≈6.4 GB usable of 10 GB; readback, not kernel time, is the frame;
  the fallback must be a real CPU path, not a software rasterizer (RapidRAW's gap). It
  does not reopen the gate.

Follow-ups recorded here, not done: `LoupeWindow` / `basicEditor` and the
`render_edit_batch` consumers (`ProofSheet`, `DuelView`, `PresetBrowser`) still receive
base64 data URLs; `EditorView` retires at slice 8.

## Notes for a fresh session
- Branch: `feature/darkroom` (cut from `feature/keeper-stats` at d935381 — the loupe-card
  work lives there and the Darkroom intends to drive the pop-out loupe as the "print").
- Read `docs/editing.md` first: the existing Basic Editor already has geometry
  (perspective → straighten → crop), tone (EV/contrast/highlights/shadows/WB), looks
  (B&W mixer, split toning, grain, fade, vignette, .cube LUTs), develop presets,
  `photo_versions`, proxy-based live preview, and one-call batch preset rendering
  (`render_edit_batch`). The Darkroom builds ON this record/engine, it does not restart it.
- Product direction chosen in chat (to be ratified at Gate 1): editing by *choosing*
  (proof sheet of developed candidates + duel refinement) as the fast path, classic
  sliders as the precision path, the pop-out loupe as the full-bleed print.
- Explicitly out of v1 (candidate v2 pillars): AI semantic masks / local adjustments,
  tone curve + HSL color mixer (v1.5 candidates), per-platform pixel resize on export.
- Histogram decision (user, 2026-09-04): "tone strip + sculpt" — an 8-zone EV band whose
  fill shows pixel mass; dragging a zone sculpts that tonal range (histogram-reshaping
  semantics). Chosen over: sculptable classic histogram, drag-on-image with zone overlay,
  waveform scope. Note: Apple patent US9917987 covers draggable vertical histogram
  slices — our mechanism is zone-band + curve-solving, reviewed at Gate 2/3.
- Research references for later gates: Hist2Style (arxiv 2606.01819, histogram-as-
  interface); Progressive Histogram Reshaping (NPAR 2010); ISPDiffuser (2503.19283,
  histogram-guided RAW→sRGB); Deep Guided Exposure Correction w/ KD (10.3390/s25247606 —
  candidate local ONNX model for the proof sheet's Auto cells, post-v1); ACM CSUR
  "ISP Meets Deep Learning" survey (10.1145/3708516); NTIRE 2025 RAW restoration
  (2506.02197).
