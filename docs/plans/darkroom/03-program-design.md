# Program Design: Darkroom

## Files

**Frontend**
- `src/components/darkroom/DarkroomView.tsx` — NEW. The Develop surface: stage (print,
  tone strip, actions), version shelf, filmstrip, right rail. Mounted by App where
  `EditorView` mounts today (same `inDevelop` gate, same props shape).
- `src/components/darkroom/ToneStrip.tsx` — NEW. The adjustable histogram: renders
  masses, drag → zone deltas; pure drag math in a helper for tests.
- `src/components/darkroom/ProofSheet.tsx` — NEW. Overlay grid of candidate renders.
- `src/components/darkroom/DuelView.tsx` — NEW. A/B panes + round strip.
- `src/components/darkroom/spreads.ts` — NEW, pure. Builds proof-spread candidate
  records and duel A/B variants. No IO — unit-tested.
- `src/components/darkroom/darkroom.css` — NEW.
- `src/components/EditorView.tsx` — CHANGED. Exports its rail sections and crop stage
  for reuse; the standalone view stays mounted until the last slice swaps it out.
- `src/modules/editing.ts` — CHANGED. TS `EditRecord` gains optional `zones`.
- `src/modules/api.ts` — CHANGED. Wrappers `editZoneMasses`, `suggestAutoTone`.
- `src/App.tsx` — CHANGED (final slice). `inDevelop` mounts `DarkroomView`.
- `src/modules/plugins/basicEditor.tsx` — CHANGED. Name/description say Darkroom;
  the registered edit renderer is untouched.

**Backend (all inside the `edit` feature)**
- `src-tauri/src/plugins/edit/zones.rs` — NEW, pure. Zone gain curve + zone masses.
- `src-tauri/src/plugins/edit/auto.rs` — NEW, pure. Classical auto-tone suggestion.
- `src-tauri/src/plugins/edit/mod.rs` — CHANGED. `EditRecord.zones`; `render_image`
  hands zones to the look stage.
- `src-tauri/src/plugins/edit/look.rs` — CHANGED. Applies the zone gain LUT inside the
  tone step (after EV/WB, before contrast).
- `src-tauri/src/commands/editing.rs` — CHANGED. Commands `edit_zone_masses`,
  `suggest_auto_tone` (same lock-then-spawn_blocking shape as `render_edit`).
- `src-tauri/src/lib.rs` — CHANGED. Register the two commands.
- `docs/editing.md` — CHANGED. Zones field + Darkroom surfaces documented.

## Types & signatures

```rust
// plugins/edit/mod.rs
pub struct EditRecord {
    // …existing fields…
    /// Tone-strip zone offsets in EV, blacks→whites. None/absent = no zone curve —
    /// serde-defaulted so every v1 record parses and renders bit-identically.
    #[serde(default)]
    pub zones: Option<[f32; 8]>,
}

// plugins/edit/zones.rs  (pure)
/// 256-entry per-luma gain LUT from 8 zone EV offsets, smoothly interpolated
/// (cosine between zone centres); all-zero zones ⇒ identity LUT.
pub fn zone_gain_lut(zones: &[f32; 8]) -> [f32; 256];
/// Share of pixels per zone (8 equal gamma-luma bands, Rec.709 luma, subsampled
/// like export::luma_histogram). Sums to ~1.0 for a non-empty image.
pub fn zone_masses(img: &image::RgbImage) -> [f32; 8];

// plugins/edit/auto.rs  (pure)
/// Classical auto-tone: percentile-stretch analysis of the proxy's histogram →
/// a *fragment* record (ev / contrast / highlights / shadows only, no look, no crop).
pub fn suggest_auto_tone(hist: &[u64; 256]) -> AutoTone;
pub struct AutoTone { pub ev: f32, pub contrast: f32, pub highlights: f32, pub shadows: f32 }

// commands/editing.rs
#[tauri::command] pub async fn edit_zone_masses(app: AppHandle, photo_id: i64, edit_json: String) -> Result<[f32; 8], String>;
#[tauri::command] pub async fn suggest_auto_tone(app: AppHandle, photo_id: i64) -> Result<String, String>; // edit-json fragment
```

```ts
// modules/editing.ts
export interface EditRecord { /* …existing… */ zones?: number[] } // length 8

// modules/api.ts
export const editZoneMasses: (photoId: number, editJson: string) => Promise<number[]>;
export const suggestAutoTone: (photoId: number) => Promise<string>;

// components/darkroom/spreads.ts  (pure)
export interface ProofCandidate { label: string; group: "auto" | "film" | "bw" | "look" | "preset" | "asShot"; record: EditRecord }
/** ~12 candidates: as-shot + auto ± warm/cool + looks over `presets`; never touches
 *  base geometry (crop/straighten/perspective are copied through untouched). */
export function proofSpread(base: EditRecord, auto: Partial<EditRecord>, presets: DevelopPreset[]): ProofCandidate[];

export type DuelDim = "ev" | "warmth" | "contrast" | "shadows";
// Narrowed at slice 5: a "look" round isn't symmetric-around-working in one dimension —
// it needs preset semantics. Deferred with the learned auto-tone (00-status.md).
/** A/B variants symmetric around `working` for the round's dimension; step size
 *  decays with `round` (coordinate descent by eye). Only that dimension differs. */
export function duelPair(working: EditRecord, dim: DuelDim, round: number): [EditRecord, EditRecord];

// components/darkroom/ToneStrip.tsx
export function applyZoneDrag(zones: number[] | undefined, zoneIndex: number, deltaEv: number): number[]; // pure, clamped ±2 EV
export function ToneStrip(props: { masses: number[]; zones?: number[]; onZones(z: number[]): void; compact?: boolean }): JSX.Element;
```

