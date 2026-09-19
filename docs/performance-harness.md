# Large Catalog Performance Harness

Issue #20 added an ignored Rust test that builds a synthetic catalog and reports timings
for optimization-sensitive paths. It does not run in normal `cargo test`.

Run it from `src-tauri`:

```bash
CHAIRPHOTO_PERF_PHOTOS=100000 cargo test catalog::performance_harness::large_catalog_shape -- --ignored --nocapture --test-threads=1
```

For a fast smoke run:

```bash
CHAIRPHOTO_PERF_PHOTOS=2000 CHAIRPHOTO_PERF_TAGS=80 CHAIRPHOTO_PERF_PENDING=500 cargo test catalog::performance_harness::large_catalog_shape -- --ignored --nocapture --test-threads=1
```

## Shape

The default run creates:

- 100k synthetic photos.
- 240 hierarchical tags and deterministic multi-tag assignment.
- Deterministic camera, lens, label, GPS, sharpness, and culling metadata for facet/filter queries.
- A local catalog-root volume plus an offline backup volume.
- A mix of local-plus-backup and backup-only photos, so storage status includes offline NAS rows.
- Real synthetic backup files that are SHA-256 verified before the backup volume is made
  unreachable for measurements.
- Written `xmp:Identifier` sidecars for materialized local files and every backup copy, plus
  pending sidecar identity repair rows for non-materialized local copies.
- Version rows for grid version badges.
- A pending-enrichment queue large enough to exercise resume loading.
- A spread of real local files so resolver output includes successful and missing paths.

## Configuration

Environment variables:

- `CHAIRPHOTO_PERF_PHOTOS`: photo rows to seed, default `100000`.
- `CHAIRPHOTO_PERF_TAGS`: tag rows to seed, default `240`.
- `CHAIRPHOTO_PERF_PENDING`: pending-enrichment rows, default `min(photos / 2, 50000)`.
- `CHAIRPHOTO_PERF_RESOLVER_SAMPLE`: photo ids sampled through the resolver, default `1000`.
- `CHAIRPHOTO_PERF_GRID_WINDOW`: rows in a measured grid window — both the `list_photos`
  page size and the ids in the window-sized badge measurement, default `500`.
- `CHAIRPHOTO_PERF_MATERIALIZED_FILES`: local files physically written under the temp root, default `512`.
- `CHAIRPHOTO_PERF_ENFORCE_THRESHOLDS=1`: fail if a required operation exceeds its loose local
  threshold. Thresholds scale with the photo count where that is useful.
- `CHAIRPHOTO_PERF_KEEP=1`: keep the generated catalog and files for inspection.

## Output

The test prints one JSON report. Use the same configuration before and after an optimization
and compare:

- `rowCounts`: SQL table sizes for photos, locations, tags, photo tags, versions, pending
  enrichment, and pending sidecar identity repairs.
- `operations`: timing, row count, loose threshold metadata, estimated command-shaped IPC JSON byte
  counts (`request`, `response`, `total`), and any truncated recorded error per measured operation.
- `resultCounts`: returned rows for broad, tag-filtered, single-facet, combined-facet, and offline
  NAS library queries.
- `windows`: the first and the last window of the full ordered set, through `photo_page` —
  the windowed path the grid uses (`list_photos_window_first` / `list_photos_window_deep`),
  including the `COUNT` that yields `total`. Compare both against `list_photos_all_date`.
  A window saves building and serializing every matching row, but not the ordering: the
  date sort is not index-backed, so both windows still sort the matching set and the deep
  one walks to its offset. Expect the first window to be a fraction of the full listing
  and the deep window to sit between the two, growing with the catalog.
- `gridStatusesWindow`: what a grid refresh costs now — storage status for the visible
  window only. The version count has no side query at all any more: it rides the photo row.
- `gridBadgesAllReturnedIds`: the shape that replaced, kept as the baseline — both badge
  maps for every returned id, which is what `App.refresh` did on each filter change. It
  uses the same command helpers as the grid path: volume-health reachability, storage
  statuses, and version counts.
