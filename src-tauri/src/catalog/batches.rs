//! Import batches ("negative film roll"): every ingest creates one, auto-created and
//! immutable. A photo belongs to the batch it was first imported in, forever. Batches
//! are their own sidebar axis (distinct from albums and tags) and filterable. See
//! docs/storage-and-import.md.
//!
//! The batch id is also meant to be mirrored to each photo's XMP (like the UUID); that
//! write is deferred together with the scan-time UUID write (scanning does not write to
//! the user's photo folders yet).

use super::{Catalog, ImportBatch, Result};
use rusqlite::{params, OptionalExtension};
use std::time::{SystemTime, UNIX_EPOCH};

impl Catalog {
    /// Create an import batch and return its id. `source_label` is a human hint about
    /// the ingest (e.g. the scanned folder).
    pub fn create_import_batch(&self, source_label: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO import_batches(uuid, source_label, created_at) VALUES(?1, ?2, ?3)",
            params![uuid::Uuid::new_v4().to_string(), source_label.trim(), now()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Assign photos to a batch — only those that don't already belong to one, so a
    /// photo keeps the batch it was *first* imported in (batches are immutable). A
    /// single atomic UPDATE so a crash can't leave a half-assigned batch.
    pub fn assign_photos_to_batch(&self, batch_id: i64, photo_ids: &[i64]) -> Result<()> {
        if photo_ids.is_empty() {
            return Ok(());
        }
        let placeholders = vec!["?"; photo_ids.len()].join(",");
        let sql = format!(
            "UPDATE photos SET import_batch_id = ?
             WHERE import_batch_id IS NULL AND id IN ({placeholders})"
        );
        let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(photo_ids.len() + 1);
        binds.push(&batch_id);
        for id in photo_ids {
            binds.push(id);
        }
        self.conn.execute(&sql, binds.as_slice())?;
        Ok(())
    }

    /// Return the UUID of an import batch by its integer id. Used by the scanner to
    /// pass the batch UUID to the XMP sidecar writer (K3: `chairphoto:ImportBatch`).
    pub fn get_import_batch_uuid(&self, batch_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT uuid FROM import_batches WHERE id = ?1",
                rusqlite::params![batch_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Return the integer id of an import batch by its UUID. Used by the bundle importer
    /// (F1d) to look up the batch id after `merge_bundle` has ensured it exists, so the
    /// newly-upserted photos can be assigned to it via `assign_photos_to_batch`.
    pub fn get_import_batch_id_by_uuid(&self, uuid: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM import_batches WHERE uuid = ?1",
                rusqlite::params![uuid],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_import_batch_note(&self, batch_id: i64, note: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE import_batches SET note = ?1 WHERE id = ?2",
            params![note, batch_id],
        )?;
        Ok(())
    }

    /// All import batches, newest first, each with its count of non-missing photos.
    pub fn list_import_batches(&self) -> Result<Vec<ImportBatch>> {
        let mut stmt = self.conn.prepare(
            "SELECT b.id, b.uuid, b.source_label, b.note, b.created_at,
                    (SELECT COUNT(*) FROM photos p
                     WHERE p.import_batch_id = b.id AND p.missing = 0) AS photo_count
             FROM import_batches b
             ORDER BY b.created_at DESC, b.id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ImportBatch {
                id: r.get(0)?,
                uuid: r.get(1)?,
                source_label: r.get(2)?,
                note: r.get(3)?,
                created_at: r.get(4)?,
                photo_count: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
