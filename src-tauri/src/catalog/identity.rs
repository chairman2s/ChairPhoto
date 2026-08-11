//! Sidecar identity binding: keeping portable catalog identity in BOTH SQLite and the
//! file's XMP sidecar.
//!
//! The catalog half is trivial — the row cannot exist without a UUID. The disk half
//! can fail: the storage may be read-only, the existing sidecar may not parse, the
//! volume may be offline, or the sidecar may already carry a *different* identity we
//! must not overwrite. Every caller used to log such a failure to stderr and carry on,
//! which leaves a catalogued photo whose identity exists only in this one SQLite file:
//! a merge or a re-import can no longer recognise the file, and nothing in the catalog
//! remembers that it should.
//!
//! So the observable operation is: **bind the sidecar field, or record a retryable repair**.
//! [`bind_sidecar_identity`] is the pure-IO half (no catalog, safe to run off the lock),
//! [`Catalog::record_sidecar_identity`] the DB half, and [`Catalog::ensure_sidecar_identity`]
//! composes them for callers that already hold a connection off the main lock (the
//! scanner, the bundle indexer). [`Catalog::repair_pending_identity`] retries the whole
//! queue; the plan → IO → record split ([`Catalog::plan_identity_repairs`] +
//! [`IdentityRepairPlan::run`]) exists so the command layer can do the sidecar IO
//! without holding the catalog lock, mirroring the storage lifecycle in `lifecycle.rs`.
//!
//! `pending_sidecar_identity` is still named for the original UUID debt, but it now carries
//! a `field` discriminator so the same retry path also covers `chairphoto:ImportBatch`.

use super::{Catalog, Result};
use rusqlite::params;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidecarField {
    Identifier,
    ImportBatch,
}

impl SidecarField {
    fn as_db_str(self) -> &'static str {
        match self {
            SidecarField::Identifier => "identifier",
            SidecarField::ImportBatch => "import_batch",
        }
    }

    fn from_db_str(value: &str) -> Self {
        match value {
            "import_batch" => SidecarField::ImportBatch,
            _ => SidecarField::Identifier,
        }
    }

    fn missing_value_message(self) -> String {
        match self {
            SidecarField::Identifier => "photo UUID is missing".to_string(),
            SidecarField::ImportBatch => "photo has no import batch UUID".to_string(),
        }
    }
}

/// What happened when we tried to put a portable identity field in a sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarIdentity {
    /// The sidecar carries the requested value (it already did, or we just wrote it).
    Bound,
    /// The queued copy is not reachable right now (offline/unmounted volume, or the
    /// original is gone). A normal state, not corruption — retry when it returns.
    Unreachable,
    /// The sidecar could not be written: read-only storage, an unparseable existing
    /// sidecar, a full disk. Carries the underlying error.
    Unwritable(String),
    /// The sidecar already carries a DIFFERENT identity. Overwriting it would destroy
    /// another photo's portable identity, so the file is left alone ("when uncertain,
    /// preserve") and the divergence is recorded for a human to resolve.
    Conflict(String),
}

// Distinguishing prefixes of the non-`Bound` `error_text()` outputs below. `error_text()`
// is the only writer of `pending_sidecar_identity.error` (see `record_sidecar_field_target`),
// so a stored row's prefix reliably identifies which `SidecarIdentity` variant produced it —
// `debt_state_from_error` uses this to recover a coarse state for display (CONTEXT.md §
// Identity: Unreachable / Unwritable / Conflict) without a redundant DB column. Both
// `error_text` and `debt_state_from_error` are defined from these same consts, so they
// cannot drift apart.
const UNWRITABLE_PREFIX: &str = "sidecar write failed";
const CONFLICT_PREFIX: &str = "sidecar carries a different identity";

impl SidecarIdentity {
    /// The message stored in `pending_sidecar_identity.error`; empty when bound.
    fn error_text(&self) -> String {
        match self {
            SidecarIdentity::Bound => String::new(),
            SidecarIdentity::Unreachable => "queued copy is not reachable".to_string(),
            SidecarIdentity::Unwritable(e) => format!("{UNWRITABLE_PREFIX}: {e}"),
            SidecarIdentity::Conflict(found) => {
                format!("{CONFLICT_PREFIX} ({found}); left untouched")
            }
        }
    }
}

