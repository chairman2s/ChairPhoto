//! Reconcile queue (E4): storage operations (backup/offload/restore) that can't run
//! while the NAS is away are queued here and drained when a backup volume is reachable.
//! See docs/storage-and-import.md ("Reconcile queue").
//!
//! This module owns the queue primitives + a synchronous `drain` that runs each pending
//! op through the E3 lifecycle. NOTE: `drain_pending_operations` holds the catalog lock
//! across all its file IO — fine for tests, but the future drain *command* must NOT just
//! wrap it in `spawn_blocking` (that would still serialise every other catalog command
//! for the whole batch). It should instead drain op-by-op with the E3 plan→IO→record
//! split: collect a plan under the lock, drop it, do the copy/verify off-thread, then
//! re-acquire to record + clear that op. The *trigger* (manual button vs. auto-detect on
//! NAS reappearance) and the prompt-then-go UX are deferred pending an owner decision.

use super::{Catalog, CatalogError, PendingOperation, Result, VolumeKind};
use rusqlite::params;
use std::time::{SystemTime, UNIX_EPOCH};

const KINDS: &[&str] = &["backup", "offload", "restore"];

/// Outcome of a drain pass.
#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DrainSummary {
    pub ran: usize,
    pub failed: usize,
    /// True when nothing was attempted because no backup volume is reachable.
    pub skipped_offline: bool,
}

impl Catalog {
    /// Queue a deferred operation (idempotent per kind+photo). Returns the row id.
    pub fn enqueue_operation(&self, kind: &str, photo_id: i64) -> Result<i64> {
        if !KINDS.contains(&kind) {
            return Err(CatalogError::Validation(format!("unknown operation: {kind}")));
        }
        self.conn.execute(
            "INSERT INTO pending_operations(kind, photo_id, created_at) VALUES(?1, ?2, ?3)
             ON CONFLICT(kind, photo_id) DO NOTHING",
            params![kind, photo_id, now()],
        )?;
        // Return the existing or newly-created row's id.
        Ok(self.conn.query_row(
            "SELECT id FROM pending_operations WHERE kind = ?1 AND photo_id = ?2",
            params![kind, photo_id],
            |r| r.get(0),
        )?)
    }

    /// All queued operations, oldest first.
    pub fn list_pending_operations(&self) -> Result<Vec<PendingOperation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, photo_id, status, error, created_at
             FROM pending_operations ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(PendingOperation {
                id: r.get(0)?,
                kind: r.get(1)?,
                photo_id: r.get(2)?,
                status: r.get(3)?,
                error: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn remove_operation(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM pending_operations WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Mark a queued op as failed, recording the error (kept for the next drain).
    pub fn set_operation_failed(&self, id: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_operations SET status = 'failed', error = ?1 WHERE id = ?2",
            params![error, id],
        )?;
        Ok(())
    }

    /// Run every queued op through the E3 lifecycle, using the single backup + local
    /// volumes. Each op is cleared on success or marked `failed` (with the error) on
    /// failure. If no backup volume is reachable, nothing is attempted (the queue is
    /// left intact to retry later) — this is the "drain when the NAS is detected" rule.
    pub fn drain_pending_operations(&self) -> Result<DrainSummary> {
        let mut summary = DrainSummary::default();

        // Resolve the single backup/local volumes and their reachability.
        let volumes = self.list_volumes()?;
        let backup = volumes.iter().find(|v| v.kind == VolumeKind::Backup);
        let local = volumes.iter().find(|v| v.kind == VolumeKind::Local);
        // Without a reachable backup, none of backup/offload/restore can proceed.
        match backup {
            Some(v) if v.reachable => {}
            _ => {
                summary.skipped_offline = true;
                return Ok(summary);
            }
        }
        let backup_id = backup.unwrap().id;

        for op in self.list_pending_operations()? {
            let result = match op.kind.as_str() {
                "backup" => self.backup_photo(op.photo_id, backup_id).map(|_| ()),
                "offload" => self.offload_photo(op.photo_id),
                "restore" => match local {
                    Some(l) => self.restore_photo(op.photo_id, l.id).map(|_| ()),
                    None => Err(CatalogError::Validation("no local volume".into())),
                },
                other => Err(CatalogError::Validation(format!("unknown operation: {other}"))),
            };
            match result {
                Ok(()) => {
                    self.remove_operation(op.id)?;
                    summary.ran += 1;
                }
                Err(e) => {
                    self.set_operation_failed(op.id, &e.to_string())?;
                    summary.failed += 1;
                }
            }
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
