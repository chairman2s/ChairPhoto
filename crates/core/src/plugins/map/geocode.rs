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

/// User-Agent sent with every Nominatim request.  Nominatim's policy requires a
/// meaningful, non-default UA that identifies the application and contact details.
const USER_AGENT: &str = "ChairPhoto/0.1 (photo-organizer; https://github.com/chairphoto/chairphoto)";

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
}