/// Recover the coarse CONTEXT.md § Identity state — `"unreachable"`, `"unwritable"`, or
/// `"conflict"` — from a stored `pending_sidecar_identity.error`, for display. `Bound`
/// copies are deleted from the queue (see `record_sidecar_field_target`), so it never
/// appears here. Unrecognised text (there should be none — `error_text` is the only
/// writer) falls back to `"unreachable"`, the reading that costs a user the least if the
/// prefix match is ever wrong: repair keeps retrying rather than a legitimately queued
/// copy silently reading as a hard failure.
fn debt_state_from_error(error: &str) -> &'static str {
    if error.starts_with(CONFLICT_PREFIX) {
        "conflict"
    } else if error.starts_with(UNWRITABLE_PREFIX) {
        "unwritable"
    } else {
        "unreachable"
    }
}

/// Ensure the file's XMP sidecar carries `uuid`. Pure filesystem work — it touches no
/// catalog and holds no lock, so it is safe to run on a blocking worker.
///
/// `found` is the identifier already read from this file's sidecar
/// ([`crate::xmp::read_identifier`]). Callers pass what they read for the upsert so the
/// common case (identity already on disk) parses the sidecar exactly once.
pub fn bind_sidecar_identity(
    photo_path: &Path,
    uuid: &str,
    found: Option<&str>,
) -> SidecarIdentity {
    match found {
        Some(existing) if existing == uuid => SidecarIdentity::Bound,
        Some(existing) => SidecarIdentity::Conflict(existing.to_string()),
        None => match crate::xmp::write_identifier(photo_path, uuid) {
            Ok(()) => SidecarIdentity::Bound,
            Err(e) => SidecarIdentity::Unwritable(e),
        },
    }
}

fn bind_sidecar_import_batch(photo_path: &Path, batch_uuid: &str) -> SidecarIdentity {
    match crate::xmp::write_import_batch(photo_path, batch_uuid) {
        Ok(()) => SidecarIdentity::Bound,
        Err(e) => SidecarIdentity::Unwritable(e),
    }
}

fn pending_sidecar_value(
    field_text: &str,
    photo_uuid: &str,
    import_batch_uuid: Option<String>,
) -> (SidecarField, Option<String>) {
    let field = SidecarField::from_db_str(field_text);
    let value = match field {
        SidecarField::Identifier => Some(photo_uuid.to_string()),
        SidecarField::ImportBatch => import_batch_uuid,
    };
    (field, value)
}

/// A photo whose sidecar still owes it a portable identity field, with the last failure.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingIdentity {
    pub photo_id: i64,
    /// The identity that must reach the sidecar (`photos.uuid`).
    pub uuid: String,
    /// The sidecar field still owed by this copy: `identifier` or `import_batch`.
    pub field: String,
    /// The value that must be written for `field` (photo UUID or import batch UUID).
    pub value: Option<String>,
    /// The photo's catalog-root-relative logical path, for display.
    pub path: String,
    /// The volume that contains the copy whose sidecar is pending.
    pub volume_id: i64,
    /// This copy's path relative to its volume's base — the other half of `volume_id`
    /// that identifies which physical copy owes the debt (`target_path` is the two
    /// joined; kept separate here so display can show "which volume" and "which path"
    /// independently, per issue #50).
    pub relative_path: String,
    /// Absolute path to the copy whose sidecar is pending, for display/diagnostics.
    pub target_path: String,
    /// The coarse CONTEXT.md § Identity state this copy is in: `"unreachable"`,
    /// `"unwritable"`, or `"conflict"` — never `"bound"` (bound copies are cleared from
    /// the queue, see `record_sidecar_field_target`). Derived from `error` by
    /// `debt_state_from_error`; display code should key off this, not the prose text.
    pub state: String,
    pub attempts: i64,
    pub error: String,
    pub queued_at: i64,
    pub last_attempt_at: i64,
}

/// Coarse counts over `pending_sidecar_identity`, for a summary badge that doesn't need
/// every row. See [`Catalog::summarize_pending_identity`].
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingIdentitySummary {
    /// Every queued copy, any field, any state (`Unreachable` / `Unwritable` / `Conflict`).
    pub total: i64,
    /// Of `total`, how many are `Conflict` — need a human, not a retry.
    pub conflicts: i64,
}

