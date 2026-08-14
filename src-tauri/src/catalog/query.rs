//! The library query: one typed value describing "which photos, in what order, and
//! which slice of them", plus the SQL builder that answers it.
//!
//! The grid used to ask for photos through an eleven-argument positional tuple that had
//! to be spelled identically in TypeScript, in the Tauri command, and in
//! `Catalog::list_photos` — three places that could drift silently, because a positional
//! argument list carries no names across the IPC boundary. [`PhotoQuery`] is that tuple
//! given a name and a type on both sides (issue #10): the TypeScript `PhotoQuery` in
//! `src/modules/api.ts` is the same object, field for field.
//!
//! Two things keep the two sides honest rather than merely parallel:
//!
//! - `deny_unknown_fields` — a field TypeScript sends that Rust does not know is a
//!   deserialization *error*, not a silently dropped filter. A dropped filter shows up as
//!   "the grid ignored my selection"; an error names the field.
//! - the string-valued options are enums, not strings, so "sharpnesss_asc" fails at the
//!   boundary with a list of the valid values instead of quietly falling through to the
//!   default sort. `Catalog` can no longer even be handed an unknown filter.
//!
//! The window ([`PhotoWindow`]) is what makes the interface incremental. It is only
//! meaningful because the ordering is **total**: every sort ends in `p.id`, so no two rows
//! compare equal and "rows 500..1000" names the same rows on every call. See
//! [`Catalog::list_photos`].

use super::{smart_albums, Catalog, Photo, Result};
use serde::{Deserialize, Serialize};

/// Which photos the library view wants, in what order, and (optionally) which slice.
///
/// Every field has a default, and the defaults spell "the whole library, oldest first",
/// so a caller states only what it is filtering on. This is the value that crosses the
/// IPC boundary; see the module docs for how it stays in step with TypeScript.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct PhotoQuery {
    /// Restrict to a tag *and its descendants* (the sidebar's tag scope).
    pub tag_id: Option<i64>,
    /// Restrict to an album's members — also switches the sort to the album's own order.
    pub album_id: Option<i64>,
    /// Restrict to one import batch.
    pub batch_id: Option<i64>,
    /// Evaluate a saved smart-album rule live (see `catalog/smart_albums.rs`).
    pub smart_album_id: Option<i64>,
    /// Derived boolean facets, ANDed in (e.g. `has-gps`, `mobile`, `published:flickr`).
    pub facets: Vec<String>,
    /// The culling filter (rating / pick / edited state).
    pub culling_filter: CullingFilter,
    /// Where the bytes live (local copy / NAS-only).
    pub storage_tier: StorageTier,
    /// Exact camera model, from `distinct_photo_values("camera")`.
    pub camera: Option<String>,
    /// Exact lens, from `distinct_photo_values("lens")`.
    pub lens: Option<String>,
    /// Colour labels to keep, OR-combined. `""` is the No-label member. Empty = no filter.
    pub labels: Vec<String>,
    /// Sort order. Ignored when `album_id` is set (an album carries its own order).
    pub sort: PhotoSort,
    /// The slice to return. `None` = every matching row (what the grid asked for before
    /// this existed, and still the right answer for a small catalog or a bulk operation).
    pub window: Option<PhotoWindow>,
}

/// A half-open slice `[offset, offset + limit)` of the ordered result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PhotoWindow {
    /// How many ordered rows to skip.
    pub offset: usize,
    /// How many rows to return at most. `0` returns none (a legal, if useless, window).
    pub limit: usize,
}

impl PhotoWindow {
    pub fn new(offset: usize, limit: usize) -> Self {
        Self { offset, limit }
    }
}

/// One window of a query's result, with the size of the whole matching set.
///
/// `total` is what the grid needs to size its scrollbar and print "N photos" without
/// holding N rows: it is the count of the *matching* set, not of `photos`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhotoPage {
    /// The rows in this window, in query order.
    pub photos: Vec<Photo>,
    /// Ordered position of `photos[0]` in the whole matching set.
    pub offset: usize,
    /// How many rows match the query in total (`>= offset + photos.len()`).
    pub total: usize,
}

