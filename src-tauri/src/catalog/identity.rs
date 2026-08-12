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
// is the only PRODUCTION writer of `pending_sidecar_identity.error` (see
// `record_sidecar_field_target`) — but it is not the *only* writer: `performance_harness.rs`
// inserts a synthetic `"synthetic harness: local sidecar not materialized"` string directly
// for its benchmark fixture, bypassing `error_text()` entirely. That string matches neither
// prefix below, so it reads as `"unreachable"` via the fallback in `debt_state_from_error`
// — harmless for the harness, but proof this is a string match standing in for a real state
// column, not an invariant the type system enforces. A stored row's prefix reliably
// identifies which `SidecarIdentity` variant produced it for every *real* writer —
// `debt_state_from_error` uses this to recover a coarse state for display (CONTEXT.md §
// Identity: Unreachable / Unwritable / Conflict) without a redundant DB column, and
// `error_text` / `debt_state_from_error` are defined from these same consts so they can't
// drift apart from EACH OTHER. Before #33 hangs Adopt/Overwrite/Dismiss actions off this
// state, it should read from an actual enum/column instead of sniffing prose.
//
// `summarize_pending_identity`'s SQL matches these same consts with `GLOB` (case-sensitive,
// `*`/`?` wildcards) rather than `LIKE` (case-insensitive, and `_`/`%` are wildcards) so it
// agrees with this Rust-side case-sensitive `starts_with` rather than silently diverging on
// a differently-cased error string.
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
/// appears here. Unrecognised text — there should be none from `error_text()` (the only
/// PRODUCTION writer), but `performance_harness.rs` inserts a synthetic string directly
/// (see the const doc above) — falls back to `"unreachable"`, the reading that costs a
/// user the least if the prefix match is ever wrong: repair keeps retrying rather than a
/// legitimately queued copy silently reading as a hard failure.
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

/// Used ONLY by [`Catalog::list_pending_identity`], the unbounded flat (one row per
/// (copy, field) pair) accessor kept for internal Rust callers and this file's own test
/// suite — never shipped over IPC (see [`PendingIdentityRow`]'s doc). The frontend-facing,
/// paged accessor is [`Catalog::list_pending_identity_page`], which groups by COPY instead
/// — sharing this flat query between the two would let a copy owing both `identifier` and
/// `import_batch` come back as 2 rows there while `summarize_pending_identity` counted it
/// as 1 (e.g. a paging label reading "Showing 1–4 of 3" at small scale, "of 74488" at real
/// scale), so the two are kept deliberately separate. `ORDER BY` is oldest-queued first so
/// paging is stable across calls; nothing about this query's performance matters for the
/// UI thread since it is never called from a paging click (see
/// `list_pending_identity_page`'s doc for the query that IS on that path, and why it's
/// shaped differently).
const PENDING_IDENTITY_QUERY: &str =
    "SELECT q.photo_id, p.uuid, p.path, q.field, q.volume_id, v.base_path, q.relative_path,
            q.attempts, q.error, q.queued_at, q.last_attempt_at, b.uuid
     FROM pending_sidecar_identity q
     JOIN photos p ON p.id = q.photo_id
     JOIN volumes v ON v.id = q.volume_id
     LEFT JOIN import_batches b ON b.id = p.import_batch_id
     ORDER BY q.queued_at, q.photo_id, q.field, q.volume_id, q.relative_path";

/// The first of [`Catalog::list_pending_identity_page`]'s two queries — pages the
/// DISTINCT copies. Named/`const`, not inlined, so a test can run `EXPLAIN QUERY PLAN`
/// against the exact SQL that ships, and assert it never regresses to a temp-b-tree sort.
/// See that method's doc comment for the full reasoning on why this orders by natural key
/// rather than `queued_at`, and why it needs `idx_pending_sidecar_identity_copy`
/// (`schema.rs`) to avoid one.
const PENDING_IDENTITY_COPY_PAGE_QUERY: &str =
    "SELECT q.photo_id, p.path, q.volume_id, q.relative_path
     FROM pending_sidecar_identity q
     JOIN photos p ON p.id = q.photo_id
     GROUP BY q.photo_id, q.volume_id, q.relative_path
     ORDER BY q.photo_id, q.volume_id, q.relative_path
     LIMIT ?1 OFFSET ?2";

