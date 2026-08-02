//! Manual albums: user-curated, ordered collections of photos (sidebar axis 3 in
//! docs/storage-and-import.md). Distinct from tags (content) and import batches
//! (per-ingest). Membership is explicit; deleting an album never deletes photos.
//!
//! Album *viewing* composes with the culling filter via `list_photos`' `album_id`
//! parameter; this module owns album identity and membership.

use super::{Album, Catalog, Result};
use rusqlite::params;
use std::time::{SystemTime, UNIX_EPOCH};

impl Catalog {
    /// Create an album (placed at the end) and return its id.
    pub fn create_album(&self, name: &str) -> Result<i64> {
        let position: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM albums",
            [],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO albums(uuid, name, position, created_at) VALUES(?1, ?2, ?3, ?4)",
            params![
                uuid::Uuid::new_v4().to_string(),
                name.trim(),
                position,
                now()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn rename_album(&self, album_id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE albums SET name = ?1 WHERE id = ?2",
            params![name.trim(), album_id],
        )?;
        Ok(())
    }

    pub fn set_album_note(&self, album_id: i64, note: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE albums SET note = ?1 WHERE id = ?2",
            params![note, album_id],
        )?;
        Ok(())
    }

    /// Delete an album. Membership cascades; photos are untouched.
    pub fn delete_album(&self, album_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM albums WHERE id = ?1", params![album_id])?;
        Ok(())
    }

    /// All albums, ordered, each with its count of non-missing photos.
    pub fn list_albums(&self) -> Result<Vec<Album>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.id, a.uuid, a.name, a.note,
                    (SELECT COUNT(*) FROM album_photos ap
                     JOIN photos p ON p.id = ap.photo_id
                     WHERE ap.album_id = a.id AND p.missing = 0) AS photo_count
             FROM albums a
             ORDER BY a.position, a.name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Album {
                id: r.get(0)?,
                uuid: r.get(1)?,
                name: r.get(2)?,
                note: r.get(3)?,
                photo_count: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Add photos to an album (idempotent), each appended at the end. Works with the
    /// app's multi-selection.
    pub fn add_photos_to_album(&self, album_id: i64, photo_ids: &[i64]) -> Result<()> {
        let mut position: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM album_photos WHERE album_id = ?1",
            params![album_id],
            |r| r.get(0),
        )?;
        for &photo_id in photo_ids {
            let changed = self.conn.execute(
                "INSERT INTO album_photos(album_id, photo_id, position, created_at)
                 VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(album_id, photo_id) DO NOTHING",
                params![album_id, photo_id, position, now()],
            )?;
            // Only advance the position counter when a row was actually inserted, so
            // re-adding existing members doesn't leave gaps.
            if changed > 0 {
                position += 1;
            }
        }
        Ok(())
    }

    pub fn remove_photos_from_album(&self, album_id: i64, photo_ids: &[i64]) -> Result<()> {
        for &photo_id in photo_ids {
            self.conn.execute(
                "DELETE FROM album_photos WHERE album_id = ?1 AND photo_id = ?2",
                params![album_id, photo_id],
            )?;
        }
        Ok(())
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