/// The culling filter: which review state a photo must be in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CullingFilter {
    /// No culling restriction.
    #[default]
    All,
    /// `rating = 0` — never rated.
    Unrated,
    /// Flagged as a keeper.
    Pick,
    /// Flagged for deletion.
    Reject,
    /// Has an external editor's sidecar (darktable, RawTherapee, …).
    Edited,
}

/// Where a photo's bytes live, by volume kind (mirrors the grid's storage icons).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageTier {
    /// Any tier.
    #[default]
    All,
    /// Has a copy on a local volume.
    Local,
    /// Offloaded: no local copy, but a backup exists.
    Nas,
}

/// Sort order for the library view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhotoSort {
    /// Oldest-first by capture time (falling back to file mtime).
    #[default]
    Date,
    /// Least-sharp first — suspect frames up front for culling. Unscored last.
    SharpnessAsc,
    /// Sharpest first. Unscored last.
    SharpnessDesc,
}

/// The photo columns every `row_to_photo` query selects, in that mapper's order.
/// `{alias}` is how the `photos` table is named in the surrounding statement.
pub(crate) fn photo_columns(alias: &str) -> String {
    format!(
        "{alias}.id, {alias}.uuid, {alias}.path, {alias}.rating, {alias}.color_label,
         {alias}.pick_state, {alias}.capture_time, {alias}.width, {alias}.height,
         {alias}.camera_model, {alias}.lens, {alias}.aperture, {alias}.shutter_speed,
         {alias}.iso, {alias}.external_editors, {alias}.thumbnail_path,
         (SELECT COUNT(*) FROM photos c WHERE c.stack_parent_id = {alias}.id),
         {alias}.stack_parent_id, {alias}.metadata_ready, {alias}.sharpness,
         {alias}.sharpness_method, {alias}.burst_flag,
         (SELECT COUNT(*) FROM photo_versions pv WHERE pv.photo_id = {alias}.id)"
    )
}

/// The FROM/JOIN/WHERE half of a query — everything the row select and the count share.
struct QuerySql {
    /// `photos p` plus whatever joins the filters need.
    from: String,
    /// ANDed predicates.
    wheres: Vec<String>,
    binds: Vec<Box<dyn rusqlite::ToSql>>,
}

impl Catalog {
    /// The photos matching `query`, in the query's order, restricted to its window.
    ///
    /// Ordering is **total**, which is what makes a window mean anything: `ORDER BY`
    /// always ends in `p.id`, the primary key. Without it two rows can compare equal —
    /// `photos.path` is UNIQUE but the sort compares it `COLLATE NOCASE`, so `A.jpg` and
    /// `a.jpg` tie; album `position` and a `NULL` sharpness tie outright — and SQLite is
    /// free to return tied rows in any order, which would let one photo appear in two
    /// windows while another appears in none.
    ///
    /// Stacked derivatives (a camera JPEG under its RAW) and missing photos are excluded;
    /// derivatives are reached through the master's Stack section.
    pub fn list_photos(&self, query: &PhotoQuery) -> Result<Vec<Photo>> {
        let parts = self.build_query(query)?;
        let mut sql = format!(
            "SELECT DISTINCT {cols} FROM {from} WHERE {wheres}{order}",
            cols = photo_columns("p"),
            from = parts.from,
            wheres = parts.wheres.join(" AND "),
            order = order_by(query),
        );
        let mut binds = parts.binds;
        if let Some(window) = query.window {
            sql.push_str(" LIMIT ? OFFSET ?");
            binds.push(Box::new(window.limit as i64));
            binds.push(Box::new(window.offset as i64));
        }
        let params: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params.as_slice(), super::row_to_photo)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many photos match `query`, ignoring its window. One aggregate over the same
    /// filters — it never builds a `Photo`, so it costs a fraction of listing them.
    pub fn count_photos(&self, query: &PhotoQuery) -> Result<usize> {
        let parts = self.build_query(query)?;
        let sql = format!(
            "SELECT COUNT(DISTINCT p.id) FROM {from} WHERE {wheres}",
            from = parts.from,
            wheres = parts.wheres.join(" AND "),
        );
        let params: Vec<&dyn rusqlite::ToSql> = parts.binds.iter().map(|b| b.as_ref()).collect();
        let count: i64 = self.conn.query_row(&sql, params.as_slice(), |r| r.get(0))?;
        Ok(count as usize)
    }

