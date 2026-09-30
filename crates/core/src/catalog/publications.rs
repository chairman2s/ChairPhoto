//! Publications — where a photo has been posted and **which version** went out.
//!
//! A publication ties a photo + an optional version (None = the Original) to a
//! `platform` marker (e.g. "instagram"). The marker is supplied by the publishing
//! module; core never invents one and rejects an empty one. Storage is current-state:
//! one row per `(photo, platform)`, so re-posting upserts (`version_name` is snapshotted
//! so a deleted version still reads correctly). See docs/publications.md.

use super::{Catalog, CatalogError, Publication, Result};
use rusqlite::{params, OptionalExtension};
use std::time::{SystemTime, UNIX_EPOCH};

impl Catalog {
    /// Record (or update) that `photo_id` was published to `platform` as `version_id`
    /// (None = Original). A *different* version to the same platform is a new record; the
    /// *same* (photo, platform, version) upserts (the user re-marking, not re-posting).
    /// The `version_name` snapshot is resolved from the version row. Returns the id.
    pub fn record_publication(
        &self,
        photo_id: i64,
        version_id: Option<i64>,
        platform: &str,
        url: Option<&str>,
    ) -> Result<i64> {
        let platform = platform.trim();
        if platform.is_empty() {
            // The publishing module must say what the photo is marked with — core has
            // no default platform of its own.
            return Err(CatalogError::Validation("publication platform is empty".into()));
        }
        // Snapshot the version name (None for the Original, or if the id is unknown).
        let version_name = match version_id {
            Some(vid) => self.get_version(vid)?.map(|v| v.name),
            None => None,
        };
        let now = now();
        // Find an existing record for this exact (photo, platform, version). For the
        // Original we match a *true* Original row (NULL version with no snapshot name), so
        // we never clobber an orphaned record left behind when a published version was
        // deleted (its version_id is NULL but it keeps its snapshot name).
        let existing: Option<i64> = match version_id {
            Some(vid) => self
                .conn
                .query_row(
                    "SELECT id FROM publications
                     WHERE photo_id = ?1 AND platform = ?2 AND version_id = ?3",
                    params![photo_id, platform, vid],
                    |r| r.get(0),
                )
                .optional()?,
            None => self
                .conn
                .query_row(
                    "SELECT id FROM publications
                     WHERE photo_id = ?1 AND platform = ?2
                       AND version_id IS NULL AND version_name IS NULL",
                    params![photo_id, platform],
                    |r| r.get(0),
                )
                .optional()?,
        };
        if let Some(id) = existing {
            self.conn.execute(
                "UPDATE publications SET version_name = ?1, url = ?2, published_at = ?3
                 WHERE id = ?4",
                params![version_name, url, now, id],
            )?;
            Ok(id)
        } else {
            self.conn.execute(
                "INSERT INTO publications
                     (photo_id, version_id, version_name, platform, url, published_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![photo_id, version_id, version_name, platform, url, now],
            )?;
            Ok(self.conn.last_insert_rowid())
        }
    }

    /// All publications for a photo, grouped by platform (newest first within a platform).
    pub fn list_publications(&self, photo_id: i64) -> Result<Vec<Publication>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, photo_id, version_id, version_name, platform, url, published_at
             FROM publications WHERE photo_id = ?1 ORDER BY platform, published_at DESC",
        )?;
        let rows = stmt.query_map(params![photo_id], row_to_publication)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Delete a single publication record (the photo/version are untouched).
    pub fn delete_publication(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM publications WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Upsert a publication with an **explicit** historical `published_at` timestamp and
    /// URL (both required). Used when importing existing publications from an external
    /// platform so that `published_at` reflects the real posting date, not the import
    /// moment. The upsert key is the same as `record_publication` — one row per
    /// `(photo_id, platform, version_id)` — so re-running corrects the URL and date of
    /// any existing row including ones previously written with the wrong timestamp.
    pub fn record_publication_historical(
        &self,
        photo_id: i64,
        platform: &str,
        url: &str,
        published_at: i64,
    ) -> Result<i64> {
        let platform = platform.trim();
        if platform.is_empty() {
            return Err(CatalogError::Validation("publication platform is empty".into()));
        }
        // version_id=NULL, version_name=NULL → the Original (same slot as record_publication
        // with version_id=None). We look for a true Original row so we don't clobber an
        // orphaned published-version row.
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM publications
                 WHERE photo_id = ?1 AND platform = ?2
                   AND version_id IS NULL AND version_name IS NULL",
                params![photo_id, platform],
                |r| r.get(0),
            )
            .optional()?;
        let now = now();
        if let Some(id) = existing {
            self.conn.execute(
                "UPDATE publications SET url = ?1, published_at = ?2 WHERE id = ?3",
                params![url, published_at, id],
            )?;
            Ok(id)
        } else {
            self.conn.execute(
                "INSERT INTO publications
                     (photo_id, version_id, version_name, platform, url, published_at, created_at)
                 VALUES (?1, NULL, NULL, ?2, ?3, ?4, ?5)",
                params![photo_id, platform, url, published_at, now],
            )?;
            Ok(self.conn.last_insert_rowid())
        }
    }

    /// All publication rows for one platform as `(photo_id, url, published_at)` tuples.
    /// Backs the Flickr importer's "already recorded" signal (match by the photo id in the
    /// URL, or by publish-time proximity for URL-less legacy rows).
    pub fn publications_for_platform(
        &self,
        platform: &str,
    ) -> Result<Vec<(i64, Option<String>, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT photo_id, url, published_at FROM publications WHERE platform = ?1",
        )?;
        let rows =
            stmt.query_map(params![platform], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The distinct platforms that have at least one publication, sorted. Drives the
    /// "Published: <platform>" filter facets.
    pub fn published_platforms(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT platform FROM publications ORDER BY platform")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn row_to_publication(r: &rusqlite::Row) -> rusqlite::Result<Publication> {
    Ok(Publication {
        id: r.get(0)?,
        photo_id: r.get(1)?,
        version_id: r.get(2)?,
        version_name: r.get(3)?,
        platform: r.get(4)?,
        url: r.get(5)?,
        published_at: r.get(6)?,
    })
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
