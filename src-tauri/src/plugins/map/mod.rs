//! Map & geo-tagging backend — the map plugin's Rust side. Compiled only with the
//! `map` Cargo feature (so a build without it carries no map code). See
//! docs/map-and-geotagging.md.
//!
//! Sub-modules:
//!
//! - [`geocode`] — Nominatim reverse-geocode client + `map__geocode_cache` table.
//!
//! This module owns:
//!
//! - `map__fences` — a plugin-owned table (prefix convention, ensure-schema like
//!   `ai__suggestions`) holding freeform polygon fences, each bound to a hierarchical
//!   place `tag_path`.
//! - CRUD over that table returning [`Fence`] models.
//! - [`point_in_polygon`] — a ray-casting point-in-polygon test used to decide whether a
//!   photo's GPS coordinate falls inside a fence.
//! - [`apply_fence`] / [`apply_all_fences`] — the backfill engine: scan `photos` for GPS
//!   coordinates, test each against every fence, and apply place tags via `create_tag` +
//!   `assign_tag` (normal editable assignments — never auto-removed).
//! - [`map_photo_points`] — a lightweight query that returns `(id, lat, lng)` for all
//!   photos with GPS data, used by the frontend map view to render clustered markers.
//! - [`apply_fences_to_photo`] — a targeted variant used on import: applies every fence
//!   to a single photo (newly-created only, so existing assignments are never redone
//!   involuntarily).

pub mod geocode;

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

/// A geographic point as `(lat, lng)` — degrees, WGS84. Fence polygons and the photo GPS
/// columns use this ordering. Kept as a bare tuple in the JSON polygon so a fence is just
/// `[[lat, lng], …]` on disk (matches the design doc).
pub type LatLng = (f64, f64);

/// A drawn geofence: a freeform polygon bound to a hierarchical place tag. Photos whose
/// GPS falls inside the polygon get `tag_path` as a normal (editable) tag assignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fence {
    pub id: i64,
    /// Display name (e.g. "Brygga").
    pub name: String,
    /// Hierarchical place tag path applied to matching photos
    /// (e.g. `Places/Vestfold/Tønsberg/Brygga`).
    pub tag_path: String,
    /// Polygon vertices as `[lat, lng]` pairs, in order. Not implicitly closed — the
    /// point-in-polygon test wraps the last vertex to the first.
    pub polygon: Vec<LatLng>,
    /// Unix timestamp (seconds) of creation.
    pub created_at: i64,
}

/// Create the plugin-owned table if needed (prefix convention; not in core schema).
/// Called on catalog open, like the ai plugin's `ensure_schema`.
pub fn ensure_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS map__fences (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            name       TEXT NOT NULL,
            tag_path   TEXT NOT NULL,
            polygon    TEXT NOT NULL,
            created_at INTEGER NOT NULL
        );",
    )
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Serialize a polygon to the on-disk JSON form `[[lat, lng], …]`.
fn polygon_to_json(polygon: &[LatLng]) -> String {
    serde_json::to_string(polygon).unwrap_or_else(|_| "[]".into())
}

/// Parse a stored polygon back to `[(lat, lng), …]`; a corrupt value yields an empty
/// polygon (never inside any fence) rather than failing the whole query.
fn polygon_from_json(json: &str) -> Vec<LatLng> {
    serde_json::from_str::<Vec<LatLng>>(json).unwrap_or_default()
}

fn row_to_fence(row: &rusqlite::Row) -> rusqlite::Result<Fence> {
    let polygon_json: String = row.get(3)?;
    Ok(Fence {
        id: row.get(0)?,
        name: row.get(1)?,
        tag_path: row.get(2)?,
        polygon: polygon_from_json(&polygon_json),
        created_at: row.get(4)?,
    })
}

