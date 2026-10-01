//! Nominatim reverse-geocoder client with an SQLite-backed cache.
//!
//! # Design
//!
//! - **`map__geocode_cache` table** — keyed by a rounded lat/lng cell (~1 km, i.e. 0.01°
//!   precision) so that photos taken nearby share a single cached lookup. The key is stored
//!   as two `REAL` values at the rounded precision.
//! - **Usage-policy compliance** — Nominatim's [usage policy](https://operations.osmfoundation.org/policies/nominatim/)
//!   requires:
//!   1. A meaningful `User-Agent` header identifying the application.
//!   2. At most **one request per second** to the public endpoint.
//!   Both are enforced here. A configurable `geocode.endpoint` catalog setting allows
//!   self-hosting for heavier workloads.
//! - **No live network calls in tests** — tests use the `mock_endpoint` parameter in
//!   [`reverse_geocode_ll`] so the real Nominatim is never hit during CI.

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

// ── Catalog setting key ────────────────────────────────────────────────────────

/// Catalog settings key that holds the Nominatim endpoint URL.
/// Defaults to the public OSM instance when absent.
pub const SETTING_ENDPOINT: &str = "geocode.endpoint";
pub const DEFAULT_ENDPOINT: &str = "https://nominatim.openstreetmap.org";

/// User-Agent sent with every Nominatim request: the map plugin's one identification,
/// shared with the tile fetcher. Nominatim's policy requires a meaningful, non-default UA
/// that identifies the application and how to reach its authors.
use super::USER_AGENT;

// ── Rate limiter ──────────────────────────────────────────────────────────────

/// Global last-request timestamp for the Nominatim throttle (≤1 req/s).
/// Wrapped in a `tokio::sync::Mutex<Option<Instant>>` so the lock can be **held across
/// the sleep** to prevent concurrent callers from both reading the same timestamp and
/// computing the same wait duration (TOCTOU race).
static LAST_REQUEST: Mutex<Option<Instant>> = Mutex::const_new(None);

/// Minimum interval between successive Nominatim requests (1 second).
const MIN_INTERVAL: Duration = Duration::from_millis(1100); // 10% headroom over 1 s

/// Block (async sleep) until at least `MIN_INTERVAL` has elapsed since the previous
/// request, then record the new request time.
///
/// The `tokio::sync::Mutex` guard is **held across the sleep**, so two concurrent
/// `reverse_geocode_photo` Tauri commands cannot both read `LAST_REQUEST` at the same
/// instant and compute the same wait: the second caller blocks on `lock().await` until
/// the first has finished sleeping and updated the timestamp.  This guarantees that at
/// most one Nominatim request is in flight per `MIN_INTERVAL` window.
async fn throttle() {
    let mut guard = LAST_REQUEST.lock().await;
    let wait = guard
        .map(|t| {
            let elapsed = t.elapsed();
            if elapsed < MIN_INTERVAL {
                MIN_INTERVAL - elapsed
            } else {
                Duration::ZERO
            }
        })
        .unwrap_or(Duration::ZERO);
    if wait > Duration::ZERO {
        tokio::time::sleep(wait).await;
    }
    *guard = Some(Instant::now());
}

// ── Cell rounding ─────────────────────────────────────────────────────────────

/// Round a coordinate to ~1 km precision (0.01° ≈ 1.1 km at the equator).
/// Stored as the key in `map__geocode_cache`.
pub fn round_cell(coord: f64) -> f64 {
    (coord * 100.0).round() / 100.0
}

// ── Schema ────────────────────────────────────────────────────────────────────

/// Create the `map__geocode_cache` table if it doesn't exist yet.
/// Safe to call on every catalog open (like the fence store's `ensure_schema`).
pub fn ensure_cache_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS map__geocode_cache (
            lat_cell    REAL NOT NULL,
            lng_cell    REAL NOT NULL,
            city        TEXT,
            state       TEXT,
            country     TEXT,
            country_code TEXT,
            cached_at   INTEGER NOT NULL,
            PRIMARY KEY (lat_cell, lng_cell)
        );",
    )
}