    /// One window plus the size of the whole matching set — what the grid command returns.
    ///
    /// The count is only queried when the window itself cannot answer it. An unwindowed
    /// call already holds every row. A window that came back *short* (fewer rows than it
    /// asked for) has reached the end of the result, so `offset + len` is the total —
    /// unless it came back empty from a non-zero offset, which says nothing about where
    /// the result actually ends (it may lie anywhere at or before `offset`).
    pub fn photo_page(&self, query: &PhotoQuery) -> Result<PhotoPage> {
        let photos = self.list_photos(query)?;
        let (offset, total) = match query.window {
            None => (0, photos.len()),
            Some(window)
                if photos.len() < window.limit && (window.offset == 0 || !photos.is_empty()) =>
            {
                (window.offset, window.offset + photos.len())
            }
            Some(window) => (window.offset, self.count_photos(query)?),
        };
        Ok(PhotoPage {
            photos,
            offset,
            total,
        })
    }

    /// Translate a [`PhotoQuery`]'s filters into FROM/WHERE fragments + bound parameters.
    /// Every value reaching SQL is bound; the only interpolated text is from fixed
    /// allowlists (the enums above and `facet_predicate_owned`).
    fn build_query(&self, query: &PhotoQuery) -> Result<QuerySql> {
        let mut from = String::from("photos p");
        // Stacked derivatives (e.g. a camera JPEG under its RAW) are hidden from the
        // main grid; they're reached via the master's Stack section in the inspector.
        let mut wheres = vec![
            "p.missing = 0".to_string(),
            "p.stack_parent_id IS NULL".to_string(),
        ];
        let mut bind: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(tid) = query.tag_id {
            let ids = self.descendant_tag_ids(tid)?;
            let placeholders = super::sqlite_param_placeholders(ids.len());
            from.push_str(" JOIN photo_tags pt ON pt.photo_id = p.id");
            wheres.push(format!("pt.tag_id IN ({placeholders})"));
            for id in ids {
                bind.push(Box::new(id));
            }
        }

        if let Some(aid) = query.album_id {
            from.push_str(" JOIN album_photos ap ON ap.photo_id = p.id");
            wheres.push("ap.album_id = ?".to_string());
            bind.push(Box::new(aid));
        }

        if let Some(bid) = query.batch_id {
            wheres.push("p.import_batch_id = ?".to_string());
            bind.push(Box::new(bid));
        }

        // Smart album: a saved RULE evaluated LIVE. Load its rule_json, translate it to
        // WHERE fragments (over the same `p` alias) + bound parameters, and AND them in.
        // The translator is the single source of truth for the field/op allowlist; every
        // value it yields is already a bound parameter (see catalog/smart_albums.rs).
        if let Some(sid) = query.smart_album_id {
            let rule_json = self.smart_album_rule_json(sid)?;
            let (rule_wheres, rule_binds) = smart_albums::rule_to_sql(&rule_json)?;
            for w in rule_wheres {
                wheres.push(w);
            }
            for b in rule_binds {
                bind.push(b);
            }
        }

        // Derived facets (internal, never exported): each is a boolean predicate, ANDed
        // in. Most are computed from photos columns (no binds); the dynamic
        // "published:<platform>" facets take the platform as a bound parameter; the `soft`
        // facet uses a dynamic threshold from settings (via facet_predicate_owned).
        for facet in &query.facets {
            if let Some(platform) = facet.strip_prefix("published:") {
                wheres.push(
                    "EXISTS (SELECT 1 FROM publications pub \
                     WHERE pub.photo_id = p.id AND pub.platform = ?)"
                        .to_string(),
                );
                bind.push(Box::new(platform.to_string()));
            } else {
                wheres.push(self.facet_predicate_owned(facet)?);
            }
        }

        match query.culling_filter {
            CullingFilter::All => {}
            CullingFilter::Unrated => wheres.push("p.rating = 0".into()),
            CullingFilter::Pick => wheres.push("p.pick_state = 'pick'".into()),
            CullingFilter::Reject => wheres.push("p.pick_state = 'reject'".into()),
            CullingFilter::Edited => wheres.push("p.external_editors != ''".into()),
        }

        // Storage tier: where the photo's bytes live (by volume kind, matching the grid's
        // status icons). "local" = has a copy on a local volume (recent, on-disk photos);
        // "nas" = NAS-only, i.e. offloaded — no local copy but a backup exists.
        match query.storage_tier {
            StorageTier::All => {}
            StorageTier::Local => wheres.push(
                "EXISTS (SELECT 1 FROM photo_locations l JOIN volumes v ON v.id = l.volume_id \
                 WHERE l.photo_id = p.id AND v.kind = 'local')"
                    .into(),
            ),
            StorageTier::Nas => wheres.push(
                "NOT EXISTS (SELECT 1 FROM photo_locations l JOIN volumes v ON v.id = l.volume_id \
                   WHERE l.photo_id = p.id AND v.kind = 'local') \
                 AND EXISTS (SELECT 1 FROM photo_locations l JOIN volumes v ON v.id = l.volume_id \
                   WHERE l.photo_id = p.id AND v.kind = 'backup')"
                    .into(),
            ),
        }

        // Camera / lens exact-match filters (values come from distinct_photo_values, bound).
        if let Some(cam) = &query.camera {
            wheres.push("p.camera_model = ?".to_string());
            bind.push(Box::new(cam.clone()));
        }
        if let Some(l) = &query.lens {
            wheres.push("p.lens = ?".to_string());
            bind.push(Box::new(l.clone()));
        }

        // Colour labels, OR-combined within the filter (a photo has one label, so a
        // multi-select is a set-membership test). "" is a valid member: the No-label
        // option. NOCASE matches the smart-album text predicate's behaviour.
        if !query.labels.is_empty() {
            let placeholders = super::sqlite_param_placeholders(query.labels.len());
            wheres.push(format!("p.color_label COLLATE NOCASE IN ({placeholders})"));
            for label in &query.labels {
                bind.push(Box::new(label.clone()));
            }
        }

        Ok(QuerySql {
            from,
            wheres,
            binds: bind,
        })
    }
}

