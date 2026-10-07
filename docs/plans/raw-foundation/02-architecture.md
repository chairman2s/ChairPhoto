# Architecture: RAW foundation — Develop renders the real file

## Fit

The feature adds one thing the app does not have — a **working image** per photo, the
full-resolution linear decode of the RAW held in memory for the duration of a Develop
session — and points every Develop surface at it. It changes *where pixels come from*,
not how the surfaces work: the Darkroom, the loupe print, the proof sheet, duels, the
tone strip, and export all keep their commands, records and versions.

- `crates/core/src/raw/` — today a one-shot `decode_to_image` (8-bit sRGB, camera WB, used
  only by export). Becomes the **decoder**: same LibRaw FFI, new output contract (16-bit
  linear, no auto-bright, camera WB as metadata not baked), plus a supported/unsupported
  probe. LibRaw itself moves from the distro package to a **vendored, pinned snapshot**
  built with the app (§External) — the packaged 0.22.2 cannot open the user's newest camera.
- `crates/core/src/plugins/edit/` — the render engine gains a **source abstraction**: the
  pipeline's input is a `WorkingImage` (linear f32 RGB, sensor orientation applied, with its
  colour metadata) instead of a decoded JPEG. The framed-base cache, the decode cache, the
  look and the transport built on `feature/darkroom-gpu` carry over; only the *first* input
  of the chain changes. The engine grows a second, versioned pipeline (§Data, engine id):
  the existing gamma-domain pipeline stays byte-for-byte for existing records.
- `src-tauri/src/commands/` — a new `develop` job family in `JobRegistry` (`jobs.rs`) owns
  the working image: one claim per photo, tripped by a photo switch, a Develop exit, or a
  catalog switch. It is the cleanup rule the product demands, expressed in the app's
  existing ownership protocol rather than as a new one.
- `src-tauri/src/protocol.rs` / `image_pool.rs` — the `edit://` URL learns which source it
  renders from (§Endpoints). Same pool, same LIFO, same dedup.
- `crates/core/src/export/` — `full_res_source` + `tone_match_to_preview` are **deleted** for
  engine-v2 records: export renders the same `WorkingImage` through the same pipeline at
  full size. The parity test becomes exact at 100%. Engine-v1 records keep the old path.
- `crates/core/src/thumbnails/` — the Library tiers are untouched. The new **decode cache**
  lives beside them on disk (§Data) so a photo prepared once opens prepared.
- Frontend: `DarkroomView` gains the preparing/developed/unsupported states from the Gate 1
  mockup, driven by one event; `LoupeWindow` and `basicEditor` render the same source via
  the same URL builder; `EditorView` (retired at Darkroom slice 8) is not touched.

**Not in this feature:** the GPU render path (the CPU pipeline is the correctness
reference; `docs/plans/darkroom/00-status.md` § Follow-on holds the GPU gate), lens
correction, denoise, local adjustments, HDR display, colour management of the screen.

## Endpoints

Tauri commands, all behind the `raw` and `edit` Cargo features (compiled out → the
Darkroom behaves exactly as today, on the camera preview, with the *unsupported* badge
reading "no RAW decoder in this build"):

- `develop_open(photo_id) -> DevelopSource` — **new.** Claims the `develop` job family for
  this photo and starts the decode on a blocking worker (decode-cache hit → memory-map,
  else LibRaw → write cache → memory). Returns at once with the *current* source state
  (`preview` while preparing, `raw` when already resident/cached, `unsupported` with the
  camera name when the probe fails, `jpeg` for non-RAW originals) and the source token the
  URLs must carry. Emits `develop:source {photoId, job, source, token, decoder}` when the
  state changes. Preloads N±1 through the same path at lower priority.