// ── Result type ───────────────────────────────────────────────────────────────

/// Coarse reverse-geocode result: country/state/city extracted from the Nominatim
/// `address` object.  Any field may be absent if Nominatim doesn't return it for
/// the given coordinate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeocodeResult {
    pub city: Option<String>,
    pub state: Option<String>,
    pub country: Option<String>,
    pub country_code: Option<String>,
}

/// Fill empty IPTC location fields from a geocode result.
///
/// Takes the current IPTC fields and a geocode result, and returns an updated
/// IPTC struct with only the empty fields filled. This is a pure function
/// suitable for testing and reuse across commands.
///
/// **Invariant:** Never overwrites a field that is already non-empty.
/// This ensures user-entered values are always preserved.
///
/// # Arguments
/// * `current` — the current IPTC fields (may have user-entered values)
/// * `geocode` — the geocode result from Nominatim (may have None fields)
///
/// # Returns
/// A tuple of (updated IPTC fields, whether any field was actually changed)
pub fn fill_empty_iptc(
    current: &crate::catalog::IptcFields,
    geocode: &GeocodeResult,
) -> (crate::catalog::IptcFields, bool) {
    let mut updated = current.clone();
    let mut changed = false;

    // Fill city only if it's currently empty and geocode has a non-empty value.
    if updated.city.is_empty() {
        if let Some(v) = geocode.city.as_ref().filter(|s| !s.is_empty()) {
            updated.city = v.clone();
            changed = true;
        }
    }

    // Fill state only if it's currently empty and geocode has a non-empty value.
    if updated.state.is_empty() {
        if let Some(v) = geocode.state.as_ref().filter(|s| !s.is_empty()) {
            updated.state = v.clone();
            changed = true;
        }
    }

    // Fill country only if it's currently empty and geocode has a non-empty value.
    if updated.country.is_empty() {
        if let Some(v) = geocode.country.as_ref().filter(|s| !s.is_empty()) {
            updated.country = v.clone();
            changed = true;
        }
    }

    // Fill country_code only if it's currently empty and geocode has a non-empty value.
    if updated.country_code.is_empty() {
        if let Some(v) = geocode.country_code.as_ref().filter(|s| !s.is_empty()) {
            updated.country_code = v.clone();
            changed = true;
        }
    }

    (updated, changed)
}

// ── Cache read/write ──────────────────────────────────────────────────────────

/// Look up a rounded cell in the cache. Returns `None` on a cache miss.
pub fn lookup_cache(
    conn: &rusqlite::Connection,
    lat_cell: f64,
    lng_cell: f64,
) -> rusqlite::Result<Option<GeocodeResult>> {
    conn.query_row(
        "SELECT city, state, country, country_code
         FROM map__geocode_cache
         WHERE lat_cell = ?1 AND lng_cell = ?2",
        params![lat_cell, lng_cell],
        |r| {
            Ok(GeocodeResult {
                city: r.get(0)?,
                state: r.get(1)?,
                country: r.get(2)?,
                country_code: r.get(3)?,
            })
        },
    )
    .optional()
}

/// Insert or replace a geocode result for the given cell.
pub fn store_cache(
    conn: &rusqlite::Connection,
    lat_cell: f64,
    lng_cell: f64,
    result: &GeocodeResult,
) -> rusqlite::Result<()> {
    let cached_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    conn.execute(
        "INSERT OR REPLACE INTO map__geocode_cache
             (lat_cell, lng_cell, city, state, country, country_code, cached_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            lat_cell,
            lng_cell,
            result.city,
            result.state,
            result.country,
            result.country_code,
            cached_at
        ],
    )?;
    Ok(())
}

// ── Nominatim HTTP call ───────────────────────────────────────────────────────

/// Deserialise the `address` sub-object from Nominatim's `/reverse` JSON response.
#[derive(Deserialize)]
struct NominatimAddress {
    city: Option<String>,
    town: Option<String>,
    village: Option<String>,
    municipality: Option<String>,
    county: Option<String>,
    state: Option<String>,
    country: Option<String>,
    country_code: Option<String>,
}

