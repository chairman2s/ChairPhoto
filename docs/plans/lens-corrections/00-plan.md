# Plan: lens corrections from the camera's own tables

Status: agreed with the user 2026-09-28 (scope, source and slice order, in conversation).
Branch `feature/lens-corrections`, cut from `feature/raw-foundation` at `3a78139`.

## What and why

Sony bodies write per-shot lens corrections into every ARW — vignetting, lateral
chromatic aberration and distortion for the lens, focal length and aperture of that frame —
and apply them to their own JPEG. Engine 2 develops the RAW without them, so its corners
are darker than the camera's picture (up to ×1.88 at the corner on the FE 85mm at
f/1.8, measured below) and wide zooms keep their barrel distortion.

All seven Sony ARWs in the agent library carry the three tables
(`exiftool -VignettingCorrParams -ChromaticAberrationCorrParams -DistortionCorrParams`,
2026-09-28): A7 IV + FE 85mm F1.8, A7R VI + FE 85mm F1.8, A7R VI + Sigma 24-70 DG DN Art.
Olympus and Canon are not covered by this plan (see Out of scope).

## Source: RAWmakase (MIT)

The table reader and the correction model are ported from
[pch/rawmakase](https://github.com/pch/rawmakase) at `80b6433`, MIT, "Copyright (c) 2026
RAWmakase contributors" — compatible with ChairPhoto's GPL-3.0-only; the notice travels in
`MODULE_LICENSING.md` and the ported files' headers.

Taken: `src/lens/mod.rs` (radial model), `src/lens/embedded.rs` (Sony ARW and Fujifilm RAF
readers), `src/tiff.rs` (bounded TIFF directory reader), their unit tests.

Not taken, deliberately:
- **The Fujifilm vignetting exponent 0.85** — a fit to Lightroom's rendering. ChairPhoto
  applies the maker's table as written.
- **`default_on`** — RAWmakase's "matches Lightroom" flag. Whether a new edit starts
  corrected is our decision (slice 4), on the record, not a reader property.
- **LCP / DNG opcode paths, GPU kernels** — Adobe lens profiles and DNG opcodes are later
  work, if ever; there is no GPU backend.
- **Anything under the Adobe DNG SDK license** (`camera_profiles/dng_tone.rs`,
  `camera_profiles/temperature.rs`): its indemnification clause and reservation of rights
  are, on our reading, not GPL-compatible. **Tables measured from Camera Raw renders**
  (`color_mixer.bin`, `color_grade_data.rs`, `local_tone_data.rs`, `reference.rs`): built
  to reproduce Lightroom, not our look, with unclear standing under Adobe's terms.

## Design

- **Frame.** The tables' radius runs from the picture centre (0) to its corner (1). The
  picture is the camera's DefaultCrop (e.g. 9984×6656 at 12,8 on the A7R VI, inside a
  10240×7168 raster on lossless-compressed files), which is exactly what the working image
  keeps (`raw_inset_crops[0]`). So the frame is the working image itself: centre = image
  centre, half = half its diagonal — in any orientation, since the model is radial.
  RAWmakase normalizes to LibRaw's `sizes.width/height` instead; on padded rasters that
  would stretch every table. Slice 1 checks inset = DefaultCrop on the corpus.
- **Where the tables live.** Read from the original when it is decoded (a few KB of TIFF,
  no LibRaw), carried on `raw::LinearDecode` and `WorkingImage`, and stored in the `.rawf`
  header (format 3). A cache hit with the original unmounted must still correct.
- **Record.** Engine-2 records gain `lens: { builtin: true }` (slice 2). Absent = no
  correction, so every saved version renders exactly as it did.
- **Pipeline order.** Vignetting is a per-pixel gain in linear light before exposure; it
  commutes with white balance. Distortion + CA is a resample before perspective. Both
  belong to the framed base, so the lens setting joins `geometry_fingerprint`.
- **Clipping overlay.** Vignetting lifts corners above 1.0; the overlay must still mark
  *sensor* clipping, i.e. test before the gain.

## Slices

1. **Reader, model, storage.** Port `lens/` + TIFF reader; `LinearDecode.lens`,
   `WorkingImage.lens`; `.rawf` format 3 carries it. No render change.
   Proof: ported tests; round-trip through the cache; a corpus test reads every
   ARW's tables and checks the decode size equals DefaultCropSize.
   **Done 2026-09-28.** `lens::embedded::tests::corpus_tables_are_read_over_the_cameras_picture`
   (`CHAIRPHOTO_RAW_CORPUS=~/.local/share/chairphoto-agent/library`, 7 ARWs): the decode
   frame equals DefaultCrop on every file (7008×4672 on the A7 IV; 9984×6656 on the A7R VI,
   padded and unpadded rasters alike), so the frame decision holds. Corner values read:
   vignetting ×1.22–×1.88 (FE 85mm at f/1.8 the strongest), distortion −4.75 % (Sigma
   24-70 at 25 mm) to +1.75 %, lateral CA at most 0.061 % of the half diagonal —
   about 2.6 px at the corner (A7 IV + FE 85mm), under 2.5 px on the A7R VI files.
2. **Vignetting.** Record field, gain in the framed base, Darkroom toggle, overlay test
   before the gain, export = view. Proof: corner/centre EV change on the corpus; old
   versions byte-identical; parity check still passes.
3. **Distortion + lateral CA.** Per-channel radial resample with the fill scale (trim the
   undefined border). Measure with the edit bench first: a full-resolution warp on every
   geometry miss of a 67 MP image may break the 50 ms preloaded target; if so, fuse it with
   the perspective warp or run it at output resolution.
4. **Default for new edits.** Decided with the user on their own photos.

## Out of scope / follow-ons

- **Olympus (ORF) and Canon (CR2/CR3)**: RAWmakase has no reader for them. Olympus stores
  distortion/CA parameters in its maker notes; lensfun is the general route (check its
  code and database licenses before use).
- **Wide-gamut working space** — its own feature, planned after this one. Finding
  (2026-09-28): the decode asks LibRaw for sRGB into u16, and LibRaw clamps each channel
  to 0–65535 after the colour matrix (`vendor/LibRaw/src/postprocessing/postprocessing_utils.cpp:111-113`),
  so colours outside sRGB are lost before any slider. Direction: decode in camera RGB
  (`output_color = 0`, only sensor white clipped), convert to linear ProPhoto in f32,
  gamut-map in the display transform; compose `camera.2`'s fitted matrix so saved versions
  render unchanged; `.rawf` format bump. It revisits raw-foundation decision 2. The
  screen does not gain from it: the EV2750 is an sRGB-gamut panel and Hyprland 0.56 drives
  it with `colorManagementPreset: srgb`, no ICC — the gain is in processing and exports.
