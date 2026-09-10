# Program Design: RAW foundation — Develop renders the real file

## Files

**Vendored decoder and build**
- `src-tauri/vendor/LibRaw` — NEW git submodule, pinned at/after `dde798d`. `.gitmodules` NEW.
- `src-tauri/build.rs` — CHANGED. With `raw`: compile the submodule's `src/**/*.cpp` with the
  `cc` crate (C++17, `-DUSE_ZLIB -DUSE_OPENMP -fopenmp`, links `z` and `gomp`), emit
  `cargo:rustc-link-lib=static=raw_r`, and run bindgen on the *vendored* header. The
  pkg-config probe goes; the ABI-match argument now holds by construction.
- `src-tauri/Cargo.toml` — CHANGED. `raw` feature comment; `cc` as a build-dependency.
- `packaging/PKGBUILD` — CHANGED. `libraw` leaves `depends`; the submodule joins `source`
  (a second tarball pinned by commit) and `prepare()` places it. `MODULE_LICENSING.md` —
  CHANGED: a "Bundled third-party code" section recording LibRaw under CDDL-1.0.

**Backend (all inside `raw` + `edit` unless noted)**
- `src-tauri/src/raw/mod.rs` — CHANGED. Keeps `decode_to_image` (engine 1's export path,
  untouched). Adds `probe`, `decode_linear`, `decoder_version`, and the 16-bit copy-out.
- `src-tauri/src/develop/mod.rs` — NEW, core (not a plugin: the working image is a host
  service the edit engine consumes). `WorkingImage`, `SourceToken`, `DevelopSource`, the
  resident set with its byte budget.
- `src-tauri/src/develop/cache.rs` — NEW, pure I/O. The `.rawf` file format, read/write,
  LRU trimming by the size setting.
- `src-tauri/src/develop/session.rs` — NEW. The `develop` job worker: claim → probe →
  cache → decode → publish → neighbours; every step a cancellation point.
- `src-tauri/src/commands/develop.rs` — NEW. `develop_open`, `develop_close`,
  `develop_source`, `raw_probe`. Registered in `lib.rs`.
- `src-tauri/src/commands/jobs.rs` — CHANGED. `JobRegistry.develop: JobFamily<DevelopStatus>`
  (the exhaustive destructurings in `lock_for_detach`/`lock_for_publish` make omission a
  compile error, as designed).
- `src-tauri/src/plugins/edit/source.rs` — NEW. `RenderSource`: what a render reads from.
- `src-tauri/src/plugins/edit/linear.rs` — NEW, pure. Engine 2's scene-linear tone stage
  and the display transform slot. Reuses `look.rs` for everything after tone.
- `src-tauri/src/plugins/edit/mod.rs` — CHANGED. `EditRecord.engine` (serde default 1);
  `render_proxy` / `render_image_opts` take a `RenderSource`; the framed-base cache keys on
  the source token; `RenderOpts.clip_mask`.
- `src-tauri/src/plugins/edit/look.rs` — CHANGED. `apply_look` splits into
  `apply_tone` (engine 1 only) and `apply_finish` (B&W → LUT → split → fade → vignette →
  grain, shared by both engines). Byte identity for engine 1 locked by the existing tests.
- `src-tauri/src/protocol.rs`, `src-tauri/src/image_pool.rs` — CHANGED. `EditJob.source`
  parsed from `s=`; `render_edit_bytes` resolves it or 404s.
- `src-tauri/src/export/mod.rs` — CHANGED. `decode_export_source` dispatches on engine:
  2 → `WorkingImage` (resident, cached, or decoded) through `render_image_opts`; 1 → today's
  path. `tone_match_to_preview` stays for engine 1 only.
- `src-tauri/src/thumbnails/mod.rs` — CHANGED (visibility only): `exif_orientation`,
  `cache_dir` become `pub(crate)`.

**Frontend**
- `src/modules/api.ts` — CHANGED. `developOpen`, `developClose`, `developSource`,
  `onDevelopSource`, `rawProbe`; `EditRenderOpts.source`; `renderEditBatch` and
  `editZoneMasses` gain `source`.