/// Top-level Nominatim `/reverse` response we care about.
#[derive(Deserialize)]
struct NominatimResponse {
    address: Option<NominatimAddress>,
}

/// Call the Nominatim `/reverse` endpoint and return a `GeocodeResult`.
///
/// `endpoint` is the base URL (e.g. `"https://nominatim.openstreetmap.org"`).
///
/// Respects the ≤1 req/s throttle enforced by [`throttle()`].
///
/// **Never called in tests** — tests use a mock server URL; the throttle has a
/// near-zero wait in tests because the mock URL doesn't actually hit the network.
pub async fn nominatim_reverse(
    endpoint: &str,
    lat: f64,
    lng: f64,
) -> Result<GeocodeResult, String> {
    throttle().await;

    let url = format!(
        "{}/reverse?format=jsonv2&lat={}&lon={}&zoom=10&addressdetails=1",
        endpoint.trim_end_matches('/'),
        lat,
        lng,
    );

    let resp = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("geocode: failed to build HTTP client: {e}"))?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("geocode: HTTP request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!(
            "geocode: Nominatim returned HTTP {}",
            resp.status()
        ));
    }

    let body: NominatimResponse = resp
        .json()
        .await
        .map_err(|e| format!("geocode: failed to parse Nominatim response: {e}"))?;

    let addr = body.address.unwrap_or(NominatimAddress {
        city: None,
        town: None,
        village: None,
        municipality: None,
        county: None,
        state: None,
        country: None,
        country_code: None,
    });

    // Prefer city → town → village → municipality → county for the "city" field,
    // matching the coarse urban/rural hierarchy Nominatim uses.
    let city = addr
        .city
        .or(addr.town)
        .or(addr.village)
        .or(addr.municipality)
        .or(addr.county);

    Ok(GeocodeResult {
        city,
        state: addr.state,
        country: addr.country,
        country_code: addr.country_code.map(|c| c.to_uppercase()),
    })
}

// ── High-level entry points ───────────────────────────────────────────────────

/// Reverse-geocode a lat/lng coordinate: check the cache first, then call Nominatim.
///
/// `endpoint` should come from the catalog setting `geocode.endpoint`, falling back to
/// `DEFAULT_ENDPOINT`.  `conn` must already have `map__geocode_cache` schema applied
/// (via [`ensure_cache_schema`]).
pub async fn reverse_geocode_ll(
    conn: &rusqlite::Connection,
    endpoint: &str,
    lat: f64,
    lng: f64,
) -> Result<GeocodeResult, String> {
    let lat_cell = round_cell(lat);
    let lng_cell = round_cell(lng);

    // Cache hit?
    if let Some(cached) = lookup_cache(conn, lat_cell, lng_cell).map_err(|e| e.to_string())? {
        return Ok(cached);
    }

    // Cache miss — call the remote.
    let result = nominatim_reverse(endpoint, lat, lng).await?;

    // Persist the result.
    store_cache(conn, lat_cell, lng_cell, &result).map_err(|e| e.to_string())?;

    Ok(result)
}

// ── Filling IPTC location fields (the Tauri commands and the GPUI Map module) ────────

