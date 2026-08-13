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

use super::{Catalog, CatalogError, Result};
use rusqlite::{params, OptionalExtension};
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
    /// preserve") and the divergence is recorded for a human to resolve — through
    /// [`Catalog::resolve_identity_conflict`] (Adopt / Overwrite / Dismiss, #33).
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

/// The state to DISPLAY for one queue row: `"dismissed"` once a human stopped retrying it
/// (#33), otherwise the state its `error` describes.
///
/// Dismissal is a real column (`pending_sidecar_identity.dismissed_at`), not another prose
/// prefix — it is a decision, not a failure, so it never round-trips through `error`, and
/// the underlying reason stays readable in `error` for a user deciding whether to Restore.
fn debt_state(error: &str, dismissed_at: i64) -> &'static str {
    if dismissed_at != 0 {
        "dismissed"
    } else {
        debt_state_from_error(error)
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
            q.attempts, q.error, q.queued_at, q.last_attempt_at, b.uuid, q.dismissed_at
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
///
/// `?3` is the include-dismissed flag. A copy every one of whose fields has been dismissed
/// (#33) is out of the active queue — `sum(q.dismissed_at = 0) > 0` drops it — so this
/// page's row count stays a slice of `PendingIdentitySummary::total`, which counts the same
/// way. `?3 = 1` lists those copies too, matched by `total + dismissed` (the two are
/// disjoint by construction: a copy is in exactly one of them). The filter is a bound
/// parameter rather than two separate query strings so both modes share one prepared
/// statement AND one query plan — the plan the `EXPLAIN QUERY PLAN` test pins.
const PENDING_IDENTITY_COPY_PAGE_QUERY: &str =
    "SELECT q.photo_id, p.path, q.volume_id, q.relative_path
     FROM pending_sidecar_identity q
     JOIN photos p ON p.id = q.photo_id
     GROUP BY q.photo_id, q.volume_id, q.relative_path
     HAVING ?3 = 1 OR sum(q.dismissed_at = 0) > 0
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
    let dismissed_at: i64 = r.get(12)?;
    let state = debt_state(&error, dismissed_at).to_string();
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
        dismissed_at,
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
    /// `"unwritable"`, `"conflict"`, or `"dismissed"` — never `"bound"` (bound copies are
    /// cleared from the queue, see `record_sidecar_field_target`). Derived by `debt_state`.
    pub state: String,
    pub attempts: i64,
    pub error: String,
    /// The queue's sort key for this file's `ORDER BY oldest-queued-first`.
    pub queued_at: i64,
    pub last_attempt_at: i64,
    /// When a human dismissed this row (#33), or 0 if they haven't. A dismissed row is
    /// kept for the record but skipped by the repair pass and excluded from the debt count.
    pub dismissed_at: i64,
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
    /// `"unwritable"`, `"conflict"`, or `"dismissed"`. Derived by `debt_state`.
    pub state: String,
    pub attempts: i64,
    pub error: String,
    pub last_attempt_at: i64,
    /// When a human dismissed this field (#33), or 0. A copy listed in the ACTIVE page can
    /// still carry a dismissed field (it owes another one), so this is per field, not a
    /// property of the page it came back on.
    pub dismissed_at: i64,
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
    /// Every queued copy still owing something (any field, any state) — counted once even
    /// if it owes both fields. A copy whose every field has been dismissed is NOT here; it
    /// is in `dismissed` instead. The two are disjoint, and together they cover the queue.
    pub total: i64,
    /// Of `total`, how many copies have at least one un-dismissed field in `Conflict` —
    /// need a human, not a retry.
    pub conflicts: i64,
    /// Copies whose every queued field has been dismissed (#33): kept for the record,
    /// never retried, and deliberately not counted as debt (CONTEXT.md § Identity).
    pub dismissed: i64,
}

/// What a human decided to do about one conflicted copy (#33). Deserialized from the
/// command payload as `"adopt"` / `"overwrite"` / `"dismiss"` / `"restore"`.
///
/// Deliberately has no `Default` and no string fallback: "the choice is explicit rather
/// than a default" is the acceptance criterion, and an unrecognised action must fail
/// deserialization rather than silently pick the destructive one. The three outcomes are
/// CONTEXT.md § Identity's vocabulary verbatim; `Restore` is the undo for `Dismiss`, so a
/// dismissal is never a one-way door.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IdentityConflictAction {
    /// The catalog takes the identity already in the copy's sidecar. Changes the catalog,
    /// never the file. Refused if another photo already holds that identity.
    Adopt,
    /// The copy's sidecar takes the catalog's identity. Changes the file, and destroys the
    /// identifier that was there. Backs the sidecar up first.
    Overwrite,
    /// Stop retrying this copy. Changes neither the catalog nor the file.
    Dismiss,
    /// Undo a `Dismiss`: put the copy back in the queue.
    Restore,
}

impl IdentityConflictAction {
    fn as_str(self) -> &'static str {
        match self {
            IdentityConflictAction::Adopt => "adopt",
            IdentityConflictAction::Overwrite => "overwrite",
            IdentityConflictAction::Dismiss => "dismiss",
            IdentityConflictAction::Restore => "restore",
        }
    }
}