- `resolver`: sampled candidate-path and resolved-path counts, including offline backup candidates.
- `pendingEnrichment`: queued rows versus rows loadable through the resolver.
- `reconcileScannedScope`: what a scan's finalizing pass costs — the scope query
  (`photo_ids_under` over one seeded year folder) plus `reconcile_missing_for` over just
  that scope, with the scope size against the catalog size.
- `reconcile`: rows checked by the whole-catalog `reconcile_missing` (explicit maintenance,
  no longer run by every scan) and resulting missing count.

By default the harness reports thresholds without enforcing them. Set
`CHAIRPHOTO_PERF_ENFORCE_THRESHOLDS=1` when you want a local run to fail on a clear regression.

## Edit render bench

`src-tauri/src/plugins/edit/bench.rs` is a second ignored test, added for the Darkroom's
GPU-smoothness work (`docs/plans/darkroom/00-status.md`, "Follow-on: GPU smoothness"). It
times every stage a slider-drag frame pays — the cached-proxy clone, the downscale, the
RGB copy, the look loop, JPEG and PNG encode, base64 — plus the end-to-end
`render_image`, at the 720 px drag tier and the 1400 px settled tier, and the 1024 px
masses pass the settle also pays. Medians of N runs, one JSON line per edge.

Run it in **both** profiles: `tauri dev` ships the debug profile, where this crate is
unoptimized (only the decoders are, `Cargo.toml` `[profile.dev.package.*]`), and the
release profile is what users install. The numbers differ by an order of magnitude.

```bash
cargo test plugins::edit::bench::render_stage_timings -- --ignored --nocapture
cargo test --release plugins::edit::bench::render_stage_timings -- --ignored --nocapture
```

Environment variables:

- `CHAIRPHOTO_EDIT_BENCH_JPEG`: a real 2048 px preview proxy to render; default a synthetic
  2048×1365 gradient-plus-noise JPEG.
- `CHAIRPHOTO_EDIT_BENCH_LUT`: a `.cube` file to add a 3D LUT to the look; default none.
- `CHAIRPHOTO_EDIT_BENCH_N`: runs per stage, default `10`.

### Live render timings

The same stages can be read from a running app: start it with `CHAIRPHOTO_EDIT_TIMING=1`
and every `render_edit` / `render_image` prints one `[edit-timing] … profile=debug|release`
line to stderr (`src-tauri/src/plugins/edit/timing.rs`). The frontend half lives behind
Preferences → Darkroom → "Log render timings to the console": the Darkroom logs one
`[edit-timing]` line per painted frame (IPC round trip and resolve-to-paint), a summary every
2 s while frames arrive, and persists the last summary under `editor.renderTiming.lastSummary`
so a run can be read back without the web inspector. The same toggle exposes the WebGL
probe (`GlSpike.tsx`), whose last report is kept under `editor.glSpike.lastReport`.

### Shell transition timing

With the same Darkroom toggle on, leaving Develop records one transition under
`editor.renderTiming.lastShell` (`src/modules/shellTiming.ts`): time from Back to the grid's
React commit, first and last thumbnail loaded, the grid's scroll to the selection, the
longest gap between animation frames (a main-thread or UI-process stall), and every backend
command that took ≥ 50 ms during the transition, by name with a row-count hint. Read it with

```bash
sqlite3 -readonly ~/.local/share/chairphoto/default.chairphoto \
  "select value from settings where key='editor.renderTiming.lastShell'"
```

This is what found the 2.2 s Develop → Library freeze (sync commands waiting for the
catalog lock on the main thread; see `with_catalog` in `src-tauri/src/commands/mod.rs`).

## Tag-count bench

`catalog::tests::tag_count_bench` times `list_tags_with_counts` against a **copy** of a real
catalog (never the live file), both the current implementation and the recursive-closure
query it replaced, and asserts they agree:

```bash
CHAIRPHOTO_TAG_BENCH_DB=~/.local/share/chairphoto/default.chairphoto \
  cargo test --release tag_count_bench -- --ignored --nocapture
```

On a 144k-photo / 1.6k-tag catalog (2026-09-19): closure 240 ms → two queries + walk 106 ms
in release; 1028 → 630 ms in the debug profile `tauri dev` runs.