/// Reverse-geocode one photo and fill its **empty** IPTC location fields (city, state,
/// country, country_code). Fields that already hold a value are **never overwritten**.
///
/// Returns `true` when at least one field was filled, `false` when the photo has no GPS,
/// every location field is already set, or the geocoder had nothing for the place.
///
/// The catalog lock is never held across the HTTP call: (1) read GPS, the endpoint and the
/// cache under the lock; (2) ask Nominatim with no lock held; (3) re-read the IPTC under the
/// lock and fill only what is still empty (a value the user typed meanwhile wins), then
/// write the sidecar off the lock through `xmp::write_iptc`, as a manual IPTC save does.
pub async fn geocode_photo_to_iptc(state: &crate::app::AppState, photo_id: i64) -> Result<bool, String> {
    struct Step1 {
        lat: f64,
        lng: f64,
        endpoint: String,
        cached: Option<GeocodeResult>,
        original_path: std::path::PathBuf,
    }

    let step1: Step1 = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        ensure_cache_schema(c.conn()).map_err(|e| e.to_string())?;
        let row: Option<(f64, f64)> = c
            .conn()
            .query_row(
                "-- includes-hidden: by id.
                 SELECT gps_latitude, gps_longitude FROM photos
                 WHERE id = ?1 AND gps_latitude IS NOT NULL AND gps_longitude IS NOT NULL",
                params![photo_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some((lat, lng)) = row else {
            return Ok(false); // no GPS — nothing to do
        };
        // Every location field already set: no need to ask the geocoder. Step 3 re-reads.
        let iptc = c.get_iptc(photo_id).map_err(|e| e.to_string())?;
        if !iptc.city.is_empty() && !iptc.state.is_empty() && !iptc.country.is_empty() && !iptc.country_code.is_empty() {
            return Ok(false);
        }
        let endpoint =
            c.get_setting(SETTING_ENDPOINT).map_err(|e| e.to_string())?.unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
        let cached = lookup_cache(c.conn(), round_cell(lat), round_cell(lng)).map_err(|e| e.to_string())?;
        let original_path = c.require_photo_path(photo_id).map_err(|e| e.to_string())?;
        Step1 { lat, lng, endpoint, cached, original_path }
    };

    let geo = match step1.cached {
        Some(hit) => hit,
        None => {
            let result = nominatim_reverse(&step1.endpoint, step1.lat, step1.lng).await?;
            let guard = state.catalog.lock().map_err(|e| e.to_string())?;
            let c = guard.as_ref().ok_or("No catalog is open")?;
            store_cache(c.conn(), round_cell(step1.lat), round_cell(step1.lng), &result).map_err(|e| e.to_string())?;
            result
        }
    };

    let (updated, changed) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let c = guard.as_ref().ok_or("No catalog is open")?;
        let current = c.get_iptc(photo_id).map_err(|e| e.to_string())?;
        let (updated, changed) = fill_empty_iptc(&current, &geo);
        if changed {
            c.set_iptc(photo_id, &updated).map_err(|e| e.to_string())?;
        }
        (updated, changed)
    };
    if !changed {
        return Ok(false);
    }
    crate::xmp::write_iptc(&step1.original_path, &updated)?;
    Ok(true)
}

/// Summary of [`geocode_all_to_iptc`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeocodeAllSummary {
    /// Photos with GPS that had at least one empty location field.
    pub total: usize,
    /// How many had at least one field filled.
    pub filled: usize,
    /// How many were skipped (every field set meanwhile, or no result).
    pub skipped: usize,
}