/// One copy's repair, planned under the catalog lock and runnable without it.
pub struct IdentityRepairPlan {
    pub photo_id: i64,
    field: SidecarField,
    volume_id: i64,
    relative_path: String,
    value: Option<String>,
    /// The physical copy whose sidecar failed earlier.
    target_path: PathBuf,
}

impl IdentityRepairPlan {
    /// Retry the binding for the queued copy. Pure filesystem work — call this OFF
    /// the catalog lock.
    pub fn run(&self) -> SidecarIdentity {
        if !self.target_path.exists() {
            return SidecarIdentity::Unreachable;
        }

        let Some(value) = self.value.as_deref() else {
            return SidecarIdentity::Unwritable(self.field.missing_value_message());
        };

        match self.field {
            SidecarField::Identifier => {
                let found = crate::xmp::read_identifier(&self.target_path);
                bind_sidecar_identity(&self.target_path, value, found.as_deref())
            }
            SidecarField::ImportBatch => bind_sidecar_import_batch(&self.target_path, value),
        }
    }
}

/// Outcome of a repair pass over the pending sidecar-identity queue.
#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityRepairSummary {
    /// Sidecar field now on disk for this queued copy; the pending row was cleared.
    pub repaired: usize,
    /// The queued copy is not reachable right now — left queued for the next pass.
    pub unreachable: usize,
    /// Retried and still failing (unwritable sidecar, or an identity conflict a
    /// human has to resolve). Left queued.
    pub failed: usize,
}

impl IdentityRepairSummary {
    pub fn tally(&mut self, outcome: &SidecarIdentity) {
        match outcome {
            SidecarIdentity::Bound => self.repaired += 1,
            SidecarIdentity::Unreachable => self.unreachable += 1,
            SidecarIdentity::Unwritable(_) | SidecarIdentity::Conflict(_) => self.failed += 1,
        }
    }
}

impl Catalog {
    /// Record the outcome of a UUID binding for one physical copy: clear that copy's
    /// identifier repair row when the identity reached its sidecar, otherwise queue
    /// (or re-stamp) that same copy for repair.
    ///
    /// This is the *only* place the queue is written, so a re-scan doubles as a repair
    /// pass for the file it re-reads without hiding debt for another copy.
    pub fn record_sidecar_identity(
        &self,
        photo_id: i64,
        photo_path: &Path,
        outcome: &SidecarIdentity,
    ) -> Result<()> {
        let (volume_id, relative_path) = self.volume_for_path(photo_path)?;
        self.record_sidecar_field_target(
            photo_id,
            SidecarField::Identifier,
            volume_id,
            &relative_path,
            outcome,
        )
    }

    fn record_sidecar_import_batch(
        &self,
        photo_id: i64,
        photo_path: &Path,
        outcome: &SidecarIdentity,
    ) -> Result<()> {
        let (volume_id, relative_path) = self.volume_for_path(photo_path)?;
        self.record_sidecar_field_target(
            photo_id,
            SidecarField::ImportBatch,
            volume_id,
            &relative_path,
            outcome,
        )
    }

    fn record_sidecar_field_target(
        &self,
        photo_id: i64,
        field: SidecarField,
        volume_id: i64,
        relative_path: &str,
        outcome: &SidecarIdentity,
    ) -> Result<()> {
        if matches!(outcome, SidecarIdentity::Bound) {
            self.conn.execute(
                "DELETE FROM pending_sidecar_identity
                 WHERE photo_id = ?1 AND field = ?2 AND volume_id = ?3 AND relative_path = ?4",
                params![photo_id, field.as_db_str(), volume_id, relative_path],
            )?;
            return Ok(());
        }
        let ts = now();
        self.conn.execute(
            "INSERT INTO pending_sidecar_identity
                 (photo_id, field, volume_id, relative_path, attempts, error, queued_at, last_attempt_at)
             VALUES(?1, ?2, ?3, ?4, 1, ?5, ?6, ?6)
             ON CONFLICT(photo_id, field, volume_id, relative_path) DO UPDATE SET
                 attempts        = attempts + 1,
                 error           = excluded.error,
                 last_attempt_at = excluded.last_attempt_at",
            params![
                photo_id,
                field.as_db_str(),
                volume_id,
                relative_path,
                outcome.error_text(),
                ts
            ],
        )?;
        Ok(())
    }

