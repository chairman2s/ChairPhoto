# Status: Darkroom (the complete develop feature)

- Gate 1 — Product: APPROVED 2026-09-04
- Gate 2 — Architecture: APPROVED 2026-09-04
- Gate 3 — Program Design: APPROVED 2026-09-04
- Gate 4 — Slice plan: APPROVED 2026-09-05

## Slices
- [x] Slice 1 — tracer bullet: zones field end-to-end + bare DarkroomView behind `editor.darkroom` toggle (2026-09-05)
- [ ] Slice 2 — zone curve + masses for real (engine + command + tests)
- [ ] Slice 3 — live print on the pop-out loupe
- [ ] Slice 4 — proof sheet (auto-tone + spread + adopt)
- [ ] Slice 5 — duel refinement (+ keep-both forking)
- [ ] Slice 6 — rail extraction from EditorView (no behaviour change)
- [ ] Slice 7 — version shelf & save parity
- [ ] Slice 8 — the swap: Darkroom becomes Develop

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