/// What one [`Catalog::resolve_identity_conflict`] actually did, for the UI to report back.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityConflictOutcome {
    /// Echo of the action applied — the UI states what happened, never infers it.
    pub action: String,
    pub photo_id: i64,
    /// `photos.uuid` after the resolution. Adopt changes it; nothing else does.
    pub catalog_uuid: String,
    /// The identifier the sidecar carried BEFORE the resolution — the adopted value, or
    /// the one Overwrite destroyed. Empty for Dismiss/Restore, which read no file.
    pub previous_sidecar_uuid: String,
    /// Other copies of this photo whose queue row an Adopt re-evaluated (read-only). Adopt
    /// changes the photo's identity, so copies bound to the OLD one now diverge; see
    /// [`Catalog::recheck_other_copies_after_adopt`].
    pub rechecked_copies: usize,
    /// Where Overwrite preserved the previous sidecar, if it wrote a backup. `None` when a
    /// backup from an earlier write was already there and was deliberately left alone.
    pub sidecar_backup: Option<String>,
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
    ///
    /// `include_dismissed` widens the page from the ACTIVE queue (copies still owing an
    /// un-dismissed field — the slice of `PendingIdentitySummary::total`) to every copy in
    /// the table, so a user can see and Restore what they dismissed (#33). Either way, a
    /// listed copy comes back with ALL its fields, dismissed ones included and flagged
    /// `state: "dismissed"`: a copy owing one field and having dismissed the other is a
    /// single copy with one of each, and hiding half of it would misdescribe it.
    pub fn list_pending_identity_page(
        &self,
        limit: i64,
        offset: i64,
        include_dismissed: bool,
    ) -> Result<Vec<PendingIdentity>> {
        // Both statements run inside one read transaction so they share a single
        // snapshot: see this method's doc comment for why an autocommit pair could
        // otherwise return a copy the first statement (`PENDING_IDENTITY_COPY_PAGE_QUERY`)
        // just listed with `fields: []` from the second, if another connection deletes or
        // inserts rows for that copy in between.
        let tx = self.conn.unchecked_transaction()?;
        let mut copy_stmt = tx.prepare(PENDING_IDENTITY_COPY_PAGE_QUERY)?;
        let copies = copy_stmt
            .query_map(params![limit, offset, i64::from(include_dismissed)], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut field_stmt = tx.prepare(
            "SELECT field, error, attempts, last_attempt_at, dismissed_at
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
                    let dismissed_at: i64 = r.get(4)?;
                    let state = debt_state(&error, dismissed_at).to_string();
                    Ok(PendingIdentityField {
                        field,
                        state,
                        attempts: r.get(2)?,
                        error,
                        last_attempt_at: r.get(3)?,
                        dismissed_at,
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

    /// How many known photo copies are missing their UUID identity on disk. Excludes rows
    /// a human dismissed (#33) — those are a decision on the record, not outstanding debt.
    pub fn count_pending_identity(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT count(*) FROM pending_sidecar_identity
                 WHERE field = 'identifier' AND dismissed_at = 0",
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
    /// Dismissed rows (#33) are deliberately NOT debt: CONTEXT.md's Dismiss is "the copy
    /// stops counting as debt". A copy whose every field is dismissed leaves `total` for
    /// `dismissed`; a copy that still owes an un-dismissed field stays in `total` (it does
    /// still owe something) but a dismissed field of it never counts toward `conflicts`.
    /// Without this, dismissing the last conflict in a catalog would leave the panel
    /// reporting permanent debt no action could ever clear — the complaint #33 was filed
    /// about, just relocated.
    ///
    /// Matches `error` with `GLOB` (case-sensitive, `*`/`?` wildcards), not `LIKE`
    /// (case-insensitive, `_`/`%` wildcards), so this agrees with `debt_state_from_error`'s
    /// case-sensitive Rust `starts_with` instead of silently diverging on a differently
    /// cased error string.
    pub fn summarize_pending_identity(&self) -> Result<PendingIdentitySummary> {
        Ok(self.conn.query_row(
            "SELECT coalesce(sum(active > 0), 0),
                    coalesce(sum(active_conflicts > 0), 0),
                    coalesce(sum(active = 0), 0)
             FROM (
                 SELECT sum(dismissed_at = 0) AS active,
                        sum(dismissed_at = 0 AND error GLOB ?1) AS active_conflicts
                 FROM pending_sidecar_identity
                 GROUP BY photo_id, volume_id, relative_path
             )",
            params![format!("{CONFLICT_PREFIX}*")],
            |r| {
                Ok(PendingIdentitySummary {
                    total: r.get(0)?,
                    conflicts: r.get(1)?,
                    dismissed: r.get(2)?,
                })
            },
        )?)
    }

    /// Plan the repair of every queued copy. PURE SQL, so it is safe to call while
    /// holding the catalog lock; run each plan off the lock and record the outcome
    /// afterwards.
    ///
    /// Dismissed rows (#33) are skipped: "stop retrying this copy" is the whole content of
    /// a Dismiss, so a dismissed row must cost the pass neither a sidecar read nor a line
    /// in its summary. This is what lets a catalog whose only remaining debts are resolved
    /// or dismissed conflicts finish a pass with every counter at zero.
    pub fn plan_identity_repairs(&self) -> Result<Vec<IdentityRepairPlan>> {
        let mut plans = Vec::new();
        let mut stmt = self.conn.prepare(
            "SELECT q.photo_id, p.uuid, q.field, q.volume_id, q.relative_path, v.base_path, b.uuid
             FROM pending_sidecar_identity q
             JOIN photos p ON p.id = q.photo_id
             JOIN volumes v ON v.id = q.volume_id
             LEFT JOIN import_batches b ON b.id = p.import_batch_id
             WHERE q.dismissed_at = 0
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

    /// Resolve ONE conflicted copy the way a human decided to (#33): Adopt the identifier
    /// the file already carries, Overwrite the file with the catalog's, Dismiss the copy,
    /// or Restore a dismissed one. Every precondition is checked and reported by name — a
    /// refusal must say what it refused and why, never surface as a bare SQL error (#32).
    ///
    /// **Not** safe under the catalog lock: Adopt and Overwrite read (and Overwrite writes)
    /// the sidecar. Call it the way `commands::storage::resolve_identity_conflict` does —
    /// on a secondary connection, on a blocking worker — exactly as `repair_pending_identity`
    /// is called.
    ///
    /// The conflict is re-read from the file, never taken from the queue row's stored
    /// prose: that text records what was true at the last attempt, and the sidecar may have
    /// changed since. If it has, this refuses and says so rather than acting on a stale
    /// premise.
    ///
    /// Adopt changes `photos.uuid`, which is what catalog merge matches on and what
    /// `chairphoto://<uuid>` deep links address. See `docs/storage-and-import.md` § Identity
    /// conflicts for what that means for a catalog that has already been merged or bundled.
    pub fn resolve_identity_conflict(
        &self,
        photo_id: i64,
        volume_id: i64,
        relative_path: &str,
        action: IdentityConflictAction,
    ) -> Result<IdentityConflictOutcome> {
        let (error, dismissed_at, catalog_uuid, base_path): (String, i64, String, String) = self
            .conn
            .query_row(
                "SELECT q.error, q.dismissed_at, p.uuid, v.base_path
                 FROM pending_sidecar_identity q
                 JOIN photos p ON p.id = q.photo_id
                 JOIN volumes v ON v.id = q.volume_id
                 WHERE q.photo_id = ?1 AND q.field = 'identifier'
                   AND q.volume_id = ?2 AND q.relative_path = ?3",
                params![photo_id, volume_id, relative_path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| {
                CatalogError::NotFound(format!(
                    "no queued identity for photo {photo_id} at {relative_path} on volume \
                     {volume_id}"
                ))
            })?;
        let target = Path::new(&base_path).join(relative_path);

        let mut outcome = IdentityConflictOutcome {
            action: action.as_str().to_string(),
            photo_id,
            catalog_uuid: catalog_uuid.clone(),
            previous_sidecar_uuid: String::new(),
            rechecked_copies: 0,
            sidecar_backup: None,
        };

        if action == IdentityConflictAction::Restore {
            if dismissed_at == 0 {
                return Err(CatalogError::Validation(format!(
                    "{relative_path} is not dismissed, so there is nothing to restore"
                )));
            }
            self.set_identity_dismissal(photo_id, volume_id, relative_path, 0)?;
            return Ok(outcome);
        }

        // Adopt / Overwrite / Dismiss all answer the same question — "whose identity wins
        // for this copy?" — so all three require the copy to actually be in Conflict. A
        // copy that merely can't be written (or reached) has no such question to answer;
        // it needs a repair pass, not a decision.
        let state = debt_state_from_error(&error);
        if state != "conflict" {
            return Err(CatalogError::Validation(format!(
                "{relative_path} is not in conflict (state: {state}); Adopt, Overwrite and \
                 Dismiss resolve a sidecar that carries a different identity, and this one \
                 reports: {error}"
            )));
        }

        if action == IdentityConflictAction::Dismiss {
            if dismissed_at != 0 {
                return Err(CatalogError::Validation(format!(
                    "{relative_path} is already dismissed"
                )));
            }
            self.set_identity_dismissal(photo_id, volume_id, relative_path, now())?;
            return Ok(outcome);
        }

        // Adopt and Overwrite both act on the identifier that is in the file NOW.
        let found = crate::xmp::read_identifier(&target);
        let Some(found) = found else {
            return Err(CatalogError::Validation(if target.exists() {
                format!(
                    "{}'s sidecar no longer carries an identifier, so there is no conflict \
                     to resolve — run a repair pass to bind it",
                    target.display()
                )
            } else {
                format!(
                    "{} is not reachable right now, so its sidecar cannot be read; \
                     reconnect the volume (or relocate the photo) and try again",
                    target.display()
                )
            }));
        };
        if found == catalog_uuid {
            return Err(CatalogError::Validation(format!(
                "{}'s sidecar already carries this photo's identity ({found}); the recorded \
                 conflict is stale — run a repair pass to clear it",
                target.display()
            )));
        }
        outcome.previous_sidecar_uuid = found.clone();

        match action {
            IdentityConflictAction::Adopt => {
                // Refuse BEFORE the write, naming the photo that already holds it. The
                // `photos.uuid` UNIQUE constraint would also stop this, but only as an
                // opaque SQL error where a stated precondition belongs (#32) — and
                // "resolving one conflict manufactures another" is exactly the failure this
                // check exists to prevent.
                if let Some((other_id, other_path)) = self.photo_holding_uuid(&found, photo_id)? {
                    return Err(CatalogError::Validation(format!(
                        "cannot adopt {found}: photo {other_id} ({other_path}) already holds \
                         that identity, and no two photos may share one. Overwrite this \
                         copy's sidecar instead, or resolve the other photo first"
                    )));
                }
                self.conn.execute(
                    "UPDATE photos SET uuid = ?1, updated_at = ?2 WHERE id = ?3",
                    params![found, now(), photo_id],
                )?;
                outcome.catalog_uuid = found.clone();
                // This copy is bound by construction — its sidecar is where the identity
                // came from.
                self.record_sidecar_field_target(
                    photo_id,
                    SidecarField::Identifier,
                    volume_id,
                    relative_path,
                    &SidecarIdentity::Bound,
                )?;
                outcome.rechecked_copies = self.recheck_other_copies_after_adopt(
                    photo_id,
                    &found,
                    volume_id,
                    relative_path,
                )?;
            }
            IdentityConflictAction::Overwrite => {
                let backup = crate::xmp::overwrite_identifier(&target, &catalog_uuid)
                    .map_err(CatalogError::Io)?;
                outcome.sidecar_backup = backup.map(|p| p.to_string_lossy().to_string());
                self.record_sidecar_field_target(
                    photo_id,
                    SidecarField::Identifier,
                    volume_id,
                    relative_path,
                    &SidecarIdentity::Bound,
                )?;
            }
            IdentityConflictAction::Dismiss | IdentityConflictAction::Restore => unreachable!(
                "Dismiss and Restore return above, before any sidecar is read"
            ),
        }
        Ok(outcome)
    }

    /// The other photo already holding `uuid`, if any — `(id, path)` so a refusal can name
    /// it. `photos.uuid` is `UNIQUE`, so this is at most one row.
    fn photo_holding_uuid(&self, uuid: &str, except_photo_id: i64) -> Result<Option<(i64, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, path FROM photos WHERE uuid = ?1 AND id <> ?2",
                params![uuid, except_photo_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    fn set_identity_dismissal(
        &self,
        photo_id: i64,
        volume_id: i64,
        relative_path: &str,
        dismissed_at: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_sidecar_identity SET dismissed_at = ?4
             WHERE photo_id = ?1 AND field = 'identifier'
               AND volume_id = ?2 AND relative_path = ?3",
            params![photo_id, volume_id, relative_path, dismissed_at],
        )?;
        Ok(())
    }

    /// After an Adopt, re-evaluate the photo's OTHER copies against the identity it just
    /// took. Returns how many copies were re-recorded.
    ///
    /// Adopt changes `photos.uuid`, so a copy that was correctly Bound to the previous
    /// identity now carries somebody else's as far as the catalog is concerned. Left alone,
    /// the catalog would silently believe those copies were bound when they are not —
    /// exactly the "identity exists only in this one SQLite file" failure this module
    /// exists to prevent.
    ///
    /// **Read-only with respect to files**: CONTEXT.md's Adopt "changes the catalog, never
    /// the file", and that must hold for the photo's other copies too, so this classifies
    /// each one and records the outcome without ever calling a sidecar writer:
    ///
    /// - carries the adopted identity → `Bound`; any stale queue row is cleared.
    /// - carries a different one → `Conflict`; queued for its own human decision.
    /// - not reachable → `Unreachable`; queued, because we could not check it. A repair
    ///   pass re-reads it once the volume is back, and reports the truth then.
    /// - reachable but carries no identifier → left exactly as it was. That copy owes an
    ///   ordinary identifier WRITE, which is the repair pass's job, not Adopt's; whatever
    ///   queue row it already has (or doesn't) is unaffected by whose identity won here.
    fn recheck_other_copies_after_adopt(
        &self,
        photo_id: i64,
        uuid: &str,
        resolved_volume_id: i64,
        resolved_relative_path: &str,
    ) -> Result<usize> {
        let mut stmt = self.conn.prepare(
            "SELECT l.volume_id, l.relative_path, v.base_path
             FROM photo_locations l
             JOIN volumes v ON v.id = l.volume_id
             WHERE l.photo_id = ?1",
        )?;
        let copies = stmt
            .query_map(params![photo_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let mut rechecked = 0usize;
        for (volume_id, relative_path, base_path) in copies {
            if volume_id == resolved_volume_id && relative_path == resolved_relative_path {
                continue;
            }
            let target = Path::new(&base_path).join(&relative_path);
            let outcome = match crate::xmp::read_identifier(&target) {
                Some(found) if found == uuid => SidecarIdentity::Bound,
                Some(found) => SidecarIdentity::Conflict(found),
                None if !target.exists() => SidecarIdentity::Unreachable,
                None => continue,
            };
            self.record_sidecar_field_target(
                photo_id,
                SidecarField::Identifier,
                volume_id,
                &relative_path,
                &outcome,
            )?;
            rechecked += 1;
        }
        Ok(rechecked)
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

    /// A copy that is genuinely in `Conflict`, produced by the real path rather than by
    /// hand: a file whose sidecar carries `foreign_uuid` (written by the real XMP writer),
    /// catalogued under a different UUID, then bound through `ensure_sidecar_identity`.
    /// Returns `(photo_id, path, catalog_uuid)`.
    fn seed_conflicted_copy(
        catalog: &Catalog,
        root: &Path,
        name: &str,
        foreign_uuid: &str,
    ) -> (i64, PathBuf, String) {
        let path = root.join(name);
        std::fs::write(&path, b"raw-bytes").unwrap();
        crate::xmp::write_identifier(&path, foreign_uuid).unwrap();
        let up = catalog.upsert_photo(&path, None, 1, 9).unwrap();
        let found = crate::xmp::read_identifier(&path);
        let outcome = catalog
            .ensure_sidecar_identity(up.id, &path, &up.uuid, found.as_deref())
            .unwrap();
        assert_eq!(
            outcome,
            SidecarIdentity::Conflict(foreign_uuid.to_string()),
            "fixture must produce a real Conflict, not a hand-written row"
        );
        (up.id, path, up.uuid)
    }

    /// The copy coordinates `resolve_identity_conflict` takes — this file's tests always
    /// resolve a copy on the catalog-root volume, at `name`.
    fn copy_of(catalog: &Catalog, path: &Path) -> (i64, String) {
        catalog.volume_for_path(path).unwrap()
    }

    fn photo_uuid(catalog: &Catalog, photo_id: i64) -> String {
        catalog
            .conn()
            .query_row("SELECT uuid FROM photos WHERE id = ?1", params![photo_id], |r| {
                r.get(0)
            })
            .unwrap()
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

        let page1 = catalog.list_pending_identity_page(4, 0, false).unwrap();
        let page2 = catalog.list_pending_identity_page(4, 4, false).unwrap();
        assert_eq!(page1.len(), 4);
        assert_eq!(page2.len(), 2, "the last page is partial, not padded or empty");
        assert!(
            catalog.list_pending_identity_page(4, 6, false).unwrap().is_empty(),
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

        let page = catalog.list_pending_identity_page(10, 0, false).unwrap();
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
            .query_map(params![500i64, 0i64, 0i64], |r| r.get::<_, String>(3))
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

        let page = catalog.list_pending_identity_page(10, 0, false).unwrap();
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
        let after = catalog.list_pending_identity_page(10, 0, false).unwrap();
        assert!(after.is_empty(), "the concurrent delete must be visible to a NEW call");
    }

    // --- resolving a conflict (#33) -----------------------------------------------

    /// THE case that turns this fix into a new bug: adopting an identifier a DIFFERENT
    /// photo in the catalog already holds would make two rows share one identity, which
    /// catalog merge matches on. It must be refused before the write, naming the photo that
    /// holds it — not left to surface as a `photos.uuid` UNIQUE-constraint error (#32) —
    /// and nothing (catalog row, sidecar, queue row) may be changed by the refusal.
    #[test]
    fn adopting_an_identity_another_photo_already_holds_is_refused() {
        let (catalog, root, _dir) = temp_catalog("resolve-adopt-duplicate");
        // The other photo's real, catalogued identity — this is what makes the adopt a
        // duplicate rather than a merely unknown UUID.
        let (other_id, other_path) = seed_photo(&catalog, &root, "already-holds-it.arw");
        let other_uuid = photo_uuid(&catalog, other_id);

        let (photo_id, path, catalog_uuid) =
            seed_conflicted_copy(&catalog, &root, "conflicted.arw", &other_uuid);
        let (volume_id, relative_path) = copy_of(&catalog, &path);

        let err = catalog
            .resolve_identity_conflict(
                photo_id,
                volume_id,
                &relative_path,
                IdentityConflictAction::Adopt,
            )
            .expect_err("adopting a UUID another photo holds must be refused");
        let message = err.to_string();
        assert!(
            message.contains(&other_uuid) && message.contains(&other_id.to_string()),
            "the refusal must name the identity AND the photo that already holds it, got: \
             {message}"
        );
        assert!(
            message.contains("already-holds-it.arw"),
            "the refusal must name the other photo's path so the user can go look at it, \
             got: {message}"
        );

        // Refused means nothing moved: neither photo's identity changed, ...
        assert_eq!(photo_uuid(&catalog, photo_id), catalog_uuid);
        assert_eq!(photo_uuid(&catalog, other_id), other_uuid);
        // ... the file still carries what it carried, ...
        assert_eq!(
            crate::xmp::read_identifier(&path).as_deref(),
            Some(other_uuid.as_str())
        );
        assert!(crate::xmp::read_identifier(&other_path).is_none());
        // ... and the conflict is still queued for a different decision.
        let pending = catalog.list_pending_identity().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].state, "conflict");
        assert_eq!(pending[0].dismissed_at, 0);
    }

    /// Adopt: the catalog takes the identity already in the sidecar, the FILE is not
    /// touched at all (CONTEXT.md § Identity — "changes the catalog, never the file"), and
    /// the copy leaves the queue.
    #[test]
    fn adopt_takes_the_sidecars_identity_without_touching_the_file() {
        let (catalog, root, _dir) = temp_catalog("resolve-adopt");
        let (photo_id, path, catalog_uuid) =
            seed_conflicted_copy(&catalog, &root, "adopt-me.arw", "identity-from-the-file");
        let (volume_id, relative_path) = copy_of(&catalog, &path);
        let sidecar_before = std::fs::read_to_string(crate::xmp::sidecar_path(&path)).unwrap();

        let outcome = catalog
            .resolve_identity_conflict(
                photo_id,
                volume_id,
                &relative_path,
                IdentityConflictAction::Adopt,
            )
            .unwrap();

        assert_eq!(outcome.action, "adopt");
        assert_eq!(outcome.catalog_uuid, "identity-from-the-file");
        assert_eq!(outcome.previous_sidecar_uuid, "identity-from-the-file");
        assert_eq!(outcome.sidecar_backup, None, "Adopt writes no file, so it backs none up");
        assert_eq!(photo_uuid(&catalog, photo_id), "identity-from-the-file");
        assert_ne!(catalog_uuid, "identity-from-the-file", "sanity: it really changed");
        assert_eq!(
            std::fs::read_to_string(crate::xmp::sidecar_path(&path)).unwrap(),
            sidecar_before,
            "Adopt must not rewrite the sidecar — not even to re-stamp chairphoto:LastWrite"
        );
        assert!(
            catalog.list_pending_identity().unwrap().is_empty(),
            "the resolved copy is bound by construction and must leave the queue"
        );
        assert_eq!(catalog.summarize_pending_identity().unwrap().total, 0);
    }

    /// Adopt changes the photo's identity, so its OTHER copies — correctly bound to the
    /// PREVIOUS one a moment ago — now carry somebody else's. They must come back as their
    /// own queued conflicts rather than being silently assumed bound, and the file itself
    /// must still not be written (the re-check reads, never writes).
    #[test]
    fn adopt_requeues_another_copy_still_carrying_the_previous_identity() {
        let (catalog, root, _dir) = temp_catalog("resolve-adopt-other-copies");
        let (photo_id, path, catalog_uuid) =
            seed_conflicted_copy(&catalog, &root, "primary.arw", "identity-from-the-file");
        let (volume_id, relative_path) = copy_of(&catalog, &path);

        // A backup copy of the same photo, correctly bound to the catalog's CURRENT uuid.
        let backup_dir = root.parent().unwrap().join("backup-adopt");
        std::fs::create_dir_all(&backup_dir).unwrap();
        let backup = backup_dir.join("primary.arw");
        std::fs::write(&backup, b"raw-bytes").unwrap();
        let backup_volume = catalog
            .add_volume("Backup", &backup_dir, crate::catalog::VolumeKind::Backup)
            .unwrap();
        catalog
            .add_location(photo_id, backup_volume, "primary.arw", crate::catalog::LocationRole::Backup)
            .unwrap();
        assert_eq!(
            catalog
                .ensure_sidecar_identity(photo_id, &backup, &catalog_uuid, None)
                .unwrap(),
            SidecarIdentity::Bound
        );
        let backup_sidecar_before =
            std::fs::read_to_string(crate::xmp::sidecar_path(&backup)).unwrap();

        let outcome = catalog
            .resolve_identity_conflict(
                photo_id,
                volume_id,
                &relative_path,
                IdentityConflictAction::Adopt,
            )
            .unwrap();
        assert_eq!(outcome.rechecked_copies, 1, "the backup copy must be re-evaluated");

        let pending = catalog.list_pending_identity().unwrap();
        assert_eq!(pending.len(), 1, "the adopted copy left; the backup copy arrived");
        assert_eq!(pending[0].volume_id, backup_volume);
        assert_eq!(pending[0].state, "conflict");
        assert!(
            pending[0].error.contains(&catalog_uuid),
            "the backup copy's conflict must name the identity it still carries, got {:?}",
            pending[0].error
        );
        assert_eq!(
            std::fs::read_to_string(crate::xmp::sidecar_path(&backup)).unwrap(),
            backup_sidecar_before,
            "re-checking another copy must read it, never write it"
        );
    }

    /// Overwrite: the file takes the catalog's identity, and the sidecar it destroys is
    /// preserved first (AGENTS.md "XMP safety"), through the shared `SidecarDocument`
    /// backup path — not an open-coded copy in the catalog layer.
    #[test]
    fn overwrite_replaces_the_sidecar_identity_and_backs_it_up_first() {
        let (catalog, root, _dir) = temp_catalog("resolve-overwrite");
        let (photo_id, path, catalog_uuid) =
            seed_conflicted_copy(&catalog, &root, "overwrite-me.arw", "somebody-elses-uuid");
        let (volume_id, relative_path) = copy_of(&catalog, &path);
        let sidecar_before = std::fs::read_to_string(crate::xmp::sidecar_path(&path)).unwrap();

        let outcome = catalog
            .resolve_identity_conflict(
                photo_id,
                volume_id,
                &relative_path,
                IdentityConflictAction::Overwrite,
            )
            .unwrap();

        assert_eq!(outcome.action, "overwrite");
        assert_eq!(outcome.previous_sidecar_uuid, "somebody-elses-uuid");
        assert_eq!(outcome.catalog_uuid, catalog_uuid, "Overwrite never changes the catalog");
        assert_eq!(photo_uuid(&catalog, photo_id), catalog_uuid);
        assert_eq!(
            crate::xmp::read_identifier(&path).as_deref(),
            Some(catalog_uuid.as_str())
        );

        let backup = outcome.sidecar_backup.expect("the destroyed sidecar must be preserved");
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            sidecar_before,
            "the backup must be the sidecar as it was, byte for byte"
        );
        assert!(
            std::fs::read_to_string(&backup).unwrap().contains("somebody-elses-uuid"),
            "the destroyed identity must still be recoverable from the backup"
        );
        assert!(catalog.list_pending_identity().unwrap().is_empty());
    }

    /// Dismiss: neither the catalog nor the file changes, the row is KEPT for the record,
    /// the repair pass stops retrying it, and it stops counting as debt. Restore puts it
    /// back — a dismissal is never a one-way door.
    #[test]
    fn dismiss_stops_the_retry_and_the_debt_count_and_restore_undoes_it() {
        let (catalog, root, _dir) = temp_catalog("resolve-dismiss");
        let (photo_id, path, catalog_uuid) =
            seed_conflicted_copy(&catalog, &root, "dismiss-me.arw", "not-my-problem-uuid");
        let (volume_id, relative_path) = copy_of(&catalog, &path);
        let sidecar_before = std::fs::read_to_string(crate::xmp::sidecar_path(&path)).unwrap();

        catalog
            .resolve_identity_conflict(
                photo_id,
                volume_id,
                &relative_path,
                IdentityConflictAction::Dismiss,
            )
            .unwrap();

        assert_eq!(photo_uuid(&catalog, photo_id), catalog_uuid, "the catalog is unchanged");
        assert_eq!(
            std::fs::read_to_string(crate::xmp::sidecar_path(&path)).unwrap(),
            sidecar_before,
            "the file is unchanged"
        );

        // The record is kept — and readable, with its original reason still in `error`.
        let pending = catalog.list_pending_identity().unwrap();
        assert_eq!(pending.len(), 1, "the row is kept for the record, not deleted");
        assert_eq!(pending[0].state, "dismissed");
        assert_ne!(pending[0].dismissed_at, 0);
        assert!(pending[0].error.contains("not-my-problem-uuid"),
            "the reason must stay readable for a user deciding whether to Restore");

        // It stops counting as debt, and stops being retried.
        let summary = catalog.summarize_pending_identity().unwrap();
        assert_eq!((summary.total, summary.conflicts, summary.dismissed), (0, 0, 1));
        assert_eq!(catalog.count_pending_identity().unwrap(), 0);
        assert_eq!(catalog.plan_identity_repairs().unwrap().len(), 0);
        let attempts_before = pending[0].attempts;
        let repair = catalog.repair_pending_identity().unwrap();
        assert_eq!(
            (repair.bound, repair.unreachable, repair.conflicts, repair.failed),
            (0, 0, 0, 0),
            "a dismissed copy must cost the pass neither IO nor a line in its summary"
        );
        assert_eq!(
            catalog.list_pending_identity().unwrap()[0].attempts,
            attempts_before,
            "a skipped row must not have its attempt counter bumped"
        );

        // Restore puts it back in the queue, exactly as it was.
        catalog
            .resolve_identity_conflict(
                photo_id,
                volume_id,
                &relative_path,
                IdentityConflictAction::Restore,
            )
            .unwrap();
        let restored = catalog.list_pending_identity().unwrap();
        assert_eq!(restored[0].state, "conflict");
        assert_eq!(restored[0].dismissed_at, 0);
        let summary = catalog.summarize_pending_identity().unwrap();
        assert_eq!((summary.total, summary.conflicts, summary.dismissed), (1, 1, 0));
    }

    /// The acceptance criterion: a catalog whose only debts are conflicts must be able to
    /// reach a clean terminal state. Before #33 that was impossible — every pass re-read
    /// the same files and re-reported the same conflicts forever, with no action that could
    /// ever empty the queue. Each of the three resolutions is exercised on its own copy,
    /// because "the choice is explicit" means all three have to work, not just one.
    #[test]
    fn a_catalog_whose_only_debts_are_conflicts_can_reach_a_clean_terminal_state() {
        let (catalog, root, _dir) = temp_catalog("resolve-clean-terminal");
        let (adopt_id, adopt_path, _) =
            seed_conflicted_copy(&catalog, &root, "adopt.arw", "uuid-to-adopt");
        let (overwrite_id, overwrite_path, _) =
            seed_conflicted_copy(&catalog, &root, "overwrite.arw", "uuid-to-destroy");
        let (dismiss_id, dismiss_path, _) =
            seed_conflicted_copy(&catalog, &root, "dismiss.arw", "uuid-to-ignore");

        // The starting point #33 describes: the queue is all conflicts, and a repair pass
        // does nothing but re-report them.
        let summary = catalog.summarize_pending_identity().unwrap();
        assert_eq!((summary.total, summary.conflicts), (3, 3));
        let repair = catalog.repair_pending_identity().unwrap();
        assert_eq!((repair.bound, repair.conflicts, repair.failed), (0, 3, 0));
        assert_eq!(catalog.summarize_pending_identity().unwrap().total, 3, "nothing moved");

        for (photo_id, path, action) in [
            (adopt_id, &adopt_path, IdentityConflictAction::Adopt),
            (overwrite_id, &overwrite_path, IdentityConflictAction::Overwrite),
            (dismiss_id, &dismiss_path, IdentityConflictAction::Dismiss),
        ] {
            let (volume_id, relative_path) = copy_of(&catalog, path);
            catalog
                .resolve_identity_conflict(photo_id, volume_id, &relative_path, action)
                .unwrap_or_else(|e| panic!("{action:?} failed: {e}"));
        }

        let summary = catalog.summarize_pending_identity().unwrap();
        assert_eq!(
            (summary.total, summary.conflicts, summary.dismissed),
            (0, 0, 1),
            "no outstanding debt, no conflicts; the dismissed copy is on the record only"
        );
        let repair = catalog.repair_pending_identity().unwrap();
        assert_eq!(
            (repair.bound, repair.unreachable, repair.conflicts, repair.failed),
            (0, 0, 0, 0),
            "a repair pass over the resolved catalog must be clean in EVERY counter"
        );
    }

    /// Adopt/Overwrite/Dismiss answer "whose identity wins for this copy?". A copy that is
    /// merely unwritable (or unreachable) has no such question — it needs a repair pass —
    /// so the refusal names the state it found instead of doing something plausible.
    #[test]
    fn resolving_a_copy_that_is_not_in_conflict_is_refused_by_name() {
        let (catalog, root, _dir) = temp_catalog("resolve-not-a-conflict");
        let (photo_id, path) = seed_photo(&catalog, &root, "unwritable.arw");
        catalog
            .record_sidecar_identity(
                photo_id,
                &path,
                &SidecarIdentity::Unwritable("disk full".to_string()),
            )
            .unwrap();
        let (volume_id, relative_path) = copy_of(&catalog, &path);

        for action in [
            IdentityConflictAction::Adopt,
            IdentityConflictAction::Overwrite,
            IdentityConflictAction::Dismiss,
        ] {
            let message = catalog
                .resolve_identity_conflict(photo_id, volume_id, &relative_path, action)
                .expect_err("only a conflict can be resolved this way")
                .to_string();
            assert!(
                message.contains("not in conflict") && message.contains("unwritable"),
                "{action:?} must name the state it actually found, got: {message}"
            );
        }
        // Restore is refused too — nothing was dismissed.
        let message = catalog
            .resolve_identity_conflict(
                photo_id,
                volume_id,
                &relative_path,
                IdentityConflictAction::Restore,
            )
            .expect_err("nothing to restore")
            .to_string();
        assert!(message.contains("not dismissed"), "got: {message}");

        // A copy that isn't queued at all is a "not found", not a silent no-op.
        let message = catalog
            .resolve_identity_conflict(photo_id, volume_id, "nowhere.arw", IdentityConflictAction::Dismiss)
            .expect_err("an unqueued copy has nothing to resolve")
            .to_string();
        assert!(message.contains("no queued identity"), "got: {message}");
    }

    /// The conflict is re-read from the file at resolve time, never taken from the queue
    /// row's stored prose: between the failed bind and the user's decision, the sidecar may
    /// have been fixed by another tool. Acting on the stale premise would silently adopt an
    /// identity that is no longer there.
    #[test]
    fn a_conflict_that_the_file_no_longer_has_is_refused_rather_than_acted_on() {
        let (catalog, root, _dir) = temp_catalog("resolve-stale");
        let (photo_id, path, catalog_uuid) =
            seed_conflicted_copy(&catalog, &root, "changed.arw", "was-conflicting");
        let (volume_id, relative_path) = copy_of(&catalog, &path);

        // Somebody else fixed the sidecar in the meantime.
        crate::xmp::write_identifier(&path, &catalog_uuid).unwrap();

        for action in [IdentityConflictAction::Adopt, IdentityConflictAction::Overwrite] {
            let message = catalog
                .resolve_identity_conflict(photo_id, volume_id, &relative_path, action)
                .expect_err("the recorded conflict is stale")
                .to_string();
            assert!(
                message.contains("already carries this photo's identity"),
                "{action:?} must refuse on the CURRENT file, got: {message}"
            );
        }
        assert_eq!(photo_uuid(&catalog, photo_id), catalog_uuid, "nothing adopted");

        // And the queue is not stuck: an ordinary repair pass clears the stale row.
        let repair = catalog.repair_pending_identity().unwrap();
        assert_eq!((repair.bound, repair.conflicts), (1, 0));
        assert!(catalog.list_pending_identity().unwrap().is_empty());
    }

    /// An unreachable copy cannot be adopted from or overwritten — there is no sidecar to
    /// read — and the refusal says so rather than reporting a missing identifier.
    #[test]
    fn resolving_an_unreachable_copy_says_it_is_unreachable() {
        let (catalog, root, _dir) = temp_catalog("resolve-unreachable");
        let (photo_id, path, _) =
            seed_conflicted_copy(&catalog, &root, "gone.arw", "foreign-uuid");
        let (volume_id, relative_path) = copy_of(&catalog, &path);
        std::fs::remove_file(crate::xmp::sidecar_path(&path)).unwrap();
        std::fs::remove_file(&path).unwrap();

        let message = catalog
            .resolve_identity_conflict(photo_id, volume_id, &relative_path, IdentityConflictAction::Adopt)
            .expect_err("an unreachable copy cannot be adopted from")
            .to_string();
        assert!(message.contains("not reachable"), "got: {message}");

        // Dismiss still works: deciding to stop retrying needs no file at all.
        catalog
            .resolve_identity_conflict(photo_id, volume_id, &relative_path, IdentityConflictAction::Dismiss)
            .unwrap();
        assert_eq!(catalog.summarize_pending_identity().unwrap().dismissed, 1);
    }

    /// The action is explicit or it is nothing: the payload enum accepts exactly the four
    /// CONTEXT.md § Identity names, has no `Default`, and no near-miss (case, plural,
    /// empty) silently resolves to one — least of all the destructive one.
    #[test]
    fn the_resolution_action_has_no_default_and_no_fuzzy_match() {
        for (text, expected) in [
            ("\"adopt\"", IdentityConflictAction::Adopt),
            ("\"overwrite\"", IdentityConflictAction::Overwrite),
            ("\"dismiss\"", IdentityConflictAction::Dismiss),
            ("\"restore\"", IdentityConflictAction::Restore),
        ] {
            assert_eq!(
                serde_json::from_str::<IdentityConflictAction>(text).unwrap(),
                expected
            );
        }
        for text in ["\"Adopt\"", "\"OVERWRITE\"", "\"\"", "null", "\"overwrites\""] {
            assert!(
                serde_json::from_str::<IdentityConflictAction>(text).is_err(),
                "{text} must not deserialize to an action"
            );
        }
    }

    /// A dismissed copy leaves the ACTIVE page (it is not debt any more) but must stay
    /// reachable, or Restore would be unreachable from the UI. A copy that owes another
    /// field as well stays on the active page, carrying its dismissed field with it — it
    /// still owes something, and hiding half of it would misdescribe it.
    #[test]
    fn the_page_hides_fully_dismissed_copies_unless_asked_for_them() {
        let (catalog, root, _dir) = temp_catalog("resolve-page-dismissed");
        let (dismissed_id, dismissed_path, _) =
            seed_conflicted_copy(&catalog, &root, "dismissed.arw", "foreign-a");
        let (mixed_id, mixed_path, _) =
            seed_conflicted_copy(&catalog, &root, "mixed.arw", "foreign-b");
        // The mixed copy also owes its import batch, which is NOT dismissed.
        catalog
            .record_sidecar_import_batch(mixed_id, &mixed_path, &SidecarIdentity::Unreachable)
            .unwrap();

        for (photo_id, path) in [(dismissed_id, &dismissed_path), (mixed_id, &mixed_path)] {
            let (volume_id, relative_path) = copy_of(&catalog, path);
            catalog
                .resolve_identity_conflict(
                    photo_id,
                    volume_id,
                    &relative_path,
                    IdentityConflictAction::Dismiss,
                )
                .unwrap();
        }

        let summary = catalog.summarize_pending_identity().unwrap();
        assert_eq!(
            (summary.total, summary.conflicts, summary.dismissed),
            (1, 0, 1),
            "the mixed copy still owes its import batch; the fully dismissed one does not"
        );

        let active = catalog.list_pending_identity_page(50, 0, false).unwrap();
        assert_eq!(active.len() as i64, summary.total, "the active page IS the debt count");
        assert_eq!(active[0].photo_id, mixed_id);
        let states: std::collections::HashMap<&str, &str> = active[0]
            .fields
            .iter()
            .map(|f| (f.field.as_str(), f.state.as_str()))
            .collect();
        assert_eq!(states.get("identifier"), Some(&"dismissed"));
        assert_eq!(states.get("import_batch"), Some(&"unreachable"));

        let all = catalog.list_pending_identity_page(50, 0, true).unwrap();
        assert_eq!(
            all.len() as i64,
            summary.total + summary.dismissed,
            "asking for dismissed copies must show exactly the two disjoint groups"
        );
        assert!(all.iter().any(|p| p.photo_id == dismissed_id));
    }
}