## Call stack

**Deal a proof sheet**
`DarkroomView.dealProofs()` → `suggestAutoTone(photoId)` → `proofSpread(working, auto, presets)` → `renderEditBatch(photoId, records, 320)` → `ProofSheet` renders cells → click → `DarkroomView.adopt(record)` → debounced `renderEdit` (stage) + `broadcastPhoto(photoId, workingJson)` (loupe) + `editZoneMasses` (strip).

**Drag a tone-strip zone**
`ToneStrip` pointer drag → `applyZoneDrag` → `onZones` → `DarkroomView.setWorking({…, zones})` → (debounced) `renderEdit` + `broadcastPhoto` + `editZoneMasses`.
Backend: `edit_zone_masses` → resolve path → `preview_bytes` → `render_image(…)` → `zones::zone_masses`.

**A duel round**
`DarkroomView.startDuel()` → per round: `duelPair(working, dim, n)` → `renderEditBatch(photoId, [a, b], 1024)` → `DuelView` → pick → working = winner → next dim; "keep both" → `createVersion` + `setVersionEdit(loser)`.

**Save**
`DarkroomView.save()` → active version ? `setVersionEdit` : `createVersion` + `setVersionEdit` — identical to EditorView's auto-save today.

**Render (engine)**
`render_image` → geometry (unchanged) → `look::apply_look(rgb, &edit, lut)` which now, in its tone step: EV/WB → **zone LUT** (`zone_gain_lut`) → contrast → …rest unchanged.

## Test plan

**Rust (`cargo test`)**
- `zones_all_zero_is_identity` — `zone_gain_lut([0;8])` maps every level to gain 1.
- `zone_lift_raises_only_its_band` — +1 EV on zone 2: gains > 1 inside its band, ≈ 1 beyond the neighbouring bands (feathering bounded).
- `zone_masses_sum_to_one` and `zone_masses_of_gradient_spread_evenly` — synthetic ramp.
- `v1_record_renders_bit_identically_with_zones_field_added` — same synthetic render before/after the serde change, byte-equal.
- `zones_record_matches_across_preview_and_export_paths` — plug a zones record into the existing `assert_paths_match` harness.
- `auto_tone_brightens_dark_histogram` / `auto_tone_leaves_good_exposure_alone` — fragment ev sign/magnitude.
- `edit_zone_masses_command_rejects_without_edit_feature` — cfg parity with `render_edit`.

**TS (vitest)**
- `proofSpread_always_deals_as_shot_first` — and exactly one `asShot` cell.
- `proofSpread_copies_geometry_untouched` — candidate crops/straighten equal base.
- `duelPair_differs_only_in_its_dimension` — field-wise diff assertion per dim.
- `duelPair_step_decays_with_round` — |Δ(round 3)| < |Δ(round 1)|.
- `applyZoneDrag_clamps_and_is_pure` — ±2 EV clamp; input array not mutated.

## Least confident decisions

1. **Zone curve position** (after EV/WB, before contrast): zones overlap what the
   highlights/shadows sliders do — two controls steering the same ranges. Alternative:
   zones *replace* highlights/shadows internally. Chosen: independent, because presets
   and old records use highlights/shadows and must keep meaning the same thing.
2. **8 equal gamma-luma bands labelled as EV zones** — not true scene EV. Honest enough
   for a control; the labels stay ("blacks…whites"), the ±EV numbers are drag units.
3. **Reuse-by-extraction from EditorView** (rail sections, crop stage) vs rebuilding the
   rail fresh — extraction risks churn in a 1.3k-line file; rebuilding risks drift.
   Chosen: extraction, done in its own slice with no behaviour change.
4. **Duel step policy** — start ±0.4 EV (warmth ±12, contrast ±0.2, shadows ±0.25),
   halve per revisit; "same" skips the dimension. Tunable constants in `spreads.ts`.
5. **Proof cell render size 320px** (batch pre-scales to 2×) — 12 cells ≈ one proxy
   decode + 12 small look passes; may need 256 on slower machines.
6. **`suggest_auto_tone` quality** — percentile stretch can look worse than a good
   preset; if it underwhelms, the Auto cells drop to plain "as shot ± EV" until the
   learned model lands (post-v1, same command).
