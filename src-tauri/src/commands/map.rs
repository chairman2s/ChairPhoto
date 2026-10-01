//! Map & geo-tagging commands (H2/H3) — polygon geofences that tag photos with
//! hierarchical place tags, plus Nominatim reverse-geocoding into IPTC location fields.
//!
//! Gated on the `map` Cargo feature; see `docs/map-and-geotagging.md` and `plugins/map/`.

use super::*;
use tauri::State;

/// Return every stored geofence.
#[cfg(feature = "map")]
#[tauri::command(async)]
pub fn list_fences(
    state: State<'_, AppState>,
) -> Result<Vec<crate::plugins::map::Fence>, String> {
    with_catalog(&state, |c| {
        crate::plugins::map::ensure_schema(c.conn())?;
        crate::plugins::map::list_fences(c.conn()).map_err(crate::catalog::CatalogError::Sqlite)
    })
}

/// Create a geofence and return the stored row (with its id and created_at).
#[cfg(feature = "map")]
#[tauri::command(async)]
pub fn create_fence(
    state: State<'_, AppState>,
    name: String,
    tag_path: String,
    polygon: Vec<crate::plugins::map::LatLng>,
) -> Result<crate::plugins::map::Fence, String> {
    with_catalog(&state, |c| {
        crate::plugins::map::ensure_schema(c.conn())?;
        crate::plugins::map::create_fence(c.conn(), &name, &tag_path, &polygon)
            .map_err(crate::catalog::CatalogError::Sqlite)
    })
}

/// Update a fence's name, tag path and polygon in place. Returns the number of rows
/// changed (0 means no such fence).
#[cfg(feature = "map")]
#[tauri::command(async)]
pub fn update_fence(
    state: State<'_, AppState>,
    fence_id: i64,
    name: String,
    tag_path: String,
    polygon: Vec<crate::plugins::map::LatLng>,
) -> Result<usize, String> {
    with_catalog(&state, |c| {
        crate::plugins::map::ensure_schema(c.conn())?;
        crate::plugins::map::update_fence(c.conn(), fence_id, &name, &tag_path, &polygon)
            .map_err(crate::catalog::CatalogError::Sqlite)
    })
}

/// Delete a geofence by id. Existing tag assignments on photos are left untouched
/// (they are owned by the user once seeded). Returns the number of rows removed.
#[cfg(feature = "map")]
#[tauri::command(async)]
pub fn delete_fence(
    state: State<'_, AppState>,
    fence_id: i64,
) -> Result<usize, String> {
    with_catalog(&state, |c| {
        crate::plugins::map::ensure_schema(c.conn())?;
        crate::plugins::map::delete_fence(c.conn(), fence_id)
            .map_err(crate::catalog::CatalogError::Sqlite)
    })
}

/// Apply a single fence to all photos in the catalog: any photo whose GPS falls inside
/// the fence gets the fence's tag assigned as a normal editable assignment. Returns the
/// number of *new* assignments created (photos already tagged, or outside the fence,
/// are not counted). Idempotent — safe to call multiple times.
#[cfg(feature = "map")]
#[tauri::command]
pub async fn apply_fence(
    state: State<'_, AppState>,
    fence_id: i64,
) -> Result<usize, String> {
    with_catalog_blocking(&state, move |c| {
        crate::plugins::map::ensure_schema(c.conn())?;
        crate::plugins::map::apply_fence(c, fence_id)
    })
    .await
}

/// Apply all fences to all photos. Returns the total number of new tag assignments
/// created across every fence. Idempotent.
#[cfg(feature = "map")]
#[tauri::command]
pub async fn apply_all_fences(
    state: State<'_, AppState>,
) -> Result<usize, String> {
    with_catalog_blocking(&state, move |c| {
        crate::plugins::map::ensure_schema(c.conn())?;
        crate::plugins::map::apply_all_fences(c)
    })
    .await
}

/// Return `(id, lat, lng)` for every photo that has GPS coordinates (and is not missing).
/// Used by the frontend map view to render clustered markers.
#[cfg(feature = "map")]
#[tauri::command]
pub async fn map_photo_points(
    state: State<'_, AppState>,
) -> Result<Vec<crate::plugins::map::PhotoPoint>, String> {
    with_catalog_blocking(&state, move |c| {
        crate::plugins::map::ensure_schema(c.conn())?;
        crate::plugins::map::map_photo_points(c.conn())
            .map_err(crate::catalog::CatalogError::Sqlite)
    })
    .await
}

/// Assign a GPS position to one or more photos:
///
/// 1. Updates `gps_latitude` / `gps_longitude` in the `photos` table.
/// 2. Writes GPS merge-safely into each photo's XMP sidecar
///    (`exif:GPSLatitude` / `exif:GPSLongitude`; all other sidecar content preserved).
/// 3. Re-applies all geofences to the moved photos so place tags follow the new
///    position.
///
/// Returns the number of new fence-tag assignments created (sum across all photos).
/// An offline/missing photo's sidecar write is skipped (non-fatal); the catalog
/// update is the authoritative record.
#[cfg(feature = "map")]
#[tauri::command]
pub async fn set_photo_gps(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
    lat: f64,
    lng: f64,
) -> Result<usize, String> {
    with_catalog_blocking(&state, move |c| {
        crate::plugins::map::ensure_schema(c.conn())?;
        crate::plugins::map::set_photo_gps(c, &photo_ids, lat, lng)
    })
    .await
}