/// The ORDER BY clause for a query — always ending in `p.id` so the order is total.
///
/// Albums are explicitly ordered collections: honour their member order when viewing one.
/// Otherwise sort by "date taken", oldest first. Prefer the EXIF capture time; when it's
/// missing (e.g. iPhone HEICs / files stripped of EXIF) fall back to the file's
/// modification time (mtime_ns, always recorded on scan) so those photos slot into the
/// timeline chronologically instead of piling up at the end. capture_time is local-clock
/// ISO ("YYYY-MM-DDTHH:MM:SS"); mtime is true unix seconds — the small tz skew at the
/// EXIF/no-EXIF boundary is acceptable for sort.
fn order_by(query: &PhotoQuery) -> &'static str {
    if query.album_id.is_some() {
        return " ORDER BY ap.position, p.path COLLATE NOCASE, p.id";
    }
    match query.sort {
        // Sharpness sorts: unscored photos (NULL) always sort last so the ordering makes
        // sense in both directions and newly-indexed photos don't jump around.
        PhotoSort::SharpnessAsc => {
            " ORDER BY CASE WHEN p.sharpness IS NULL THEN 1 ELSE 0 END, \
             p.sharpness ASC, p.path COLLATE NOCASE, p.id"
        }
        PhotoSort::SharpnessDesc => {
            " ORDER BY CASE WHEN p.sharpness IS NULL THEN 1 ELSE 0 END, \
             p.sharpness DESC, p.path COLLATE NOCASE, p.id"
        }
        PhotoSort::Date => {
            " ORDER BY COALESCE(CAST(strftime('%s', p.capture_time) AS INTEGER), \
             p.mtime_ns / 1000000000), p.path COLLATE NOCASE, p.id"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn query_defaults_to_the_whole_library_oldest_first() {
        let query: PhotoQuery = serde_json::from_value(json!({})).unwrap();
        assert_eq!(query, PhotoQuery::default());
        assert_eq!(query.culling_filter, CullingFilter::All);
        assert_eq!(query.storage_tier, StorageTier::All);
        assert_eq!(query.sort, PhotoSort::Date);
        assert!(query.window.is_none());
    }

    #[test]
    fn query_deserializes_the_camel_case_the_frontend_sends() {
        let query: PhotoQuery = serde_json::from_value(json!({
            "tagId": 7,
            "smartAlbumId": 3,
            "cullingFilter": "unrated",
            "storageTier": "nas",
            "sort": "sharpness_asc",
            "labels": ["Red", ""],
            "window": { "offset": 500, "limit": 250 },
        }))
        .unwrap();
        assert_eq!(query.tag_id, Some(7));
        assert_eq!(query.smart_album_id, Some(3));
        assert_eq!(query.culling_filter, CullingFilter::Unrated);
        assert_eq!(query.storage_tier, StorageTier::Nas);
        assert_eq!(query.sort, PhotoSort::SharpnessAsc);
        assert_eq!(query.labels, vec!["Red".to_string(), String::new()]);
        assert_eq!(query.window, Some(PhotoWindow::new(500, 250)));
    }

    // The typed boundary is only worth having if it *rejects*. These three are what the
    // old positional string tuple let through: a typo'd sort silently fell back to "date",
    // and an unknown filter/tier only failed once it reached the SQL builder.
    #[test]
    fn a_misspelled_sort_is_rejected_at_the_boundary() {
        let err = serde_json::from_value::<PhotoQuery>(json!({ "sort": "sharpnesss_asc" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("sharpness_asc"), "{err}");
    }

    #[test]
    fn an_unknown_filter_or_tier_is_rejected_at_the_boundary() {
        assert!(serde_json::from_value::<PhotoQuery>(json!({ "cullingFilter": "bogus" })).is_err());
        assert!(serde_json::from_value::<PhotoQuery>(json!({ "storageTier": "cloud" })).is_err());
    }

    /// The drift alarm: a field TypeScript sends that Rust does not know must fail loudly.
    /// Accepted silently, a renamed filter would just stop being applied.
    #[test]
    fn an_unknown_field_is_rejected_rather_than_ignored() {
        let err = serde_json::from_value::<PhotoQuery>(json!({ "colorLabel": "Red" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("colorLabel"), "{err}");
    }

    #[test]
    fn every_sort_ends_in_the_primary_key_so_the_order_is_total() {
        let sorts = [
            PhotoSort::Date,
            PhotoSort::SharpnessAsc,
            PhotoSort::SharpnessDesc,
        ];
        for sort in sorts {
            let clause = order_by(&PhotoQuery {
                sort,
                ..Default::default()
            });
            assert!(clause.trim_end().ends_with("p.id"), "{sort:?}: {clause}");
        }
        let album = order_by(&PhotoQuery {
            album_id: Some(1),
            ..Default::default()
        });
        assert!(album.trim_end().ends_with("p.id"), "{album}");
        assert!(album.contains("ap.position"), "{album}");
    }
}