    /// Bind a photo's UUID to the file's sidecar, or queue a retryable repair — the
    /// whole observable operation in one call, for callers already off the main catalog
    /// lock (the scanner and the bundle indexer run on their own connections).
    ///
    /// `found` is the identifier already read from the sidecar; see
    /// [`bind_sidecar_identity`]. Returns the outcome so a caller can count it; it is
    /// already durable in the catalog either way.
    pub fn ensure_sidecar_identity(
        &self,
        photo_id: i64,
        photo_path: &Path,
        uuid: &str,
        found: Option<&str>,
    ) -> Result<SidecarIdentity> {
        let outcome = bind_sidecar_identity(photo_path, uuid, found);
        self.record_sidecar_identity(photo_id, photo_path, &outcome)?;
        Ok(outcome)
    }

    /// Bind the photo's immutable import-batch UUID to the sidecar, or queue a repair.
    pub fn ensure_sidecar_import_batch(
        &self,
        photo_id: i64,
        photo_path: &Path,
        batch_uuid: &str,
    ) -> Result<SidecarIdentity> {
        let outcome = bind_sidecar_import_batch(photo_path, batch_uuid);
        self.record_sidecar_import_batch(photo_id, photo_path, &outcome)?;
        Ok(outcome)
    }

    /// Every copy whose sidecar still owes it a portable identity field, oldest first.
    pub fn list_pending_identity(&self) -> Result<Vec<PendingIdentity>> {
        let mut stmt = self.conn.prepare(
            "SELECT q.photo_id, p.uuid, p.path, q.field, q.volume_id, v.base_path, q.relative_path,
                    q.attempts, q.error, q.queued_at, q.last_attempt_at, b.uuid
             FROM pending_sidecar_identity q
             JOIN photos p ON p.id = q.photo_id
             JOIN volumes v ON v.id = q.volume_id
             LEFT JOIN import_batches b ON b.id = p.import_batch_id
             ORDER BY q.queued_at, q.photo_id, q.field, q.volume_id, q.relative_path",
        )?;
        let rows = stmt.query_map([], |r| {
            let field_text: String = r.get(3)?;
            let photo_uuid: String = r.get(1)?;
            let import_batch_uuid: Option<String> = r.get(11)?;
            let (field, value) =
                pending_sidecar_value(&field_text, &photo_uuid, import_batch_uuid);
            let base: String = r.get(5)?;
            let relative_path: String = r.get(6)?;
            let target_path = Path::new(&base)
                .join(&relative_path)
                .to_string_lossy()
                .to_string();
            let error: String = r.get(8)?;
            let state = debt_state_from_error(&error).to_string();
            Ok(PendingIdentity {
                photo_id: r.get(0)?,
                uuid: photo_uuid,
                field: field.as_db_str().to_string(),
                value,
                path: r.get(2)?,
                volume_id: r.get(4)?,
                relative_path,
                target_path,
                state,
                attempts: r.get(7)?,
                error,
                queued_at: r.get(9)?,
                last_attempt_at: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many known photo copies are missing their UUID identity on disk.
    pub fn count_pending_identity(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT count(*) FROM pending_sidecar_identity WHERE field = 'identifier'",
                [],
                |r| r.get(0),
            )?)
    }

    /// Coarse counts over the whole pending-identity queue (every field, not just
    /// `identifier`) — cheap enough to compute without pulling every row across IPC, for
    /// a summary badge/header. `total` matches `list_pending_identity().len()`;
    /// `conflicts` is the subset a human must resolve rather than one a repair pass can
    /// retry into success (see CONTEXT.md § Identity).
    pub fn summarize_pending_identity(&self) -> Result<PendingIdentitySummary> {
        Ok(self.conn.query_row(
            "SELECT count(*), count(*) FILTER (WHERE error LIKE ?1)
             FROM pending_sidecar_identity",
            params![format!("{CONFLICT_PREFIX}%")],
            |r| {
                Ok(PendingIdentitySummary {
                    total: r.get(0)?,
                    conflicts: r.get(1)?,
                })
            },
        )?)
    }

    /// Plan the repair of every queued copy. PURE SQL, so it is safe to call while
    /// holding the catalog lock; run each plan off the lock and record the outcome
    /// afterwards.
    pub fn plan_identity_repairs(&self) -> Result<Vec<IdentityRepairPlan>> {
        let mut plans = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT q.photo_id, p.uuid, q.field, q.volume_id, q.relative_path, v.base_path, b.uuid
             FROM pending_sidecar_identity q
             JOIN photos p ON p.id = q.photo_id
             JOIN volumes v ON v.id = q.volume_id
             LEFT JOIN import_batches b ON b.id = p.import_batch_id
             ORDER BY q.queued_at, q.photo_id, q.field, q.volume_id, q.relative_path",
        )?;
        let rows = stmt.query_map([], |r| {
            let photo_uuid: String = r.get(1)?;
            let field_text: String = r.get(2)?;
            let import_batch_uuid: Option<String> = r.get(6)?;
            let (field, value) =
                pending_sidecar_value(&field_text, &photo_uuid, import_batch_uuid);
            let relative_path: String = r.get(4)?;
            let base: String = r.get(5)?;
            Ok(IdentityRepairPlan {
                photo_id: r.get(0)?,
                field,
                volume_id: r.get(3)?,
                target_path: Path::new(&base).join(&relative_path),
                relative_path,
                value,
            })
        })?;
        for plan in rows {
            plans.push(plan?);
        }
        Ok(plans)
    }

    /// Retry every queued repair on this connection, clearing the ones that succeed.
    /// Composes plan → IO → record for tests and simple callers; the Tauri command
    /// runs the same three steps with the lock released around the IO.
    pub fn repair_pending_identity(&self) -> Result<IdentityRepairSummary> {
        let mut summary = IdentityRepairSummary::default();
        for plan in self.plan_identity_repairs()? {
            let outcome = plan.run();
            self.record_sidecar_field_target(
                plan.photo_id,
                plan.field,
                plan.volume_id,
                &plan.relative_path,
                &outcome,
            )?;
            summary.tally(&outcome);
        }
        Ok(summary)
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_catalog(tag: &str) -> (Catalog, PathBuf) {
        let dir = std::env::temp_dir().join(format!("chairphoto-identity-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
        (catalog, root)
    }

    fn seed_photo(catalog: &Catalog, root: &Path, name: &str) -> (i64, PathBuf) {
        let path = root.join(name);
        std::fs::write(&path, b"raw-bytes").unwrap();
        let up = catalog.upsert_photo(&path, None, 1, 9).unwrap();
        (up.id, path)
    }

    /// Each non-`Bound` `SidecarIdentity` variant must round-trip through
    /// `list_pending_identity` as the matching CONTEXT.md § Identity state — this is
    /// what lets the UI tell an `Unreachable` copy (normal, not an error) apart from a
    /// `Conflict` (needs a human) or an `Unwritable` failure, per issue #50.
    #[test]
    fn list_pending_identity_reports_the_state_matching_each_outcome() {
        let (catalog, root) = temp_catalog("state-per-variant");
        let (unreachable_id, unreachable_path) = seed_photo(&catalog, &root, "unreachable.arw");
        let (unwritable_id, unwritable_path) = seed_photo(&catalog, &root, "unwritable.arw");
        let (conflict_id, conflict_path) = seed_photo(&catalog, &root, "conflict.arw");

        catalog
            .record_sidecar_identity(unreachable_id, &unreachable_path, &SidecarIdentity::Unreachable)
            .unwrap();
        catalog
            .record_sidecar_identity(
                unwritable_id,
                &unwritable_path,
                &SidecarIdentity::Unwritable("disk full".to_string()),
            )
            .unwrap();
        catalog
            .record_sidecar_identity(
                conflict_id,
                &conflict_path,
                &SidecarIdentity::Conflict("some-other-uuid".to_string()),
            )
            .unwrap();

        let pending = catalog.list_pending_identity().unwrap();
        assert_eq!(pending.len(), 3);
        let state_for = |id: i64| {
            pending
                .iter()
                .find(|p| p.photo_id == id)
                .unwrap_or_else(|| panic!("no pending row for photo {id}"))
                .state
                .clone()
        };
        assert_eq!(state_for(unreachable_id), "unreachable");
        assert_eq!(state_for(unwritable_id), "unwritable");
        assert_eq!(state_for(conflict_id), "conflict");

        // relative_path must identify the specific copy (independent of volume_id), not
        // come back empty/placeholder — the UI shows it next to the volume, per #50.
        let row = pending.iter().find(|p| p.photo_id == conflict_id).unwrap();
        assert_eq!(row.relative_path, "conflict.arw");
        assert!(row.target_path.ends_with("conflict.arw"));
    }

    /// Two copies of the SAME photo on different volumes must stay two independent
    /// rows/states — issue #50 explicitly calls out not collapsing them.
    #[test]
    fn two_copies_of_one_photo_on_different_volumes_stay_two_rows() {
        let (catalog, root) = temp_catalog("two-copies-two-volumes");
        let (photo_id, local_path) = seed_photo(&catalog, &root, "same-photo.arw");

        let other_dir = root.parent().unwrap().join("second-volume");
        std::fs::create_dir_all(&other_dir).unwrap();
        let other_volume = catalog
            .add_volume("Second", &other_dir, crate::catalog::VolumeKind::Backup)
            .unwrap();
        let other_path = other_dir.join("same-photo.arw");
        std::fs::write(&other_path, b"raw-bytes-2").unwrap();

        catalog
            .record_sidecar_identity(photo_id, &local_path, &SidecarIdentity::Unreachable)
            .unwrap();
        catalog
            .record_sidecar_identity(
                photo_id,
                &other_path,
                &SidecarIdentity::Conflict("different-uuid".to_string()),
            )
            .unwrap();

        let pending = catalog.list_pending_identity().unwrap();
        assert_eq!(pending.len(), 2, "one photo, two volumes, must be two rows: {pending:?}");
        assert!(pending.iter().all(|p| p.photo_id == photo_id));
        let volumes: std::collections::HashSet<i64> = pending.iter().map(|p| p.volume_id).collect();
        assert_eq!(volumes.len(), 2, "each copy must keep its own volume_id");
        assert!(volumes.contains(&other_volume));
        let states: std::collections::HashSet<&str> =
            pending.iter().map(|p| p.state.as_str()).collect();
        assert_eq!(states, std::collections::HashSet::from(["unreachable", "conflict"]));
    }

    #[test]
    fn summarize_pending_identity_counts_conflicts_separately_from_total() {
        // Deliberately unbalanced (2 unwritable, 1 unreachable, 1 conflict) so a
        // `conflicts` count that accidentally matched the wrong state would produce a
        // visibly different number, not a coincidentally-equal one.
        let (catalog, root) = temp_catalog("summarize");
        let (a, a_path) = seed_photo(&catalog, &root, "a.arw");
        let (b, b_path) = seed_photo(&catalog, &root, "b.arw");
        let (b2, b2_path) = seed_photo(&catalog, &root, "b2.arw");
        let (c, c_path) = seed_photo(&catalog, &root, "c.arw");

        catalog.record_sidecar_identity(a, &a_path, &SidecarIdentity::Unreachable).unwrap();
        catalog
            .record_sidecar_identity(b, &b_path, &SidecarIdentity::Unwritable("nope".to_string()))
            .unwrap();
        catalog
            .record_sidecar_identity(b2, &b2_path, &SidecarIdentity::Unwritable("nope2".to_string()))
            .unwrap();
        catalog
            .record_sidecar_identity(c, &c_path, &SidecarIdentity::Conflict("other".to_string()))
            .unwrap();

        let summary = catalog.summarize_pending_identity().unwrap();
        assert_eq!(summary.total, 4);
        assert_eq!(summary.conflicts, 1, "only the Conflict row should count, not the two Unwritable rows");
        assert_eq!(summary.total as usize, catalog.list_pending_identity().unwrap().len());
    }

    /// A repair pass that clears a row (bound) must also drop out of the summary —
    /// `summarize_pending_identity` must not double-count against a stale total.
    #[test]
    fn summarize_pending_identity_drops_repaired_rows() {
        let (catalog, root) = temp_catalog("summarize-repair");
        let (a, a_path) = seed_photo(&catalog, &root, "a.arw");
        catalog.record_sidecar_identity(a, &a_path, &SidecarIdentity::Unreachable).unwrap();
        assert_eq!(catalog.summarize_pending_identity().unwrap().total, 1);

        // Clearing (Bound) must delete the row, not just relabel it.
        catalog.record_sidecar_identity(a, &a_path, &SidecarIdentity::Bound).unwrap();
        let summary = catalog.summarize_pending_identity().unwrap();
        assert_eq!(summary.total, 0);
        assert_eq!(summary.conflicts, 0);
    }
}