- `src/modules/editing.ts` — CHANGED. `VersionEdit.engine?`, `ENGINE_LINEAR`, `isLinear`.
- `src/modules/loupe.ts` — CHANGED. `LoupePhoto.source?: string`.
- `src/components/darkroom/developSource.ts` — NEW, pure. The source state machine the
  view renders: event → `{ token, badge, canRender }`; tested.
- `src/components/darkroom/DarkroomView.tsx` — CHANGED. Open/close lifecycle, owned event
  subscription (`ownedEvents`), badge, token threaded into every render call and the loupe
  broadcast, the "Develop with the new engine" fork for engine-1 versions, clipping toggle.
- `src/components/darkroom/ProofSheet.tsx`, `DuelView.tsx` — CHANGED. Pass the token.
- `src/LoupeWindow.tsx`, `src/modules/plugins/basicEditor.tsx` — CHANGED. Render with the
  broadcast token; without one, exactly today.
- `src/components/Preferences.tsx` — CHANGED. Darkroom section: decode cache size.
- `src/components/darkroom/darkroom.css` — CHANGED. Badge and clip-overlay styles.
- `docs/editing.md`, `docs/plans/raw-foundation/00-status.md` — CHANGED per slice.

## Types & signatures

```rust
// raw/mod.rs
pub struct RawIdentity { pub make: String, pub model: String, pub width: u32, pub height: u32 }
pub enum RawSupport { Supported(RawIdentity), Unsupported { model: Option<String>, reason: String }, NotRaw }
/// open + identify only (no unpack): tens of ms, safe to call from a probe.
pub fn probe(path: &Path) -> RawSupport;
/// 16-bit linear, sRGB/Rec.709 primaries, as-shot WB applied, no auto-bright, highlights
/// clipped at sensor white (never brightened), inset-cropped, sensor orientation.
pub struct LinearDecode {
    pub width: u32, pub height: u32,
    pub rgb16: Vec<u16>,                 // interleaved RGB, row-major
    pub orientation: image::metadata::Orientation,
    pub cam_mul: [f32; 4], pub black: u32, pub maximum: u32,
    pub rgb_cam: [[f32; 3]; 3],
}
pub fn decode_linear(path: &Path, abort: &AtomicBool) -> Result<LinearDecode, String>;
pub fn decoder_version() -> &'static str;   // libraw_version(), e.g. "0.22.0-Devel202609"

// develop/mod.rs
/// Names the pixels a URL renders from. `Preview` is today's path; `Working` is resident
/// only while its generation is the claim's current one.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum SourceToken { Preview, Working { photo_id: i64, generation: u64 } }
impl SourceToken { pub fn parse(s: &str) -> Option<Self>; pub fn to_query(&self) -> String; }

pub struct WorkingImage {
    pub width: u32, pub height: u32,
    pub linear: Arc<image::Rgb32FImage>,   // oriented, inset-cropped, as-shot WB, 0.0 = black, 1.0 = sensor white
    pub cam_mul: [f32; 4], pub rgb_cam: [[f32; 3]; 3],
    pub decoder: &'static str,
}
#[derive(Clone, Serialize)] #[serde(tag = "source", rename_all = "lowercase")]
pub enum DevelopSource {
    Preview { preparing: bool },
    Raw { token: String, bits: u8, megapixels: f32, decoder: String },
    Unsupported { camera: Option<String>, reason: String },
    Jpeg,
    NoDecoder,                              // `raw` feature compiled out
}
pub struct ResidentSet { budget_bytes: u64, images: Vec<(i64, u64, Arc<WorkingImage>)> }
impl ResidentSet {
    pub fn get(&self, token: &SourceToken) -> Option<Arc<WorkingImage>>;
    pub fn insert(&mut self, photo_id: i64, generation: u64, img: Arc<WorkingImage>) -> bool; // false = over budget, not inserted
    pub fn clear(&mut self);
}

// develop/cache.rs
pub struct CacheKey { path: PathBuf, mtime_ns: u128, len: u64, decoder: &'static str }
pub fn cache_path(key: &CacheKey) -> PathBuf;          // <cache>/chairphoto/raw<v>/<fnv>.rawf
pub fn read(key: &CacheKey) -> Option<LinearDecode>;   // None on miss or header mismatch
pub fn write(key: &CacheKey, d: &LinearDecode) -> Result<(), String>;   // temp + rename
pub fn trim_to(budget_bytes: u64);                     // LRU by atime/mtime, best-effort

// develop/session.rs
#[derive(Clone, Copy, Serialize)] pub struct DevelopStatus { pub job: u64, pub photo_id: i64, pub generation: u64, pub resident: bool }
impl JobStatus for DevelopStatus { fn job_id(&self) -> u64 }
/// Claim the family for `photo_id`; spawn the worker; return the state right now.
pub fn open(app: &AppHandle, state: &AppState, photo_id: i64, neighbours: &[i64]) -> Result<DevelopSource, String>;
pub fn close(state: &AppState) -> Result<(), String>;
pub fn current(state: &AppState, photo_id: i64) -> Result<DevelopSource, String>;
/// The worker body — every `?` after a step is preceded by an abort check.
fn prepare(claim: JobClaim<DevelopStatus>, app: AppHandle, photo_id: i64, path: PathBuf, neighbours: Vec<i64>);
/// Event `develop:source`, payload:
#[derive(Serialize)] pub struct DevelopSourceEvent { pub photo_id: i64, pub job: u64, #[serde(flatten)] pub source: DevelopSource }

// commands/develop.rs  (all #[tauri::command]; cfg(not(feature="raw")) → DevelopSource::NoDecoder / Err)
pub async fn develop_open(app: AppHandle, photo_id: i64, neighbours: Vec<i64>) -> Result<DevelopSource, String>;
pub async fn develop_close(app: AppHandle) -> Result<(), String>;
pub async fn develop_source(app: AppHandle, photo_id: i64) -> Result<DevelopSource, String>;
pub async fn raw_probe(app: AppHandle, photo_id: i64) -> Result<RawSupport, String>;

// plugins/edit/source.rs
pub enum RenderSource<'a> { PreviewJpeg(&'a [u8]), Working(Arc<WorkingImage>) }
impl RenderSource<'_> { pub fn fingerprint(&self) -> u64; }   // jpeg bytes hash | (photo_id, generation)

// plugins/edit/mod.rs (changed)
struct EditRecord { #[serde(default = "engine_v1")] engine: u32, /* … existing … */ #[serde(default)] display: Option<String> }
pub struct RenderOpts { pub skip_look: bool, pub clip_mask: bool }
pub fn render_proxy(src: RenderSource<'_>, edit_json: &str, max_edge: u32, opts: RenderOpts) -> Result<DynamicImage, String>;
pub fn render_image_opts(src: RenderSource<'_>, edit_json: &str, max_edge: u32, opts: RenderOpts) -> Result<DynamicImage, String>;
/// Engine dispatch inside: engine 1 requires PreviewJpeg (or any 8-bit image, as today);
/// engine 2 requires Working — a mismatch is an error, never a silent substitution.

// plugins/edit/linear.rs (pure)
/// Engine-2 white balance on the record — tagged, so a version means one thing forever.
#[derive(Deserialize)] #[serde(tag = "mode", rename_all = "lowercase")]
pub enum WbSpec { Relative { temp: f32, tint: f32 }, Kelvin { kelvin: f32, tint: f32 } }   // Kelvin: parsed now, rendered in the Kelvin slice
pub fn wb_multipliers(spec: &WbSpec, cam_mul: &[f32; 4], rgb_cam: &[[f32; 3]; 3]) -> [f32; 3];
pub struct LinearTone { pub ev: f32, pub wb: [f32; 3], pub highlights: f32, pub shadows: f32, pub whites: f32, pub blacks: f32, pub contrast: f32 }
pub fn apply_tone_linear(img: &mut Rgb32FImage, t: &LinearTone, zones: Option<&[f32; 8]>);
pub enum DisplayTransform { Srgb, Soft { shoulder: f32 } }   // the slot; the default is chosen on the corpus in slice 2
pub fn to_display(img: &Rgb32FImage, t: DisplayTransform, baseline_ev: f32) -> RgbImage;  // → 8-bit sRGB for apply_finish
pub fn clip_mask(img: &Rgb32FImage) -> GrayImage;           // 255 where any channel ≥ 1.0

// plugins/edit/look.rs (split)
pub(super) fn apply_tone(img: &mut RgbImage, edit: &EditRecord);              // engine 1, byte-identical to today
pub(super) fn apply_finish(img: &mut RgbImage, edit: &EditRecord, lut: Option<&CubeLut>);  // both engines

// image_pool.rs / protocol.rs (changed)
pub struct EditJob { /* … */ pub source: SourceToken }
// edit://<id>?r=&m=&b=&hi=&s=<p | w:<photo>:<gen>>
```

