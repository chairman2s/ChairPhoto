//! Photo identity binding: keeping a photo's UUID in BOTH the catalog row and the
//! file's `xmp:Identifier` sidecar (AGENTS.md, "Photo identity").
//!
//! The catalog half is trivial — the row cannot exist without a UUID. The disk half
//! can fail: the storage may be read-only, the existing sidecar may not parse, the
//! volume may be offline, or the sidecar may already carry a *different* identity we
//! must not overwrite. Every caller used to log such a failure to stderr and carry on,
//! which leaves a catalogued photo whose identity exists only in this one SQLite file:
//! a merge or a re-import can no longer recognise the file, and nothing in the catalog
//! remembers that it should.
//!
//! So the observable operation is: **bind the identity, or record a retryable repair**.
//! [`bind_sidecar_identity`] is the pure-IO half (no catalog, safe to run off the lock),
//! [`Catalog::record_sidecar_identity`] the DB half, and [`Catalog::ensure_sidecar_identity`]
//! composes them for callers that already hold a connection off the main lock (the
//! scanner, the bundle indexer). [`Catalog::repair_pending_identity`] retries the whole
//! queue; the plan → IO → record split ([`Catalog::plan_identity_repairs`] +
//! [`IdentityRepairPlan::run`]) exists so the command layer can do the sidecar IO
//! without holding the catalog lock, mirroring the storage lifecycle in `lifecycle.rs`.

use super::{Catalog, Result};
use rusqlite::params;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// What happened when we tried to put a photo's UUID in its sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarIdentity {
    /// The sidecar carries this photo's UUID (it already did, or we just wrote it).
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

impl SidecarIdentity {
    /// The message stored in `pending_sidecar_identity.error`; empty when bound.
    fn error_text(&self) -> String {
        match self {
            SidecarIdentity::Bound => String::new(),
            SidecarIdentity::Unreachable => "queued copy is not reachable".to_string(),
            SidecarIdentity::Unwritable(e) => format!("sidecar write failed: {e}"),
            SidecarIdentity::Conflict(found) => {
                format!("sidecar carries a different identity ({found}); left untouched")
            }
        }
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

/// A photo whose sidecar still owes it its UUID, with the last failure.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingIdentity {
    pub photo_id: i64,
    /// The identity that must reach the sidecar (`photos.uuid`).
    pub uuid: String,
    /// The photo's catalog-root-relative logical path, for display.
    pub path: String,
    /// The volume that contains the copy whose sidecar is pending.
    pub volume_id: i64,
    /// Absolute path to the copy whose sidecar is pending, for display/diagnostics.
    pub target_path: String,
    pub attempts: i64,
    pub error: String,
    pub queued_at: i64,
    pub last_attempt_at: i64,
}

/// One copy's repair, planned under the catalog lock and runnable without it.
pub struct IdentityRepairPlan {
    pub photo_id: i64,
    volume_id: i64,
    relative_path: String,
    uuid: String,
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

        let found = crate::xmp::read_identifier(&self.target_path);
        bind_sidecar_identity(&self.target_path, &self.uuid, found.as_deref())
    }
}

/// Outcome of a repair pass over the pending-identity queue.
#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityRepairSummary {
    /// Identity now on disk for this queued copy; the pending row was cleared.
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
    /// Record the outcome of an identity binding for one physical copy: clear that
    /// copy's repair row when the identity reached its sidecar, otherwise queue (or
    /// re-stamp) that same copy for repair.
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
        self.record_sidecar_identity_target(photo_id, volume_id, &relative_path, outcome)
    }

    fn record_sidecar_identity_target(
        &self,
        photo_id: i64,
        volume_id: i64,
        relative_path: &str,
        outcome: &SidecarIdentity,
    ) -> Result<()> {
        if matches!(outcome, SidecarIdentity::Bound) {
            self.conn.execute(
                "DELETE FROM pending_sidecar_identity
                 WHERE photo_id = ?1 AND volume_id = ?2 AND relative_path = ?3",
                params![photo_id, volume_id, relative_path],
            )?;
            return Ok(());
        }
        let ts = now();
        self.conn.execute(
            "INSERT INTO pending_sidecar_identity
                 (photo_id, volume_id, relative_path, attempts, error, queued_at, last_attempt_at)
             VALUES(?1, ?2, ?3, 1, ?4, ?5, ?5)
             ON CONFLICT(photo_id, volume_id, relative_path) DO UPDATE SET
                 attempts        = attempts + 1,
                 error           = excluded.error,
                 last_attempt_at = excluded.last_attempt_at",
            params![photo_id, volume_id, relative_path, outcome.error_text(), ts],
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

    /// Every copy whose sidecar still owes it its UUID, oldest first.
    pub fn list_pending_identity(&self) -> Result<Vec<PendingIdentity>> {
        let mut stmt = self.conn.prepare(
            "SELECT q.photo_id, p.uuid, p.path, q.volume_id, v.base_path, q.relative_path,
                    q.attempts, q.error, q.queued_at, q.last_attempt_at
             FROM pending_sidecar_identity q
             JOIN photos p ON p.id = q.photo_id
             JOIN volumes v ON v.id = q.volume_id
             ORDER BY q.queued_at, q.photo_id, q.volume_id, q.relative_path",
        )?;
        let rows = stmt.query_map([], |r| {
            let base: String = r.get(4)?;
            let relative_path: String = r.get(5)?;
            let target_path = Path::new(&base)
                .join(&relative_path)
                .to_string_lossy()
                .to_string();
            Ok(PendingIdentity {
                photo_id: r.get(0)?,
                uuid: r.get(1)?,
                path: r.get(2)?,
                volume_id: r.get(3)?,
                target_path,
                attempts: r.get(6)?,
                error: r.get(7)?,
                queued_at: r.get(8)?,
                last_attempt_at: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many known photo copies are missing their identity on disk.
    pub fn count_pending_identity(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM pending_sidecar_identity", [], |r| r.get(0))?)
    }

    /// Plan the repair of every queued copy. PURE SQL, so it is safe to call while
    /// holding the catalog lock; run each plan off the lock and record the outcome
    /// afterwards.
    pub fn plan_identity_repairs(&self) -> Result<Vec<IdentityRepairPlan>> {
        let mut plans = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT q.photo_id, p.uuid, q.volume_id, q.relative_path, v.base_path
             FROM pending_sidecar_identity q
             JOIN photos p ON p.id = q.photo_id
             JOIN volumes v ON v.id = q.volume_id
             ORDER BY q.queued_at, q.photo_id, q.volume_id, q.relative_path",
        )?;
        let rows = stmt.query_map([], |r| {
            let relative_path: String = r.get(3)?;
            let base: String = r.get(4)?;
            Ok(IdentityRepairPlan {
                photo_id: r.get(0)?,
                uuid: r.get(1)?,
                volume_id: r.get(2)?,
                target_path: Path::new(&base).join(&relative_path),
                relative_path,
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
            self.record_sidecar_identity_target(
                plan.photo_id,
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
