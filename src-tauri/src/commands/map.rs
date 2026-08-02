//! Map & geo-tagging commands (H2/H3) — polygon geofences that tag photos with
//! hierarchical place tags, plus Nominatim reverse-geocoding into IPTC location fields.
//!
//! Gated on the `map` Cargo feature; see `docs/map-and-geotagging.md` and `plugins/map/`.

use super::*;
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

/// Return every stored geofence.
#[cfg(feature = "map")]
#[tauri::command]
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
#[tauri::command]
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
#[tauri::command]
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
#[tauri::command]
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
                "SELECT gps_latitude, gps_longitude FROM photos
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
/// (city, state, country, country_code).  Fields that already have a non-empty
/// value are **never overwritten** — this is a fill-in-the-blanks operation, not
/// a replace.
///
/// Returns `true` when at least one field was filled in, `false` when the photo
/// has no GPS, all location fields were already set, or the geocoder returned no
/// data for this location.
///
/// The write path (`set_iptc` + `xmp::write_iptc`) is exactly the same as the
/// manual IPTC-save path, keeping XMP sidecars merge-safe.
#[cfg(feature = "map")]
#[tauri::command]
pub async fn geocode_to_iptc(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<bool, String> {
    use crate::plugins::map::geocode::{
        ensure_cache_schema, lookup_cache, nominatim_reverse, round_cell, store_cache,
        DEFAULT_ENDPOINT, SETTING_ENDPOINT,
    };
    use rusqlite::OptionalExtension;

    // ── Step 1: read GPS + check cache (no .await while locked) ──────────────
    // We also do a quick all-fields-filled check here to avoid an unnecessary
    // HTTP round-trip.  The IPTC is NOT carried into later steps — Step 3 re-reads
    // it under the lock immediately before set_iptc (TOCTOU fix).
    struct Step1 {
        lat: f64,
        lng: f64,
        endpoint: String,
        cached: Option<crate::plugins::map::geocode::GeocodeResult>,
        original_path: std::path::PathBuf,
    }

    let step1: Step1 = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;

        ensure_cache_schema(c.conn()).map_err(|e| e.to_string())?;

        // Read GPS.
        let row: Option<(f64, f64)> = c
            .conn()
            .query_row(
                "SELECT gps_latitude, gps_longitude FROM photos
                 WHERE id = ?1 AND gps_latitude IS NOT NULL AND gps_longitude IS NOT NULL",
                rusqlite::params![photo_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;

        let Some((lat, lng)) = row else {
            return Ok(false); // no GPS — nothing to do
        };

        // Quick early-exit: if all location fields are already filled we can skip the
        // geocoder entirely and avoid an HTTP round-trip.  The actual write (Step 3)
        // re-reads IPTC afresh under the lock to stay race-free.
        let iptc = c.get_iptc(photo_id).map_err(|e| e.to_string())?;
        let all_filled = !iptc.city.is_empty()
            && !iptc.state.is_empty()
            && !iptc.country.is_empty()
            && !iptc.country_code.is_empty();
        if all_filled {
            return Ok(false);
        }

        let endpoint = c
            .get_setting(SETTING_ENDPOINT)
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());

        let cached =
            lookup_cache(c.conn(), round_cell(lat), round_cell(lng))
                .map_err(|e| e.to_string())?;

        let original_path = c.require_photo_path(photo_id).map_err(|e| e.to_string())?;

        Step1 { lat, lng, endpoint, cached, original_path }
    };

    // ── Step 2: async HTTP call (no lock held) ────────────────────────────────
    let geo = if let Some(hit) = step1.cached {
        hit
    } else {
        let result = nominatim_reverse(&step1.endpoint, step1.lat, step1.lng).await?;
        // Cache it.
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
        result
    };

    // ── Step 3: re-read current IPTC under the lock, fill only empty fields,
    //           and write if anything changed.
    //
    // TOCTOU fix: we cannot use the snapshot from Step 1 here — the user may have
    // edited a location field manually (via set_iptc / MetadataPanel) between the
    // Step 1 read and now.  Re-reading inside the same lock acquisition as set_iptc
    // ensures we never overwrite a value the user has since entered.
    let (updated, changed) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;

        // Fresh read — the authoritative current state.
        let current = c.get_iptc(photo_id).map_err(|e| e.to_string())?;

        // Use the production fill logic (testable helper).
        let (updated, changed) =
            crate::plugins::map::geocode::fill_empty_iptc(&current, &geo);

        if changed {
            c.set_iptc(photo_id, &updated).map_err(|e| e.to_string())?;
        }
        (updated, changed)
    };

    if !changed {
        return Ok(false);
    }

    // XMP sidecar write is off the catalog lock (file I/O can be slow).
    crate::xmp::write_iptc(&step1.original_path, &updated)?;

    Ok(true)
}

/// Summary returned by `geocode_all_to_iptc`.
#[cfg(feature = "map")]
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeocodeAllSummary {
    /// Total photos with GPS that had at least one empty location field.
    pub total: usize,
    /// How many had at least one field filled in.
    pub filled: usize,
    /// How many were skipped (all fields already set, or geocoder returned nothing).
    pub skipped: usize,
}

/// Progress event payload for `geocode_all_to_iptc`, emitted as `geocode:progress`.
#[cfg(feature = "map")]
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GeocodeProgress {
    pub done: usize,
    pub total: usize,
    pub filled: usize,
}