```ts
// modules/api.ts
export type DevelopSource =
  | { source: "preview"; preparing: boolean }
  | { source: "raw"; token: string; bits: number; megapixels: number; decoder: string }
  | { source: "unsupported"; camera: string | null; reason: string }
  | { source: "jpeg" }
  | { source: "nodecoder" };
export const developOpen: (photoId: number, neighbours: number[]) => Promise<DevelopSource>;
export const developClose: () => Promise<void>;
export const developSource: (photoId: number) => Promise<DevelopSource>;
export const onDevelopSource: (h: (e: DevelopSource & { photoId: number; job: number }) => void) => Promise<UnlistenFn>;
export interface EditRenderOpts { maxEdge?: number; hiRes?: boolean; baseOnly?: boolean; clipMask?: boolean; bust?: number; source?: string }
export const renderEditBatch: (photoId: number, editJsons: string[], maxEdge?: number, source?: string) => Promise<(string | null)[]>;
export const editZoneMasses: (photoId: number, editJson: string, source?: string) => Promise<number[]>;

// modules/editing.ts
export const ENGINE_LINEAR = 2;
export interface VersionEdit { /* … */ engine?: number; display?: string }
export const isLinear: (e: VersionEdit) => boolean;
export const forLinearEngine: (e: VersionEdit) => VersionEdit;  // fresh record, geometry copied, tone reset, engine: 2

// components/darkroom/developSource.ts (pure)
export interface SourceState { token: string | undefined; badge: string; renderable: boolean; engine: 1 | 2 }
export function reduceSource(prev: SourceState, e: DevelopSource, photoId: number): SourceState;
export function stageQuery(s: SourceState): Pick<EditRenderOpts, "source">;

// modules/loupe.ts
export interface LoupePhoto { photoId: number | null; editJson: string | null; source?: string }
```

