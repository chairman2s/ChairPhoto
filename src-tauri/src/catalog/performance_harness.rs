//! Opt-in large-catalog performance harness.
//!
//! This is a test-only module so it can seed realistic catalog internals quickly without
//! widening the production `Catalog` API. It is ignored by default; see
//! `docs/performance-harness.md` for run instructions.

use super::{Catalog, LocationRole, Result, StorageStatus, VolumeKind};
use rusqlite::params;
use serde::Serialize;
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_PHOTO_COUNT: usize = 100_000;
const DEFAULT_TAG_COUNT: usize = 240;
const DEFAULT_GRID_WINDOW: usize = 500;
const DEFAULT_RESOLVER_SAMPLE: usize = 1_000;
const DEFAULT_MATERIALIZED_FILES: usize = 512;
const SYNTHETIC_PHOTO_BYTES: &[u8] = b"synthetic photo bytes";

#[test]
#[ignore = "large synthetic catalog performance harness; see docs/performance-harness.md"]
fn large_catalog_shape() {
    let config = HarnessConfig::from_env();
    let temp = TempCatalog::create();
    let catalog = Catalog::open(&temp.catalog_path, &temp.photo_root).unwrap();
    let volume_health = crate::volume_health::VolumeHealth::with_ttl(Duration::MAX);
    let mut operations = Vec::new();

    let seed = required_operation(&mut operations, &config, Operation::SeedCatalog, || {
        seed_catalog(&catalog, &temp.photo_root, &config).map(Measured::unmetered)
    });

    let row_counts = required_operation(&mut operations, &config, Operation::CountSqlRows, || {
        Ok(Measured::unmetered(RowCounts::load(&catalog)?))
    });

    let all_photos = required_operation(
        &mut operations,
        &config,
        Operation::ListPhotosAllDate,
        || ListPhotosQuery::all_date().measure(&catalog),
    );
    let all_ids: Vec<i64> = all_photos.iter().map(|p| p.id).collect();
    assert_eq!(all_ids.len(), config.photo_count);

    let tag_filtered = required_operation(
        &mut operations,
        &config,
        Operation::ListPhotosTagFilter,
        || ListPhotosQuery::tag_date(seed.summary.representative_tag_id).measure(&catalog),
    );

    let facet_filtered = required_operation(
        &mut operations,
        &config,
        Operation::ListPhotosFacetsAndSort,
        || ListPhotosQuery::single_facet_and_sort().measure(&catalog),
    );

    let combined_facet_filtered = required_operation(
        &mut operations,
        &config,
        Operation::ListPhotosCombinedFacetsAndFilters,
        || ListPhotosQuery::combined_facets_and_filters().measure(&catalog),
    );

    let nas_photos = required_operation(
        &mut operations,
        &config,
        Operation::ListPhotosOfflineNas,
        || ListPhotosQuery::offline_nas_date().measure(&catalog),
    );

    let tags_count = required_operation(
        &mut operations,
        &config,
        Operation::ListTagsWithCounts,
        || {
            let tags = catalog.list_tags_with_counts()?;
            let payload = command_payload(Operation::ListTagsWithCounts.name(), json!({}), &tags);
            Ok(Measured::new(tags.len(), Some(tags.len()), Some(payload)))
        },
    );

    let _volume_reachability = required_operation(
        &mut operations,
        &config,
        Operation::VolumeReachability,
        || {
            let reachable = load_volume_reachability(&catalog, &volume_health)?;
            let payload = measurement_payload(Operation::VolumeReachability, json!({}), &reachable);
            Ok(Measured::new(
                reachable,
                Some(volume_reachability_len(&catalog)?),
                Some(payload),
            ))
        },
    );

    let window_ids: Vec<i64> = all_ids
        .iter()
        .copied()
        .take(config.grid_window_size)
        .collect();
    let _window_badges = required_operation(
        &mut operations,
        &config,
        Operation::GridBadgesWindow,
        || {
            let measured = measure_grid_badges(&catalog, &volume_health, &window_ids)?;
            Ok(Measured::new(
                measured.summary,
                Some(window_ids.len()),
                Some(measured.payload_bytes),
            ))
        },
    );

    let all_badges = required_operation(
        &mut operations,
        &config,
        Operation::GridBadgesAllReturnedIds,
        || {
            let measured = measure_grid_badges(&catalog, &volume_health, &all_ids)?;
            Ok(Measured::new(
                measured.summary,
                Some(all_ids.len()),
                Some(measured.payload_bytes),
            ))
        },
    );

    let resolver = required_operation(&mut operations, &config, Operation::ResolverSample, || {
        let measured = measure_resolver(
            &catalog,
            &all_ids,
            &seed.materialized_photo_ids,
            config.resolver_sample_count,
        )?;
        let rows = measured.summary.sampled_photos;
        Ok(Measured::new(
            measured.summary,
            Some(rows),
            Some(measured.payload_bytes),
        ))
    });

    let pending_enrichment = required_operation(
        &mut operations,
        &config,
        Operation::LoadPendingEnrichment,
        || {
            let loaded = catalog.load_pending_enrichment()?;
            let summary = PendingSummary {
                queued_rows: row_counts.pending_enrichment,
                load_returned_rows: loaded.len(),
            };
            let payload = measurement_payload(
                Operation::LoadPendingEnrichment,
                json!({ "queuedRows": row_counts.pending_enrichment }),
                &summary,
            );
            Ok(Measured::new(
                summary,
                Some(row_counts.pending_enrichment),
                Some(payload),
            ))
        },
    );

    let reconcile = required_operation(
        &mut operations,
        &config,
        Operation::ReconcileMissing,
        || {
            catalog.reconcile_missing()?;
            let missing = table_count_where(&catalog, "photos", "missing = 1")?;
            let summary = ReconcileSummary {
                photos_checked: config.photo_count,
                missing_after_reconcile: missing,
            };
            let payload = measurement_payload(
                Operation::ReconcileMissing,
                json!({ "photoCount": config.photo_count }),
                &summary,
            );
            Ok(Measured::new(
                summary,
                Some(config.photo_count),
                Some(payload),
            ))
        },
    );

    let report = HarnessReport {
        config,
        catalog_path: temp.catalog_path.display().to_string(),
        schema_version: super::schema::SCHEMA_VERSION,
        seed: seed.summary,
        row_counts,
        result_counts: ResultCounts {
            all_photos: all_ids.len(),
            tag_filtered: tag_filtered.len(),
            facet_filtered: facet_filtered.len(),
            combined_facet_filtered: combined_facet_filtered.len(),
            offline_nas: nas_photos.len(),
            tags: tags_count,
        },
        grid_badges_all_returned_ids: all_badges,
        resolver,
        pending_enrichment,
        reconcile,
        operations,
    };

    println!("{}", serde_json::to_string_pretty(&report).unwrap());

    if !report.config.keep_catalog {
        drop(catalog);
        let _ = fs::remove_dir_all(&temp.base_dir);
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct HarnessConfig {
    photo_count: usize,
    tag_count: usize,
    pending_enrichment_count: usize,
    resolver_sample_count: usize,
    grid_window_size: usize,
    materialized_file_count: usize,
    enforce_thresholds: bool,
    keep_catalog: bool,
}

impl HarnessConfig {
    fn from_env() -> Self {
        let photo_count = env_usize("CHAIRPHOTO_PERF_PHOTOS", DEFAULT_PHOTO_COUNT).max(1);
        let tag_count = env_usize("CHAIRPHOTO_PERF_TAGS", DEFAULT_TAG_COUNT).max(1);
        let pending_default = (photo_count / 2).min(50_000);
        Self {
            photo_count,
            tag_count,
            pending_enrichment_count: env_usize("CHAIRPHOTO_PERF_PENDING", pending_default)
                .min(photo_count),
            resolver_sample_count: env_usize(
                "CHAIRPHOTO_PERF_RESOLVER_SAMPLE",
                DEFAULT_RESOLVER_SAMPLE,
            )
            .min(photo_count),
            grid_window_size: env_usize("CHAIRPHOTO_PERF_GRID_WINDOW", DEFAULT_GRID_WINDOW)
                .min(photo_count),
            materialized_file_count: env_usize(
                "CHAIRPHOTO_PERF_MATERIALIZED_FILES",
                DEFAULT_MATERIALIZED_FILES,
            )
            .min(photo_count),
            enforce_thresholds: env_bool("CHAIRPHOTO_PERF_ENFORCE_THRESHOLDS"),
            keep_catalog: env_bool("CHAIRPHOTO_PERF_KEEP"),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HarnessReport {
    config: HarnessConfig,
    catalog_path: String,
    schema_version: i64,
    seed: SeedSummary,
    row_counts: RowCounts,
    result_counts: ResultCounts,
    grid_badges_all_returned_ids: BadgeSummary,
    resolver: ResolverSummary,
    pending_enrichment: PendingSummary,
    reconcile: ReconcileSummary,
    operations: Vec<OperationMetric>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OperationMetric {
    name: &'static str,
    ok: bool,
    elapsed_ms: f64,
    threshold_ms: Option<f64>,
    within_threshold: Option<bool>,
    rows: Option<usize>,
    estimated_ipc_payload_bytes: Option<PayloadBytes>,
    error: Option<String>,
}

struct Measured<T> {
    value: T,
    rows: Option<usize>,
    estimated_ipc_payload_bytes: Option<PayloadBytes>,
}

impl<T> Measured<T> {
    fn new(
        value: T,
        rows: Option<usize>,
        estimated_ipc_payload_bytes: Option<PayloadBytes>,
    ) -> Self {
        Self {
            value,
            rows,
            estimated_ipc_payload_bytes,
        }
    }

    fn unmetered(value: T) -> Self {
        Self::new(value, None, None)
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct PayloadBytes {
    request: usize,
    response: usize,
    total: usize,
}

impl PayloadBytes {
    fn new(request: usize, response: usize) -> Self {
        Self {
            request,
            response,
            total: request + response,
        }
    }

    fn combine(values: &[Self]) -> Self {
        values.iter().fold(Self::new(0, 0), |sum, value| {
            Self::new(sum.request + value.request, sum.response + value.response)
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    SeedCatalog,
    CountSqlRows,
    ListPhotosAllDate,
    ListPhotosTagFilter,
    ListPhotosFacetsAndSort,
    ListPhotosCombinedFacetsAndFilters,
    ListPhotosOfflineNas,
    ListTagsWithCounts,
    VolumeReachability,
    GridBadgesWindow,
    GridBadgesAllReturnedIds,
    ResolverSample,
    LoadPendingEnrichment,
    ReconcileMissing,
}

impl Operation {
    fn info(self) -> OperationInfo {
        match self {
            Operation::SeedCatalog => OperationInfo::scaled("seed_catalog", 60_000.0),
            Operation::CountSqlRows => OperationInfo::unthresholded("count_sql_rows"),
            Operation::ListPhotosAllDate => OperationInfo::scaled("list_photos_all_date", 30_000.0),
            Operation::ListPhotosTagFilter => {
                OperationInfo::scaled("list_photos_tag_filter", 10_000.0)
            }
            Operation::ListPhotosFacetsAndSort => {
                OperationInfo::scaled("list_photos_facets_and_sort", 15_000.0)
            }
            Operation::ListPhotosCombinedFacetsAndFilters => {
                OperationInfo::scaled("list_photos_combined_facets_and_filters", 15_000.0)
            }
            Operation::ListPhotosOfflineNas => {
                OperationInfo::scaled("list_photos_offline_nas", 20_000.0)
            }
            Operation::ListTagsWithCounts => {
                OperationInfo::scaled("list_tags_with_counts", 20_000.0)
            }
            Operation::VolumeReachability => OperationInfo::fixed("volume_reachability", 1_000.0),
            Operation::GridBadgesWindow => OperationInfo::fixed("grid_badges_window", 5_000.0),
            Operation::GridBadgesAllReturnedIds => {
                OperationInfo::scaled("grid_badges_all_returned_ids", 20_000.0)
            }
            Operation::ResolverSample => OperationInfo::fixed("resolver_sample", 10_000.0),
            Operation::LoadPendingEnrichment => {
                OperationInfo::scaled("load_pending_enrichment", 60_000.0)
            }
            Operation::ReconcileMissing => OperationInfo::scaled("reconcile_missing", 180_000.0),
        }
    }

    fn name(self) -> &'static str {
        self.info().name
    }

    fn threshold_ms(self, photo_count: usize) -> Option<f64> {
        self.info().threshold.ms(photo_count)
    }
}

#[derive(Debug, Clone, Copy)]
struct OperationInfo {
    name: &'static str,
    threshold: Threshold,
}

impl OperationInfo {
    fn scaled(name: &'static str, ms: f64) -> Self {
        Self {
            name,
            threshold: Threshold::Scaled(ms),
        }
    }

    fn fixed(name: &'static str, ms: f64) -> Self {
        Self {
            name,
            threshold: Threshold::Fixed(ms),
        }
    }

    fn unthresholded(name: &'static str) -> Self {
        Self {
            name,
            threshold: Threshold::None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Threshold {
    None,
    Fixed(f64),
    Scaled(f64),
}

impl Threshold {
    fn ms(self, photo_count: usize) -> Option<f64> {
        let scale = (photo_count as f64 / DEFAULT_PHOTO_COUNT as f64).max(0.1);
        match self {
            Threshold::None => None,
            Threshold::Fixed(ms) => Some(ms),
            Threshold::Scaled(ms) => Some(ms * scale),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SeedSummary {
    local_volume_id: i64,
    offline_backup_volume_id: i64,
    representative_tag_id: i64,
    materialized_files: usize,
    verified_backup_files: usize,
    pending_sidecar_identity_rows: usize,
}

struct SeedData {
    summary: SeedSummary,
    materialized_photo_ids: Vec<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RowCounts {
    photos: usize,
    photo_locations: usize,
    photo_tags: usize,
    tags: usize,
    photo_versions: usize,
    pending_enrichment: usize,
    pending_sidecar_identity: usize,
}

impl RowCounts {
    fn load(catalog: &Catalog) -> Result<Self> {
        Ok(Self {
            photos: table_count(catalog, "photos")?,
            photo_locations: table_count(catalog, "photo_locations")?,
            photo_tags: table_count(catalog, "photo_tags")?,
            tags: table_count(catalog, "tags")?,
            photo_versions: table_count(catalog, "photo_versions")?,
            pending_enrichment: table_count(catalog, "pending_enrichment")?,
            pending_sidecar_identity: table_count(catalog, "pending_sidecar_identity")?,
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResultCounts {
    all_photos: usize,
    tag_filtered: usize,
    facet_filtered: usize,
    combined_facet_filtered: usize,
    offline_nas: usize,
    tags: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BadgeSummary {
    requested_ids: usize,
    storage_rows: usize,
    version_rows: usize,
    statuses: BTreeMap<String, usize>,
}

struct BadgeMeasurement {
    summary: BadgeSummary,
    payload_bytes: PayloadBytes,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolverSummary {
    sampled_photos: usize,
    candidate_paths: usize,
    resolved_paths: usize,
    local_candidates: usize,
    backup_candidates: usize,
    offline_backup_candidates: usize,
}

struct ResolverMeasurement {
    summary: ResolverSummary,
    payload_bytes: PayloadBytes,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PendingSummary {
    queued_rows: usize,
    load_returned_rows: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReconcileSummary {
    photos_checked: usize,
    missing_after_reconcile: usize,
}

struct TempCatalog {
    base_dir: PathBuf,
    catalog_path: PathBuf,
    photo_root: PathBuf,
}

impl TempCatalog {
    fn create() -> Self {
        let base_dir = env::temp_dir().join(format!(
            "chairphoto-perf-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        let _ = fs::remove_dir_all(&base_dir);
        let photo_root = base_dir.join("photos");
        fs::create_dir_all(&photo_root).unwrap();
        Self {
            catalog_path: base_dir.join("large.chairphoto"),
            base_dir,
            photo_root,
        }
    }
}

struct ListPhotosQuery {
    tag_id: Option<i64>,
    facets: Vec<String>,
    culling_filter: &'static str,
    storage_tier: &'static str,
    camera: Option<&'static str>,
    lens: Option<&'static str>,
    labels: Vec<String>,
    sort: Option<&'static str>,
}

impl ListPhotosQuery {
    fn all_date() -> Self {
        Self {
            tag_id: None,
            facets: Vec::new(),
            culling_filter: "all",
            storage_tier: "all",
            camera: None,
            lens: None,
            labels: Vec::new(),
            sort: Some("date"),
        }
    }

    fn tag_date(tag_id: i64) -> Self {
        Self {
            tag_id: Some(tag_id),
            ..Self::all_date()
        }
    }

    fn single_facet_and_sort() -> Self {
        Self {
            facets: vec!["has-gps".to_string()],
            storage_tier: "local",
            sort: Some("sharpness_asc"),
            ..Self::all_date()
        }
    }

    fn combined_facets_and_filters() -> Self {
        Self {
            facets: vec!["has-gps".to_string(), "mobile".to_string()],
            camera: Some("Camera 00"),
            labels: vec!["Red".to_string()],
            sort: Some("sharpness_desc"),
            ..Self::all_date()
        }
    }

    fn offline_nas_date() -> Self {
        Self {
            storage_tier: "nas",
            ..Self::all_date()
        }
    }

    fn measure(&self, catalog: &Catalog) -> Result<Measured<Vec<super::Photo>>> {
        let photos = catalog.list_photos(
            self.tag_id,
            None,
            None,
            &self.facets,
            self.culling_filter,
            None,
            self.storage_tier,
            self.camera,
            self.lens,
            &self.labels,
            self.sort,
        )?;
        let rows = photos.len();
        let payload = list_photos_payload(self, &photos);
        Ok(Measured::new(photos, Some(rows), Some(payload)))
    }
}

fn seed_catalog(catalog: &Catalog, photo_root: &Path, config: &HarnessConfig) -> Result<SeedData> {
    let local_volume_id = catalog.ensure_default_volume()?;
    let offline_backup_base = photo_root.parent().unwrap().join("offline-nas");
    fs::create_dir_all(&offline_backup_base).map_err(io_error)?;
    let offline_backup_volume_id =
        catalog.add_volume("offline-nas", &offline_backup_base, VolumeKind::Backup)?;
    let now = unix_now();
    let tx = catalog.conn.unchecked_transaction()?;
    let tag_ids = seed_tags(&tx, config.tag_count, now)?;
    let representative_tag_id = tag_ids[tag_ids.len().min(2) - 1];
    let photo_seed = seed_photos(
        &tx,
        photo_root,
        &offline_backup_base,
        config,
        local_volume_id,
        offline_backup_volume_id,
        &tag_ids,
        now,
    )?;
    tx.commit()?;
    detach_backup_volume(&offline_backup_base)?;
    Ok(SeedData {
        summary: SeedSummary {
            local_volume_id,
            offline_backup_volume_id,
            representative_tag_id,
            materialized_files: photo_seed.materialized_photo_ids.len(),
            verified_backup_files: photo_seed.verified_backup_files,
            pending_sidecar_identity_rows: photo_seed.pending_sidecar_identity_rows,
        },
        materialized_photo_ids: photo_seed.materialized_photo_ids,
    })
}

struct PhotoSeed {
    materialized_photo_ids: Vec<i64>,
    verified_backup_files: usize,
    pending_sidecar_identity_rows: usize,
}

fn seed_tags(
    tx: &rusqlite::Transaction<'_>,
    tag_count: usize,
    now: i64,
) -> rusqlite::Result<Vec<i64>> {
    let mut ids = Vec::with_capacity(tag_count);
    let mut group_parent: Option<i64> = None;
    let mut insert = tx.prepare(
        "INSERT INTO tags(uuid, name, name_norm, parent_id, full_path, full_path_norm,
            description, exportable, private, created_at, updated_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, '', 1, ?7, ?8, ?8)",
    )?;
    for index in 0..tag_count {
        let is_group = index % 10 == 0;
        let group = index / 10;
        let name = if is_group {
            format!("Group {group:03}")
        } else {
            format!("Tag {index:03}")
        };
        let parent_id = if is_group { None } else { group_parent };
        let full_path = if let Some(parent) = parent_id {
            let parent_index = ids.iter().position(|id| *id == parent).unwrap_or(index);
            format!("Group {:03}/{name}", parent_index / 10)
        } else {
            name.clone()
        };
        insert.execute(params![
            uuid::Uuid::new_v4().to_string(),
            name,
            name.to_ascii_lowercase(),
            parent_id,
            full_path,
            full_path.to_ascii_lowercase(),
            (index % 37 == 0) as i64,
            now,
        ])?;
        let id = tx.last_insert_rowid();
        if is_group {
            group_parent = Some(id);
        }
        ids.push(id);
    }
    Ok(ids)
}

fn seed_photos(
    tx: &rusqlite::Transaction<'_>,
    photo_root: &Path,
    backup_root: &Path,
    config: &HarnessConfig,
    local_volume_id: i64,
    offline_backup_volume_id: i64,
    tag_ids: &[i64],
    now: i64,
) -> Result<PhotoSeed> {
    let mut materialized_photo_ids = Vec::with_capacity(config.materialized_file_count);
    let mut verified_backup_files = 0;
    let mut pending_sidecar_identity_rows = 0;
    let mut insert_photo = tx.prepare(
        "INSERT INTO photos(
            id, uuid, path, mtime_ns, size, extension, mime_type, width, height,
            capture_time, camera_make, camera_model, lens, focal_length, aperture,
            shutter_speed, iso, gps_latitude, gps_longitude, thumbnail_path, rating,
            color_label, pick_state, external_editors, metadata_ready, sharpness,
            sharpness_method, burst_flag, missing, created_at, updated_at
         )
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'image/x-sony-arw', ?7, ?8, ?9, ?10, ?11,
            ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25,
            ?26, ?27, 0, ?28, ?28)",
    )?;
    let mut insert_location = tx.prepare(
        "INSERT INTO photo_locations(photo_id, volume_id, relative_path, role, verified_hash, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut insert_tag = tx.prepare(
        "INSERT OR IGNORE INTO photo_tags(photo_id, tag_id, created_at) VALUES(?1, ?2, ?3)",
    )?;
    let mut insert_version = tx.prepare(
        "INSERT INTO photo_versions(photo_id, name, edit_json, position, created_at, updated_at)
         VALUES(?1, ?2, '{}', ?3, ?4, ?4)",
    )?;
    let mut insert_pending =
        tx.prepare("INSERT INTO pending_enrichment(photo_id, queued_at) VALUES(?1, ?2)")?;
    let mut insert_pending_identity = tx.prepare(
        "INSERT INTO pending_sidecar_identity(
            photo_id, field, volume_id, relative_path, attempts, error, queued_at, last_attempt_at
         )
         VALUES(?1, 'identifier', ?2, ?3, 1, ?4, ?5, ?5)",
    )?;

    for index in 0..config.photo_count {
        let photo_id = index as i64 + 1;
        let photo_uuid = uuid::Uuid::new_v4().to_string();
        let rel = format!(
            "{:04}/{:02}/{:02}/IMG_{:06}.ARW",
            2020 + (index % 7),
            1 + (index % 12),
            1 + (index % 28),
            index
        );
        let capture_time = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            2020 + (index % 7),
            1 + (index % 12),
            1 + (index % 28),
            index % 24,
            index % 60,
            (index / 60) % 60
        );
        let camera_make = if index % 17 == 0 { "Apple" } else { "Sony" };
        let camera_model = format!("Camera {:02}", index % 12);
        let lens = format!("Lens {:02}", index % 18);
        let gps_latitude = (index % 6 == 0).then_some(59.0 + (index % 1_000) as f64 / 10_000.0);
        let gps_longitude = (index % 6 == 0).then_some(10.0 + (index % 1_000) as f64 / 10_000.0);
        let label = match index % 6 {
            0 => "Red",
            1 => "Yellow",
            2 => "Green",
            3 => "Blue",
            _ => "",
        };
        let pick_state = if index % 11 == 0 {
            "pick"
        } else if index % 29 == 0 {
            "reject"
        } else {
            "none"
        };
        let sharpness = (index % 5 != 0).then_some((index % 250) as f64 / 3.0);
        let sharpness_method = sharpness.map(|_| "tile");
        let burst_flag = if index % 97 == 0 {
            Some("sharpest-of-burst")
        } else if index % 41 == 0 {
            Some("soft-in-burst")
        } else {
            None
        };
        let thumbnail_path = Some(format!("thumbs/{:06}.jpg", index));
        insert_photo.execute(params![
            photo_id,
            photo_uuid,
            rel,
            1_700_000_000_000_000_000_i64 + index as i64,
            25_000_000_i64 + (index % 1_000) as i64,
            "arw",
            6_000_i64,
            4_000_i64,
            capture_time,
            camera_make,
            camera_model,
            lens,
            35.0 + (index % 200) as f64 / 10.0,
            2.8 + (index % 10) as f64 / 10.0,
            "1/500",
            100_i64 + (index % 8) as i64 * 100,
            gps_latitude,
            gps_longitude,
            thumbnail_path,
            (index % 6) as i64,
            label,
            pick_state,
            if index % 23 == 0 { "darktable" } else { "" },
            if index % 3 == 0 { 0_i64 } else { 1_i64 },
            sharpness,
            sharpness_method,
            burst_flag,
            now,
        ])?;

        let nas_only = index % 4 == 0;
        let materialize = !nas_only
            && materialized_photo_ids.len() < config.materialized_file_count
            && index * config.materialized_file_count / config.photo_count
                >= materialized_photo_ids.len();
        if !nas_only {
            insert_location.execute(params![
                photo_id,
                local_volume_id,
                rel,
                LocationRole::Primary.as_db_str(),
                Option::<String>::None,
                now,
            ])?;
            if materialize {
                let path = photo_root.join(&rel);
                materialize_local_photo(&path, &photo_uuid)?;
                materialized_photo_ids.push(photo_id);
            } else {
                insert_pending_identity.execute(params![
                    photo_id,
                    local_volume_id,
                    rel,
                    "synthetic harness: local sidecar not materialized",
                    now,
                ])?;
                pending_sidecar_identity_rows += 1;
            }
        }
        let backup_path = backup_root.join(&rel);
        let backup_hash = materialize_verified_backup(&backup_path, &photo_uuid)?;
        verified_backup_files += 1;
        insert_location.execute(params![
            photo_id,
            offline_backup_volume_id,
            rel,
            if nas_only {
                LocationRole::Primary.as_db_str()
            } else {
                LocationRole::Backup.as_db_str()
            },
            backup_hash,
            now,
        ])?;
        for tag_id in assigned_tags(index, tag_ids) {
            insert_tag.execute(params![photo_id, tag_id, now])?;
        }

        if index % 20 == 0 {
            insert_version.execute(params![photo_id, "Color", 0_i64, now])?;
            insert_version.execute(params![photo_id, "Black and white", 1_i64, now])?;
        }
        if index < config.pending_enrichment_count {
            insert_pending.execute(params![photo_id, now + index as i64])?;
        }
    }

    Ok(PhotoSeed {
        materialized_photo_ids,
        verified_backup_files,
        pending_sidecar_identity_rows,
    })
}

fn materialize_local_photo(path: &Path, uuid: &str) -> Result<()> {
    write_synthetic_photo(path)?;
    crate::xmp::write_identifier(path, uuid).map_err(super::CatalogError::Io)
}

fn materialize_verified_backup(path: &Path, uuid: &str) -> Result<String> {
    write_synthetic_photo(path)?;
    crate::xmp::write_identifier(path, uuid).map_err(super::CatalogError::Io)?;
    super::lifecycle::sha256_file(path)
}

fn write_synthetic_photo(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_error)?;
    }
    fs::write(path, SYNTHETIC_PHOTO_BYTES).map_err(io_error)
}

fn detach_backup_volume(backup_base: &Path) -> Result<()> {
    let detached = backup_base.with_file_name("offline-nas-detached");
    fs::rename(backup_base, detached).map_err(io_error)
}

fn io_error(error: std::io::Error) -> super::CatalogError {
    super::CatalogError::Io(error.to_string())
}

fn assigned_tags(index: usize, tag_ids: &[i64]) -> Vec<i64> {
    let mut tags = vec![
        tag_ids[index % tag_ids.len()],
        tag_ids[(index * 7 + 3) % tag_ids.len()],
        tag_ids[(index * 13 + 5) % tag_ids.len()],
    ];
    tags.sort_unstable();
    tags.dedup();
    tags
}

fn measure_resolver(
    catalog: &Catalog,
    photo_ids: &[i64],
    materialized_photo_ids: &[i64],
    sample_count: usize,
) -> Result<ResolverMeasurement> {
    let volume_rows = catalog.volume_rows()?;
    let volume_kinds = volume_rows
        .iter()
        .map(|v| (v.id, v.kind))
        .collect::<HashMap<_, _>>();
    let offline_backup_volumes = volume_rows
        .iter()
        .filter(|v| v.kind == VolumeKind::Backup && !Path::new(&v.base_path).is_dir())
        .map(|v| v.id)
        .collect::<std::collections::HashSet<_>>();
    let mut summary = ResolverSummary {
        sampled_photos: 0,
        candidate_paths: 0,
        resolved_paths: 0,
        local_candidates: 0,
        backup_candidates: 0,
        offline_backup_candidates: 0,
    };
    let sampled_ids = resolver_sample_ids(photo_ids, materialized_photo_ids, sample_count);
    for photo_id in &sampled_ids {
        let photo_id = *photo_id;
        let candidates = catalog.photo_path_candidates(photo_id)?;
        summary.sampled_photos += 1;
        summary.candidate_paths += candidates.len();
        for candidate in &candidates {
            match candidate
                .volume_id
                .and_then(|id| volume_kinds.get(&id).copied())
            {
                Some(VolumeKind::Backup) => {
                    summary.backup_candidates += 1;
                    if candidate
                        .volume_id
                        .is_some_and(|id| offline_backup_volumes.contains(&id))
                    {
                        summary.offline_backup_candidates += 1;
                    }
                }
                Some(VolumeKind::Local) | None => summary.local_candidates += 1,
            }
        }
        if catalog.resolve_photo_path(photo_id)?.is_some() {
            summary.resolved_paths += 1;
        }
    }
    let payload_bytes = measurement_payload(
        Operation::ResolverSample,
        json!({ "photoIds": sampled_ids }),
        &summary,
    );
    Ok(ResolverMeasurement {
        summary,
        payload_bytes,
    })
}

fn measure_grid_badges(
    catalog: &Catalog,
    volume_health: &crate::volume_health::VolumeHealth,
    photo_ids: &[i64],
) -> Result<BadgeMeasurement> {
    let reachable = load_volume_reachability(catalog, volume_health)?;
    let statuses =
        crate::commands::grid_photo_statuses_with_reachability(catalog, photo_ids, &reachable)?;
    let versions = crate::commands::grid_version_counts(catalog, photo_ids)?;
    let status_payload = command_payload(
        "photo_statuses",
        json!({ "photoIds": photo_ids }),
        &statuses,
    );
    let version_payload = command_payload(
        "version_counts",
        json!({ "photoIds": photo_ids }),
        &versions,
    );
    let payload_bytes = PayloadBytes::combine(&[status_payload, version_payload]);
    Ok(BadgeMeasurement {
        summary: BadgeSummary {
            requested_ids: photo_ids.len(),
            storage_rows: statuses.len(),
            version_rows: versions.len(),
            statuses: count_statuses(&statuses),
        },
        payload_bytes,
    })
}

fn load_volume_reachability(
    catalog: &Catalog,
    volume_health: &crate::volume_health::VolumeHealth,
) -> Result<HashMap<i64, bool>> {
    let volume_pairs = crate::commands::grid_status_volume_pairs(catalog)?;
    Ok(volume_health.refresh(&volume_pairs))
}

fn volume_reachability_len(catalog: &Catalog) -> Result<usize> {
    Ok(catalog.volume_rows()?.len())
}

fn resolver_sample_ids(
    photo_ids: &[i64],
    materialized_photo_ids: &[i64],
    sample_count: usize,
) -> Vec<i64> {
    let sample_count = sample_count.min(photo_ids.len());
    let materialized_target = materialized_photo_ids.len().min(sample_count.min(64));
    let mut sample = Vec::with_capacity(sample_count);
    let mut seen = std::collections::HashSet::with_capacity(sample_count);

    for photo_id in spread_sample(materialized_photo_ids, materialized_target) {
        if seen.insert(photo_id) {
            sample.push(photo_id);
        }
    }
    for photo_id in spread_sample(photo_ids, sample_count) {
        if sample.len() >= sample_count {
            break;
        }
        if seen.insert(photo_id) {
            sample.push(photo_id);
        }
    }
    sample
}

fn spread_sample(photo_ids: &[i64], sample_count: usize) -> impl Iterator<Item = i64> + '_ {
    let sample_count = sample_count.min(photo_ids.len());
    (0..sample_count).map(move |offset| {
        let index = if sample_count == photo_ids.len() {
            offset
        } else {
            offset * photo_ids.len() / sample_count
        };
        photo_ids[index]
    })
}

fn required_operation<T, F>(
    metrics: &mut Vec<OperationMetric>,
    config: &HarnessConfig,
    operation: Operation,
    f: F,
) -> T
where
    F: FnOnce() -> Result<Measured<T>>,
{
    let start = Instant::now();
    match f() {
        Ok(measured) => {
            let metric = OperationMetric::new(
                operation,
                true,
                elapsed_ms(start),
                config.photo_count,
                measured.rows,
                measured.estimated_ipc_payload_bytes,
                None,
            );
            let threshold_error = threshold_error(config, &metric);
            metrics.push(metric);
            if let Some(message) = threshold_error {
                panic!("{message}");
            }
            measured.value
        }
        Err(error) => {
            metrics.push(OperationMetric::new(
                operation,
                false,
                elapsed_ms(start),
                config.photo_count,
                None,
                None,
                Some(short_error(&error)),
            ));
            panic!("{} failed: {error}", operation.name());
        }
    }
}

impl OperationMetric {
    fn new(
        operation: Operation,
        ok: bool,
        elapsed_ms: f64,
        photo_count: usize,
        rows: Option<usize>,
        estimated_ipc_payload_bytes: Option<PayloadBytes>,
        error: Option<String>,
    ) -> Self {
        let threshold_ms = operation.threshold_ms(photo_count);
        let within_threshold = if ok {
            threshold_ms.map(|threshold| elapsed_ms <= threshold)
        } else {
            None
        };
        Self {
            name: operation.name(),
            ok,
            elapsed_ms,
            threshold_ms,
            within_threshold,
            rows,
            estimated_ipc_payload_bytes,
            error,
        }
    }
}

fn threshold_error(config: &HarnessConfig, metric: &OperationMetric) -> Option<String> {
    if !config.enforce_thresholds || metric.within_threshold != Some(false) {
        return None;
    }
    Some(format!(
        "{} exceeded loose threshold: {:.1} ms > {:.1} ms",
        metric.name,
        metric.elapsed_ms,
        metric.threshold_ms.unwrap_or_default()
    ))
}

fn short_error(error: &super::CatalogError) -> String {
    const LIMIT: usize = 500;
    let out = error.to_string().replace('\n', " ");
    if out.chars().count() > LIMIT {
        let truncated: String = out.chars().take(LIMIT).collect();
        format!("{truncated}...")
    } else {
        out
    }
}

fn count_statuses(pairs: &[(i64, StorageStatus)]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for (_, status) in pairs {
        *counts.entry(format!("{status:?}")).or_insert(0) += 1;
    }
    counts
}

fn table_count(catalog: &Catalog, table: &str) -> Result<usize> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    Ok(catalog
        .conn
        .query_row(&sql, [], |row| row.get::<_, i64>(0))? as usize)
}

fn table_count_where(catalog: &Catalog, table: &str, predicate: &str) -> Result<usize> {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {predicate}");
    Ok(catalog
        .conn
        .query_row(&sql, [], |row| row.get::<_, i64>(0))? as usize)
}

fn list_photos_payload<T: Serialize>(query: &ListPhotosQuery, response: &T) -> PayloadBytes {
    command_payload(
        "list_photos",
        json!({
            "tagId": query.tag_id,
            "albumId": Option::<i64>::None,
            "batchId": Option::<i64>::None,
            "facets": &query.facets,
            "cullingFilter": query.culling_filter,
            "smartAlbumId": Option::<i64>::None,
            "storageTier": query.storage_tier,
            "camera": query.camera,
            "lens": query.lens,
            "labels": &query.labels,
            "sort": query.sort,
        }),
        response,
    )
}

fn command_payload<T: Serialize>(
    command: &str,
    args: serde_json::Value,
    response: &T,
) -> PayloadBytes {
    let request = json!({
        "command": command,
        "args": args,
    });
    PayloadBytes::new(json_bytes(&request), json_bytes(response))
}

fn measurement_payload<T: Serialize>(
    operation: Operation,
    args: serde_json::Value,
    response: &T,
) -> PayloadBytes {
    let request = json!({
        "operation": operation.name(),
        "args": args,
    });
    PayloadBytes::new(json_bytes(&request), json_bytes(response))
}

fn json_bytes<T: Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).unwrap().len()
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1_000.0
}

fn env_usize(key: &str, default: usize) -> usize {
    env::var(key)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

fn env_bool(key: &str) -> bool {
    env::var(key)
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}
