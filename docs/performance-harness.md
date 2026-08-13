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
- `CHAIRPHOTO_PERF_GRID_WINDOW`: ids used for the window-sized badge measurement, default `500`.
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
  including the `COUNT` that yields `total`. A deep window costing what a shallow one does
  is the claim windowing rests on; compare both against `list_photos_all_date`.
- `gridBadgesAllReturnedIds`: current full-result badge shape. This uses the same command helpers
  as the grid refresh path: volume-health reachability, storage statuses, and version counts.
- `resolver`: sampled candidate-path and resolved-path counts, including offline backup candidates.
- `pendingEnrichment`: queued rows versus rows loadable through the resolver.
- `reconcileScannedScope`: what a scan's finalizing pass costs — the scope query
  (`photo_ids_under` over one seeded year folder) plus `reconcile_missing_for` over just
  that scope, with the scope size against the catalog size.
- `reconcile`: rows checked by the whole-catalog `reconcile_missing` (explicit maintenance,
  no longer run by every scan) and resulting missing count.

By default the harness reports thresholds without enforcing them. Set
`CHAIRPHOTO_PERF_ENFORCE_THRESHOLDS=1` when you want a local run to fail on a clear regression.