/// Row mapper for [`PENDING_IDENTITY_QUERY`].
fn map_pending_identity_row(r: &rusqlite::Row) -> rusqlite::Result<PendingIdentityRow> {
    let field_text: String = r.get(3)?;
    let photo_uuid: String = r.get(1)?;
    let import_batch_uuid: Option<String> = r.get(11)?;
    let (field, value) = pending_sidecar_value(&field_text, &photo_uuid, import_batch_uuid);
    let base: String = r.get(5)?;
    let relative_path: String = r.get(6)?;
    let target_path = Path::new(&base)
        .join(&relative_path)
        .to_string_lossy()
        .to_string();
    let error: String = r.get(8)?;
    let state = debt_state_from_error(&error).to_string();
    Ok(PendingIdentityRow {
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
}

/// One (copy, field) pair still owing a portable identity field, with the last failure.
/// Flat: a copy owing both `identifier` and `import_batch` is TWO of these.
///
/// This is [`Catalog::list_pending_identity`]'s row type only. It is never sent over IPC
/// (no `Serialize`) — the frontend debt panel (issue #50) renders
/// [`Catalog::list_pending_identity_page`]'s copy-grouped [`PendingIdentity`] instead. This
/// flat/field-grain shape is kept for this file's own test suite, which wants one
/// assertion per (copy, field), not pre-folded into a copy; production code has no caller
/// for it (the repair pass uses its own field-grained query,
/// [`Catalog::plan_identity_repairs`]).
#[derive(Debug, Clone)]
pub struct PendingIdentityRow {
    pub photo_id: i64,
    /// The identity that must reach the sidecar (`photos.uuid`).
    pub uuid: String,
    /// The sidecar field still owed by this copy: `identifier` or `import_batch`.
    pub field: String,
    /// The value that must be written for `field` (photo UUID or import batch UUID).
    pub value: Option<String>,
    /// The photo's catalog-root-relative logical path.
    pub path: String,
    /// The volume that contains the copy whose sidecar is pending.
    pub volume_id: i64,
    /// This copy's path relative to its volume's base — the other half of `volume_id`
    /// that identifies which physical copy owes the debt.
    pub relative_path: String,
    /// Absolute path to the copy whose sidecar is pending.
    pub target_path: String,
    /// The coarse CONTEXT.md § Identity state this row is in: `"unreachable"`,
    /// `"unwritable"`, or `"conflict"` — never `"bound"` (bound copies are cleared from
    /// the queue, see `record_sidecar_field_target`). Derived from `error` by
    /// `debt_state_from_error`.
    pub state: String,
    pub attempts: i64,
    pub error: String,
    /// The queue's sort key for this file's `ORDER BY oldest-queued-first`.
    pub queued_at: i64,
    pub last_attempt_at: i64,
}

/// One field a COPY still owes (`identifier` or `import_batch`), with its own retry
/// history — an entry in [`PendingIdentity::fields`]. A copy can owe up to both, queued
/// and retried independently, so each keeps its own attempts/error/last-attempt.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingIdentityField {
    /// `identifier` or `import_batch`.
    pub field: String,
    /// The coarse CONTEXT.md § Identity state this FIELD is in: `"unreachable"`,
    /// `"unwritable"`, or `"conflict"`. Derived from `error` by `debt_state_from_error`.
    pub state: String,
    pub attempts: i64,
    pub error: String,
    pub last_attempt_at: i64,
}

/// One COPY that still owes at least one sidecar field — [`Catalog::list_pending_identity_page`]'s
/// row type, and the only `PendingIdentity*` shape shipped over IPC.
///
/// Grouped by `(photo_id, volume_id, relative_path)` — CONTEXT.md's "Copy", and the same
/// unit [`PendingIdentitySummary::total`] counts. A copy owing both `identifier` and
/// `import_batch` is ONE of these, with both entries in `fields` — never two rows: sharing
/// the flat (copy, field)-row query with `list_pending_identity` would let this list's row
/// count exceed the copy-counted `total` the summary reports, e.g. `pagingLabel` rendering
/// "Showing 1–4 of 3", which is exactly what keeps the two queries separate.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingIdentity {
    pub photo_id: i64,
    /// The photo's catalog-root-relative logical path, for display.
    pub path: String,
    /// The volume that contains this copy.
    pub volume_id: i64,
    /// This copy's path relative to its volume's base — pair with `volume_id` to show
    /// "which volume" and "which path" independently, per issue #50.
    pub relative_path: String,
    /// Every sidecar field this copy still owes — 1 or 2 entries, never 0 (a copy with no
    /// owed field isn't in the queue at all).
    pub fields: Vec<PendingIdentityField>,
}