/// Insert a new fence and return the stored row (with its assigned `id` and `created_at`).
pub fn create_fence(
    conn: &rusqlite::Connection,
    name: &str,
    tag_path: &str,
    polygon: &[LatLng],
) -> rusqlite::Result<Fence> {
    let created_at = now();
    conn.execute(
        "INSERT INTO map__fences(name, tag_path, polygon, created_at) VALUES(?1, ?2, ?3, ?4)",
        rusqlite::params![name, tag_path, polygon_to_json(polygon), created_at],
    )?;
    let id = conn.last_insert_rowid();
    Ok(Fence {
        id,
        name: name.to_string(),
        tag_path: tag_path.to_string(),
        polygon: polygon.to_vec(),
        created_at,
    })
}

/// All fences, newest first.
pub fn list_fences(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<Fence>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, tag_path, polygon, created_at FROM map__fences ORDER BY created_at DESC, id DESC",
    )?;
    let rows = stmt.query_map([], row_to_fence)?;
    rows.collect()
}

/// A single fence by id, or `None` if it doesn't exist.
pub fn get_fence(conn: &rusqlite::Connection, id: i64) -> rusqlite::Result<Option<Fence>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT id, name, tag_path, polygon, created_at FROM map__fences WHERE id = ?1",
        [id],
        row_to_fence,
    )
    .optional()
}

/// Update a fence's name, tag binding and polygon in place. `created_at` is left as-is.
/// Returns the number of rows changed (0 = no such fence).
pub fn update_fence(
    conn: &rusqlite::Connection,
    id: i64,
    name: &str,
    tag_path: &str,
    polygon: &[LatLng],
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE map__fences SET name = ?2, tag_path = ?3, polygon = ?4 WHERE id = ?1",
        rusqlite::params![id, name, tag_path, polygon_to_json(polygon)],
    )
}

/// Delete a fence. Returns the number of rows removed (0 = no such fence). Existing tag
/// assignments are *not* touched — geofence place tags are owned by the user once seeded
/// (see docs/map-and-geotagging.md).
pub fn delete_fence(conn: &rusqlite::Connection, id: i64) -> rusqlite::Result<usize> {
    conn.execute("DELETE FROM map__fences WHERE id = ?1", [id])
}

// ── Apply engine ─────────────────────────────────────────────────────────────

/// A `(photo_id, lat, lng)` row returned by [`map_photo_points`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PhotoPoint {
    pub id: i64,
    pub lat: f64,
    pub lng: f64,
}

/// Lightweight GPS-point query for the map view: returns one row per photo that has
/// both `gps_latitude` and `gps_longitude`, with no missing-file filter. The frontend
/// renders these as clustered markers; clicking one calls `api.selectPhoto(id)`.
pub fn map_photo_points(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<Vec<PhotoPoint>> {
    let mut stmt = conn.prepare(
        "SELECT id, gps_latitude, gps_longitude FROM photos
         WHERE gps_latitude IS NOT NULL AND gps_longitude IS NOT NULL
           AND missing = 0",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PhotoPoint {
            id: r.get(0)?,
            lat: r.get(1)?,
            lng: r.get(2)?,
        })
    })?;
    rows.collect()
}