/// Reverse-geocode a single photo by its `photo_id` using OSM Nominatim (or a
/// configured self-hosted endpoint).
///
/// Returns `null` (serialised as `None`) when the photo has no GPS coordinates.
/// Returns the cached result immediately when the photo's ~1 km grid cell has
/// been looked up before; otherwise calls the remote endpoint, caches the result,
/// and returns it.
///
/// The catalog's `geocode.endpoint` setting overrides the default public Nominatim
/// URL — set it to a self-hosted instance for heavier workloads.
///
/// The implementation honours Nominatim's usage policy: a custom `User-Agent`
/// header identifies the application, and at most one request per second is sent to
/// the endpoint (enforced globally, not per-photo).
#[cfg(feature = "map")]
#[tauri::command]
pub async fn reverse_geocode_photo(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<Option<crate::plugins::map::geocode::GeocodeResult>, String> {
    use crate::plugins::map::geocode::{
        ensure_cache_schema, lookup_cache, nominatim_reverse, round_cell, store_cache,
        DEFAULT_ENDPOINT, SETTING_ENDPOINT,
    };
    use rusqlite::OptionalExtension;

    // ── Step 1: read GPS + check cache (sync — no await while lock is held) ─────
    //
    // We extract everything we need before releasing the lock so no MutexGuard
    // crosses an await point (which would make AppState non-Send).
    struct Step1 {
        lat: f64,
        lng: f64,
        endpoint: String,
        cached: Option<crate::plugins::map::geocode::GeocodeResult>,
    }

    let step1: Step1 = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;

        // Ensure the cache table exists (idempotent).
        ensure_cache_schema(c.conn()).map_err(|e| e.to_string())?;

        // Read GPS from the photos table.
        let row: Option<(f64, f64)> = c
            .conn()
            .query_row(
                "-- includes-hidden: by id.
                 SELECT gps_latitude, gps_longitude FROM photos
                 WHERE id = ?1 AND gps_latitude IS NOT NULL AND gps_longitude IS NOT NULL",
                rusqlite::params![photo_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;

        let Some((lat, lng)) = row else {
            // No GPS — return early without reaching the network.
            return Ok(None);
        };

        // Resolve endpoint: catalog setting → default public Nominatim.
        let endpoint = c
            .get_setting(SETTING_ENDPOINT)
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());

        // Check the cache.
        let cached =
            lookup_cache(c.conn(), round_cell(lat), round_cell(lng))
                .map_err(|e| e.to_string())?;

        Step1 { lat, lng, endpoint, cached }
    }; // ← Mutex guard dropped here; safe to .await below.

    // Cache hit — no network call needed.
    if let Some(hit) = step1.cached {
        return Ok(Some(hit));
    }

    // ── Step 2: async HTTP call (no lock held) ────────────────────────────────
    let result = nominatim_reverse(&step1.endpoint, step1.lat, step1.lng).await?;

    // ── Step 3: store result in cache (sync again) ────────────────────────────
    {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        store_cache(
            c.conn(),
            round_cell(step1.lat),
            round_cell(step1.lng),
            &result,
        )
        .map_err(|e| e.to_string())?;
    }

    Ok(Some(result))
}

/// Reverse-geocode a single photo and fill its **empty** IPTC location fields
/// (city, state, country, country_code); existing values are never overwritten. Returns
/// whether anything was filled. The work is the core's
/// (`plugins::map::geocode::geocode_photo_to_iptc`), shared with the GPUI Map module.
#[cfg(feature = "map")]
#[tauri::command]
pub async fn geocode_to_iptc(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<bool, String> {
    crate::plugins::map::geocode::geocode_photo_to_iptc(&state, None, photo_id).await
}

/// Summary returned by `geocode_all_to_iptc`.
#[cfg(feature = "map")]
pub use crate::plugins::map::geocode::GeocodeAllSummary;

/// Reverse-geocode **all** photos that have GPS coordinates and at least one empty
/// IPTC location field, emitting `geocode:progress { done, total, filled }` (through the
/// state's event sink, i.e. to the webview) after each photo. Returns the totals. The work
/// is the core's (`plugins::map::geocode::geocode_all_to_iptc`).
#[cfg(feature = "map")]
#[tauri::command]
pub async fn geocode_all_to_iptc(
    state: State<'_, AppState>,
) -> Result<GeocodeAllSummary, String> {
    crate::plugins::map::geocode::geocode_all_to_iptc(&state, None).await
}

// ── Face-tagging model commands (feature = "faces") ───────────────────────────
//
// H13a wires the model manager. The store, indexing job and UI come in H13b+. The
// frontend checks `plugin_features()` and keeps the Faces module inert when it's off.