## Call stack

**Open a keeper (RAW, first time)**
`DarkroomView` mount → `developOpen(photoId, [next, prev])` → `commands::develop_open` →
`session::open` → `JobFamily::begin` (catalog → abort → slot; trips the previous photo's
claim) → spawn `prepare` on a dedicated thread (not the image pool: a decode must not block
tile serving) → returns `Preview{preparing:true}` → view renders `edit://…&s=p` as today.
`prepare`: `raw::probe` → Unsupported? emit + clear slot + return ⟶ `cache::read` → miss →
`raw::decode_linear(path, &claim.abort)` (LibRaw checks the flag between unpack, process and
copy-out) → `cache::write` → `WorkingImage::from(LinearDecode)` (u16→f32, orientation via
`thumbnails::exif_orientation`) → `ResidentSet::insert` under the develop lock → `slot.publish`
→ `app.emit("develop:source", Raw{token})` → for each neighbour, the same chain into the
same claim while `!abort` and the budget allows, without emitting.
View on the event: `reduceSource` → `token` → next stage/settled/masses/loupe calls carry
`s=w:<photo>:<gen>`; badge *RAW · 14-bit linear · 67 MP*.

**A slider frame (engine 2)**
`editRenderUrl(…, {source})` → `edit://` → `edit_job_from_uri` (parses `s=`) → pool →
`render_edit_bytes` → `develop::resident(token)` → `None` ⟹ 404 · `Some(img)` ⟹
`render_proxy(RenderSource::Working(img), …)` → framed-base cache keyed by
`(source.fingerprint(), geometry, edge)` → miss: `frame_image` on an f32 image (geometry +
downscale in linear) → `linear::apply_tone_linear` → `linear::to_display` → `look::apply_finish`
→ JPEG q90 → response. Hit: the last four steps only.