/// Apply one fence to all photos in the catalog that fall inside it, assigning the
/// fence's `tag_path` as a normal editable tag. Returns the number of photos newly
/// tagged (photos already tagged, or outside the fence, are not counted). The tag
/// hierarchy is built via the catalog's `create_tag` (idempotent).
///
/// A `CatalogRef` is passed so we can reuse the `create_tag` and `assign_tag` methods
/// which live on `Catalog` (not on a bare `rusqlite::Connection`).
pub fn apply_fence(
    catalog: &crate::catalog::Catalog,
    fence_id: i64,
) -> crate::catalog::Result<usize> {
    let fence = get_fence(catalog.conn(), fence_id)?
        .ok_or_else(|| crate::catalog::CatalogError::NotFound(format!("fence {fence_id}")))?;

    if fence.polygon.len() < 3 {
        // Degenerate fence — no area, matches nothing.
        return Ok(0);
    }

    // Build (or reuse) the place tag for this fence's path.
    let tag_id = catalog.create_tag(&fence.tag_path)?;

    // Load all photos with GPS coordinates.
    let points = map_photo_points(catalog.conn())?;

    let mut matched = 0usize;
    for pt in points {
        if point_in_polygon((pt.lat, pt.lng), &fence.polygon) {
            // assign_tag is INSERT OR IGNORE, so re-applying is safe.
            // We track whether the assignment is new by checking before/after row count.
            let before: i64 = catalog.conn().query_row(
                "SELECT COUNT(*) FROM photo_tags WHERE photo_id = ?1 AND tag_id = ?2",
                rusqlite::params![pt.id, tag_id],
                |r| r.get(0),
            )?;
            catalog.assign_tag(pt.id, tag_id)?;
            let after: i64 = catalog.conn().query_row(
                "SELECT COUNT(*) FROM photo_tags WHERE photo_id = ?1 AND tag_id = ?2",
                rusqlite::params![pt.id, tag_id],
                |r| r.get(0),
            )?;
            if after > before {
                matched += 1;
            }
        }
    }
    Ok(matched)
}

/// Apply **all** fences to all photos. Returns the total number of new assignments
/// created across every fence. Each fence is applied independently (so a photo inside
/// two overlapping fences gets both place tags).
pub fn apply_all_fences(
    catalog: &crate::catalog::Catalog,
) -> crate::catalog::Result<usize> {
    let fences = list_fences(catalog.conn())?;
    let mut total = 0;
    for fence in fences {
        total += apply_fence(catalog, fence.id)?;
    }
    Ok(total)
}

