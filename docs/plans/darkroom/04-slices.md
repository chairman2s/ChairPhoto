# Vertical Slices: Darkroom

Build order. Every slice ends runnable and user-visible; the Darkroom mounts behind a
`editor.darkroom` settings toggle from slice 1, and only slice 8 makes it the default
Develop surface.

1. **Tracer bullet** — `EditRecord.zones` lands end-to-end: serde field (+ byte-identical
   v1 render test), identity `zone_gain_lut`, and a bare `DarkroomView` (print + tone
   strip with placeholder masses) behind the toggle; dragging a zone really re-renders
   via `renderEdit` with a zones record. Does almost nothing — but it runs on a real photo.
2. **Zones for real** — `zone_gain_lut` curve + `look.rs` application + `zone_masses` +
   `edit_zone_masses` command: the strip shows true masses and sculpts the image (full
   Rust test set from Gate 3).
3. **The print goes to the loupe** — throttled `broadcastPhoto(photoId, workingJson)` +
   `printOnLoupe` setting; second-screen live render verified.
4. **Proof sheet** — `auto.rs` + `suggest_auto_tone` command + `proofSpread` +
   `ProofSheet` over one `renderEditBatch`; click adopts; as-shot always dealt.
5. **Duel** — `duelPair` + `DuelView` + round strip + keep-both forks a version.
6. **Rail extraction** — EditorView's slider rail + crop stage extracted and embedded
   (no behaviour change to the old view); Darkroom reaches full slider/geometry parity.
7. **Version shelf & save** — shelf chips + fork + auto-save parity with today's Develop.
8. **The swap** — Darkroom becomes the Develop surface, toggle removed, EditorView shell
   retired, `docs/editing.md` + module description updated, full suites green.