/// Coarse counts over `pending_sidecar_identity`, for a summary badge that doesn't need
/// every row. See [`Catalog::summarize_pending_identity`].
///
/// Both counts are in **copies** (`DISTINCT photo_id, volume_id, relative_path`), not queue
/// rows: `PRIMARY KEY(photo_id, field, volume_id, relative_path)` means one copy owing both
/// `identifier` and `import_batch` is two rows but one copy, and CONTEXT.md's "Copy" / issue
/// #50's "the number of copies currently in identity debt" are both about the physical copy,
/// not the (copy, field) pair.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingIdentitySummary {
    /// Every queued copy (any field, any state) — counted once even if it owes both fields.
    pub total: i64,
    /// Of `total`, how many copies have at least one field in `Conflict` — need a human,
    /// not a retry.
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
    /// "Bound" is the canonical CONTEXT.md § Identity term for this outcome — not
    /// "repaired".
    pub bound: usize,
    /// The queued copy is not reachable right now — left queued for the next pass. A
    /// normal state, not a failure.
    pub unreachable: usize,
    /// The sidecar already carries a different identity. **Not a failure** — the file was
    /// deliberately left untouched ("when uncertain, preserve") and a human has to resolve
    /// it (#33), not retry it. Kept out of `failed` so the UI never reports a Conflict as
    /// an error.
    pub conflicts: usize,
    /// Retried and is still genuinely failing (unwritable sidecar: read-only storage, an
    /// unparseable sidecar, a full disk). Left queued.
    pub failed: usize,
}