/// Apply every fence to a **single photo** (used on import for newly-created photos
/// only). Returns the number of fence tags assigned to this photo.
///
/// This is the import-time hook: it is called after EXIF/GPS metadata has been stored
/// so `photos.gps_latitude/gps_longitude` are available.
pub fn apply_fences_to_photo(
    catalog: &crate::catalog::Catalog,
    photo_id: i64,
) -> crate::catalog::Result<usize> {
    // Read this photo's GPS columns.
    let row: Option<(f64, f64)> = catalog.conn().query_row(
        "SELECT gps_latitude, gps_longitude FROM photos
         WHERE id = ?1 AND gps_latitude IS NOT NULL AND gps_longitude IS NOT NULL",
        rusqlite::params![photo_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).optional()?;

    let Some((lat, lng)) = row else {
        return Ok(0); // no GPS → nothing to do
    };

    let fences = list_fences(catalog.conn())?;
    let mut count = 0usize;
    for fence in fences {
        if fence.polygon.len() < 3 {
            continue;
        }
        if point_in_polygon((lat, lng), &fence.polygon) {
            let tag_id = catalog.create_tag(&fence.tag_path)?;
            let before: i64 = catalog.conn().query_row(
                "SELECT COUNT(*) FROM photo_tags WHERE photo_id = ?1 AND tag_id = ?2",
                rusqlite::params![photo_id, tag_id],
                |r| r.get(0),
            )?;
            catalog.assign_tag(photo_id, tag_id)?;
            let after: i64 = catalog.conn().query_row(
                "SELECT COUNT(*) FROM photo_tags WHERE photo_id = ?1 AND tag_id = ?2",
                rusqlite::params![photo_id, tag_id],
                |r| r.get(0),
            )?;
            if after > before {
                count += 1;
            }
        }
    }
    Ok(count)
}

// ── Catalog-level wrappers ────────────────────────────────────────────────────
//
// These thin wrappers expose the connection-level CRUD functions through a `Catalog`
// reference so integration tests (which cannot access the `pub(crate) conn()` method)
// can call them without reaching into catalog internals.

/// Ensure the `map__fences` table exists. Equivalent to `ensure_schema(catalog.conn())`.
pub fn ensure_schema_for(catalog: &crate::catalog::Catalog) -> rusqlite::Result<()> {
    ensure_schema(catalog.conn())
}

/// Create a fence via a `Catalog` reference (test / command helper).
pub fn create_fence_for(
    catalog: &crate::catalog::Catalog,
    name: &str,
    tag_path: &str,
    polygon: &[LatLng],
) -> rusqlite::Result<Fence> {
    create_fence(catalog.conn(), name, tag_path, polygon)
}

/// List all fences via a `Catalog` reference.
pub fn list_fences_for(catalog: &crate::catalog::Catalog) -> rusqlite::Result<Vec<Fence>> {
    list_fences(catalog.conn())
}

/// Return GPS photo points via a `Catalog` reference.
pub fn map_photo_points_for(
    catalog: &crate::catalog::Catalog,
) -> rusqlite::Result<Vec<PhotoPoint>> {
    map_photo_points(catalog.conn())
}

/// Set the GPS position of one or more photos:
///
/// 1. Update `gps_latitude` / `gps_longitude` in the `photos` table for each id.
/// 2. Write the coordinates merge-safely into each photo's XMP sidecar
///    (`exif:GPSLatitude` / `exif:GPSLongitude`) using [`crate::xmp::write_gps`].
///    The sidecar write uses the resolver — never a bare path from `photos.path`.
/// 3. Re-apply all fences to each moved photo so geofence place tags follow the
///    new position.
///
/// Returns the number of new fence-tag assignments created across all photos.
/// XMP sidecar write failures are logged as warnings but do not abort the whole
/// operation (the catalog update is the authoritative record).
pub fn set_photo_gps(
    catalog: &crate::catalog::Catalog,
    photo_ids: &[i64],
    lat: f64,
    lng: f64,
) -> crate::catalog::Result<usize> {
    let mut total_fence_assignments = 0usize;

    for &photo_id in photo_ids {
        // 1. Update catalog GPS columns.
        catalog.conn().execute(
            "UPDATE photos SET gps_latitude = ?1, gps_longitude = ?2, updated_at = ?3 WHERE id = ?4",
            rusqlite::params![lat, lng, now(), photo_id],
        )?;

        // 2. Merge-safe XMP sidecar write — resolve the path via the resolver, never
        //    from photos.path directly (AGENTS.md invariant).
        match catalog.resolve_photo_path(photo_id) {
            Ok(Some(photo_path)) => {
                if let Err(e) = crate::xmp::write_gps(&photo_path, lat, lng) {
                    // Sidecar write failure is non-fatal: log it and continue.
                    eprintln!(
                        "set_photo_gps: sidecar write failed for photo {photo_id}: {e}"
                    );
                }
            }
            Ok(None) => {
                // Photo file is offline — skip the sidecar write. The catalog update
                // is the authoritative record; the sidecar can be re-synced later.
            }
            Err(e) => {
                eprintln!(
                    "set_photo_gps: could not resolve path for photo {photo_id}: {e}"
                );
            }
        }

        // 3. Re-apply all fences so place tags follow the new position.
        total_fence_assignments += apply_fences_to_photo(catalog, photo_id)?;
    }

    Ok(total_fence_assignments)
}

/// Ray-casting point-in-polygon test.
///
/// Returns `true` when `point` `(lat, lng)` lies inside `polygon` (a list of `(lat, lng)`
/// vertices, treated as implicitly closed — the last vertex connects back to the first).
///
/// The classic even-odd rule: cast a horizontal ray from the point and count edge
/// crossings; an odd count means inside. To make the result stable on shared boundaries,
/// a point that lies exactly *on* an edge or vertex is reported as inside.
///
/// Degenerate polygons (fewer than 3 vertices) enclose no area and always return `false`
/// — except a boundary hit, which still counts as "on" the shape.
///
/// Coordinates are treated as planar; over the small extents of a hand-drawn geofence the
/// error from ignoring the earth's curvature is negligible.
pub fn point_in_polygon(point: LatLng, polygon: &[LatLng]) -> bool {
    let (py, px) = point; // (lat, lng)
    let n = polygon.len();

    // A point sitting on the boundary is always "inside", even for degenerate polygons.
    for i in 0..n {
        let a = polygon[i];
        let b = polygon[(i + 1) % n];
        if point_on_segment(point, a, b) {
            return true;
        }
    }

    if n < 3 {
        return false;
    }

    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (yi, xi) = polygon[i];
        let (yj, xj) = polygon[j];
        // Does the horizontal ray at `py` cross edge (j -> i)?
        let crosses = (yi > py) != (yj > py);
        if crosses {
            // x-coordinate where the edge meets the ray's latitude.
            let x_at = (xj - xi) * (py - yi) / (yj - yi) + xi;
            if px < x_at {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

/// True when `point` lies on the segment `a`–`b` (within a tiny epsilon), including its
/// endpoints. Handles the degenerate segment (`a == b`, i.e. a single point) too.
fn point_on_segment(point: LatLng, a: LatLng, b: LatLng) -> bool {
    const EPS: f64 = 1e-12;
    let (py, px) = point;
    let (ay, ax) = a;
    let (by, bx) = b;

    // Cross product magnitude: zero (collinear) when the point is on the infinite line.
    let cross = (px - ax) * (by - ay) - (py - ay) * (bx - ax);
    if cross.abs() > EPS {
        return false;
    }
    // Collinear — check it falls within the segment's bounding box.
    px >= ax.min(bx) - EPS
        && px <= ax.max(bx) + EPS
        && py >= ay.min(by) - EPS
        && py <= ay.max(by) + EPS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit square with corners (0,0)-(1,1) in (lat, lng).
    fn unit_square() -> Vec<LatLng> {
        vec![(0.0, 0.0), (0.0, 1.0), (1.0, 1.0), (1.0, 0.0)]
    }

    #[test]
    fn interior_point_is_inside() {
        assert!(point_in_polygon((0.5, 0.5), &unit_square()));
    }

    #[test]
    fn far_exterior_point_is_outside() {
        assert!(!point_in_polygon((5.0, 5.0), &unit_square()));
        assert!(!point_in_polygon((-1.0, 0.5), &unit_square()));
        assert!(!point_in_polygon((0.5, 2.0), &unit_square()));
    }

    #[test]
    fn vertices_count_as_inside() {
        let sq = unit_square();
        for &v in &sq {
            assert!(point_in_polygon(v, &sq), "vertex {v:?} should be inside");
        }
    }

    #[test]
    fn edge_points_count_as_inside() {
        let sq = unit_square();
        // Midpoints of each edge.
        assert!(point_in_polygon((0.0, 0.5), &sq)); // bottom edge
        assert!(point_in_polygon((0.5, 1.0), &sq)); // right edge
        assert!(point_in_polygon((1.0, 0.5), &sq)); // top edge
        assert!(point_in_polygon((0.5, 0.0), &sq)); // left edge
    }

    #[test]
    fn just_outside_edge_is_outside() {
        let sq = unit_square();
        assert!(!point_in_polygon((0.5, -0.0001), &sq));
        assert!(!point_in_polygon((0.5, 1.0001), &sq));
    }

    #[test]
    fn concave_polygon_notch_excludes_and_includes_correctly() {
        // A "C"/arrow-ish concave shape: a square with a rectangular notch cut into the
        // right side.
        //   (0,0) - (0,2) - (1,2) - (1,1) - (2,1) - (2,2) - (3,2) - (3,0)
        let poly = vec![
            (0.0, 0.0),
            (0.0, 2.0),
            (1.0, 2.0),
            (1.0, 1.0),
            (2.0, 1.0),
            (2.0, 2.0),
            (3.0, 2.0),
            (3.0, 0.0),
        ];
        // Deep in the solid left/bottom region: inside.
        assert!(point_in_polygon((0.5, 0.5), &poly));
        assert!(point_in_polygon((2.5, 0.5), &poly));
        // In the notch (lat between 1 and 2, lng between 1 and 2): outside.
        assert!(!point_in_polygon((1.5, 1.5), &poly));
        // On the far side of the notch but still in solid material (lng < 1): inside.
        assert!(point_in_polygon((1.5, 0.5), &poly));
    }

    #[test]
    fn degenerate_polygons_enclose_no_area() {
        // Empty polygon.
        assert!(!point_in_polygon((0.0, 0.0), &[]));
        // Single point: only that exact point is "on" it.
        let single = vec![(1.0, 1.0)];
        assert!(point_in_polygon((1.0, 1.0), &single));
        assert!(!point_in_polygon((1.0, 1.5), &single));
        // Two points (a line segment): only points on the segment count.
        let seg = vec![(0.0, 0.0), (0.0, 2.0)];
        assert!(point_in_polygon((0.0, 1.0), &seg)); // on the segment
        assert!(!point_in_polygon((0.5, 1.0), &seg)); // off the segment
        assert!(!point_in_polygon((0.0, 3.0), &seg)); // collinear but past the end
    }

    // --- store CRUD -------------------------------------------------------------

    fn mem_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn ensure_schema_is_idempotent() {
        let conn = mem_db();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
    }

    #[test]
    fn create_get_list_roundtrip() {
        let conn = mem_db();
        let poly = unit_square();
        let f = create_fence(&conn, "Brygga", "Places/Vestfold/Tønsberg/Brygga", &poly).unwrap();
        assert!(f.id > 0);
        assert_eq!(f.name, "Brygga");
        assert_eq!(f.tag_path, "Places/Vestfold/Tønsberg/Brygga");
        assert_eq!(f.polygon, poly);
        assert!(f.created_at > 0);

        let got = get_fence(&conn, f.id).unwrap().unwrap();
        assert_eq!(got.id, f.id);
        assert_eq!(got.name, "Brygga");
        assert_eq!(got.polygon, poly);

        assert!(get_fence(&conn, 9999).unwrap().is_none());

        let all = list_fences(&conn).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, f.id);
    }

    #[test]
    fn update_and_delete() {
        let conn = mem_db();
        let f = create_fence(&conn, "Old", "Places/Old", &unit_square()).unwrap();

        let new_poly = vec![(10.0, 10.0), (10.0, 11.0), (11.0, 11.0)];
        let changed = update_fence(&conn, f.id, "New", "Places/New", &new_poly).unwrap();
        assert_eq!(changed, 1);

        let got = get_fence(&conn, f.id).unwrap().unwrap();
        assert_eq!(got.name, "New");
        assert_eq!(got.tag_path, "Places/New");
        assert_eq!(got.polygon, new_poly);
        assert_eq!(got.created_at, f.created_at); // preserved

        // Updating a missing fence changes nothing.
        assert_eq!(update_fence(&conn, 9999, "x", "y", &new_poly).unwrap(), 0);

        assert_eq!(delete_fence(&conn, f.id).unwrap(), 1);
        assert!(get_fence(&conn, f.id).unwrap().is_none());
        assert_eq!(delete_fence(&conn, f.id).unwrap(), 0);
        assert!(list_fences(&conn).unwrap().is_empty());
    }

    #[test]
    fn corrupt_polygon_json_yields_empty_polygon() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO map__fences(name, tag_path, polygon, created_at) VALUES(?1, ?2, ?3, ?4)",
            rusqlite::params!["bad", "Places/Bad", "{not valid json", 123],
        )
        .unwrap();
        let all = list_fences(&conn).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].polygon.is_empty());
    }
}