**Photo switch / Develop exit / catalog switch**
`developOpen(other)` or `developClose()` → `session::open`/`close` → `begin`/`cancel` trips
the claim → worker stops at its next check and clears the slot only if it still owns it →
`ResidentSet::clear` for the old photo under the develop lock → in-flight `edit://…&s=w:old`
requests reach `resident()` and 404 → the stage keeps its last frame. Catalog switch: the
registry's `lock_for_detach` now includes `develop`; nothing else to add.

**Export (engine 2)**
`export_versions` → `decode_export_source` → record `engine == 2` →
`develop::resident_or_load(path)` (resident → cached `.rawf` → `decode_linear`) →
`render_image_opts(Working, json, 0, default)` → JPEG q92. The tone-match step is not on
this path.

**Loading an engine-1 version into the Darkroom**
`reduceSource` sees `engine: 1` on the record → the stage renders with `s=p` and engine 1 (as
today), badge *engine 1 · camera preview*; a chip *Develop with the new engine* →
`forLinearEngine(record)` → `createVersion` + `setVersionEdit` → the new version opens on
`s=w:…`. The old version is never rewritten.

## Test plan

**Rust**
- `raw::probe_reports_unsupported_without_panicking` — a synthetic non-RAW and a truncated
  ARW header → `Unsupported`/`NotRaw`, never a crash.
- `raw::decode_linear_is_16bit_linear_and_never_brightens` — on a fixture (a small DNG
  generated in-test with `image`/`tiff` writer, or `SKIPPED: … — no RAW fixture`): output
  bits 16, mean of a mid-grey patch equals the sensor value scaled, no histogram stretch.
- `raw::decode_linear_honours_abort` — flag tripped before unpack → `Err` within one step.
- `develop::cache_roundtrip_is_lossless` — write → read → identical `rgb16` and header.
- `develop::cache_rejects_other_decoder_version` — same bytes, different decoder string →
  miss.
- `develop::cache_trim_keeps_newest_within_budget` — five files, budget for three → the
  two oldest gone.
- `develop::resident_set_never_evicts_the_current_photo` — budget for one image; inserting
  a neighbour returns `false` and the current stays.
- `develop::source_token_parses_and_prints_roundtrip` and `…_rejects_garbage`.
- `jobs::develop_family_is_tripped_by_catalog_switch` — the forced-race pattern already in
  `jobs.rs`: park a `prepare` inside its claim, switch catalogs, assert the flag is set and
  the slot cleared only by its owner.
- `session::open_for_another_photo_trips_the_previous_claim` — two opens; the first
  worker's abort flag is set; `resident()` for the first token is `None` after publish.
- `edit::engine_field_defaults_to_1_and_engine1_renders_byte_identically` — every existing
  fixture record with and without `"engine":1` → equal bytes.
- `edit::engine2_refuses_a_preview_source` — `render_proxy(PreviewJpeg, engine-2 record)` →
  `Err`, never a silent engine-1 render.
- `edit::wb_spec_relative_zero_is_as_shot_and_kelvin_parses` — relative (0,0) yields the
  identity multipliers; a Kelvin record parses and is rejected with a clear error until the
  Kelvin slice renders it (never silently treated as relative).
- `edit::linear_tone_ev_is_a_pure_multiply` — +1 EV doubles every channel before the
  transform; values above 1.0 survive until `to_display`.
- `edit::headroom_recovers_above_display_white` — a synthetic linear image with a patch at
  1.4× display white: at 0 EV the display shows 255; at −1 EV it shows detail; the same
  through engine 1 on the 8-bit rendering shows 255 both times.
- `edit::apply_finish_is_the_old_tail` — `apply_tone` + `apply_finish` == old `apply_look`
  byte for byte on the full-record fixture.
- `edit::clip_mask_marks_only_sensor_white` — 255 exactly where any channel ≥ 1.0.
- `protocol::edit_job_parses_source_token` — `s=p`, `s=w:5:3`, missing (→ Preview), bad (404).
- `export::engine2_export_equals_the_develop_render_at_100pct` — the parity harness gains an
  exact-equality variant: `render_image_opts(Working, json, 0)` for export vs
  `render_proxy(Working, json, 0)` → identical bytes (no JPEG in between; both encoded once).
- `export::engine1_export_path_is_untouched` — the existing `assert_paths_match` tests pass
  unchanged.
- `build`: `cargo check --no-default-features`, `--features edit` (no `raw`) and
  `--all-features --all-targets` warning-free; the vendored build must not require a system
  `libraw`.