- `develop_close(photo_id)` — **new.** Trips the claim; the working image is dropped
  (neighbours' too) — the Develop-exit cleanup. Idempotent.
- `develop_source(photo_id) -> DevelopSource` — **new**, query only; lets a remounted
  Darkroom or a freshly opened loupe re-attach instead of assuming.
- `edit://<photoId>?r=…&m=…[&b=1][&hi=1]&s=<token>` — **existing, extended.** `s` names the
  source the frame renders from: `p` (camera preview, today's path) or `raw:<generation>`.
  Same URL ⇒ same pixels still holds: a URL for a generation that is no longer resident
  404s rather than silently re-rendering from a different source, and the frontend never
  builds a `raw:` URL before `develop:source` said it is resident.
- `render_edit`, `render_edit_batch`, `edit_zone_masses`, `suggest_auto_tone` — **existing,
  extended** with the same `source` argument. The proof sheet and duels render from the
  working image; the masses describe it.
- `raw_probe(photo_id) -> RawSupport` — **new**, cheap (LibRaw open + identify, no unpack):
  `Supported{camera}` | `Unsupported{camera, reason}` | `NotRaw`. Also used by the Library
  inspector badge later; the Darkroom calls it through `develop_open`.
- Export (`export_versions`, unchanged signature) — dispatches on the record's engine id.

## Data

- **No new tables.** Versions, settings, LUTs exist.
- **Edit record schema v3 — the engine id.** `"engine": 2` on records written by the new
  Develop. Absent (`1`) means the gamma-domain pipeline on the camera preview, rendered
  exactly as before; `2` means the linear pipeline on the working image. The Darkroom
  writes `2` on every *new* version it saves; loading an engine-1 version into the
  Darkroom renders it with engine 1 until the user chooses "develop with the new engine",
  which forks a new version (an old EV is not a new EV). `parseEdit` already round-trips
  unknown fields; the Rust record gains the field serde-defaulted to 1.
- **Working image (memory):** `WorkingImage { pixels: linear f32 RGB, width, height,
  orientation, camera_wb: [f32;4], matrix: camera→XYZ, black/white levels, decoder:
  &'static str }`. Owned by the `develop` claim; at most current + N±1 resident, bounded
  by a byte budget (default 4 GB; the Sony file is ~800 MB) — beyond it neighbours are
  not preloaded rather than evicting the current photo.
- **Decode cache (disk):** `~/.cache/chairphoto/raw<v>/<key>.rawf` — 16-bit linear RGB,
  little-endian, a fixed header (magic, dims, orientation, WB, matrix, decoder version);
  key = path + mtime + size, exactly like the preview tiers' `cache_path_for`, plus the
  LibRaw version string in the directory name so a decoder upgrade never serves an old
  decode. Uncompressed (400 MB for the Sony file; NVMe reads it in ~0.2 s, which is the
  whole point). LRU-bounded by a setting `develop.decodeCacheGb` (default 20; the volume
  has 966 GB free and the preview cache already holds 147 GB).
- **Caches on `feature/darkroom-gpu`:** the decode cache (one slot) and the framed-base
  cache key on the *source token* instead of the JPEG fingerprint when `s=raw:*`. Their
  byte-identity tests are re-run against the new source.
- **Settings:** `develop.decodeCacheGb`; `develop.preloadNeighbours` (default on).

## Flow

**Open a keeper in the Darkroom (RAW, first time)**
1. `DarkroomView` mounts → `develop_open(photoId)` → claim the `develop` family (catalog →
   abort → slot, `JobFamily::begin`) → returns `{source: "preview", token: "p"}` → the stage
   renders `edit://…&s=p` as today, badge *camera preview · preparing*.
2. Worker: `raw_probe` → decode-cache lookup → miss → LibRaw decode to 16-bit linear
   (OpenMP on, ~1.5–2 s expected for 67 MP on 24 threads; 5.6 s measured single-threaded)
   → write `.rawf` → convert to f32 → publish as the claim's working image → emit
   `develop:source {source: "raw", token: "raw:<gen>"}`.
3. Frontend on the event: the stage's *next* render uses `s=raw:<gen>`; the settled frame
   swaps at the same framing and zoom; badge *RAW · 14-bit linear · 67 MP*; the masses and
   the loupe broadcast carry the token, so the tone strip and the print follow.
4. Every slider frame from here: `edit://…&s=raw:<gen>` → pool → `render_proxy` reads the
   framed base (miss: downscale the f32 working image once per geometry; hit: look only) →
   linear pipeline → display transform → JPEG → stage. Cost per frame is what
   `feature/darkroom-gpu` measured, because the per-frame work is unchanged.
5. Neighbours: after the current photo is resident, N+1 then N−1 are prepared at low
   priority into the same claim's budget.

**Open a keeper prepared before** — step 2 is a cache hit: read `.rawf` (~0.2 s), convert,
publish. The preview state is visible for a fraction of a second, if at all.

**Unsupported camera** — `raw_probe` fails → `develop:source {source: "unsupported",
camera}` → badge *camera preview · RAW not supported yet · Sony ILCE-7RM6*; the Darkroom
continues on `s=p` exactly as today, and saves engine-1 records (nothing pretends).

**Photo switch / Develop exit / catalog switch** — `develop_open(other)` or
`develop_close` trips the claim; the worker stops at its next cancellation point; the
working image and neighbours are dropped; any `edit://…&s=raw:<old gen>` still in flight
404s and the stage keeps its last frame until the new photo's first frame lands.

**Export** — engine 2: `WorkingImage` (from the claim if resident, else decode-cache, else
decode) → the same linear pipeline at full size → sRGB 8-bit JPEG. Engine 1: the existing
path, tone match included, untouched.

**Sensor-clipping overlay** — a stage toggle; the linear pipeline emits a mask where any
channel of the *working image* is at its white level, composited by the frontend over the
frame at reduced opacity (no extra render: the mask is a per-geometry cached PNG through
`edit://…&b=2`).

## External

- **LibRaw, vendored.** A git submodule at `crates/core/vendor/LibRaw` pinned to a master
  commit at or after `dde798d` (2026-09-09, decodes the ILCE-7RM6 "Compressed RAW 2").
  Built by `build.rs` with the `cc` crate from the snapshot's `src/**` (the Makefile.dist
  object list), `USE_ZLIB` + `USE_OPENMP`, statically linked; bindgen reads the vendored
  header, so the ABI-match argument in `build.rs` holds by construction. The `raw` feature
  stops depending on a system `libraw`; `packaging/PKGBUILD` drops `libraw` from `depends`
  and gains the submodule as a source. Licence: LibRaw is dual LGPL-2.1 / CDDL-1.0; static
  linking is taken under **CDDL-1.0** (file-scope copyleft, no relink obligation), recorded
  in `MODULE_LICENSING.md`.
- **Build tooling:** `cc` and `bindgen` are already in the registry cache; `clang` stays a
  makedepend. No new runtime dependency. `--no-default-features` and `--features edit`
  without `raw` must stay green (the whole feature is behind `raw`).
- **Env var names:** `CHAIRPHOTO_EDIT_TIMING` (existing) gains `decode` and `cache` stages;
  no new env vars.
- **Colour rendering — the decision this doc does not make.** Camera characterisation
  (LibRaw's camera matrix → a working space) and the display transform (a plain sRGB
  curve vs. a filmic/sigmoid/AgX-style compression) determine whether the first RAW render
  looks worse than the camera JPEG. Gate 3 designs the transform slot and its default;
  choosing the default happens on a corpus of the user's own photos (Sony, Olympus, Canon)
  during slice 2, not on paper.
