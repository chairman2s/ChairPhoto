//! The non-destructive **edit record** — the core's small, editing-agnostic contract
//! for photo editing (see docs/plugin-system.md, "Editing / Develop").
//!
//! Editing itself is modular and divergent, so core stores the edit record as an
//! **opaque JSON document** and never interprets its meaning. Editing modules own a
//! namespaced section of it (e.g. `{"basic-editor": {"exposure": 0.3}}`); core only
//! persists it, serves it, and (in the frontend) exposes a render hook that a module
//! implements to turn `(photo, edit record)` into a rendered preview/export.
//!
//! Storage is a catalog row (not just a sidecar) so derived state stays queryable —
//! e.g. a future B&W edit can drive the monochrome facet / filters. XMP mirroring of
//! edits can ride along later with the (currently deferred) scan-time sidecar writes.

use super::{
    sqlite_param_placeholders, Catalog, CatalogError, PhotoVersion, Result, SQLITE_PARAM_CHUNK,
};
use rusqlite::{params, OptionalExtension};
use std::time::{SystemTime, UNIX_EPOCH};

impl Catalog {
    /// The photo's edit record as a JSON string, or `None` if it has no edits.
    pub fn get_edit_record(&self, photo_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT edit_json FROM photo_edits WHERE photo_id = ?1",
                params![photo_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Set (replace) the photo's edit record. The value must be a valid JSON document
    /// — core validates only that it parses (it does not interpret the contents). An
    /// empty/whitespace value clears the record (removes the row).
    pub fn set_edit_record(&self, photo_id: i64, edit_json: &str) -> Result<()> {
        let trimmed = edit_json.trim();
        if trimmed.is_empty() {
            return self.clear_edit_record(photo_id);
        }
        // Neutral guard: ensure it's well-formed JSON so we never store garbage. Core
        // stays editing-agnostic — it checks syntax, not schema.
        serde_json::from_str::<serde_json::Value>(trimmed)
            .map_err(|e| CatalogError::Validation(format!("edit record is not valid JSON: {e}")))?;
        self.conn.execute(
            "INSERT INTO photo_edits(photo_id, edit_json, updated_at) VALUES(?1, ?2, ?3)
             ON CONFLICT(photo_id) DO UPDATE SET edit_json = excluded.edit_json,
                                                 updated_at = excluded.updated_at",
            params![photo_id, trimmed, now()],
        )?;
        Ok(())
    }

    pub fn clear_edit_record(&self, photo_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM photo_edits WHERE photo_id = ?1", params![photo_id])?;
        Ok(())
    }

    /// Whether the photo has a (non-empty) edit record — for filters/derived state.
    pub fn photo_has_edits(&self, photo_id: i64) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM photo_edits WHERE photo_id = ?1)",
            params![photo_id],
            |r| r.get(0),
        )?)
    }

    // --- photo versions (named crop/exposure variants) -----------------------
    // Core stores the rows; the edit_json is opaque (interpreted by the editing
    // module). See docs/editing.md.

    /// The photo a version belongs to.
    pub fn version_photo_id(&self, version_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT photo_id FROM photo_versions WHERE id = ?1",
            params![version_id],
            |r| r.get(0),
        )?)
    }

    /// Create a new (empty) version for a photo, appended at the end. Returns its id.
    pub fn create_version(&self, photo_id: i64, name: &str) -> Result<i64> {
        let name = name.trim();
        if name.is_empty() {
            return Err(CatalogError::Validation("version name is empty".into()));
        }
        let position: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM photo_versions WHERE photo_id = ?1",
            params![photo_id],
            |r| r.get(0),
        )?;
        let now = now();
        self.conn.execute(
            "INSERT INTO photo_versions(photo_id, name, edit_json, position, created_at, updated_at)
             VALUES(?1, ?2, '{}', ?3, ?4, ?4)",
            params![photo_id, name, position, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// All versions of a photo, in display order.
    pub fn list_versions(&self, photo_id: i64) -> Result<Vec<PhotoVersion>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, photo_id, name, edit_json, position FROM photo_versions
             WHERE photo_id = ?1 ORDER BY position, id",
        )?;
        let rows = stmt.query_map(params![photo_id], |r| {
            Ok(PhotoVersion {
                id: r.get(0)?,
                photo_id: r.get(1)?,
                name: r.get(2)?,
                edit_json: r.get(3)?,
                position: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// One version by id, or `None`.
    pub fn get_version(&self, version_id: i64) -> Result<Option<PhotoVersion>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, photo_id, name, edit_json, position FROM photo_versions WHERE id = ?1",
                params![version_id],
                |r| {
                    Ok(PhotoVersion {
                        id: r.get(0)?,
                        photo_id: r.get(1)?,
                        name: r.get(2)?,
                        edit_json: r.get(3)?,
                        position: r.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn rename_version(&self, version_id: i64, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Err(CatalogError::Validation("version name is empty".into()));
        }
        self.conn.execute(
            "UPDATE photo_versions SET name = ?1, updated_at = ?2 WHERE id = ?3",
            params![name, now(), version_id],
        )?;
        Ok(())
    }

    /// Replace a version's edit record. Validates JSON syntax only (core stays
    /// editing-agnostic), like `set_edit_record`.
    pub fn set_version_edit(&self, version_id: i64, edit_json: &str) -> Result<()> {
        let trimmed = edit_json.trim();
        let value = if trimmed.is_empty() { "{}" } else { trimmed };
        serde_json::from_str::<serde_json::Value>(value)
            .map_err(|e| CatalogError::Validation(format!("edit record is not valid JSON: {e}")))?;
        self.conn.execute(
            "UPDATE photo_versions SET edit_json = ?1, updated_at = ?2 WHERE id = ?3",
            params![value, now(), version_id],
        )?;
        Ok(())
    }

    pub fn delete_version(&self, version_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM photo_versions WHERE id = ?1", params![version_id])?;
        Ok(())
    }

    /// Duplicate a version (same edit record), appended at the end. Returns the new id.
    pub fn duplicate_version(&self, version_id: i64) -> Result<i64> {
        let src = self
            .get_version(version_id)?
            .ok_or_else(|| CatalogError::Validation("version not found".into()))?;
        let position: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM photo_versions WHERE photo_id = ?1",
            params![src.photo_id],
            |r| r.get(0),
        )?;
        let now = now();
        self.conn.execute(
            "INSERT INTO photo_versions(photo_id, name, edit_json, position, created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?5)",
            params![src.photo_id, format!("{} copy", src.name), src.edit_json, position, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Set the display order for a photo's versions from an ordered id list. Atomic —
    /// a partial reorder would otherwise leave inconsistent positions.
    pub fn reorder_versions(&self, photo_id: i64, ordered_ids: &[i64]) -> Result<()> {
        let now = now();
        let tx = self.conn.unchecked_transaction()?;
        for (pos, id) in ordered_ids.iter().enumerate() {
            tx.execute(
                "UPDATE photo_versions SET position = ?1, updated_at = ?2
                 WHERE id = ?3 AND photo_id = ?4",
                params![pos as i64, now, id, photo_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Version counts for many photos at once (for the grid badge). Returns only photos
    /// that have at least one version.
    pub fn version_counts(&self, photo_ids: &[i64]) -> Result<Vec<(i64, i64)>> {
        if photo_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut counts = Vec::new();
        for chunk in photo_ids.chunks(SQLITE_PARAM_CHUNK) {
            let placeholders = sqlite_param_placeholders(chunk.len());
            let mut stmt = self.conn.prepare(&format!(
                "SELECT photo_id, COUNT(*) FROM photo_versions
                 WHERE photo_id IN ({placeholders}) GROUP BY photo_id"
            ))?;
            let params = rusqlite::params_from_iter(chunk.iter());
            let rows = stmt.query_map(params, |r| Ok((r.get(0)?, r.get(1)?)))?;
            counts.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(counts)
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