**TS (vitest)**
- `developSource.reduce_preview_then_raw_yields_token_and_badge`,
  `…_unsupported_names_the_camera`, `…_nodecoder_is_honest`,
  `…_engine1_record_keeps_engine1_source`, `…_ignores_events_for_another_photo`.
- `editing.forLinearEngine_copies_geometry_resets_tone_sets_engine2`.
- `api.editRenderUrl_carries_the_source_token` (extends the existing URL test).
- `loupe.broadcast_carries_source` — payload shape.

**Manual, on the user's photos** (recorded in `00-status.md`, not automatable):
the swap on first open and the absence of it on the second; the badge states on the A7R VI,
the A7 IV, an Olympus and a JPEG; a blown-sky recovery; export equals view at 100% with the
inspector; a photo switch mid-decode leaves no stale frame; quit from the Darkroom releases
memory (RSS before/after).

## Least confident decisions

1. **White balance — DECIDED with the user (2026-09-10).** The working image carries the
   camera's as-shot multipliers applied, and keeps `cam_mul` and `rgb_cam` beside the
   pixels. The engine-2 record's `wb` is a tagged shape that can express either meaning:
   `{"mode":"relative","temp":…,"tint":…}` (warmer/cooler than as-shot — a *look*, what the
   film presets mean and what ships in this project) or `{"mode":"kelvin","kelvin":…,
   "tint":…}` (a stated scene light — portable between photos and, given each body's
   matrix, between cameras). The mode is on the record, never a preference: a saved
   version renders the same forever. A preference may later pick which *slider* new edits
   show. Kelvin is the conversion function plus a numeric slider, no re-decode; it is
   scheduled as the slice after the pipeline works (not the tracer bullet) because the
   proof sheet's warm/cool cells and auto-tone are better expressed as Kelvin offsets
   around as-shot than as relative nudges. Presets carry whichever mode their author
   meant and apply it as-is.
2. **Working space = linear Rec.709/sRGB primaries** (`output_color=1`, gamma 1/1). A
   wide-gamut space (Rec.2020) would keep more saturated colour through the pipeline; the
   cost is a display conversion on every frame and a colour-management story the app does
   not have. Revisit when the screen path is colour-managed.
3. **Highlights — DECIDED with the user (2026-09-10): honest clipping first.** The decode
   clips every channel at sensor white (`highlight=0`) and never brightens; the recoverable
   range is the ~0.5–1 EV the camera JPEG lifts above a plain decode, and the
   sensor-clipping overlay marks what is truly gone. Slice 2 measures on the user's own
   clipped-sky keepers how much comes back. If that is not enough for the shots that
   matter, the next lever is LibRaw's unclip/reconstruct modes plus a highlight
   desaturation step, which changes the decode contract and regenerates the `.rawf` cache
   once (the decoder-version key handles that).
4. **`baseline_ev` in `to_display`** — a fixed lift so the as-shot render lands near the
   camera JPEG's brightness rather than a stop darker. Per-camera values would be better;
   one global default first, measured on the corpus.
5. **`display` on the record** as a string slot with `Srgb` and one `Soft` transform; the
   default is decided in slice 2 on real photos. If neither looks right, this is where AgX
   or filmic would plug in without touching the record shape.
6. **The decode runs on its own thread, not the image pool**, so a 2 s decode never blocks
   tile serving; the budget of one decode at a time (current, then neighbours) is enforced
   by the single claim, not by a pool.
7. **`.rawf` uncompressed** — 400 MB per Sony file, 20 GB default budget ≈ 50 photos. If the
   volume budget bites, zstd via the `zip` crate's deflate is the fallback (slower reads).
8. **OpenMP linking through `cc`** — `-fopenmp` and `gomp` are gcc-specific; clang builds
   need `omp`. The build script detects the compiler; if it proves fragile, the decode stays
   single-threaded (5.6 s) and the cache does the work.
9. **Neighbour preload inside the same claim** rather than a family per photo: one owner,
   one cleanup, but a slow neighbour decode delays nothing visible only because it runs after
   the current photo is published. If it competes for CPU with the drag, it gets a lower
   thread priority or is disabled by the setting.
