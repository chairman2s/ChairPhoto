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
    sqlite_param_placeholders, Catalog, CatalogError, CoverPin, HistoryStep, PhotoVersion, Result,
    VersionHistory, SQLITE_PARAM_CHUNK,
};

/// Steps kept per version; the oldest go first. Settings are small (a few hundred bytes),
/// so this bounds rows, not space.
pub const HISTORY_CAP: i64 = 200;

/// The label of step 0: the settings the version had when its history began.
pub const HISTORY_BASELINE_LABEL: &str = "Before";

/// What a new version is to its photo's automatic Library face (#252).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewVersion {
    /// A change of the photo ("+ New version", a duplicate, a first save): the automatic face
    /// moves to it.
    Change,
    /// A side branch banked beside the version being edited (a Duel's "What-if"): the face
    /// stays where it is. It is not a candidate for the automatic face until its settings
    /// are written.
    Aside,
}

fn validated_edit_json(edit_json: &str) -> Result<&str> {
    let trimmed = edit_json.trim();
    let value = if trimmed.is_empty() { "{}" } else { trimmed };
    serde_json::from_str::<serde_json::Value>(value)
        .map_err(|e| CatalogError::Validation(format!("edit record is not valid JSON: {e}")))?;
    Ok(value)
}
use rusqlite::{params, Connection, OptionalExtension};
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
        self.create_version_with(photo_id, name, "{}", NewVersion::Change)
    }

    /// Create a version holding `edit_json`, appended at the end, in one write: no reader
    /// ever sees it empty, nor the face on it before its settings (review of #252, N2).
    /// [`NewVersion`] says whether it moves the automatic face. Returns its id.
    pub fn create_version_with(&self, photo_id: i64, name: &str, edit_json: &str, kind: NewVersion) -> Result<i64> {
        let name = name.trim();
        if name.is_empty() {
            return Err(CatalogError::Validation("version name is empty".into()));
        }
        let value = validated_edit_json(edit_json)?;
        let position: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM photo_versions WHERE photo_id = ?1",
            params![photo_id],
            |r| r.get(0),
        )?;
        let now = now();
        atomically(&self.conn, || {
            self.conn.execute(
                "INSERT INTO photo_versions(photo_id, name, edit_json, position, created_at, updated_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?5)",
                params![photo_id, name, value, position, now],
            )?;
            let id = self.conn.last_insert_rowid();
            match kind {
                // A new version is the latest change: the automatic face moves to it.
                NewVersion::Change => settings_written(&self.conn, id)?,
                NewVersion::Aside => set_aside(&self.conn, id, photo_id)?,
            }
            Ok(id)
        })
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
        atomically(&self.conn, || {
            let written = self.conn.execute(
                "UPDATE photo_versions SET edit_json = ?1, updated_at = ?2 WHERE id = ?3",
                params![value, now(), version_id],
            )?;
            if written > 0 {
                settings_written(&self.conn, version_id)?;
            }
            Ok(())
        })
    }

    /// A version's edit history: steps oldest first, and the current one.
    pub fn version_history(&self, version_id: i64) -> Result<VersionHistory> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, label, created_at FROM photo_version_history
             WHERE version_id = ?1 ORDER BY seq",
        )?;
        let steps = stmt
            .query_map(params![version_id], |r| {
                Ok(HistoryStep { seq: r.get(0)?, label: r.get(1)?, created_at: r.get(2)? })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let head = self
            .conn
            .query_row(
                "SELECT seq FROM photo_version_history_head WHERE version_id = ?1",
                params![version_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(VersionHistory { version_id, steps, head })
    }

    /// Save `edit_json` as the version's settings and record it as a history step labelled
    /// `label`, in one transaction. The first change seeds step 0 ("Before") with the
    /// settings the version had, so it can be undone. Steps after the current one are
    /// dropped first (a change after stepping back replaces them). `amend` replaces the
    /// current step instead of adding one — the same control still moving — but never the
    /// baseline, and only at the tip. Identical settings are not a step. Keeps at most
    /// [`HISTORY_CAP`] steps. Returns the history as it now stands.
    pub fn commit_version_edit(
        &self,
        version_id: i64,
        edit_json: &str,
        label: &str,
        amend: bool,
    ) -> Result<VersionHistory> {
        let value = validated_edit_json(edit_json)?;
        let label = label.trim();
        let label = if label.is_empty() { "Edit" } else { label };
        let now = now();
        let tx = self.conn.unchecked_transaction()?;
        let current: String = tx
            .query_row(
                "SELECT edit_json FROM photo_versions WHERE id = ?1",
                params![version_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| CatalogError::Validation("version not found".into()))?;
        let head: Option<i64> = tx
            .query_row(
                "SELECT seq FROM photo_version_history_head WHERE version_id = ?1",
                params![version_id],
                |r| r.get(0),
            )
            .optional()?;
        let head = match head {
            Some(h) => h,
            None => {
                tx.execute(
                    "INSERT INTO photo_version_history(version_id, seq, label, edit_json, created_at)
                     VALUES(?1, 0, ?2, ?3, ?4)",
                    params![version_id, HISTORY_BASELINE_LABEL, current, now],
                )?;
                0
            }
        };
        let head_json: String = tx.query_row(
            "SELECT edit_json FROM photo_version_history WHERE version_id = ?1 AND seq = ?2",
            params![version_id, head],
            |r| r.get(0),
        )?;
        let new_head = if head_json == value {
            head // no change: not a step
        } else {
            let tip: i64 = tx.query_row(
                "SELECT MAX(seq) FROM photo_version_history WHERE version_id = ?1",
                params![version_id],
                |r| r.get(0),
            )?;
            if amend && head > 0 && head == tip {
                tx.execute(
                    "UPDATE photo_version_history SET label = ?1, edit_json = ?2, created_at = ?3
                     WHERE version_id = ?4 AND seq = ?5",
                    params![label, value, now, version_id, head],
                )?;
                head
            } else {
                tx.execute(
                    "DELETE FROM photo_version_history WHERE version_id = ?1 AND seq > ?2",
                    params![version_id, head],
                )?;
                tx.execute(
                    "INSERT INTO photo_version_history(version_id, seq, label, edit_json, created_at)
                     VALUES(?1, ?2, ?3, ?4, ?5)",
                    params![version_id, head + 1, label, value, now],
                )?;
                tx.execute(
                    "DELETE FROM photo_version_history WHERE version_id = ?1 AND seq <= ?2",
                    params![version_id, head + 1 - HISTORY_CAP],
                )?;
                head + 1
            }
        };
        tx.execute(
            "INSERT INTO photo_version_history_head(version_id, seq) VALUES(?1, ?2)
             ON CONFLICT(version_id) DO UPDATE SET seq = excluded.seq",
            params![version_id, new_head],
        )?;
        if current != value {
            tx.execute(
                "UPDATE photo_versions SET edit_json = ?1, updated_at = ?2 WHERE id = ?3",
                params![value, now, version_id],
            )?;
            settings_written(&tx, version_id)?;
        }
        tx.commit()?;
        self.version_history(version_id)
    }

    /// Make step `seq` current: the version's settings become that step's. Nothing is
    /// deleted — the steps after it stay until the next change replaces them. Returns the
    /// step's settings and the history.
    pub fn goto_version_step(&self, version_id: i64, seq: i64) -> Result<(String, VersionHistory)> {
        let tx = self.conn.unchecked_transaction()?;
        let json: String = tx
            .query_row(
                "SELECT edit_json FROM photo_version_history WHERE version_id = ?1 AND seq = ?2",
                params![version_id, seq],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| CatalogError::Validation("no such history step".into()))?;
        tx.execute(
            "UPDATE photo_version_history_head SET seq = ?1 WHERE version_id = ?2",
            params![seq, version_id],
        )?;
        tx.execute(
            "UPDATE photo_versions SET edit_json = ?1, updated_at = ?2 WHERE id = ?3",
            params![json, now(), version_id],
        )?;
        // A history step is a settings write, the same JSON or not.
        settings_written(&tx, version_id)?;
        tx.commit()?;
        Ok((json, self.version_history(version_id)?))
    }

    /// Pin `version_id` as the photo's face ("Use as cover"), or with `None` unpin it so the
    /// face follows the latest change again. Returns the new face token (`"version:rev"`),
    /// `None` when the original is the face. See [`set_cover_pin`](Self::set_cover_pin).
    pub fn set_cover_version(&self, photo_id: i64, version_id: Option<i64>) -> Result<Option<String>> {
        self.set_cover_pin(photo_id, version_id.map_or(CoverPin::Auto, CoverPin::Version))
    }

    /// Choose how the photo's face is picked (#252): pinned to a version (it must be this
    /// photo's) or to the original, or automatic — the most recently changed version, else
    /// the original. The face's `rev` rises whatever the pin was, so the look the rows name
    /// is new. Returns the new face token, `None` when the original is the face.
    pub fn set_cover_pin(&self, photo_id: i64, pin: CoverPin) -> Result<Option<String>> {
        if let CoverPin::Version(vid) = pin {
            let owner: Option<i64> = self
                .conn
                .query_row("SELECT photo_id FROM photo_versions WHERE id = ?1", params![vid], |r| r.get(0))
                .optional()?;
            if owner != Some(photo_id) {
                return Err(CatalogError::Validation("that version does not belong to this photo".into()));
            }
        }
        atomically(&self.conn, || {
            let face = face_under(&self.conn, photo_id, pin)?;
            store_face(&self.conn, photo_id, face, pin)
        })?;
        Ok(self.cover_of(photo_id)?.map(|(v, rev, _)| format!("{v}:{rev}")))
    }

    /// How the photo's face is chosen. A version pin whose version is gone reads as
    /// automatic, as the face itself does.
    pub fn cover_pin(&self, photo_id: i64) -> Result<CoverPin> {
        Ok(stored_face(&self.conn, photo_id)?.map_or(CoverPin::Auto, |(_, pin)| pin))
    }

    /// The photo's face when it is a version: (version id, rev, the version's settings) —
    /// pinned or automatic. `None`: the original is the face.
    pub fn cover_of(&self, photo_id: i64) -> Result<Option<(i64, i64, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT pc.version_id, pc.rev, pv.edit_json FROM photo_cover pc
                 JOIN photo_versions pv ON pv.id = pc.version_id WHERE pc.photo_id = ?1",
                params![photo_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    /// Delete a version. If it was the photo's face the face falls back to the next most
    /// recently changed version, else the original; a pin on it is lifted (#252).
    pub fn delete_version(&self, version_id: i64) -> Result<()> {
        let conn = &self.conn;
        atomically(conn, || {
            let photo_id: Option<i64> = conn
                .query_row("SELECT photo_id FROM photo_versions WHERE id = ?1", params![version_id], |r| r.get(0))
                .optional()?;
            let Some(photo_id) = photo_id else { return Ok(()) };
            // The face leaves the version before the version goes, its rev rising even when
            // the original takes over: the next version created may reuse this id.
            conn.execute(
                "UPDATE photo_cover SET version_id = NULL, rev = rev + 1,
                        pin = CASE pin WHEN ?3 THEN ?4 ELSE pin END
                 WHERE photo_id = ?1 AND version_id = ?2",
                params![photo_id, version_id, PIN_VERSION, PIN_AUTO],
            )?;
            conn.execute("DELETE FROM photo_versions WHERE id = ?1", params![version_id])?;
            refresh_face(conn, photo_id)
        })
    }

    /// Bring every photo's face up to date (the #252 migration: versions were changed before
    /// faces followed them). Faces already current are left alone.
    pub(crate) fn refresh_all_faces(&self) -> Result<()> {
        let photos: Vec<i64> = {
            let mut stmt = self
                .conn
                .prepare("SELECT photo_id FROM photo_versions UNION SELECT photo_id FROM photo_cover")?;
            let ids = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            ids
        };
        for photo_id in photos {
            refresh_face(&self.conn, photo_id)?;
        }
        Ok(())
    }

    /// The trigger that keeps `changed_seq` true for every writer — an older build too, which
    /// changes a version's settings without knowing the column (review of #252, L1): a
    /// version whose `edit_json` changes becomes its photo's most recently changed, and the
    /// face's `rev` rises if it is that version, as its look changed. It lives in the catalog
    /// file, so it fires whichever build writes. This build's own settings writes go through
    /// [`settings_written`] as well (the trigger's bump then counts once more; only the order
    /// matters), and the trigger's is their one `rev` bump ([`refresh_face`]). What it does not
    /// do is move `photo_cover`: that is [`Self::heal_faces`], on the next open by this build.
    /// (No insert trigger: a version left at `changed_seq` 0 is no candidate for the face, and
    /// only [`Self::heal_faces`] decides that an older build made it.) A rebuild of
    /// `photo_versions` must create it again.
    pub(crate) fn ensure_face_trigger(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS photo_versions_settings_changed
             AFTER UPDATE OF edit_json ON photo_versions
             WHEN NEW.edit_json IS NOT OLD.edit_json
             BEGIN
                 UPDATE photo_versions
                    SET changed_seq = (SELECT MAX(COALESCE(MAX(changed_seq), 0), 0) + 1
                                         FROM photo_versions WHERE photo_id = NEW.photo_id)
                  WHERE id = NEW.id;
                 UPDATE photo_cover SET rev = rev + 1 WHERE photo_id = NEW.photo_id AND version_id = NEW.id;
             END;",
        )?;
        Ok(())
    }

    /// Heal the faces, on every open (review of #252, L1). An older build — the packaged
    /// 2026.8.0, or one from before #252 — writes versions without keeping the faces current:
    /// its settings changes reach `changed_seq` only through [`Self::ensure_face_trigger`], the
    /// versions it creates are left at `changed_seq` 0, and it never moves `photo_cover`.
    ///
    /// - `older_build_opened` (the catalog was stamped below v28 since this build last opened
    ///   it): each version at 0 was created by such a build, and becomes its photo's latest
    ///   change, in `updated_at` order (then id). They count after everything else that build
    ///   did — its settings changes were ordered as they were made, its new versions only now.
    /// - Always: every photo whose stored face is not the one its pin gives — an automatic
    ///   face that is not the most recently changed version, or a version pin whose version
    ///   is gone — is refreshed. One query finds them; on a catalog only this build wrote it
    ///   finds none.
    pub(crate) fn heal_faces(&self, older_build_opened: bool) -> Result<()> {
        // A version pin whose version is gone (an older build deleted it; `ON DELETE SET NULL`)
        // already reads as automatic (`stored_face`); say so in the row, so its `cover_pin`
        // does too. The face itself is refreshed below.
        self.conn.execute(
            "UPDATE photo_cover SET pin = ?1 WHERE pin = ?2 AND version_id IS NULL",
            params![PIN_AUTO, PIN_VERSION],
        )?;
        if older_build_opened {
            let created: Vec<i64> = {
                let mut stmt = self
                    .conn
                    .prepare("SELECT id FROM photo_versions WHERE changed_seq = 0 ORDER BY updated_at, id")?;
                let ids = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                ids
            };
            for id in created {
                settings_written(&self.conn, id)?;
            }
        }
        let stale: Vec<i64> = {
            let mut stmt = self.conn.prepare(
                "SELECT p.photo_id
                   FROM (SELECT DISTINCT photo_id FROM photo_versions) p
                   LEFT JOIN photo_cover c ON c.photo_id = p.photo_id
                  WHERE (c.photo_id IS NULL OR c.pin = 0 OR (c.pin = 1 AND c.version_id IS NULL))
                    AND c.version_id IS NOT (
                        SELECT v.id FROM photo_versions v
                         WHERE v.photo_id = p.photo_id AND v.changed_seq > 0
                         ORDER BY v.changed_seq DESC, v.id DESC LIMIT 1)",
            )?;
            let ids = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            ids
        };
        for photo_id in stale {
            refresh_face(&self.conn, photo_id)?;
        }
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
        atomically(&self.conn, || {
            self.conn.execute(
                "INSERT INTO photo_versions(photo_id, name, edit_json, position, created_at, updated_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?5)",
                params![src.photo_id, format!("{} copy", src.name), src.edit_json, position, now],
            )?;
            let id = self.conn.last_insert_rowid();
            settings_written(&self.conn, id)?;
            Ok(id)
        })
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

// --- the Library face (#252) -----------------------------------------------------------------
// `photo_cover` holds each photo's face: the version shown (NULL = the original), how it is
// chosen (`pin`), and `rev`. The rule is `face_under`; every write that can move the face
// calls `settings_written` or `refresh_face` inside its own transaction.

/// Run `write` as one unit under a savepoint, which — unlike a transaction — nests: a bundle
/// import calls these writes inside its own transaction (`bundle::importer`).
fn atomically<T>(conn: &Connection, write: impl FnOnce() -> Result<T>) -> Result<T> {
    conn.execute_batch("SAVEPOINT version_write")?;
    match write() {
        Ok(value) => {
            conn.execute_batch("RELEASE version_write")?;
            Ok(value)
        }
        Err(e) => {
            // After some errors (SQLITE_FULL, an I/O error) SQLite has already rolled the
            // whole transaction back and the savepoint is gone, so this fails too: the
            // write's own error is the one to report, not that.
            let _ = conn.execute_batch("ROLLBACK TO version_write; RELEASE version_write");
            Err(e)
        }
    }
}

const PIN_AUTO: i64 = 0;
const PIN_VERSION: i64 = 1;
const PIN_ORIGINAL: i64 = 2;

/// `version_id`'s settings were just written (a save, a commit, a history step, a new,
/// duplicated or merged-in version): it becomes its photo's most recently changed version
/// and the face is brought up to date. (If this version already was the face and its
/// settings changed, the `photo_versions_settings_changed` trigger raised its rev: its look
/// changed.) Call inside the write's transaction.
pub(crate) fn settings_written(conn: &Connection, version_id: i64) -> Result<()> {
    let photo_id: i64 =
        conn.query_row("SELECT photo_id FROM photo_versions WHERE id = ?1", params![version_id], |r| r.get(0))?;
    conn.execute(
        "UPDATE photo_versions
            SET changed_seq = (SELECT MAX(COALESCE(MAX(changed_seq), 0), 0) + 1
                                 FROM photo_versions WHERE photo_id = ?2)
          WHERE id = ?1",
        params![version_id, photo_id],
    )?;
    refresh_face(conn, photo_id)
}

/// `version_id` was banked aside ([`NewVersion::Aside`]): its `changed_seq` goes below every
/// other version's and below 0, so it is never the automatic face — not even as the photo's
/// only version, or the one left when the face is deleted — until its settings are written
/// ([`settings_written`] puts it above them all). Call inside the insert's transaction.
///
/// **The convention for every version that must not be a change** — a What-if, and a version
/// merged into an existing photo from a bundle or another catalog (#252 decision 2): store it
/// below 0 through this function. `changed_seq` 0 is reserved for a row a build that knows no
/// `changed_seq` inserted: [`Catalog::heal_faces`] makes each such row its photo's latest
/// change once an older build has opened the catalog, so a version this build leaves at 0
/// would move the face then.
pub(crate) fn set_aside(conn: &Connection, version_id: i64, photo_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE photo_versions
            SET changed_seq = (SELECT MIN(COALESCE(MIN(changed_seq), 0), 0) - 1
                                 FROM photo_versions WHERE photo_id = ?2 AND id <> ?1)
          WHERE id = ?1",
        params![version_id, photo_id],
    )?;
    Ok(())
}

/// The face a pin gives: the pinned version or the original, or — automatic — the version
/// whose settings were written last, else the original. A version whose `changed_seq` is not
/// above 0 was never written as a change here (banked aside, or inserted by an older build
/// and not healed yet) and is no candidate.
fn face_under(conn: &Connection, photo_id: i64, pin: CoverPin) -> Result<Option<i64>> {
    Ok(match pin {
        CoverPin::Version(v) => Some(v),
        CoverPin::Original => None,
        CoverPin::Auto => conn
            .query_row(
                "SELECT id FROM photo_versions WHERE photo_id = ?1 AND changed_seq > 0
                  ORDER BY changed_seq DESC, id DESC LIMIT 1",
                params![photo_id],
                |r| r.get(0),
            )
            .optional()?,
    })
}

/// The stored face and its pin, if the photo has a row. A version pin whose version is gone
/// (`ON DELETE SET NULL` by a path other than `delete_version`) reads as automatic, never
/// as a pinned original.
fn stored_face(conn: &Connection, photo_id: i64) -> Result<Option<(Option<i64>, CoverPin)>> {
    let row: Option<(Option<i64>, i64)> = conn
        .query_row("SELECT version_id, pin FROM photo_cover WHERE photo_id = ?1", params![photo_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    Ok(row.map(|(shown, pin)| {
        let pin = match (pin, shown) {
            (PIN_ORIGINAL, _) => CoverPin::Original,
            (PIN_VERSION, Some(v)) => CoverPin::Version(v),
            _ => CoverPin::Auto,
        };
        (shown, pin)
    }))
}

/// Store `face` under `pin`, its rev one past the row's. A new row starts at 0: no look of
/// this photo but the original's was shown before.
fn store_face(conn: &Connection, photo_id: i64, face: Option<i64>, pin: CoverPin) -> Result<()> {
    let pin = match pin {
        CoverPin::Auto => PIN_AUTO,
        CoverPin::Version(_) => PIN_VERSION,
        CoverPin::Original => PIN_ORIGINAL,
    };
    conn.execute(
        "INSERT INTO photo_cover(photo_id, version_id, rev, pin) VALUES(?1, ?2, 0, ?3)
         ON CONFLICT(photo_id) DO UPDATE SET version_id = excluded.version_id, pin = excluded.pin,
                                             rev = photo_cover.rev + 1",
        params![photo_id, face, pin],
    )?;
    Ok(())
}

/// Recompute the photo's face from its pin and store it when it moved. Nothing is stored when
/// the face stays put: a change of the face's own settings raises its rev in the
/// `photo_versions_settings_changed` trigger, whichever build made it
/// ([`Catalog::ensure_face_trigger`]).
pub(crate) fn refresh_face(conn: &Connection, photo_id: i64) -> Result<()> {
    let stored = stored_face(conn, photo_id)?;
    let pin = stored.map_or(CoverPin::Auto, |(_, pin)| pin);
    let face = face_under(conn, photo_id, pin)?;
    let unchanged = match stored {
        // No row: only the original was ever shown.
        None => face.is_none(),
        Some((shown, _)) => shown == face,
    };
    if unchanged {
        return Ok(());
    }
    store_face(conn, photo_id, face, pin)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod face_tests {
    // --- the Library face follows the last changed version unless pinned (#252) ----------
    use super::*;
    use crate::test_support::TestTmpDir;

    fn catalog(tag: &str) -> (Catalog, TestTmpDir, i64) {
        let dir = TestTmpDir::new(&format!("face-{tag}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("t.chairphoto"), &root).unwrap();
        let photo = catalog.upsert_photo(&root.join("a.arw"), None, 1, 1).unwrap().id;
        (catalog, dir, photo)
    }

    /// The face's version, as the photo row names it (`None`: the original).
    fn face(c: &Catalog, photo: i64) -> Option<i64> {
        let token = c.get_photo(photo).unwrap().cover_token;
        assert_eq!(token.is_some(), c.cover_of(photo).unwrap().is_some(), "the row and cover_of agree");
        token.map(|t| t.split_once(':').unwrap().0.parse().unwrap())
    }

    fn rev(c: &Catalog, photo: i64) -> i64 {
        c.conn().query_row("SELECT rev FROM photo_cover WHERE photo_id = ?1", [photo], |r| r.get(0)).unwrap()
    }

    #[test]
    fn the_automatic_face_is_the_last_changed_version() {
        let (c, _dir, p) = catalog("auto");
        assert_eq!(face(&c, p), None, "no versions: the original");
        let v1 = c.create_version(p, "V1").unwrap();
        let v2 = c.create_version(p, "V2").unwrap();
        assert_eq!(face(&c, p), Some(v2), "a new version is the latest change");
        c.commit_version_edit(v1, r#"{"fade":0.1}"#, "Fade", false).unwrap();
        assert_eq!(face(&c, p), Some(v1));
        c.set_version_edit(v2, r#"{"fade":0.2}"#).unwrap();
        assert_eq!(face(&c, p), Some(v2), "edit v1, then v2: v2");
        c.commit_version_edit(v1, r#"{"fade":0.3}"#, "Fade", false).unwrap();
        assert_eq!(face(&c, p), Some(v1), "v1 again: v1");
        c.set_version_edit(v2, r#"{"fade":0.4}"#).unwrap();
        assert_eq!(face(&c, p), Some(v2));
        c.goto_version_step(v1, 1).unwrap();
        assert_eq!(face(&c, p), Some(v1), "a history step (an undo) is a change");
        c.set_version_edit(v2, r#"{"fade":0.5}"#).unwrap();
        assert_eq!(face(&c, p), Some(v2));
        // Neither a rename, a reorder nor a commit of the settings v1 holds is a change.
        c.rename_version(v1, "Renamed").unwrap();
        c.reorder_versions(p, &[v2, v1]).unwrap();
        c.commit_version_edit(v1, r#"{"fade":0.1}"#, "Fade", false).unwrap();
        assert_eq!(face(&c, p), Some(v2));
        let dup = c.duplicate_version(v1).unwrap();
        assert_eq!(face(&c, p), Some(dup), "a duplicate is a new version");
        assert_eq!(c.cover_pin(p).unwrap(), CoverPin::Auto);
        assert_eq!(c.get_photo(p).unwrap().cover_pin, CoverPin::Auto);
    }

    #[test]
    fn a_pinned_version_beats_the_automatic_face_until_unpinned() {
        let (c, _dir, p) = catalog("pinned");
        let v1 = c.create_version(p, "V1").unwrap();
        let v2 = c.create_version(p, "V2").unwrap();
        let token = c.set_cover_version(p, Some(v1)).unwrap().unwrap();
        assert!(token.starts_with(&format!("{v1}:")));
        assert_eq!(c.cover_pin(p).unwrap(), CoverPin::Version(v1));
        assert_eq!(c.get_photo(p).unwrap().cover_pin, CoverPin::Version(v1), "the row carries the pin");
        c.commit_version_edit(v2, r#"{"fade":0.1}"#, "Fade", false).unwrap();
        let v3 = c.create_version(p, "V3").unwrap();
        assert_eq!(face(&c, p), Some(v1), "pinned beats the later changes");
        // Unpinned, the face is the last changed version again.
        let token = c.set_cover_version(p, None).unwrap().unwrap();
        assert!(token.starts_with(&format!("{v3}:")), "{token}");
        assert_eq!(face(&c, p), Some(v3));
        assert_eq!(c.cover_pin(p).unwrap(), CoverPin::Auto);
        assert_eq!(c.get_photo(p).unwrap().cover_pin, CoverPin::Auto);
        // Another photo's version is refused.
        let other = c.upsert_photo(&c.root().join("b.arw"), None, 1, 1).unwrap().id;
        let foreign = c.create_version(other, "X").unwrap();
        assert!(c.set_cover_pin(p, CoverPin::Version(foreign)).is_err());
        assert_eq!(c.cover_pin(p).unwrap(), CoverPin::Auto);
    }

    #[test]
    fn the_original_can_be_pinned_over_edited_versions() {
        let (c, _dir, p) = catalog("original");
        let v = c.create_version(p, "V").unwrap();
        c.set_version_edit(v, r#"{"fade":0.1}"#).unwrap();
        assert_eq!(c.set_cover_pin(p, CoverPin::Original).unwrap(), None);
        assert_eq!(face(&c, p), None, "the original, though a version was changed");
        assert_eq!(c.get_photo(p).unwrap().cover_pin, CoverPin::Original);
        c.commit_version_edit(v, r#"{"fade":0.2}"#, "Fade", false).unwrap();
        c.create_version(p, "W").unwrap();
        assert_eq!(face(&c, p), None, "still the original after later edits");
        // Deleting a version does not lift a pin on the original.
        c.delete_version(v).unwrap();
        assert_eq!(c.cover_pin(p).unwrap(), CoverPin::Original);
        assert_eq!(face(&c, p), None);
        c.set_cover_pin(p, CoverPin::Auto).unwrap();
        assert!(face(&c, p).is_some());
    }

    #[test]
    fn deleting_the_face_falls_back_to_the_next_most_recently_changed() {
        let (c, _dir, p) = catalog("delete");
        let v1 = c.create_version(p, "V1").unwrap();
        let v2 = c.create_version(p, "V2").unwrap();
        let v3 = c.create_version(p, "V3").unwrap();
        c.set_version_edit(v1, r#"{"fade":0.1}"#).unwrap(); // changed: v2, v3, v1
        c.set_version_edit(v3, r#"{"fade":0.3}"#).unwrap(); // changed: v2, v1, v3
        assert_eq!(face(&c, p), Some(v3));
        c.delete_version(v3).unwrap();
        assert_eq!(face(&c, p), Some(v1), "the next most recently changed, not the newest id");
        // A pinned face whose version is deleted becomes unpinned.
        c.set_cover_version(p, Some(v2)).unwrap();
        c.delete_version(v2).unwrap();
        assert_eq!(c.cover_pin(p).unwrap(), CoverPin::Auto);
        assert_eq!(c.get_photo(p).unwrap().cover_pin, CoverPin::Auto);
        assert_eq!(face(&c, p), Some(v1));
        c.delete_version(v1).unwrap();
        assert_eq!(face(&c, p), None, "no versions left: the original");
    }

    /// A version banked aside (a Duel's "What-if") is created with its settings in one write
    /// and leaves the face where it is — on another version, on the original, and when the
    /// face is deleted — until its own settings are written (#252 decision, 2026-10-06).
    #[test]
    fn a_version_banked_aside_never_moves_the_face_until_it_is_edited() {
        let (c, _dir, p) = catalog("aside");
        let only = c.create_version_with(p, "What-if — ev", r#"{"fade":0.1}"#, NewVersion::Aside).unwrap();
        assert_eq!(c.get_version(only).unwrap().unwrap().edit_json, r#"{"fade":0.1}"#, "its settings, in one write");
        assert_eq!(face(&c, p), None, "the photo's only version, banked aside: the original stays the face");
        let v1 = c.create_version_with(p, "V1", r#"{"fade":0.2}"#, NewVersion::Change).unwrap();
        assert_eq!(face(&c, p), Some(v1), "+ New version moves it");
        let before = rev(&c, p);
        let aside = c.create_version_with(p, "What-if — contrast", "{}", NewVersion::Aside).unwrap();
        assert_eq!((face(&c, p), rev(&c, p)), (Some(v1), before), "the face and its token stay");
        c.delete_version(v1).unwrap();
        assert_eq!(face(&c, p), None, "the face deleted: banked variants are not the fallback");
        // Edited, a variant is a change like any other.
        c.set_version_edit(aside, r#"{"fade":0.3}"#).unwrap();
        assert_eq!(face(&c, p), Some(aside));
        c.commit_version_edit(only, r#"{"fade":0.4}"#, "Fade", false).unwrap();
        assert_eq!(face(&c, p), Some(only));
        assert!(c.create_version_with(p, "Bad", "{", NewVersion::Aside).is_err(), "not JSON: nothing created");
        assert_eq!(c.list_versions(p).unwrap().len(), 2);
    }

    /// The face's rev rises on every change of the face, and its token is never one shown
    /// before — not even when the next version created reuses a deleted face's id.
    #[test]
    fn the_rev_rises_on_every_face_change() {
        let (c, _dir, p) = catalog("rev");
        let mut seen: Vec<String> = Vec::new();
        let mut last = -1;
        let mut changed = |c: &Catalog, what: &str| {
            let r = rev(c, p);
            assert!(r > last, "{what}: rev {r} after {last}");
            last = r;
            if let Some(t) = c.get_photo(p).unwrap().cover_token {
                assert!(!seen.contains(&t), "{what}: token {t} was shown before");
                seen.push(t);
            }
        };
        let v1 = c.create_version(p, "V1").unwrap();
        changed(&c, "the first version");
        let v2 = c.create_version(p, "V2").unwrap();
        changed(&c, "the face moves to a new version");
        c.set_version_edit(v2, r#"{"fade":0.1}"#).unwrap();
        changed(&c, "the face's settings");
        c.commit_version_edit(v1, r#"{"fade":0.2}"#, "Fade", false).unwrap();
        changed(&c, "the face moves to v1");
        c.goto_version_step(v1, 0).unwrap();
        changed(&c, "a history step on the face");
        c.set_cover_pin(p, CoverPin::Version(v1)).unwrap();
        changed(&c, "pin");
        c.set_cover_pin(p, CoverPin::Original).unwrap();
        changed(&c, "pin the original");
        c.set_cover_pin(p, CoverPin::Auto).unwrap();
        changed(&c, "unpin");
        c.delete_version(v1).unwrap();
        changed(&c, "the face's version deleted");
        c.delete_version(v2).unwrap();
        changed(&c, "the last version deleted");
        let again = c.create_version(p, "Again").unwrap();
        changed(&c, "a version created after the deletions (its id may be reused)");
        c.set_cover_pin(p, CoverPin::Version(again)).unwrap();
        changed(&c, "pin");
        // A change to a version that is not the face leaves it alone.
        let other = c.create_version(p, "Other").unwrap();
        c.set_version_edit(other, r#"{"fade":0.4}"#).unwrap();
        assert_eq!(rev(&c, p), last, "a version that is not the face");
    }

    /// A catalog from before #252: covers set by hand migrate as pinned, a cleared one as
    /// automatic, and an edited photo's face is its most recently changed version at once.
    #[test]
    fn covers_from_before_auto_faces_migrate_as_pinned() {
        let dir = TestTmpDir::new("face-migrate");
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let path = dir.join("t.chairphoto");
        let (pinned, cleared, edited, versions);
        {
            let c = Catalog::open(&path, &root).unwrap();
            pinned = c.upsert_photo(&root.join("a.arw"), None, 1, 1).unwrap().id;
            cleared = c.upsert_photo(&root.join("b.arw"), None, 1, 1).unwrap().id;
            edited = c.upsert_photo(&root.join("c.arw"), None, 1, 1).unwrap().id;
            let mut vs = Vec::new();
            for photo in [pinned, pinned, cleared, edited, edited] {
                vs.push(c.create_version(photo, "V").unwrap());
            }
            versions = vs;
            // The pre-#252 shape: no `pin`, no `changed_seq` (nor the triggers on it); covers
            // as they were stored.
            c.conn()
                .execute_batch(&format!(
                    "DROP TRIGGER photo_versions_settings_changed;
                     DELETE FROM photo_cover;
                     ALTER TABLE photo_cover DROP COLUMN pin;
                     ALTER TABLE photo_versions DROP COLUMN changed_seq;
                     INSERT INTO photo_cover(photo_id, version_id, rev) VALUES({pinned}, {}, 4), ({cleared}, NULL, 2);
                     UPDATE photo_versions SET updated_at = 100;
                     UPDATE photo_versions SET updated_at = 200 WHERE id = {};",
                    versions[0], versions[3]
                ))
                .unwrap();
        }
        let c = Catalog::open(&path, &root).unwrap();
        assert_eq!(c.cover_pin(pinned).unwrap(), CoverPin::Version(versions[0]), "a hand-set cover is pinned");
        assert_eq!(c.get_photo(pinned).unwrap().cover_token, Some(format!("{}:4", versions[0])), "and unchanged");
        assert_eq!(c.cover_pin(cleared).unwrap(), CoverPin::Auto);
        assert_eq!(face(&c, cleared), Some(versions[2]), "a cleared cover follows the versions");
        assert!(rev(&c, cleared) > 2, "past every token shown before");
        assert_eq!(face(&c, edited), Some(versions[3]), "the most recently updated, not the newest");
        // The backfilled order holds for the next change.
        c.set_version_edit(versions[4], "{}").unwrap();
        assert_eq!(face(&c, edited), Some(versions[4]));
        drop(c);
        // A second open migrates nothing again.
        let c = Catalog::open(&path, &root).unwrap();
        assert_eq!(c.get_photo(pinned).unwrap().cover_token, Some(format!("{}:4", versions[0])));
        assert_eq!(face(&c, edited), Some(versions[4]));
    }

    // --- an older build's writes heal on the next open (review of #252, L1) -----------------

    /// What an older build writes — the packaged 2026.8.0 knows neither `changed_seq` nor
    /// `photo_cover`; a pre-#252 build knows no `pin` — replayed as its SQL, then the catalog
    /// stamped with its schema as such a build does on open. The trigger orders its settings
    /// changes, the next open by this build counts the versions it created as the latest
    /// changes, and moves every face it left behind.
    #[test]
    fn an_older_builds_version_writes_move_the_faces_on_the_next_open() {
        let dir = TestTmpDir::new("face-older-build");
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let path = dir.join("t.chairphoto");
        let (p, q, r, v1, v2, w1, w2, x1);
        {
            let c = Catalog::open(&path, &root).unwrap();
            p = c.upsert_photo(&root.join("a.arw"), None, 1, 1).unwrap().id;
            q = c.upsert_photo(&root.join("b.arw"), None, 1, 1).unwrap().id;
            r = c.upsert_photo(&root.join("c.arw"), None, 1, 1).unwrap().id;
            v1 = c.create_version(p, "V1").unwrap();
            v2 = c.create_version(p, "V2").unwrap();
            w1 = c.create_version(q, "W1").unwrap();
            w2 = c.create_version(q, "W2").unwrap();
            x1 = c.create_version(r, "X1").unwrap();
            c.set_cover_pin(r, CoverPin::Version(x1)).unwrap();
            assert_eq!((face(&c, p), face(&c, q), face(&c, r)), (Some(v2), Some(w2), Some(x1)));
        }
        // The older build: p — an edit of v1 (the latest change); q — an edit of w1, then a
        // duplicate (the duplicate is); r — its pinned version deleted, and a new one.
        let older = rusqlite::Connection::open(&path).unwrap();
        older.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let insert = |photo: i64, name: &str| {
            older
                .execute(
                    "INSERT INTO photo_versions(photo_id, name, edit_json, position, created_at, updated_at)
                     VALUES(?1, ?2, '{}', 9, 1, 1)",
                    params![photo, name],
                )
                .unwrap();
            older.last_insert_rowid()
        };
        let edit = |version: i64, json: &str| {
            older
                .execute("UPDATE photo_versions SET edit_json = ?1, updated_at = 2 WHERE id = ?2", params![json, version])
                .unwrap();
        };
        edit(v1, r#"{"fade":0.5}"#);
        edit(w1, r#"{"fade":0.6}"#);
        let w3 = insert(q, "W1 copy");
        older.execute("DELETE FROM photo_versions WHERE id = ?1", params![x1]).unwrap();
        let x2 = insert(r, "X2");
        older.execute("UPDATE settings SET value = '19' WHERE key = 'schema_version'", []).unwrap();
        drop(older);

        let c = Catalog::open(&path, &root).unwrap();
        assert_eq!(face(&c, p), Some(v1), "v1 edited: v1 (not {v2})");
        assert_eq!(face(&c, q), Some(w3), "w1 edited, then duplicated: the copy (not {w1})");
        assert_eq!(c.cover_pin(r).unwrap(), CoverPin::Auto, "the pinned version is gone: unpinned");
        let pin: i64 = c.conn().query_row("SELECT pin FROM photo_cover WHERE photo_id = ?1", [r], |row| row.get(0)).unwrap();
        assert_eq!(pin, PIN_AUTO, "and the row says so: no stale version pin left");
        assert_eq!(face(&c, r), Some(x2));
        assert_eq!(
            c.get_setting("schema_version").unwrap(),
            Some(super::super::schema::SCHEMA_VERSION.to_string()),
            "stamped with this build's schema again"
        );
        // Healed, the next change here moves the face as usual.
        c.set_version_edit(v2, r#"{"fade":0.7}"#).unwrap();
        assert_eq!(face(&c, p), Some(v2));
        let _ = w2;
        // Opened again by this build: nothing is an older build's any more.
        drop(c);
        let c = Catalog::open(&path, &root).unwrap();
        assert_eq!((face(&c, p), face(&c, q), face(&c, r)), (Some(v2), Some(w3), Some(x2)));
    }

    /// A version this build left at `changed_seq` 0 (none of its own writes do; a merge path
    /// might) is no candidate for the face, and an open that saw no older build leaves it so.
    #[test]
    fn a_version_at_zero_is_promoted_only_after_an_older_build() {
        let (c, dir, p) = catalog("zero-seq");
        let v1 = c.create_version(p, "V1").unwrap();
        c.conn()
            .execute(
                "INSERT INTO photo_versions(photo_id, name, edit_json, position, created_at, updated_at)
                 VALUES(?1, 'Zero', '{}', 9, 1, 1)",
                [p],
            )
            .unwrap();
        let zero = c.conn().last_insert_rowid();
        // A version this build set aside (a What-if; a version merged into an existing photo)
        // stays aside after an older build opened the catalog: it is below 0, not at 0.
        let aside = c.create_version_with(p, "Merged", "{}", NewVersion::Aside).unwrap();
        drop(c);
        let path = dir.join("t.chairphoto");
        let root = dir.join("photos");
        let c = Catalog::open(&path, &root).unwrap();
        assert_eq!(face(&c, p), Some(v1), "no older build since: not promoted");
        c.set_setting("schema_version", "26").unwrap();
        drop(c);
        let c = Catalog::open(&path, &root).unwrap();
        assert_eq!(face(&c, p), Some(zero), "an older build since: its new version is the latest change");
        let seq: i64 =
            c.conn().query_row("SELECT changed_seq FROM photo_versions WHERE id = ?1", [aside], |r| r.get(0)).unwrap();
        assert!(seq < 0, "the version set aside is not promoted: {seq}");
    }

    /// An older build deletes a photo's pinned, only version (`ON DELETE SET NULL` leaves the
    /// row pinned to nothing): the next open says the row is automatic, not a stale version pin.
    #[test]
    fn a_pin_left_on_a_deleted_version_is_cleared_on_open() {
        let (c, dir, p) = catalog("older-pin-gone");
        let v = c.create_version(p, "V").unwrap();
        c.set_cover_pin(p, CoverPin::Version(v)).unwrap();
        drop(c);
        let path = dir.join("t.chairphoto");
        let older = rusqlite::Connection::open(&path).unwrap();
        older.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        older.execute("DELETE FROM photo_versions WHERE id = ?1", [v]).unwrap();
        drop(older);
        let c = Catalog::open(&path, &dir.join("photos")).unwrap();
        let pin: i64 = c.conn().query_row("SELECT pin FROM photo_cover WHERE photo_id = ?1", [p], |r| r.get(0)).unwrap();
        assert_eq!(pin, PIN_AUTO);
        assert_eq!(c.get_photo(p).unwrap().cover_pin, CoverPin::Auto);
        assert_eq!(face(&c, p), None);
    }

    /// An older build's settings change to the face's own version gives it a new look token
    /// (`rev`), so no view keeps showing the look it cached for the old settings.
    #[test]
    fn an_older_builds_edit_of_the_face_gives_it_a_new_token() {
        let (c, dir, p) = catalog("older-face-rev");
        let v = c.create_version(p, "V").unwrap();
        let before = c.get_photo(p).unwrap().cover_token;
        let path = dir.join("t.chairphoto");
        let older = rusqlite::Connection::open(&path).unwrap();
        older.execute("UPDATE photo_versions SET edit_json = '{\"fade\":0.2}' WHERE id = ?1", [v]).unwrap();
        drop(older);
        let after = c.get_photo(p).unwrap().cover_token;
        assert!(after.is_some() && after != before, "{before:?} -> {after:?}");
    }

    /// A catalog a newer build stamped with a schema this one does not know is refused,
    /// and nothing is written to it.
    #[test]
    fn a_catalog_from_a_newer_build_is_refused() {
        let (c, dir, _p) = catalog("newer-schema");
        let newer = super::super::schema::SCHEMA_VERSION + 1;
        c.set_setting("schema_version", &newer.to_string()).unwrap();
        drop(c);
        let path = dir.join("t.chairphoto");
        let refused = Catalog::open(&path, &dir.join("photos"));
        assert!(
            matches!(refused, Err(CatalogError::NewerSchema { found, known }) if found == newer && known == newer - 1),
            "{:?}",
            refused.err()
        );
        let raw = rusqlite::Connection::open(&path).unwrap();
        let stamp: String =
            raw.query_row("SELECT value FROM settings WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap();
        assert_eq!(stamp, newer.to_string(), "not restamped");
    }
}