/// Reverse-geocode **all** photos that have GPS coordinates and at least one empty
/// IPTC location field (city / state / country / country_code).
///
/// Emits `geocode:progress { done, total, filled }` events as each photo is
/// processed (so the UI can show a progress bar).
///
/// Returns a `GeocodeAllSummary` with the final counts.  The per-photo fill logic
/// is identical to `geocode_to_iptc` — empty fields are filled, existing values
/// are never overwritten.
///
/// The Nominatim throttle (≤1 req/s) is honoured across both commands since the
/// rate limiter is a global static.
#[cfg(feature = "map")]
#[tauri::command]
pub async fn geocode_all_to_iptc(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<GeocodeAllSummary, String> {
    use crate::plugins::map::geocode::{
        ensure_cache_schema, lookup_cache, nominatim_reverse, round_cell, store_cache,
        DEFAULT_ENDPOINT, SETTING_ENDPOINT,
    };

    // ── Step 1: collect candidates ────────────────────────────────────────────
    // A "candidate" is a photo with GPS that has at least one empty location field.
    // We do NOT snapshot IPTC here — the per-photo write (Step 3) re-reads IPTC
    // under the lock immediately before set_iptc (TOCTOU fix: user edits between
    // the initial read and the final write must not be overwritten).
    struct Candidate {
        photo_id: i64,
        lat: f64,
        lng: f64,
        original_path: std::path::PathBuf,
    }

    let (candidates, endpoint) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;

        ensure_cache_schema(c.conn()).map_err(|e| e.to_string())?;

        let endpoint = c
            .get_setting(SETTING_ENDPOINT)
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());

        // Photos with GPS and at least one empty IPTC location column.
        let mut stmt = c.conn().prepare(
            "SELECT id, gps_latitude, gps_longitude
             FROM photos
             WHERE gps_latitude IS NOT NULL AND gps_longitude IS NOT NULL
               AND missing = 0
               AND (iptc_city IS NULL OR iptc_city = ''
                    OR iptc_state IS NULL OR iptc_state = ''
                    OR iptc_country IS NULL OR iptc_country = ''
                    OR iptc_country_code IS NULL OR iptc_country_code = '')",
        ).map_err(|e| e.to_string())?;

        let rows: Vec<(i64, f64, f64)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, f64>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<rusqlite::Result<_>>()
            .map_err(|e| e.to_string())?;

        let mut candidates = Vec::new();
        for (photo_id, lat, lng) in rows {
            let path = match c.require_photo_path(photo_id) {
                Ok(p) => p,
                Err(_) => continue, // missing/offline — skip
            };
            candidates.push(Candidate { photo_id, lat, lng, original_path: path });
        }
        (candidates, endpoint)
    }; // ← Mutex guard dropped here

    let total = candidates.len();
    let mut filled_count = 0usize;
    let mut done = 0usize;

    for candidate in candidates {
        // Per-~1km cell: check the cache first without holding the catalog lock.
        let lat_cell = round_cell(candidate.lat);
        let lng_cell = round_cell(candidate.lng);

        let geo = {
            // Quick cache probe (need lock just for this read).
            let maybe_cached = {
                let guard = state.catalog.lock().map_err(|e| e.to_string())?;
                let c = guard.as_ref().ok_or("No catalog is open")?;
                lookup_cache(c.conn(), lat_cell, lng_cell).map_err(|e| e.to_string())?
            };

            if let Some(cached) = maybe_cached {
                cached
            } else {
                // Network call — no lock held.
                let result = nominatim_reverse(&endpoint, candidate.lat, candidate.lng).await?;
                // Store in cache.
                {
                    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
                    let c = guard.as_ref().ok_or("No catalog is open")?;
                    store_cache(c.conn(), lat_cell, lng_cell, &result)
                        .map_err(|e| e.to_string())?;
                }
                result
            }
        };

        // Fill empty fields.
        //
        // TOCTOU fix: the candidate's IPTC snapshot was taken at the beginning of
        // the loop (before HTTP calls for earlier photos), so it may be stale — the
        // user could have edited a location field manually while the geocoder was
        // running.  Re-read the current IPTC inside the same lock acquisition as
        // set_iptc so we never overwrite a value the user has since entered.
        let (updated, changed) = {
            let guard = state.catalog.lock().map_err(|e| e.to_string())?;
            let c = guard.as_ref().ok_or("No catalog is open")?;

            // Fresh read — the authoritative current state.
            let current = c.get_iptc(candidate.photo_id).map_err(|e| e.to_string())?;

            // Use the production fill logic (testable helper).
            let (updated, changed) =
                crate::plugins::map::geocode::fill_empty_iptc(&current, &geo);

            if changed {
                c.set_iptc(candidate.photo_id, &updated).map_err(|e| e.to_string())?;
            }
            (updated, changed)
        };

        if changed {
            // XMP write is off the catalog lock — intentionally (file I/O can be slow).
            // Only count as filled when the sidecar write also succeeds; otherwise the
            // catalog row is updated but the sidecar is not, which is still a diverged
            // state — report it as skipped so the summary toast is accurate.
            if crate::xmp::write_iptc(&candidate.original_path, &updated).is_ok() {
                filled_count += 1;
            }
        }

        done += 1;
        let _ = app.emit(
            "geocode:progress",
            GeocodeProgress { done, total, filled: filled_count },
        );
    }

    Ok(GeocodeAllSummary {
        total,
        filled: filled_count,
        skipped: total - filled_count,
    })
}

// ── Face-tagging model commands (feature = "faces") ───────────────────────────
//
// H13a wires the model manager. The store, indexing job and UI come in H13b+. The
// frontend checks `plugin_features()` and keeps the Faces module inert when it's off.

