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
    /// No copy of the file is reachable right now (offline/unmounted volume, or the
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
            SidecarIdentity::Unreachable => "no reachable copy of this photo".to_string(),
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
    pub attempts: i64,
    pub error: String,
    pub queued_at: i64,
    pub last_attempt_at: i64,
}

/// One photo's repair, planned under the catalog lock and runnable without it.
pub struct IdentityRepairPlan {
    pub photo_id: i64,
    uuid: String,
    /// Where the file might be, best role first (from the resolver's candidate list).
    candidates: Vec<PathBuf>,
}

impl IdentityRepairPlan {
    /// Retry the binding. Pure filesystem work — call this OFF the catalog lock.
    pub fn run(&self) -> SidecarIdentity {
        let Some(path) = self.candidates.iter().find(|p| p.exists()) else {
            return SidecarIdentity::Unreachable;
        };
        let found = crate::xmp::read_identifier(path);
        bind_sidecar_identity(path, &self.uuid, found.as_deref())
    }
}

/// Outcome of a repair pass over the pending-identity queue.
#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityRepairSummary {
    /// Identity now on disk; the pending row was cleared.
    pub repaired: usize,
    /// No copy reachable right now — left queued for the next pass.
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
    /// Record the outcome of an identity binding: clear the photo's repair row when the
    /// identity reached the sidecar, otherwise queue (or re-stamp) it for repair.
    ///
    /// This is the *only* place the queue is written, so "bound" and "not bound" can
    /// never both be true for a photo. It also means a re-scan doubles as a repair
    /// pass: every file it re-reads clears or re-stamps that photo's row.
    pub fn record_sidecar_identity(
        &self,
        photo_id: i64,
        outcome: &SidecarIdentity,
    ) -> Result<()> {
        if matches!(outcome, SidecarIdentity::Bound) {
            self.conn.execute(
                "DELETE FROM pending_sidecar_identity WHERE photo_id = ?1",
                params![photo_id],
            )?;
            return Ok(());
        }
        let ts = now();
        self.conn.execute(
            "INSERT INTO pending_sidecar_identity
                 (photo_id, attempts, error, queued_at, last_attempt_at)
             VALUES(?1, 1, ?2, ?3, ?3)
             ON CONFLICT(photo_id) DO UPDATE SET
                 attempts        = attempts + 1,
                 error           = excluded.error,
                 last_attempt_at = excluded.last_attempt_at",
            params![photo_id, outcome.error_text(), ts],
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
        self.record_sidecar_identity(photo_id, &outcome)?;
        Ok(outcome)
    }

    /// Every photo whose sidecar still owes it its UUID, oldest first.
    pub fn list_pending_identity(&self) -> Result<Vec<PendingIdentity>> {
        let mut stmt = self.conn.prepare(
            "SELECT q.photo_id, p.uuid, p.path, q.attempts, q.error, q.queued_at, q.last_attempt_at
             FROM pending_sidecar_identity q JOIN photos p ON p.id = q.photo_id
             ORDER BY q.queued_at, q.photo_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PendingIdentity {
                photo_id: r.get(0)?,
                uuid: r.get(1)?,
                path: r.get(2)?,
                attempts: r.get(3)?,
                error: r.get(4)?,
                queued_at: r.get(5)?,
                last_attempt_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many photos are missing their identity on disk.
    pub fn count_pending_identity(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM pending_sidecar_identity", [], |r| r.get(0))?)
    }

    /// Plan the repair of every queued photo. PURE SQL (the resolver's candidate list
    /// never stats the filesystem), so it is safe to call while holding the catalog
    /// lock; run each plan off the lock and record the outcome afterwards.
    pub fn plan_identity_repairs(&self) -> Result<Vec<IdentityRepairPlan>> {
        let mut plans = Vec::new();
        for pending in self.list_pending_identity()? {
            let candidates = self
                .photo_path_candidates(pending.photo_id)?
                .into_iter()
                .map(|c| c.path)
                .collect();
            plans.push(IdentityRepairPlan {
                photo_id: pending.photo_id,
                uuid: pending.uuid,
                candidates,
            });
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
            self.record_sidecar_identity(plan.photo_id, &outcome)?;
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