impl IdentityRepairSummary {
    pub fn tally(&mut self, outcome: &SidecarIdentity) {
        match outcome {
            SidecarIdentity::Bound => self.bound += 1,
            SidecarIdentity::Unreachable => self.unreachable += 1,
            SidecarIdentity::Conflict(_) => self.conflicts += 1,
            SidecarIdentity::Unwritable(_) => self.failed += 1,
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

    /// Every (copy, field) row still owing a portable identity field, oldest first. Flat —
    /// a copy owing both `identifier` and `import_batch` is two of these.
    ///
    /// Unbounded — pulls the whole queue, which reached 74,488 rows on the 100k harness
    /// shape in #20 (tens of MB of JSON if this crossed IPC — but it never does: this is
    /// Rust-only, not a Tauri command). Kept for internal/test callers that want one
    /// assertion per (copy, field) — see [`PendingIdentityRow`]'s struct doc: this file's
    /// own test suite is the only caller today (no production code path uses it — the
    /// repair pass has its own field-grained query, [`Catalog::plan_identity_repairs`]).
    /// The IPC command and the frontend debt panel use
    /// [`Catalog::list_pending_identity_page`] instead, which groups by copy.
    pub fn list_pending_identity(&self) -> Result<Vec<PendingIdentityRow>> {
        let mut stmt = self.conn.prepare(PENDING_IDENTITY_QUERY)?;
        let rows = stmt.query_map([], map_pending_identity_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// A bounded window over the pending-identity queue, grouped by COPY — for the Tauri
    /// command and the debt panel, so a 74k-row queue is never pulled across IPC in one
    /// payload, AND so the page's row count is always a slice of the same unit
    /// [`Catalog::summarize_pending_identity`] counts (copies), never more. A copy owing
    /// both `identifier` and `import_batch` is ONE row on this page, with both fields
    /// folded into `PendingIdentity::fields`, never split across two rows the way
    /// [`PENDING_IDENTITY_QUERY`] (flat, one row per (copy, field)) would — that split is
    /// exactly what would let this page's row count exceed
    /// `summarize_pending_identity`'s copy-counted total, e.g. a paging label reading
    /// "Showing 1–4 of 3".
    ///
    /// Two-step, both index-driven (see `idx_pending_sidecar_identity_copy` in
    /// `schema.rs`) and run inside one read transaction (below): (1) page the DISTINCT
    /// copies — `GROUP BY photo_id, volume_id, relative_path`, ordered and LIMIT/OFFSET'd
    /// by that same natural key; (2) for each copy on the page, a second small query
    /// fetches the field(s) it owes (at most 2, indexed by `photo_id`). The transaction
    /// exists because these two statements would otherwise run as independent autocommit
    /// reads: a write committed by another connection (a scan on its own
    /// `Catalog::open_secondary` connection is the expected case, not a contrived one —
    /// see `scanner/mod.rs`) strictly between them would be visible to statement (2) but
    /// not (1), so a copy step (1) had just listed could come back from step (2) with
    /// `fields: []` — contradicting [`PendingIdentity::fields`]'s "never 0" doc. Wrapping
    /// both in one `BEGIN DEFERRED` transaction gives them the same snapshot, so the pair
    /// can only ever agree.
    ///
    /// **Ordered by the copy's natural key, not by `queued_at`.** This is a deliberate
    /// choice, for two reasons: (a) once a copy can own two fields queued at different
    /// times, "this copy's queued_at" is ambiguous — natural-key order has no such
    /// ambiguity, and the stability guarantee paging actually needs ("Prev/Next never
    /// skips or repeats a copy") only requires SOME deterministic total order, not a
    /// specific one; (b) it is the only ordering [`EXPLAIN QUERY PLAN`] confirms
    /// `idx_pending_sidecar_identity_copy` can serve directly — no
    /// `USE TEMP B-TREE FOR ORDER BY`. Ordering by `MIN(queued_at)` per copy was tried and
    /// rejected: SQLite must fully materialize and sort the grouped result before applying
    /// `LIMIT`/`OFFSET` regardless of any index, because no btree can be simultaneously
    /// sorted by a grouping key and by an aggregate computed FROM that grouping.
    ///
    /// Without `idx_pending_sidecar_identity_copy`, `EXPLAIN QUERY PLAN` on this query
    /// shows `USE TEMP B-TREE FOR ORDER BY` — SQLite sorts the WHOLE matching set before
    /// applying `LIMIT`/`OFFSET`, on every page turn, while `with_catalog_blocking` holds
    /// the shared catalog mutex for the duration; the index removes that sort (pinned by
    /// `list_pending_identity_page_query_plan_has_no_temp_btree_sort` below). It does not
    /// make deep offsets cheap — SQLite's `OFFSET` still walks the skipped rows even off an
    /// index — only the sort. Measured end to end (this function, mutex held) on a
    /// 74,488-row table shaped like the #20 harness (50,000 distinct copies, 24,488 of them
    /// owing both fields), `LIMIT 500`: ~5 ms at offset 0, ~45 ms at offset 25,000, ~73 ms
    /// at offset 49,500 — cost scales with offset, not with table size once the index is
    /// in place.
    pub fn list_pending_identity_page(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<PendingIdentity>> {
        // Both statements run inside one read transaction so they share a single
        // snapshot: see this method's doc comment for why an autocommit pair could
        // otherwise return a copy the first statement (`PENDING_IDENTITY_COPY_PAGE_QUERY`)
        // just listed with `fields: []` from the second, if another connection deletes or
        // inserts rows for that copy in between.
        let tx = self.conn.unchecked_transaction()?;
        let mut copy_stmt = tx.prepare(PENDING_IDENTITY_COPY_PAGE_QUERY)?;
        let copies = copy_stmt
            .query_map(params![limit, offset], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut field_stmt = tx.prepare(
            "SELECT field, error, attempts, last_attempt_at
             FROM pending_sidecar_identity
             WHERE photo_id = ?1 AND volume_id = ?2 AND relative_path = ?3
             ORDER BY field",
        )?;
        let mut out = Vec::with_capacity(copies.len());
        for (photo_id, path, volume_id, relative_path) in copies {
            let fields = field_stmt
                .query_map(params![photo_id, volume_id, relative_path], |r| {
                    let field: String = r.get(0)?;
                    let error: String = r.get(1)?;
                    let state = debt_state_from_error(&error).to_string();
                    Ok(PendingIdentityField {
                        field,
                        state,
                        attempts: r.get(2)?,
                        error,
                        last_attempt_at: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.push(PendingIdentity {
                photo_id,
                path,
                volume_id,
                relative_path,
                fields,
            });
        }
        drop(copy_stmt);
        drop(field_stmt);
        tx.commit()?;
        Ok(out)
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
    /// a summary badge/header.
    ///
    /// Both counts are in **copies** (`DISTINCT photo_id, volume_id, relative_path`), not
    /// queue rows: `total` matches the total row count [`Catalog::list_pending_identity_page`]
    /// would return across all its pages, NOT `list_pending_identity().len()` (that one is
    /// flat, one row per (copy, field) — see its struct doc) — a copy owing both
    /// `identifier` and `import_batch` is two rows there but one copy here, matching
    /// CONTEXT.md's "Copy" and issue #50's "the number of copies currently in identity
    /// debt". `conflicts` is the subset of those copies with at least one field in
    /// `Conflict` — needs a human, not a retry (see CONTEXT.md § Identity).
    ///
    /// Matches `error` with `GLOB` (case-sensitive, `*`/`?` wildcards), not `LIKE`
    /// (case-insensitive, `_`/`%` wildcards), so this agrees with `debt_state_from_error`'s
    /// case-sensitive Rust `starts_with` instead of silently diverging on a differently
    /// cased error string.
    pub fn summarize_pending_identity(&self) -> Result<PendingIdentitySummary> {
        Ok(self.conn.query_row(
            "SELECT count(*), coalesce(sum(has_conflict), 0)
             FROM (
                 SELECT max(error GLOB ?1) AS has_conflict
                 FROM pending_sidecar_identity
                 GROUP BY photo_id, volume_id, relative_path
             )",
            params![format!("{CONFLICT_PREFIX}*")],
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

    /// A test's own temp directory, keyed by pid + tag and removed on drop — mirrors
    /// `thumbnails::tests::TestTmpDir` (see `src-tauri/src/thumbnails/mod.rs`).
    ///
    /// This file previously keyed on `tag` alone (`chairphoto-identity-test-{tag}`),
    /// which every `cargo test` process on the machine shares; `remove_dir_all` on entry
    /// then deletes a directory another process is still writing into, and nothing
    /// cleans up after a panicking test either. That is the same bug #45 / commit
    /// 9cd6d83 fixed for the thumbnail tests. `src-tauri/src/test_support.rs` on the
    /// #45 branch adds a shared
    /// `TestTmpDir` with this same shape but is not merged yet; this is a local copy in
    /// the same shape so the two converge trivially once it lands — collapse this into
    /// `test_support::TestTmpDir` post-merge instead of keeping both.
    struct TestTmpDir(PathBuf);

    impl TestTmpDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "chairphoto-identity-test-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestTmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Returns the catalog, its photo root, and the `TestTmpDir` guard — bind the guard
    /// too (`let (catalog, root, _dir) = temp_catalog(...)`) so it lives for the whole
    /// test; dropping it early deletes the directory the test is still using.
    fn temp_catalog(tag: &str) -> (Catalog, PathBuf, TestTmpDir) {
        let dir = TestTmpDir::new(tag);
        let root = dir.path().join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.path().join("test.chairphoto"), &root).unwrap();
        (catalog, root, dir)
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
        let (catalog, root, _dir) = temp_catalog("state-per-variant");
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
        let (catalog, root, _dir) = temp_catalog("two-copies-two-volumes");
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
        let (catalog, root, _dir) = temp_catalog("summarize");
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
        let (catalog, root, _dir) = temp_catalog("summarize-repair");
        let (a, a_path) = seed_photo(&catalog, &root, "a.arw");
        catalog.record_sidecar_identity(a, &a_path, &SidecarIdentity::Unreachable).unwrap();
        assert_eq!(catalog.summarize_pending_identity().unwrap().total, 1);

        // Clearing (Bound) must delete the row, not just relabel it.
        catalog.record_sidecar_identity(a, &a_path, &SidecarIdentity::Bound).unwrap();
        let summary = catalog.summarize_pending_identity().unwrap();
        assert_eq!(summary.total, 0);
        assert_eq!(summary.conflicts, 0);
    }

    /// `total`/`conflicts` must count DISTINCT copies (`photo_id, volume_id,
    /// relative_path`), not queue rows — `PRIMARY KEY(photo_id, field, volume_id,
    /// relative_path)` means one copy owing both `identifier` and `import_batch` is two
    /// rows but one copy. A version of `summarize_pending_identity` that filtered to
    /// `WHERE field = 'identifier'` would pass every OTHER test in this file, because
    /// every other fixture row happens to use `field = 'identifier'`. This test seeds a
    /// copy that owes ONLY `import_batch` — which such a filter would silently drop from
    /// the count — and a copy that owes BOTH fields — which a naive `count(*)` would
    /// double-count — so either mistake changes the numbers asserted below.
    #[test]
    fn summarize_pending_identity_counts_copies_not_field_pairs() {
        let (catalog, root, _dir) = temp_catalog("summarize-copies-not-pairs");
        let (a, a_path) = seed_photo(&catalog, &root, "a.arw");
        let (b, b_path) = seed_photo(&catalog, &root, "b.arw");
        let (c, c_path) = seed_photo(&catalog, &root, "c.arw");

        // Copy A owes only `identifier`.
        catalog
            .record_sidecar_identity(a, &a_path, &SidecarIdentity::Unreachable)
            .unwrap();
        // Copy B owes only `import_batch` — a `field = 'identifier'` filter would drop
        // this copy from the count entirely.
        catalog
            .record_sidecar_import_batch(b, &b_path, &SidecarIdentity::Unreachable)
            .unwrap();
        // Copy C owes BOTH fields: two queue rows, but ONE copy, and its import_batch
        // field is the only Conflict — so `conflicts` must also count copies, not rows.
        catalog
            .record_sidecar_identity(c, &c_path, &SidecarIdentity::Unreachable)
            .unwrap();
        catalog
            .record_sidecar_import_batch(
                c,
                &c_path,
                &SidecarIdentity::Conflict("other-uuid".to_string()),
            )
            .unwrap();

        // Sanity: 4 queue rows across 3 copies.
        assert_eq!(catalog.list_pending_identity().unwrap().len(), 4);

        let summary = catalog.summarize_pending_identity().unwrap();
        assert_eq!(summary.total, 3, "3 copies, not 4 (copy, field) rows");
        assert_eq!(
            summary.conflicts, 1,
            "copy C is one conflicted COPY, not two conflicted rows"
        );
    }

    /// `Conflict` is "not an error and not repairable by retrying" — `tally()` must route
    /// it to its own bucket, never into `failed`, so a post-repair summary never calls a
    /// Conflict a failure.
    #[test]
    fn repair_summary_tally_keeps_conflict_out_of_failed() {
        let mut summary = IdentityRepairSummary::default();
        summary.tally(&SidecarIdentity::Bound);
        summary.tally(&SidecarIdentity::Unreachable);
        summary.tally(&SidecarIdentity::Unwritable("disk full".to_string()));
        summary.tally(&SidecarIdentity::Conflict("other-uuid".to_string()));

        assert_eq!(summary.bound, 1);
        assert_eq!(summary.unreachable, 1);
        assert_eq!(summary.failed, 1, "only Unwritable is a genuine failure");
        assert_eq!(
            summary.conflicts, 1,
            "Conflict needs a human, not a retry — must never land in `failed`"
        );
    }

    /// The paged listing must page over COPIES, not (copy, field) rows — a copy owing
    /// both `identifier` and `import_batch` must occupy exactly ONE slot on the page, with
    /// both fields folded into `.fields`, so `rows.length` never exceeds
    /// `summarize_pending_identity().total` for the same queue. If the paged list instead
    /// shared its query with the flat, field-grain `list_pending_identity`, this exact
    /// fixture — 5 single-field copies + 1 two-field copy, 6 copies / 7 rows — would
    /// return 7 rows across the same pages, and the panel's `pagingLabel` would read
    /// "Showing 1–7 of 6". Also covers a partial last page and past-the-end paging.
    #[test]
    fn list_pending_identity_page_windows_every_copy_exactly_once() {
        let (catalog, root, _dir) = temp_catalog("page-windows");
        let names = ["a.arw", "b.arw", "c.arw", "d.arw", "e.arw"];
        for name in names {
            let (id, path) = seed_photo(&catalog, &root, name);
            catalog
                .record_sidecar_identity(id, &path, &SidecarIdentity::Unreachable)
                .unwrap();
        }
        // A 6th copy owes BOTH fields — must still occupy exactly one page slot.
        let (both_id, both_path) = seed_photo(&catalog, &root, "both.arw");
        catalog
            .record_sidecar_identity(both_id, &both_path, &SidecarIdentity::Unreachable)
            .unwrap();
        catalog
            .record_sidecar_import_batch(both_id, &both_path, &SidecarIdentity::Unreachable)
            .unwrap();

        let total = catalog.summarize_pending_identity().unwrap().total;
        assert_eq!(total, 6, "6 distinct copies, one of which owes two fields");
        assert_eq!(
            catalog.list_pending_identity().unwrap().len(),
            7,
            "sanity: 7 flat (copy, field) rows behind those 6 copies"
        );

        let page1 = catalog.list_pending_identity_page(4, 0).unwrap();
        let page2 = catalog.list_pending_identity_page(4, 4).unwrap();
        assert_eq!(page1.len(), 4);
        assert_eq!(page2.len(), 2, "the last page is partial, not padded or empty");
        assert!(
            catalog.list_pending_identity_page(4, 6).unwrap().is_empty(),
            "past the end of the queue: empty, not an error"
        );

        let both_row = page1
            .iter()
            .chain(page2.iter())
            .find(|p| p.photo_id == both_id)
            .expect("the two-field copy must appear on some page");
        assert_eq!(
            both_row.fields.len(),
            2,
            "the copy owing both fields is ONE row with two fields, not two rows"
        );
        let field_names: std::collections::HashSet<&str> =
            both_row.fields.iter().map(|f| f.field.as_str()).collect();
        assert_eq!(
            field_names,
            std::collections::HashSet::from(["identifier", "import_batch"])
        );

        let paged_ids: Vec<i64> = page1.iter().chain(page2.iter()).map(|p| p.photo_id).collect();
        assert_eq!(
            paged_ids.len() as i64,
            total,
            "every distinct copy covered exactly once, matching summarize_pending_identity's unit"
        );
        let mut unique_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
        assert!(
            paged_ids.iter().all(|id| unique_ids.insert(*id)),
            "no copy repeated across pages: {paged_ids:?}"
        );
    }

    /// The paged listing orders copies by their natural key (`photo_id, volume_id,
    /// relative_path`), NOT by `queued_at` (see the doc comment on
    /// `list_pending_identity_page` for why): once a copy can own two fields queued at
    /// different times, "this copy's queued_at" is ambiguous, and natural-key order is
    /// what lets `idx_pending_sidecar_identity_copy` drive the GROUP BY / ORDER BY /
    /// LIMIT-OFFSET without a temp b-tree sort. `queued_at` is forced to run in the
    /// OPPOSITE order from `photo_id` via a direct
    /// UPDATE (real wall-clock timestamps at second resolution can't be trusted to differ
    /// within a fast test) — a query that still (wrongly) sorted by `queued_at` would
    /// return the photos in reverse.
    #[test]
    fn list_pending_identity_page_orders_by_natural_key_not_queued_at() {
        let (catalog, root, _dir) = temp_catalog("page-natural-key-order");
        let names = ["a.arw", "b.arw", "c.arw", "d.arw", "e.arw"];
        let mut ids = Vec::new();
        for name in names {
            let (id, path) = seed_photo(&catalog, &root, name);
            catalog
                .record_sidecar_identity(id, &path, &SidecarIdentity::Unreachable)
                .unwrap();
            ids.push(id);
        }
        for (i, id) in ids.iter().enumerate() {
            let reversed_queued_at = (ids.len() - i) as i64; // a=5, b=4, c=3, d=2, e=1
            catalog
                .conn()
                .execute(
                    "UPDATE pending_sidecar_identity SET queued_at = ?1 WHERE photo_id = ?2",
                    params![reversed_queued_at, id],
                )
                .unwrap();
        }

        let page = catalog.list_pending_identity_page(10, 0).unwrap();
        let paged_ids: Vec<i64> = page.iter().map(|p| p.photo_id).collect();
        let mut expected = ids.clone();
        expected.sort();
        assert_eq!(
            paged_ids, expected,
            "must order by (photo_id, volume_id, relative_path) ascending, not by \
             queued_at — got {paged_ids:?}, photo_id order is {expected:?}"
        );
    }

    /// Guards the query shape directly, not just by measurement: runs
    /// `EXPLAIN QUERY PLAN` against the exact SQL [`Catalog::list_pending_identity_page`]
    /// ships (`PENDING_IDENTITY_COPY_PAGE_QUERY`) and asserts no plan step is a temp
    /// b-tree sort. A change that reverts the `ORDER BY` to an aggregate like
    /// `MIN(queued_at)`, or that drops/renames `idx_pending_sidecar_identity_copy`
    /// (`schema.rs`), fails this test immediately instead of only showing up as a slow
    /// page turn at 74k rows.
    #[test]
    fn list_pending_identity_page_query_plan_has_no_temp_btree_sort() {
        let (catalog, _root, _dir) = temp_catalog("page-query-plan");
        let mut stmt = catalog
            .conn()
            .prepare(&format!("EXPLAIN QUERY PLAN {PENDING_IDENTITY_COPY_PAGE_QUERY}"))
            .unwrap();
        let plan: Vec<String> = stmt
            .query_map(params![500i64, 0i64], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            !plan.iter().any(|line| line.contains("B-TREE")),
            "query plan uses a temp b-tree sort — the index or ORDER BY regressed: {plan:?}"
        );
        // Sanity: the plan must actually use the covering index, not just happen to
        // avoid a b-tree some other way (e.g. a full unordered scan would also lack one).
        assert!(
            plan.iter()
                .any(|line| line.contains("idx_pending_sidecar_identity_copy")),
            "expected the copy-covering index to drive this query, got: {plan:?}"
        );
    }

    /// D3: `list_pending_identity_page` runs a copy-page query, then — for each copy on
    /// the page — a field-lookup query. Outside a shared transaction, these are two
    /// independent autocommit reads: a write another connection commits strictly between
    /// them is visible to the second statement but not the first, so a copy the first
    /// statement already listed can come back from the second with `fields: []`,
    /// contradicting [`PendingIdentity::fields`]'s "never 0" doc. That other connection is
    /// not contrived — a concurrent scan writes this exact table on its own
    /// `Catalog::open_secondary` connection (see `scanner/mod.rs`).
    ///
    /// Forces exactly that interleaving deterministically, without any thread-timing
    /// gamble: register an `authorizer` on the primary connection (SQLite invokes it once
    /// per top-level `SELECT` it compiles) and, on the SECOND `Select` action — which is
    /// `list_pending_identity_page`'s field-lookup query; the copy-page query, and every
    /// row it returns, is already fully executed in Rust by the time that statement is
    /// even prepared — delete the seeded copy's row from a SEPARATE connection standing in
    /// for the concurrent scan, and commit it. If the two statements do not share a
    /// snapshot, the field lookup sees the deletion and comes back empty.
    #[test]
    fn list_pending_identity_page_survives_a_write_committed_between_its_two_queries() {
        let (catalog, root, _dir) = temp_catalog("page-torn-read");
        let (id, path) = seed_photo(&catalog, &root, "a.arw");
        catalog
            .record_sidecar_identity(id, &path, &SidecarIdentity::Unreachable)
            .unwrap();

        let secondary = Catalog::open_secondary(catalog.db_path(), &root).unwrap();
        let secondary = std::panic::AssertUnwindSafe(secondary);
        let selects_seen = std::sync::atomic::AtomicUsize::new(0);
        catalog.conn().authorizer(Some(move |ctx: rusqlite::hooks::AuthContext<'_>| {
            // Force whole-value capture of `secondary` (an `AssertUnwindSafe` wrapper)
            // rather than RFC 2229 disjoint capture of its `.0` field, which would recover
            // the un-wrapped `Catalog` and defeat the wrapper's purpose.
            let secondary = &secondary;
            if matches!(ctx.action, rusqlite::hooks::AuthAction::Select) {
                let n = selects_seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if n == 2 {
                    secondary
                        .0
                        .conn()
                        .execute(
                            "DELETE FROM pending_sidecar_identity WHERE photo_id = ?1",
                            params![id],
                        )
                        .unwrap();
                }
            }
            rusqlite::hooks::Authorization::Allow
        }));

        let page = catalog.list_pending_identity_page(10, 0).unwrap();
        catalog
            .conn()
            .authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>);

        assert_eq!(page.len(), 1, "the copy was already paged before the concurrent delete");
        assert_eq!(
            page[0].fields.len(),
            1,
            "the field lookup must see the SAME snapshot the copy-page query saw — an \
             empty `fields` here is exactly the torn read D3 describes"
        );

        // Sanity: the delete really did commit — a fresh call (a new transaction, a new
        // snapshot) now sees it gone.
        let after = catalog.list_pending_identity_page(10, 0).unwrap();
        assert!(after.is_empty(), "the concurrent delete must be visible to a NEW call");
    }
}