/// [`geocode_photo_to_iptc`] across the library: every visible photo with GPS and at least
/// one empty IPTC location field, sending `geocode:progress { done, total, filled }` through
/// the state's event sink after each photo. The Nominatim throttle is global, so this and
/// the single-photo path share one ≤ 1 req/s budget.
pub async fn geocode_all_to_iptc(state: &crate::app::AppState) -> Result<GeocodeAllSummary, String> {
    use crate::app::{CoreEvent, EventSink as _, GeocodeProgress};

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
        let endpoint =
            c.get_setting(SETTING_ENDPOINT).map_err(|e| e.to_string())?.unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
        let mut stmt = c
            .conn()
            .prepare(
                "SELECT id, gps_latitude, gps_longitude
                 FROM photos_visible
                 WHERE gps_latitude IS NOT NULL AND gps_longitude IS NOT NULL
                   AND (iptc_city IS NULL OR iptc_city = ''
                        OR iptc_state IS NULL OR iptc_state = ''
                        OR iptc_country IS NULL OR iptc_country = ''
                        OR iptc_country_code IS NULL OR iptc_country_code = '')",
            )
            .map_err(|e| e.to_string())?;
        let rows: Vec<(i64, f64, f64)> = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?)))
            .map_err(|e| e.to_string())?
            .collect::<rusqlite::Result<_>>()
            .map_err(|e| e.to_string())?;
        let mut candidates = Vec::new();
        for (photo_id, lat, lng) in rows {
            // Missing or offline: skipped, not an error.
            if let Ok(path) = c.require_photo_path(photo_id) {
                candidates.push(Candidate { photo_id, lat, lng, original_path: path });
            }
        }
        (candidates, endpoint)
    };

    let total = candidates.len();
    let (mut filled, mut done) = (0usize, 0usize);
    for candidate in candidates {
        let (lat_cell, lng_cell) = (round_cell(candidate.lat), round_cell(candidate.lng));
        let cached = {
            let guard = state.catalog.lock().map_err(|e| e.to_string())?;
            let c = guard.as_ref().ok_or("No catalog is open")?;
            lookup_cache(c.conn(), lat_cell, lng_cell).map_err(|e| e.to_string())?
        };
        let geo = match cached {
            Some(hit) => hit,
            None => {
                let result = nominatim_reverse(&endpoint, candidate.lat, candidate.lng).await?;
                let guard = state.catalog.lock().map_err(|e| e.to_string())?;
                let c = guard.as_ref().ok_or("No catalog is open")?;
                store_cache(c.conn(), lat_cell, lng_cell, &result).map_err(|e| e.to_string())?;
                result
            }
        };
        // Re-read under the lock that writes: a value the user typed meanwhile wins.
        let (updated, changed) = {
            let guard = state.catalog.lock().map_err(|e| e.to_string())?;
            let c = guard.as_ref().ok_or("No catalog is open")?;
            let current = c.get_iptc(candidate.photo_id).map_err(|e| e.to_string())?;
            let (updated, changed) = fill_empty_iptc(&current, &geo);
            if changed {
                c.set_iptc(candidate.photo_id, &updated).map_err(|e| e.to_string())?;
            }
            (updated, changed)
        };
        // Counted as filled only when the sidecar write also succeeded, so the summary does
        // not claim a photo whose sidecar diverged.
        if changed && crate::xmp::write_iptc(&candidate.original_path, &updated).is_ok() {
            filled += 1;
        }
        done += 1;
        state.send(CoreEvent::GeocodeProgress(GeocodeProgress { done, total, filled }));
    }
    Ok(GeocodeAllSummary { total, filled, skipped: total - filled })
}

