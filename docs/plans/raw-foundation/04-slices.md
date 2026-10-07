# Vertical Slices: RAW foundation

Build order. Every slice ends runnable and user-visible on the user's own photos. The RAW
path mounts behind a `develop.rawEngine` settings toggle from slice 1 (default off), and
only slice 8 turns it on by default — until then the Darkroom behaves exactly as today.
Branch `feature/raw-foundation` off `feature/darkroom-gpu`.

1. **Tracer bullet — the vendored decoder answers.** LibRaw as a pinned submodule, built by
   `build.rs` with `cc` (OpenMP on), bindgen from the vendored header; `raw::probe` and
   `raw_probe` command; the Darkroom bar shows the source badge from the probe alone:
   *RAW · 14-bit · 67 MP* / *JPEG · 8-bit* / *RAW not supported yet · <camera>*. No
   decode, no render change — but the A7R VI is identified by the app for the first time,
   `--no-default-features` and `--features edit` stay green, and the PKGBUILD/CI checkout
   carry the submodule.
2. **The working image exists and renders.** `raw::decode_linear` (16-bit linear, as-shot
   WB, honest clipping), `WorkingImage`, the `develop` job family with `develop_open` /
   `develop_close` / the `develop:source` event, `RenderSource`, the engine id on the
   record, `apply_look` split into tone + finish, `linear.rs` with the plain sRGB display
   transform and `baseline_ev`, and the `s=` token on `edit://`. With the toggle on, the
   stage swaps from preview to RAW on open and every slider renders engine 2. **This is
   the slice that measures on the corpus:** decode time on the A7R VI, headroom recovered
   on the clipped-sky keepers, and how far the default rendering sits from the camera
   JPEG — the numbers that decide `baseline_ev`, the `Soft` transform default, and whether
   decision 3 stands. Recorded in `00-status.md` before slice 3 starts.
3. **Ownership and cleanup, forced.** Photo switch, Develop exit and catalog switch trip
   the claim; stale `s=w:` URLs 404; the resident set and its byte budget; RSS measured
   before/after a quit from the Darkroom. The forced-race tests from the plan land here.
4. **Prepared once, prepared always.** The `.rawf` decode cache with its size setting
   (Preferences → Darkroom), version-keyed by decoder; second open of a photo shows no
   preview state; neighbour preload into the claim's budget.
5. **Every surface on the same source.** Proof sheet, duels, the tone strip's masses, and
   the loupe print (broadcast carries the token; `LoupeWindow` / `basicEditor` render it)
   all read the working image; the sensor-clipping overlay toggle on the stage.
6. **Export is the view.** Engine-2 export renders the working image through the same
   pipeline at full size; the tone-match step is gone from that path; the parity harness
   gains the exact-equality test; the in-app "export equals view" check behind the metric.
7. **Old versions, honestly.** Loading an engine-1 version keeps engine 1 and says so;
   *Develop with the new engine* forks a fresh engine-2 version with geometry copied and
   tone reset; presets apply on engine 2 through the shared finish stage.
8. **The swap.** `develop.rawEngine` defaults on; the toggle is removed; `docs/editing.md`
   rewritten (source, engines, caches, decoder); `MODULE_LICENSING.md` carries LibRaw;
   full suites green in every feature combination.
9. **Kelvin (first follow-on, same branch or next).** `WbSpec::Kelvin` rendered via
   `wb_multipliers`, a numeric slider, the preference for which slider new edits show, and
   the proof sheet's warm/cool cells as Kelvin offsets around as-shot.