// NOTE: A `reverse_geocode_photo(catalog, photo_id, …)` convenience wrapper is
// intentionally NOT provided here.  Any such function would need to hold a
// `&rusqlite::Connection` (borrowed from the catalog's Mutex guard) across the
// `.await` in `reverse_geocode_ll` / `nominatim_reverse`, which would make the
// MutexGuard cross an await point and block the catalog for the full duration of
// the HTTP call (potentially several seconds).
//
// The Tauri command `commands::reverse_geocode_photo` implements the correct
// three-step pattern instead: (1) lock catalog, read GPS + cache, drop lock;
// (2) async HTTP call without any lock held; (3) lock catalog again, store result.
// Follow that pattern if you need to add batch geocoding.

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helper: in-memory DB with cache schema ─────────────────────────────

    fn mem_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        ensure_cache_schema(&conn).unwrap();
        conn
    }

    // ── round_cell ─────────────────────────────────────────────────────────

    #[test]
    fn round_cell_rounds_to_hundredths() {
        // 59.9123 → 59.91
        assert_eq!(round_cell(59.9123), 59.91);
        // 10.005 → 10.01 (rounds up at exact .005 boundary per f64 round())
        // We just care it's stable, not the exact rounding mode.
        let v = round_cell(10.005);
        assert!((v - 10.01).abs() < 1e-9 || (v - 10.00).abs() < 1e-9);
        // Negative coords work too.
        assert_eq!(round_cell(-33.8651), -33.87);
    }

    #[test]
    fn round_cell_nearby_points_share_same_cell() {
        // Two points within the same 0.01° cell map to the same value.
        // 59.9100 and 59.9149 both round to 59.91.
        assert_eq!(round_cell(59.9100), round_cell(59.9149));
        // Symmetry: -59.9100 and -59.9149 both round to -59.91.
        assert_eq!(round_cell(-59.9100), round_cell(-59.9149));
    }

    // ── Schema ─────────────────────────────────────────────────────────────

    #[test]
    fn ensure_cache_schema_is_idempotent() {
        let conn = mem_db();
        ensure_cache_schema(&conn).unwrap();
        ensure_cache_schema(&conn).unwrap();
    }

    // ── Cache CRUD ─────────────────────────────────────────────────────────

    #[test]
    fn cache_miss_returns_none() {
        let conn = mem_db();
        let result = lookup_cache(&conn, 59.91, 10.75).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn cache_store_and_lookup_roundtrip() {
        let conn = mem_db();
        let result = GeocodeResult {
            city: Some("Oslo".into()),
            state: Some("Oslo".into()),
            country: Some("Norway".into()),
            country_code: Some("NO".into()),
        };
        store_cache(&conn, 59.91, 10.75, &result).unwrap();
        let got = lookup_cache(&conn, 59.91, 10.75).unwrap().unwrap();
        assert_eq!(got, result);
    }

    #[test]
    fn cache_null_fields_roundtrip() {
        let conn = mem_db();
        let result = GeocodeResult {
            city: None,
            state: None,
            country: Some("Antarctica".into()),
            country_code: None,
        };
        store_cache(&conn, -90.0, 0.0, &result).unwrap();
        let got = lookup_cache(&conn, -90.0, 0.0).unwrap().unwrap();
        assert_eq!(got, result);
    }

    #[test]
    fn cache_replace_on_same_cell() {
        let conn = mem_db();
        let r1 = GeocodeResult {
            city: Some("Old".into()),
            state: None,
            country: None,
            country_code: None,
        };
        let r2 = GeocodeResult {
            city: Some("New".into()),
            state: None,
            country: None,
            country_code: None,
        };
        store_cache(&conn, 59.91, 10.75, &r1).unwrap();
        store_cache(&conn, 59.91, 10.75, &r2).unwrap();
        let got = lookup_cache(&conn, 59.91, 10.75).unwrap().unwrap();
        assert_eq!(got.city.as_deref(), Some("New"));
    }

    #[test]
    fn different_cells_stored_independently() {
        let conn = mem_db();
        let r1 = GeocodeResult {
            city: Some("Oslo".into()),
            state: None,
            country: None,
            country_code: None,
        };
        let r2 = GeocodeResult {
            city: Some("Bergen".into()),
            state: None,
            country: None,
            country_code: None,
        };
        store_cache(&conn, 59.91, 10.75, &r1).unwrap();
        store_cache(&conn, 60.39, 5.33, &r2).unwrap();
        assert_eq!(
            lookup_cache(&conn, 59.91, 10.75)
                .unwrap()
                .unwrap()
                .city
                .as_deref(),
            Some("Oslo")
        );
        assert_eq!(
            lookup_cache(&conn, 60.39, 5.33)
                .unwrap()
                .unwrap()
                .city
                .as_deref(),
            Some("Bergen")
        );
    }

    // ── reverse_geocode_ll against a mock server ───────────────────────────

    /// Minimal HTTP mock: spawn a tiny tokio listener that always returns the given JSON body.
    async fn run_mock_server(response_body: &'static str) -> (tokio::task::JoinHandle<()>, u16) {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            // Accept one connection, send the response, then exit.
            if let Ok((mut stream, _)) = listener.accept().await {
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        (handle, port)
    }

    #[tokio::test]
    async fn reverse_geocode_ll_parses_city_state_country() {
        let body = r#"{
            "address": {
                "city": "Oslo",
                "state": "Oslo",
                "country": "Norway",
                "country_code": "no"
            }
        }"#;
        let (handle, port) = run_mock_server(body).await;
        let endpoint = format!("http://127.0.0.1:{port}");

        let conn = mem_db();
        let result = reverse_geocode_ll(&conn, &endpoint, 59.9139, 10.7522)
            .await
            .unwrap();

        assert_eq!(result.city.as_deref(), Some("Oslo"));
        assert_eq!(result.state.as_deref(), Some("Oslo"));
        assert_eq!(result.country.as_deref(), Some("Norway"));
        // country_code is uppercased by our code.
        assert_eq!(result.country_code.as_deref(), Some("NO"));

        handle.abort();
    }

    #[tokio::test]
    async fn reverse_geocode_ll_falls_back_to_town() {
        // No "city" field — should fall back to "town".
        let body = r#"{
            "address": {
                "town": "Tønsberg",
                "state": "Vestfold",
                "country": "Norway",
                "country_code": "no"
            }
        }"#;
        let (handle, port) = run_mock_server(body).await;
        let endpoint = format!("http://127.0.0.1:{port}");

        let conn = mem_db();
        let result = reverse_geocode_ll(&conn, &endpoint, 59.2721, 10.4076)
            .await
            .unwrap();

        assert_eq!(result.city.as_deref(), Some("Tønsberg"));

        handle.abort();
    }

    #[tokio::test]
    async fn reverse_geocode_ll_caches_result() {
        let body = r#"{
            "address": {
                "city": "Oslo",
                "country": "Norway",
                "country_code": "no"
            }
        }"#;
        let (handle, port) = run_mock_server(body).await;
        let endpoint = format!("http://127.0.0.1:{port}");

        let conn = mem_db();
        let lat = 59.9139;
        let lng = 10.7522;

        let r1 = reverse_geocode_ll(&conn, &endpoint, lat, lng)
            .await
            .unwrap();

        // The mock server only handles one request; a second call must hit the cache.
        // If it tried the network again, the connection would fail (server is gone).
        handle.abort();
        // Brief pause to ensure the server is really gone.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let r2 = reverse_geocode_ll(&conn, &endpoint, lat, lng)
            .await
            .unwrap();

        assert_eq!(r1, r2);
        assert_eq!(r2.city.as_deref(), Some("Oslo"));
    }

    #[tokio::test]
    async fn reverse_geocode_ll_nearby_shares_cached_cell() {
        // Two coords that round to the same 0.01° cell should share one cache entry.
        // 59.9101 and 59.9149 both round to 59.91; 10.7501 and 10.7549 both round to 10.75.
        let body = r#"{
            "address": {
                "city": "Oslo",
                "country": "Norway",
                "country_code": "no"
            }
        }"#;
        let (handle, port) = run_mock_server(body).await;
        let endpoint = format!("http://127.0.0.1:{port}");

        let conn = mem_db();
        // First coord — populates the cache for cell (59.91, 10.75).
        let _r1 = reverse_geocode_ll(&conn, &endpoint, 59.9101, 10.7501)
            .await
            .unwrap();

        // Kill the mock — second coord must use the cache (same cell).
        handle.abort();
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Second coord also rounds to (59.91, 10.75) — should be a cache hit.
        let r2 = reverse_geocode_ll(&conn, &endpoint, 59.9149, 10.7549)
            .await
            .unwrap();

        assert_eq!(r2.city.as_deref(), Some("Oslo"));
    }

    #[tokio::test]
    async fn reverse_geocode_ll_missing_address_returns_empty_fields() {
        // Nominatim sometimes returns no address object for open-sea locations.
        let body = r#"{}"#;
        let (handle, port) = run_mock_server(body).await;
        let endpoint = format!("http://127.0.0.1:{port}");

        let conn = mem_db();
        let result = reverse_geocode_ll(&conn, &endpoint, 0.0, 0.0)
            .await
            .unwrap();

        assert!(result.city.is_none());
        assert!(result.state.is_none());
        assert!(result.country.is_none());
        assert!(result.country_code.is_none());

        handle.abort();
    }

    /// A loopback "Nominatim" answering every request with `body`; returns its endpoint.
    async fn serve_forever(body: &'static str) -> (tokio::task::JoinHandle<()>, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        (handle, endpoint)
    }

    /// Records `geocode:progress` payloads.
    #[derive(Default)]
    struct Progress(std::sync::Mutex<Vec<(usize, usize, usize)>>);

    impl crate::app::EventSink for Progress {
        fn send(&self, event: crate::app::CoreEvent) {
            if let crate::app::CoreEvent::GeocodeProgress(p) = event {
                self.0.lock().unwrap().push((p.done, p.total, p.filled));
            }
        }
    }

    /// A catalog with one photo file at `lat, lng` and the geocoder pointed at `endpoint`.
    fn geo_catalog(tag: &str, endpoint: &str) -> (crate::test_support::TestTmpDir, crate::app::AppState, i64, std::sync::Arc<Progress>) {
        let dir = crate::test_support::TestTmpDir::new(tag);
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("IMG_1.jpg");
        std::fs::write(&file, b"not really a jpeg").unwrap();
        let catalog = crate::catalog::Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        let id = catalog.upsert_photo(&file, None, 0, 1).unwrap().id;
        catalog.conn().execute("UPDATE photos SET gps_latitude = 59.91, gps_longitude = 10.75 WHERE id = ?1", [id]).unwrap();
        catalog.set_setting(SETTING_ENDPOINT, endpoint).unwrap();
        let state = crate::app::AppState::default();
        let progress = std::sync::Arc::new(Progress::default());
        state.set_events(progress.clone());
        *state.catalog.lock().unwrap() = Some(catalog);
        (dir, state, id, progress)
    }

    /// The core entry points the Tauri commands and the GPUI module share: fill the empty
    /// location fields, report progress per photo, and never overwrite a value.
    #[tokio::test]
    async fn geocode_all_fills_empty_fields_reports_progress_and_keeps_values() {
        let (server, endpoint) =
            serve_forever(r#"{"address":{"city":"Oslo","state":"Oslo","country":"Norway","country_code":"no"}}"#).await;
        let (_dir, state, id, progress) = geo_catalog("geo-all", &endpoint);
        {
            let guard = state.catalog.lock().unwrap();
            let c = guard.as_ref().unwrap();
            let mut iptc = c.get_iptc(id).unwrap();
            iptc.country = "Noreg".into(); // the user's value
            c.set_iptc(id, &iptc).unwrap();
        }
        let summary = geocode_all_to_iptc(&state).await.unwrap();
        assert_eq!(summary, GeocodeAllSummary { total: 1, filled: 1, skipped: 0 });
        assert_eq!(*progress.0.lock().unwrap(), vec![(1, 1, 1)]);
        let iptc = state.catalog.lock().unwrap().as_ref().unwrap().get_iptc(id).unwrap();
        assert_eq!((iptc.city.as_str(), iptc.country.as_str(), iptc.country_code.as_str()), ("Oslo", "Noreg", "NO"));
        // Every field set now: the single-photo path has nothing to do.
        assert!(!geocode_photo_to_iptc(&state, id).await.unwrap());
        server.abort();
    }

    /// Nominatim blocks generic or misleading clients: every request names ChairPhoto and
    /// its real repository (the UA used to point at a repository that does not exist).
    #[tokio::test]
    async fn nominatim_requests_carry_the_chairphoto_user_agent() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            let body = r#"{"address":{}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request).to_lowercase()
        });
        nominatim_reverse(&format!("http://127.0.0.1:{port}"), 1.0, 2.0).await.unwrap();
        let request = server.await.unwrap();
        let ua = format!("user-agent: {}", USER_AGENT.to_lowercase());
        assert!(request.contains(&ua), "request without the ChairPhoto UA:\n{request}");
        assert!(USER_AGENT.contains("https://github.com/chairman2s/ChairPhoto"), "{USER_AGENT}");
    }
}
