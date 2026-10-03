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
//! scanner, the bundle indexer). [`Catalog::run_identity_repair`] retries the queue a page
//! at a time; the plan → IO → record split ([`Catalog::plan_identity_repairs_page`] +
//! [`IdentityRepairPlan::run`]) exists so the command layer can do the sidecar IO
//! without holding the catalog lock, mirroring the storage lifecycle in `lifecycle.rs`.
//!
//! `pending_sidecar_identity` is still named for the original UUID debt, but it now carries
//! a `field` discriminator so the same retry path also covers `chairphoto:ImportBatch`.
//!
//! # Who owns a queue row (#34)
//!
//! Two writers reach the same `pending_sidecar_identity` row from two connections: the
//! repair pass ([`Catalog::run_identity_repair`], one job, on a blocking worker) and a
//! human's decision ([`Catalog::resolve_identity_conflict`], one copy, on another). Before
//! #34 they were serialized only by SQLite's write lock, which orders the statements but
//! decides nothing: a resolution landing while the pass was between "read the file" and
//! "record the outcome" was overwritten by the pass's stale record, so an Adopt could be
//! silently undone and a Dismiss could be re-queued.
//!
//! The rule now is **the pass owns a row only while the row is still the one it planned**:
//!
//! 1. Every plan carries the row's version — its `attempts` and `last_attempt_at`.
//! 2. The value and version are re-read immediately before the sidecar IO
//!    ([`Catalog::refresh_identity_repair`]), so a page-old plan never acts on stale data
//!    and a row resolved or dismissed since is skipped without touching the file.
//! 3. The record is a compare-and-set on that version *and* on `dismissed_at = 0`
//!    ([`Catalog::record_planned_repair`]). If anyone wrote the row during the IO the
//!    pass's result is dropped, counted as [`IdentityRepairSummary::superseded`], and the
//!    newer decision stands.
//!
//! A resolution is therefore always the winner, and deliberately does **not** trip the
//! pass: killing a 74k-row pass because one row got a decision would be a far worse trade
//! than dropping that row's result. The pass's own ownership — job id, abort flag, status
//! slot, catalog switch — is the `app::jobs` protocol, one layer up.

use super::busy::{is_busy, retry_busy, retry_busy_or_aborted};
use super::{Catalog, CatalogError, Result};
use rusqlite::{params, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// True when `value`, as read from a sidecar's `xmp:Identifier`, can be a photo's identity: a
/// non-nil UUID in the hyphenated form ChairPhoto writes and `photos.uuid` holds (#141).
///
/// Anything else — a DAM asset id, a path, a braced or `urn:uuid:` spelling — is somebody
/// else's identifier. It is never adopted as `photos.uuid` and never used to re-home a row
/// (two files carrying the same foreign id would otherwise take turns owning one row), but
/// it is not ours to remove either: [`bind_sidecar_identity`] still sees it, reports a
/// conflict, and leaves the sidecar alone until a person resolves it.
pub fn is_photo_identity(value: &str) -> bool {
    value.len() == 36 && uuid::Uuid::parse_str(value).is_ok_and(|u| !u.is_nil())
}

/// `value` as `photos.uuid` holds it, when it is a photo identity: lowercase, as ChairPhoto
/// mints it (#146 L5). [`is_photo_identity`] accepts either case, and another tool may
/// upper-case a UUID it rewrites, so every place an identity enters the catalog or is looked
/// up in it goes through here — otherwise one photo would read as two identities.
pub fn canonical_photo_identity(value: &str) -> Option<String> {
    is_photo_identity(value).then(|| value.to_ascii_lowercase())
}

/// The UUID v5 namespace under which a legacy, non-UUID identifier is turned into a photo
/// identity (#146): `0a9b4715-6420-429d-b004-48a4fda6ddf9`, generated once for this purpose.
///
/// **Never change it.** Its whole point is that every catalog, on every machine and in every
/// version, maps the same legacy value to the same UUID. Two catalogs that each held a photo
/// as `dam:asset/1` before #141 re-mint it independently (schema v23) and must still agree on
/// its identity, because identity is the merge key and a bundle does not carry the legacy
/// value. A different namespace would split every such photo in two at the next merge.
pub const LEGACY_IDENTITY_NAMESPACE: uuid::Uuid =
    uuid::Uuid::from_u128(0x0a9b4715_6420_429d_b004_48a4fda6ddf9);

/// The photo identity a legacy, non-UUID identifier `value` stands for: UUID v5 of `value`
/// under [`LEGACY_IDENTITY_NAMESPACE`], in the canonical lowercase spelling.
///
/// The one exception to "a UUID v4 on first import" (AGENTS.md): a photo minted on import
/// still gets a v4, but a value that already served as a photo's identity before #141 — in
/// this catalog's rows or in an old bundle's manifest — is re-minted by this function, so
/// that catalogs which never met agree on the result. Distinct values give distinct
/// identities, so rows whose `photos.uuid` was `UNIQUE` stay distinct.
pub fn legacy_photo_identity(value: &str) -> String {
    uuid::Uuid::new_v5(&LEGACY_IDENTITY_NAMESPACE, value.as_bytes()).to_string()
}

/// The identity `value` names, for a caller handing the catalog an identity it trusts — a
/// bundle manifest, a deep link, a merge: [`canonical_photo_identity`] when it is a UUID,
/// otherwise the [`legacy_photo_identity`] an older catalog's non-UUID value now stands for.
/// Never a non-UUID: no path through here can store one as `photos.uuid` (#146).
///
/// `None` for an empty or whitespace-only value (#146 review N4): that is no identity at all,
/// not a legacy one. Mapping it to `v5("")` would make every such photo from every catalog
/// the same photo. Schema v23 gives such a row a random v4 for the same reason.
pub fn photo_identity_for(value: &str) -> Option<String> {
    if is_blank_identity(value) {
        return None;
    }
    Some(canonical_photo_identity(value).unwrap_or_else(|| legacy_photo_identity(value)))
}

/// True when `value` names no identity: empty, or whitespace only.
fn is_blank_identity(value: &str) -> bool {
    value.trim().is_empty()
}

/// Remember that `photo_id` once was `value`, when `value` is a legacy non-UUID identifier,
/// so a scan can still lead a file whose sidecar carries it back to the row
/// ([`Catalog::scan_identity`]). A photo keeps the first legacy value it was recorded with.
///
/// A value another row already holds is not recorded again (#150, review N3 of #146). The
/// first row to hold it is the one a person saw it on: v23's re-mint of the row that had it,
/// or the first file a scan found carrying it. A later holder is a duplicate — the v5 row an
/// old build's merge left beside a row that v23 then kept apart with a v4 — or another file
/// sharing a DAM id. Recording it there too would leave the value with two owners, which
/// [`Catalog::scan_identity`] rightly refuses to choose between, so the real original would
/// be catalogued again the next time it moved.
pub(super) const RECORD_LEGACY_IDENTIFIER_SQL: &str =
    "INSERT INTO photo_legacy_identifiers(photo_id, identifier)
     SELECT ?1, ?2
     WHERE NOT EXISTS (SELECT 1 FROM photo_legacy_identifiers
                       WHERE identifier = ?2 AND photo_id <> ?1)
     ON CONFLICT(photo_id) DO NOTHING";

/// Prefixes of `settings` keys that end in a photo's uuid, which a migration that changes the
/// uuid must carry over ([`Catalog::carry_photo_keyed_settings`]). Only the Obsidian module's
/// photo note record (`obsidian.note.<uuid>`, written by the React and GPUI modules alike)
/// today; its tag notes are keyed by a tag's uuid, which no photo migration touches. Searched
/// for `#146`: settings keys built from a photo's uuid in crates/, src-tauri/ and src/.
const PHOTO_KEYED_SETTING_PREFIXES: &[&str] = &["obsidian.note."];

// Per-thread count of legacy-identifier lookups, so a test can prove a rescan of a file
// already at its path does none (#146 review F7). Thread-local: tests run in parallel threads.
#[cfg(test)]
thread_local! {
    static LEGACY_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// True when `value` is a legacy identifier worth recording: not a UUID, not empty.
pub(super) fn is_legacy_identifier(value: &str) -> bool {
    !is_blank_identity(value) && !is_photo_identity(value)
}

/// True when a sidecar's `found` identifier is `uuid`, in either case. The sidecar is never
/// rewritten just to change the case of an identity it already carries.
fn carries_identity(found: &str, uuid: &str) -> bool {
    found.eq_ignore_ascii_case(uuid)
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
        Some(existing) if carries_identity(existing, uuid) => SidecarIdentity::Bound,
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
    /// Photos whose catalog IPTC has fields their sidecar has not received yet (#148).
    /// Photos, not copies, and not part of `total`: the repair pass retries them too.
    pub iptc_owed: i64,
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

/// What to do with every copy whose sidecar carries a non-UUID identifier (#150, L6 of the
/// #139–#142 review): the bulk form of [`IdentityConflictAction::Overwrite`] and
/// [`IdentityConflictAction::Dismiss`]. Adopt is not offered — a non-UUID cannot be adopted —
/// and neither is anything for a UUID conflict, which names another photo's identity and
/// needs a decision of its own. No default, as for one copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ForeignConflictAction {
    Overwrite,
    Dismiss,
}

impl ForeignConflictAction {
    fn single(self) -> IdentityConflictAction {
        match self {
            ForeignConflictAction::Overwrite => IdentityConflictAction::Overwrite,
            ForeignConflictAction::Dismiss => IdentityConflictAction::Dismiss,
        }
    }
}

/// What a bulk resolution of non-UUID conflicts did ([`Catalog::run_resolve_foreign_conflicts`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignConflictSummary {
    /// Copies in conflict with a non-UUID identifier when the run started — its progress
    /// denominator. The queue can change underneath it.
    pub total: usize,
    /// Copies whose sidecar now carries the catalog's identity, after a backup.
    pub overwritten: usize,
    /// Copies dismissed.
    pub dismissed: usize,
    /// Copies left as they are because they were no longer a non-UUID conflict when their
    /// turn came — resolved or dismissed meanwhile, the sidecar changed (to a UUID, or to no
    /// identifier), or, for Overwrite, the file not reachable. Not a failure.
    pub skipped: usize,
    /// Copies whose sidecar Overwrite could not write (read-only storage, an unparseable
    /// sidecar). Still queued as conflicts.
    pub failed: usize,
    /// Copies left as they were because another connection held the catalog's write lock
    /// through every retry (#182). Still queued; a later run or pass takes them up. An
    /// Overwrite counted here may already have written the sidecar: a later run then skips
    /// the copy as no longer in conflict, and the repair pass records it bound.
    pub busy: usize,
    /// True when the run stopped early (Cancel, a newer run, a catalog switch); the counts
    /// are then partial.
    pub aborted: bool,
}

impl ForeignConflictSummary {
    /// Copies the run has finished with, whatever the outcome.
    pub fn done(&self) -> usize {
        self.overwritten + self.dismissed + self.skipped + self.failed + self.busy
    }
}

/// The identifier a stored conflict recorded, from the queue row's `error` (the inverse of
/// [`SidecarIdentity::error_text`] for `Conflict`), or `None` for any other state.
fn recorded_conflict_value(error: &str) -> Option<&str> {
    error
        .strip_prefix(CONFLICT_PREFIX)?
        .strip_prefix(" (")?
        .strip_suffix("); left untouched")
}

/// True when a queue row's `error` records a conflict with a non-UUID identifier.
fn is_foreign_conflict(error: &str) -> bool {
    recorded_conflict_value(error).is_some_and(|found| !is_photo_identity(found))
}

/// The version of one queue row, as the repair pass saw it.
///
/// `attempts` moves on every non-`Bound` record and the row disappears on a `Bound` one, so
/// the pair is enough to tell "nobody has touched this row since I planned it" from "someone
/// did" — see this module's § Who owns a queue row. `dismissed_at` is deliberately *not*
/// here: a planned row is always un-dismissed (the planning query filters on it), so the
/// compare-and-set pins it to `0` literally rather than to a remembered value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QueueRowVersion {
    attempts: i64,
    last_attempt_at: i64,
}

/// Where a page of [`Catalog::plan_identity_repairs_page`] left off — the queue's primary
/// key, which is also its scan order.
///
/// Keyset, not `OFFSET`: the pass DELETES the rows it binds, so every page turn would shift
/// an offset window left and skip exactly as many rows as the previous page repaired. A
/// cursor over the key the scan is already ordered by cannot skip, and costs an index seek
/// instead of walking the rows behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityRepairCursor {
    photo_id: i64,
    field: &'static str,
    volume_id: i64,
    relative_path: String,
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
    /// The queue row as this plan last read it. Refreshed by
    /// [`Catalog::refresh_identity_repair`] and checked by
    /// [`Catalog::record_planned_repair`].
    version: QueueRowVersion,
}

impl IdentityRepairPlan {
    /// This plan's position in the queue's scan order, for the next page.
    pub fn cursor(&self) -> IdentityRepairCursor {
        IdentityRepairCursor {
            photo_id: self.photo_id,
            field: self.field.as_db_str(),
            volume_id: self.volume_id,
            relative_path: self.relative_path.clone(),
        }
    }
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
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
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
    /// Rows somebody else decided while this pass held them: a conflict resolved, a copy
    /// dismissed, a scan re-recording the same copy (#34). The pass's own result for such a
    /// row is **discarded**, never written over the newer decision, so it is reported here
    /// rather than in any of the four buckets above — counting it as `bound` or `failed`
    /// would claim an effect the pass did not have. Not a failure: the row was decided,
    /// just not by this pass. Also counts a photo owing IPTC whose values a save changed
    /// while the pass wrote them: that save's own write, or the next pass, settles it.
    pub superseded: usize,
    /// Un-dismissed queue rows plus photos owing IPTC when the pass started — the
    /// denominator its progress is reported against. The queue can grow (a concurrent scan)
    /// or shrink (a resolution) underneath it, so `done()` is not guaranteed to reach this.
    pub total: usize,
    /// True when the pass stopped before the end of the queue: a cancel, a newer pass, or a
    /// catalog switch. Every counter below is then partial, and the UI must say so rather
    /// than present them as a finished result.
    pub aborted: bool,
    /// Photos whose owed IPTC (#148) the pass wrote into their sidecar; nothing is owed for
    /// them any more.
    pub iptc_written: usize,
    /// Photos owing IPTC with no reachable copy — left owed for the next pass. A normal
    /// state, not a failure.
    pub iptc_unreachable: usize,
    /// Photos owing IPTC whose sidecar write still fails (read-only storage, an unparseable
    /// sidecar). Left owed.
    pub iptc_failed: usize,
    /// Copies and photos left as they were because another connection held the catalog's
    /// write lock through every retry (#182) — a long bundle import, say. Still queued or
    /// owed, so the next pass takes them up; not a failure. A copy counted here may already
    /// have had its sidecar written, which that next pass finds and records.
    pub busy: usize,
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

    /// Rows this pass has finished with, whatever the outcome — the numerator of its
    /// progress against [`Self::total`].
    pub fn done(&self) -> usize {
        self.bound
            + self.unreachable
            + self.conflicts
            + self.failed
            + self.superseded
            + self.iptc_written
            + self.iptc_unreachable
            + self.iptc_failed
            + self.busy
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
        let summary = self.conn.query_row(
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
                    iptc_owed: 0,
                })
            },
        )?;
        Ok(PendingIdentitySummary { iptc_owed: self.count_owed_iptc()?, ..summary })
    }

    /// How many un-dismissed queue rows there are — the repair pass's denominator.
    ///
    /// Rows, not copies: the pass retries each owed FIELD, so a copy owing both
    /// `identifier` and `import_batch` is two units of work here, while
    /// [`Catalog::summarize_pending_identity`] counts it as one copy of debt. The two answer
    /// different questions and are deliberately not the same number.
    pub fn count_active_identity_repairs(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM pending_sidecar_identity WHERE dismissed_at = 0",
            [],
            |r| r.get(0),
        )?)
    }

    /// Plan the repair of the next `limit` queued copies after `after`. PURE SQL, so it is
    /// safe to call while holding the catalog lock; run each plan off the lock and record
    /// the outcome afterwards.
    ///
    /// **Paged, and by keyset.** The queue reached 74,488 rows on the 100k harness shape in
    /// #20, and the un-paged predecessor materialized every one of them into a `Vec` — with
    /// its `target_path` — before a single byte of IO. Paging bounds that; doing it by
    /// keyset rather than `LIMIT/OFFSET` is what makes it *correct*, because the pass
    /// deletes the rows it binds: an offset window would move left under every page turn
    /// and skip exactly as many rows as the previous page repaired.
    ///
    /// Ordered by the queue's `PRIMARY KEY(photo_id, field, volume_id, relative_path)`,
    /// which SQLite's automatic index serves directly — no temp b-tree sort, and the
    /// cursor is an index seek rather than a walk over the rows already done (pinned by
    /// `identity_repair_page_query_plan_seeks_and_has_no_temp_btree_sort`). The order the
    /// queue is retried in carries no meaning of its own, so nothing is lost by not
    /// ordering on `queued_at` (which needed a sort, and which a two-field copy makes
    /// ambiguous anyway — see [`Catalog::list_pending_identity_page`]).
    ///
    /// Dismissed rows (#33) are skipped: "stop retrying this copy" is the whole content of
    /// a Dismiss, so a dismissed row must cost the pass neither a sidecar read nor a line
    /// in its summary. This is what lets a catalog whose only remaining debts are resolved
    /// or dismissed conflicts finish a pass with every counter at zero.
    pub fn plan_identity_repairs_page(
        &self,
        after: Option<&IdentityRepairCursor>,
        limit: i64,
    ) -> Result<Vec<IdentityRepairPlan>> {
        let mut stmt = self.conn.prepare(&identity_repair_page_sql(after.is_some()))?;
        let map = |r: &rusqlite::Row| {
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
                version: QueueRowVersion {
                    attempts: r.get(7)?,
                    last_attempt_at: r.get(8)?,
                },
            })
        };
        let plans = match after {
            None => stmt.query_map(params![limit], map)?.collect::<rusqlite::Result<Vec<_>>>(),
            Some(c) => stmt
                .query_map(
                    params![limit, c.photo_id, c.field, c.volume_id, c.relative_path],
                    map,
                )?
                .collect::<rusqlite::Result<Vec<_>>>(),
        }?;
        Ok(plans)
    }

    /// Re-read `plan`'s value and queue-row version straight before its sidecar IO, so a
    /// page-old plan never acts on data a resolution has since changed. Returns `false` when
    /// the row is gone or has been dismissed, meaning: leave the file alone entirely.
    ///
    /// The value matters as much as the version. Adopt rewrites `photos.uuid`, so a plan
    /// read before an Adopt carries the identity the photo no longer has — writing that into
    /// another copy's sidecar would be an actual wrong write, not merely a lost counter.
    /// One indexed primary-key lookup per row is nothing beside the sidecar parse and write
    /// it precedes (a network round trip each, on a NAS), and it shrinks the window in which
    /// a plan can be stale from "the whole page" to "this row's own IO".
    fn refresh_identity_repair(&self, plan: &mut IdentityRepairPlan) -> Result<bool> {
        let row: Option<(String, Option<String>, i64, i64, i64)> = self
            .conn
            .query_row(
                "SELECT p.uuid, b.uuid, q.attempts, q.last_attempt_at, q.dismissed_at
                 FROM pending_sidecar_identity q
                 JOIN photos p ON p.id = q.photo_id
                 LEFT JOIN import_batches b ON b.id = p.import_batch_id
                 WHERE q.photo_id = ?1 AND q.field = ?2 AND q.volume_id = ?3
                   AND q.relative_path = ?4",
                params![
                    plan.photo_id,
                    plan.field.as_db_str(),
                    plan.volume_id,
                    plan.relative_path
                ],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((photo_uuid, import_batch_uuid, attempts, last_attempt_at, dismissed_at)) = row
        else {
            return Ok(false); // resolved (Adopt/Overwrite) or the photo is gone
        };
        if dismissed_at != 0 {
            return Ok(false); // dismissed since the page was planned
        }
        let (_, value) = pending_sidecar_value(
            plan.field.as_db_str(),
            &photo_uuid,
            import_batch_uuid,
        );
        plan.value = value;
        plan.version = QueueRowVersion { attempts, last_attempt_at };
        Ok(true)
    }

    /// Record a planned repair's outcome, but **only** if the queue row is still the one the
    /// plan last read: same `attempts`, same `last_attempt_at`, still un-dismissed. Returns
    /// whether it applied.
    ///
    /// This is the compare-and-set of this module's § Who owns a queue row. Without it, a
    /// resolution landing during a row's sidecar IO is silently undone — an Adopt's cleared
    /// row is re-inserted as a conflict, a Dismiss is un-dismissed by the next `attempts +
    /// 1` — because the pass's `INSERT … ON CONFLICT DO UPDATE` predecessor wrote
    /// unconditionally and would happily re-create a row somebody had just resolved away.
    ///
    /// `dismissed_at = 0` is pinned literally rather than compared against a remembered
    /// value: a planned row is un-dismissed by construction, and this is what makes a
    /// Dismiss during the IO window reject the write too.
    ///
    /// The pass only ever UPDATEs or DELETEs, never INSERTs. Queueing new debt is
    /// `record_sidecar_field_target`'s job (the scanner, the bundle indexer, a resolution);
    /// a retry of an existing row that can no longer find that row has nothing to say.
    fn record_planned_repair(
        &self,
        plan: &IdentityRepairPlan,
        outcome: &SidecarIdentity,
    ) -> Result<bool> {
        let key = params![
            plan.photo_id,
            plan.field.as_db_str(),
            plan.volume_id,
            plan.relative_path,
            plan.version.attempts,
            plan.version.last_attempt_at,
        ];
        let changed = if matches!(outcome, SidecarIdentity::Bound) {
            self.conn.execute(
                "DELETE FROM pending_sidecar_identity
                 WHERE photo_id = ?1 AND field = ?2 AND volume_id = ?3 AND relative_path = ?4
                   AND attempts = ?5 AND last_attempt_at = ?6 AND dismissed_at = 0",
                key,
            )?
        } else {
            self.conn.execute(
                "UPDATE pending_sidecar_identity
                    SET attempts = attempts + 1, error = ?7, last_attempt_at = ?8
                  WHERE photo_id = ?1 AND field = ?2 AND volume_id = ?3 AND relative_path = ?4
                    AND attempts = ?5 AND last_attempt_at = ?6 AND dismissed_at = 0",
                params![
                    plan.photo_id,
                    plan.field.as_db_str(),
                    plan.volume_id,
                    plan.relative_path,
                    plan.version.attempts,
                    plan.version.last_attempt_at,
                    outcome.error_text(),
                    now(),
                ],
            )?
        };
        Ok(changed == 1)
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
    ///
    /// A single decision has no abort flag of its own to pass [`retry_busy`] (#224 N3): there
    /// is no cancel button for one click, and this is not the 74k-row batch
    /// [`Self::run_resolve_foreign_conflicts`]'s retry ladder (4 pauses, ~20 s on top of the
    /// connection's own 5 s `busy_timeout`) exists for. So it passes an abort flag already
    /// tripped, which makes [`retry_busy`] return on the first busy rather than pause and
    /// retry — a held lock costs the connection's plain `busy_timeout` (~5 s), as it did
    /// before #182, instead of riding that ladder on a resolution nobody asked to wait out.
    pub fn resolve_identity_conflict(
        &self,
        photo_id: i64,
        volume_id: i64,
        relative_path: &str,
        action: IdentityConflictAction,
    ) -> Result<IdentityConflictOutcome> {
        self.resolve_conflict(photo_id, volume_id, relative_path, action, false, &AtomicBool::new(true))
    }

    /// [`Self::resolve_identity_conflict`]; with `foreign_only`, Overwrite also refuses a
    /// sidecar that carries a UUID when it is read, as the bulk resolution of non-UUID
    /// conflicts requires ([`Self::run_resolve_foreign_conflicts`]).
    ///
    /// A catalog statement that meets another connection's write lock is retried
    /// ([`retry_busy`], until `abort`); one still locked after that returns the busy error,
    /// and a write it follows — Overwrite's sidecar — has then happened without its record
    /// (#182). The copy stays queued, and the repair pass records it bound.
    fn resolve_conflict(
        &self,
        photo_id: i64,
        volume_id: i64,
        relative_path: &str,
        action: IdentityConflictAction,
        foreign_only: bool,
        abort: &AtomicBool,
    ) -> Result<IdentityConflictOutcome> {
        let queued = retry_busy(abort, || {
            Ok(self
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
                .optional()?)
        })?;
        let (error, dismissed_at, catalog_uuid, base_path): (String, i64, String, String) =
            queued.ok_or_else(|| {
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
            retry_busy(abort, || self.set_identity_dismissal(photo_id, volume_id, relative_path, 0))?;
            return Ok(outcome);
        }

        // Adopt / Overwrite / Dismiss all answer the same question — "whose identity wins
        // for this copy?" — so all three require the copy to actually be in Conflict. A
        // copy that merely can't be written (or reached) has no such question to answer;
        // it needs a repair pass, not a decision.
        let state = debt_state_from_error(&error);
        if foreign_only && state == "conflict" && !is_foreign_conflict(&error) {
            return Err(CatalogError::Validation(format!(
                "{relative_path}'s conflict is with a UUID, another photo's identity, which \
                 needs a decision of its own"
            )));
        }
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
            let at = now();
            retry_busy(abort, || self.set_identity_dismissal(photo_id, volume_id, relative_path, at))?;
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
        if carries_identity(&found, &catalog_uuid) {
            return Err(CatalogError::Validation(format!(
                "{}'s sidecar already carries this photo's identity ({found}); the recorded \
                 conflict is stale — run a repair pass to clear it",
                target.display()
            )));
        }
        outcome.previous_sidecar_uuid = found.clone();

        match action {
            IdentityConflictAction::Adopt => {
                // A value that is not a UUID is another tool's identifier, not a photo
                // identity (#141): adopting it would make `photos.uuid` something no merge,
                // deep link or later scan treats as one.
                let Some(adopted) = canonical_photo_identity(&found) else {
                    return Err(CatalogError::Validation(format!(
                        "cannot adopt {found:?}: it is not a UUID, so it cannot be a photo's \
                         identity. Overwrite this copy's sidecar (it is backed up first), or \
                         Dismiss to leave it as it is"
                    )));
                };
                // Refuse BEFORE the write, naming the photo that already holds it. The
                // `photos.uuid` UNIQUE constraint would also stop this, but only as an
                // opaque SQL error where a stated precondition belongs (#32) — and
                // "resolving one conflict manufactures another" is exactly the failure this
                // check exists to prevent.
                if let Some((other_id, other_path)) = self.photo_holding_uuid(&adopted, photo_id)? {
                    return Err(CatalogError::Validation(format!(
                        "cannot adopt {found}: photo {other_id} ({other_path}) already holds \
                         that identity, and no two photos may share one. Overwrite this \
                         copy's sidecar instead, or resolve the other photo first"
                    )));
                }
                self.conn.execute(
                    "UPDATE photos SET uuid = ?1, updated_at = ?2 WHERE id = ?3",
                    params![adopted, now(), photo_id],
                )?;
                outcome.catalog_uuid = adopted.clone();
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
                    &adopted,
                    volume_id,
                    relative_path,
                )?;
            }
            IdentityConflictAction::Overwrite => {
                if foreign_only && is_photo_identity(&found) {
                    return Err(CatalogError::Validation(format!(
                        "{}'s sidecar now carries a UUID ({found}), which is another photo's \
                         identity and needs a decision of its own",
                        target.display()
                    )));
                }
                // Overwrite replaces EVERY xmp:Identifier value, but `found` is only the
                // first. In bulk, a sidecar with more than one value — a Bag a DAM appended
                // to, a second Description — may hold another photo's UUID beside the DAM
                // id; it is left for a person to look at (#150, review M1).
                if foreign_only {
                    let all = crate::xmp::read_identifiers(&target);
                    if all.len() != 1 || all.iter().any(|v| is_photo_identity(v)) {
                        return Err(CatalogError::Validation(format!(
                            "{}'s sidecar carries {} identifier values ({}); a bulk Overwrite \
                             replaces only a single non-UUID one",
                            target.display(),
                            all.len(),
                            all.join(", ")
                        )));
                    }
                }
                let backup = crate::xmp::overwrite_identifier(&target, &catalog_uuid)
                    .map_err(CatalogError::Io)?;
                outcome.sidecar_backup = backup.map(|p| p.to_string_lossy().to_string());
                retry_busy(abort, || {
                    self.record_sidecar_field_target(
                        photo_id,
                        SidecarField::Identifier,
                        volume_id,
                        relative_path,
                        &SidecarIdentity::Bound,
                    )
                })?;
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
                Some(found) if carries_identity(&found, uuid) => SidecarIdentity::Bound,
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

    /// Retry every queued repair on this connection, clearing the ones that succeed, one
    /// page at a time until the queue is exhausted or `abort` trips.
    ///
    /// The whole pass, minus the ownership that surrounds it: `commands::storage`'s
    /// `repair_pending_identity` claims the identity job family
    /// (`app::jobs::JobRegistry::identity`) and hands its abort generation in here, so
    /// a newer pass, a Cancel, or a catalog switch stops this one. `progress` is called
    /// after each row with the running summary; the command turns that into the
    /// `identity:repair_progress` event.
    ///
    /// `abort` is checked before each row rather than only between pages, so cancelling a
    /// pass against an unmounted NAS costs at most one file's timeout instead of the rest
    /// of the page's. A tripped flag stops the pass with `aborted` set and the counters
    /// partial; nothing already recorded is rolled back — a bound sidecar stays bound.
    ///
    /// Per row the sequence is refresh → IO → compare-and-set record; see this module's
    /// § Who owns a queue row for why the first and last exist.
    ///
    /// After the identity queue, the pass retries the IPTC photos owe their sidecars (#148,
    /// `iptc_owed`): same abort flag, same progress, counted in `total` from the start.
    pub fn run_identity_repair(
        &self,
        abort: &AtomicBool,
        mut progress: impl FnMut(&IdentityRepairSummary),
    ) -> Result<IdentityRepairSummary> {
        // An abort that strikes while this (or the page read below) is being retried is
        // reported the same way the per-row loop reports one: an aborted summary, not the
        // raw "database is locked" `retry_busy` would otherwise propagate (#224 N2).
        let Some(total) = retry_busy_or_aborted(abort, || {
            Ok(self.count_active_identity_repairs()? + self.count_owed_iptc()?)
        })?
        else {
            return Ok(IdentityRepairSummary { aborted: true, ..Default::default() });
        };
        let mut summary = IdentityRepairSummary { total: total as usize, ..Default::default() };
        let mut cursor: Option<IdentityRepairCursor> = None;
        loop {
            if abort.load(Ordering::Relaxed) {
                summary.aborted = true;
                return Ok(summary);
            }
            // A read: under WAL it waits for no writer, so a lock that outlasts the retries
            // here is not the transient kind and does end the pass.
            let Some(page) = retry_busy_or_aborted(abort, || {
                self.plan_identity_repairs_page(cursor.as_ref(), REPAIR_PAGE_SIZE)
            })?
            else {
                summary.aborted = true;
                return Ok(summary);
            };
            if page.is_empty() {
                // Then the IPTC the catalog owes sidecars (#148), under the same abort flag.
                summary.aborted = !self.run_iptc_repair(abort, &mut summary, &mut progress)?;
                return Ok(summary);
            }
            for mut plan in page {
                if abort.load(Ordering::Relaxed) {
                    summary.aborted = true;
                    return Ok(summary);
                }
                cursor = Some(plan.cursor());
                // Resolved or dismissed since the page was planned: the decision is
                // somebody else's and this pass has nothing to add to it.
                match retry_busy(abort, || self.refresh_identity_repair(&mut plan)) {
                    Ok(true) => {}
                    Ok(false) => {
                        summary.superseded += 1;
                        progress(&summary);
                        continue;
                    }
                    Err(e) if is_busy(&e) => {
                        summary.busy += 1;
                        progress(&summary);
                        continue;
                    }
                    Err(e) => return Err(e),
                }
                let outcome = plan.run();
                // Only the record is retried, never the IO: it is the same compare-and-set
                // on the version refreshed above. Left busy, the row keeps that version and
                // the next pass re-reads the file, which it may find already bound (#182).
                match retry_busy(abort, || self.record_planned_repair(&plan, &outcome)) {
                    Ok(true) => summary.tally(&outcome),
                    Ok(false) => summary.superseded += 1,
                    Err(e) if is_busy(&e) => summary.busy += 1,
                    Err(e) => return Err(e),
                }
                progress(&summary);
            }
        }
    }

    /// Retry every queued repair, uninterruptibly and without reporting progress.
    /// Composes plan → IO → record for tests and simple callers; the Tauri command runs the
    /// same steps under a job's abort flag (see [`Catalog::run_identity_repair`]).
    pub fn repair_pending_identity(&self) -> Result<IdentityRepairSummary> {
        self.run_identity_repair(&AtomicBool::new(false), |_| {})
    }

    /// How many copies are in conflict with a non-UUID identifier, un-dismissed — what
    /// [`Self::run_resolve_foreign_conflicts`] would act on now.
    pub fn count_foreign_conflicts(&self) -> Result<usize> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT error FROM pending_sidecar_identity
             WHERE field = 'identifier' AND dismissed_at = 0 AND error GLOB '{CONFLICT_PREFIX}*'"
        ))?;
        let mut count = 0;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if is_foreign_conflict(&row.get::<_, String>(0)?) {
                count += 1;
            }
        }
        Ok(count)
    }

    /// One keyset page of the un-dismissed identifier conflicts after `after`, in the queue's
    /// key order: `(photo_id, volume_id, relative_path, error)` of each copy. UUID conflicts
    /// are in it too — the caller skips them — so the page's last row is always the cursor.
    fn identifier_conflicts_page(
        &self,
        after: Option<&(i64, i64, String)>,
        limit: i64,
    ) -> Result<Vec<(i64, i64, String, String)>> {
        let (photo_id, volume_id, relative_path) =
            after.cloned().unwrap_or((i64::MIN, i64::MIN, String::new()));
        let rows = self
            .conn
            .prepare_cached(&format!(
                "SELECT photo_id, volume_id, relative_path, error FROM pending_sidecar_identity
                 WHERE field = 'identifier' AND dismissed_at = 0
                   AND error GLOB '{CONFLICT_PREFIX}*'
                   AND (photo_id, volume_id, relative_path) > (?1, ?2, ?3)
                 ORDER BY photo_id, volume_id, relative_path
                 LIMIT ?4"
            ))?
            .query_map(params![photo_id, volume_id, relative_path, limit], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Overwrite or Dismiss every copy whose sidecar carries a non-UUID identifier (#150, L6
    /// of the #139–#142 review), one copy at a time until the queue is exhausted or `abort`
    /// trips. A DAM-managed library can reach this state by the thousand (schema v23 queues
    /// every re-minted row's copies), and one decision per copy does not scale.
    ///
    /// Each copy goes through the same decision as one resolved by hand
    /// ([`Self::resolve_identity_conflict`]): the queue row is re-read, and Overwrite re-reads
    /// the sidecar and backs it up before it writes. A copy that is no longer a non-UUID
    /// conflict when its turn comes — resolved or dismissed meanwhile, its sidecar now
    /// carrying a UUID (another photo's identity: never overwritten in bulk) or nothing, its
    /// file unreachable — is skipped, not failed. Like a single resolution it wins over a
    /// concurrent repair pass (this module's § Who owns a queue row) and does not stop one.
    ///
    /// The job around it — id, abort generation, status slot, catalog identity, events — is
    /// `app::identity`'s. `abort` is checked before every copy; a stop keeps every decision
    /// already made and sets `aborted`.
    pub fn run_resolve_foreign_conflicts(
        &self,
        action: ForeignConflictAction,
        abort: &AtomicBool,
        mut progress: impl FnMut(&ForeignConflictSummary),
    ) -> Result<ForeignConflictSummary> {
        // As in `run_identity_repair` (#224 N2): an abort met while this or the page read is
        // being retried is reported as an aborted summary, not a raw busy error.
        let Some(total) = retry_busy_or_aborted(abort, || self.count_foreign_conflicts())? else {
            return Ok(ForeignConflictSummary { aborted: true, ..Default::default() });
        };
        let mut summary = ForeignConflictSummary { total, ..Default::default() };
        let mut cursor: Option<(i64, i64, String)> = None;
        loop {
            if abort.load(Ordering::Relaxed) {
                summary.aborted = true;
                return Ok(summary);
            }
            let Some(page) = retry_busy_or_aborted(abort, || {
                self.identifier_conflicts_page(cursor.as_ref(), REPAIR_PAGE_SIZE)
            })?
            else {
                summary.aborted = true;
                return Ok(summary);
            };
            if page.is_empty() {
                return Ok(summary);
            }
            for (photo_id, volume_id, relative_path, error) in page {
                if abort.load(Ordering::Relaxed) {
                    summary.aborted = true;
                    return Ok(summary);
                }
                cursor = Some((photo_id, volume_id, relative_path.clone()));
                if !is_foreign_conflict(&error) {
                    continue; // a UUID conflict: not this run's to decide, and not counted
                }
                let single = action.single();
                match self.resolve_conflict(photo_id, volume_id, &relative_path, single, true, abort) {
                    Ok(_) => match action {
                        ForeignConflictAction::Overwrite => summary.overwritten += 1,
                        ForeignConflictAction::Dismiss => summary.dismissed += 1,
                    },
                    Err(CatalogError::Validation(_) | CatalogError::NotFound(_)) => summary.skipped += 1,
                    Err(CatalogError::Io(_)) => summary.failed += 1,
                    // Locked through every retry: left queued, and the run carries on (#182).
                    Err(e) if is_busy(&e) => summary.busy += 1,
                    Err(e) => return Err(e),
                }
                progress(&summary);
            }
        }
    }


    /// What a scan may match or adopt as the identity of a file whose sidecar's
    /// `xmp:Identifier` holds `found`, or `None` to match by path alone and mint if new.
    ///
    /// A UUID is the identity (#141). Anything else is another tool's identifier and is
    /// never adopted, with one exception (#146): the identifier a row held before schema v23
    /// re-minted it ([`Self::remint_non_identity_photos`]), or that the file carried when a
    /// scan minted the row a UUID (#150, `IdentitySource::legacy_value`). Its sidecar still carries that
    /// value — overwriting it is a person's decision — so it is the only link from the file
    /// back to its row, and without it a moved file would be catalogued a second time and
    /// its tags, ratings and faces left on a row nobody sees. It answers the row's minted
    /// UUID only when exactly one row holds that legacy identifier and every primary copy
    /// that row records is gone ([`Self::every_primary_copy_is_gone`]): two files sharing a DAM id must not
    /// take turns owning one row (#141), and an unmounted volume is not a moved file.
    ///
    /// The scanned file's `size` must also be the row's (#146 review N1). An offloaded photo
    /// has no primary copy left to be "gone", so without it any file carrying the same DAM id
    /// — an export, a derivative — would take the row over. Originals are never modified, so
    /// a moved or restored original keeps its size, and a different file almost never has it.
    ///
    /// The caller still binds with the raw `found`, so the foreign value stays in the
    /// sidecar and is reported as a conflict.
    ///
    /// `scanned` is the file this value was just read from — passed to
    /// [`Self::every_primary_copy_is_gone`] so it can tell a copy genuinely still at its
    /// recorded path from one a case-insensitive filesystem only folds onto the file being
    /// scanned (#224 L1, the legacy-identifier half of #184).
    pub fn scan_identity(&self, found: Option<&str>, size: i64, scanned: &Path) -> Result<Option<String>> {
        let Some(found) = found.filter(|v| !is_blank_identity(v)) else {
            return Ok(None);
        };
        if let Some(identity) = canonical_photo_identity(found) {
            return Ok(Some(identity));
        }
        #[cfg(test)]
        LEGACY_LOOKUPS.with(|n| n.set(n.get() + 1));
        let owners: Vec<(i64, String, i64)> = self
            .conn
            .prepare_cached(
                "SELECT l.photo_id, p.uuid, p.size
                 FROM photo_legacy_identifiers l JOIN photos p ON p.id = l.photo_id
                 WHERE l.identifier = ?1
                 LIMIT 2",
            )?
            .query_map(params![found], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        match owners.as_slice() {
            [(photo_id, uuid, row_size)]
                if *row_size == size && self.every_primary_copy_is_gone(*photo_id, scanned)? =>
            {
                Ok(Some(uuid.clone()))
            }
            _ => Ok(None),
        }
    }

    /// True when every primary copy the catalog records for `photo_id` — each primary
    /// location, and the catalog-root path — sits on storage that is present and no longer
    /// holds the file. A copy on an absent volume (an unmounted NAS) may still be there, so
    /// it is not gone.
    ///
    /// Only primary copies count (#146 review F3). A backup, local cache or export copy is
    /// another recorded location of this very row, so its presence says nothing about which
    /// row a moved original belongs to — and since new photos get a backup enqueued, insisting
    /// on it would leave the legacy re-home dead for most real libraries.
    ///
    /// "Gone" is decided by [`copy_still_in_place`] against `scanned`, not by `exists()`
    /// (#224 L1, the same class of bug #184 fixed for a bound UUID): on a case-insensitive
    /// filesystem the old name of a case-only rename still opens the renamed file.
    fn every_primary_copy_is_gone(&self, photo_id: i64, scanned: &Path) -> Result<bool> {
        let mut copies: Vec<(PathBuf, PathBuf)> = self
            .conn
            .prepare(
                "SELECT v.base_path, l.relative_path
                 FROM photo_locations l JOIN volumes v ON v.id = l.volume_id
                 WHERE l.photo_id = ?1 AND l.role = 'primary'",
            )?
            .query_map(params![photo_id], |r| {
                let base = PathBuf::from(r.get::<_, String>(0)?);
                let relative: String = r.get(1)?;
                Ok((base.clone(), base.join(relative)))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let logical = self.get_photo(photo_id)?.path;
        copies.push((self.root().to_path_buf(), self.to_absolute(&logical)));
        Ok(copies.iter().all(|(base, file)| base.is_dir() && !copy_still_in_place(file, scanned)))
    }

    /// True when re-homing `photo_id` onto a file on `volume_id` would move one of its
    /// primary copies off a file that is still there (#150, reviews F5 and N2 of #146): the
    /// row's primary location on that volume, and — with `logical`, for a re-home that also
    /// rewrites `photos.path` — the file at the row's catalog-root path.
    ///
    /// A re-home by identity is for a file that moved. If the row's file is still in place,
    /// the file being scanned or imported is another copy carrying the same identity — a
    /// bundle's copy at another path, a duplicate a person made, the other half of a v24
    /// case collision — and pointing the row at it would leave the original with no row:
    /// the next scan catalogues it afresh, without its ratings, tags and faces, or the two
    /// files take turns owning the row. A primary copy on another volume is not checked: a
    /// copy of the photo on a second volume is a second location of the same row, and
    /// recording it moves nothing.
    ///
    /// "Still there" is decided by [`copy_still_in_place`] against `scanned`, the file being
    /// upserted, not by `exists()` (#184): on a case-insensitive filesystem the old name of a
    /// case-only rename still opens the renamed file.
    pub(super) fn primary_copy_left_in_place(
        &self,
        photo_id: i64,
        volume_id: i64,
        logical: bool,
        scanned: &Path,
    ) -> Result<bool> {
        let on_volume: Option<PathBuf> = self
            .conn
            .prepare_cached(
                "SELECT v.base_path, l.relative_path
                 FROM photo_locations l JOIN volumes v ON v.id = l.volume_id
                 WHERE l.photo_id = ?1 AND l.volume_id = ?2 AND l.role = 'primary'",
            )?
            .query_row(params![photo_id, volume_id], |r| {
                Ok(Path::new(&r.get::<_, String>(0)?).join(r.get::<_, String>(1)?))
            })
            .optional()?;
        if on_volume.is_some_and(|file| copy_still_in_place(&file, scanned)) {
            return Ok(true);
        }
        Ok(logical && copy_still_in_place(&self.to_absolute(&self.get_photo(photo_id)?.path), scanned))
    }

    /// Schema v23 (#146): give every row whose `photos.uuid` is not a photo identity the
    /// [`legacy_photo_identity`] of that value — a UUID v5, so that every catalog that held
    /// the photo under the same value agrees on its new identity. Returns how many rows were
    /// re-minted.
    ///
    /// Before #141 a scan adopted a sidecar's `xmp:Identifier` verbatim, so a DAM asset id
    /// could become `photos.uuid`. #141 stopped matching on such values, and that left those
    /// rows unreachable by identity: a moved file was catalogued again beside them. Each one
    /// now gets a UUID, and keeps its previous value in `photo_legacy_identifiers` for
    /// [`Self::scan_identity`]. Merge and bundle import map an old bundle's non-UUID ids
    /// through the same function ([`photo_identity_for`]), so they find these rows.
    ///
    /// No sidecar is touched here. Each recorded copy is queued as the conflict it now is —
    /// its sidecar carries the old value, adopted from it in the first place — so the debt
    /// panel shows it at once, and Overwrite (after a backup) or Dismiss settles it, as for
    /// any non-UUID identifier (docs/storage-and-import.md § Resolving a conflict). Adopt
    /// refuses it. The repair pass and each decision re-read the file, so a copy that never
    /// carried the value is reported as whatever it really is the first time either runs.
    pub(super) fn remint_non_identity_photos(&self) -> Result<usize> {
        let stale: Vec<(i64, String)> = self
            .conn
            .prepare("SELECT id, uuid FROM photos")?
            .query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))?
            .filter(|row| !matches!(row, Ok((_, uuid)) if is_photo_identity(uuid)))
            .collect::<rusqlite::Result<_>>()?;
        for (photo_id, previous) in &stale {
            // An empty (or blank) value was never in any sidecar: such a copy owes the write,
            // which the repair pass does for a copy queued as not yet bound. It has no
            // legacy value for another catalog to agree on, so it gets an ordinary v4.
            if is_blank_identity(previous) {
                let fresh = uuid::Uuid::new_v4().to_string();
                self.remint_photo(*photo_id, previous, &fresh, &SidecarIdentity::Unreachable)?;
                continue;
            }
            // The same value maps to the same identity in every catalog (F1 of the #146
            // review). If another row already holds it — a bundle from a migrated catalog
            // merged in before this one migrated — the two rows claim one photo and this
            // migration cannot tell which is right: keep them apart with a v4 and let the
            // queued conflict bring it to a person.
            let mut identity = legacy_photo_identity(previous);
            if self.photo_holding_uuid(&identity, *photo_id)?.is_some() {
                identity = uuid::Uuid::new_v4().to_string();
            }
            self.conn
                .prepare_cached(RECORD_LEGACY_IDENTIFIER_SQL)?
                .execute(params![photo_id, previous])?;
            self.remint_photo(*photo_id, previous, &identity, &SidecarIdentity::Conflict(previous.clone()))?;
        }
        Ok(stale.len())
    }

    /// Record `value` — a trusted manifest id, or what a scanned sidecar's `xmp:Identifier`
    /// holds — as `photo_id`'s legacy identifier when it is one (not a UUID, not empty);
    /// see [`RECORD_LEGACY_IDENTIFIER_SQL`].
    pub(super) fn record_legacy_identifier(&self, photo_id: i64, value: Option<&str>) -> Result<()> {
        if let Some(value) = value.filter(|v| is_legacy_identifier(v)) {
            self.conn
                .prepare_cached(RECORD_LEGACY_IDENTIFIER_SQL)?
                .execute(params![photo_id, value])?;
        }
        Ok(())
    }

    /// Schema v24 (#146 L5): store every photo identity in its canonical lowercase spelling.
    /// Returns how many rows changed.
    ///
    /// [`is_photo_identity`] accepts an upper-case UUID and a scan used to store it as the
    /// sidecar spelled it, while every lookup — the scan's re-home, merge, deep links —
    /// compares exactly. A sidecar that keeps the upper-case spelling stays bound: binding
    /// compares without case.
    ///
    /// If the lowercase spelling is already another row's identity, the two rows claim one
    /// identity and `photos.uuid` is `UNIQUE`. Which of them is that photo is not something
    /// a migration can know, so the upper-case row is re-minted and its copies queued as a
    /// conflict for a person, as [`Self::remint_non_identity_photos`] does.
    pub(super) fn canonicalise_photo_identities(&self) -> Result<usize> {
        let spelled: Vec<(i64, String)> = self
            .conn
            .prepare("SELECT id, uuid FROM photos WHERE uuid <> lower(uuid)")?
            .query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))?
            .filter(|row| !matches!(row, Ok((_, uuid)) if !is_photo_identity(uuid)))
            .collect::<rusqlite::Result<_>>()?;
        for (photo_id, spelling) in &spelled {
            let canonical = spelling.to_ascii_lowercase();
            if self.photo_holding_uuid(&canonical, *photo_id)?.is_some() {
                let fresh = uuid::Uuid::new_v4().to_string();
                self.remint_photo(*photo_id, spelling, &fresh, &SidecarIdentity::Conflict(spelling.clone()))?;
            } else {
                self.conn.execute(
                    "UPDATE photos SET uuid = ?1, updated_at = ?2 WHERE id = ?3",
                    params![canonical, now(), photo_id],
                )?;
                self.carry_photo_keyed_settings(spelling, &canonical)?;
            }
        }
        Ok(spelled.len())
    }

    /// Give `photo_id`, which held `previous`, the identity `uuid`, carry its uuid-keyed
    /// settings over, and record `outcome` for every copy it records, through the queue's one
    /// writer. Touches no file.
    fn remint_photo(
        &self,
        photo_id: i64,
        previous: &str,
        uuid: &str,
        outcome: &SidecarIdentity,
    ) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE photos SET uuid = ?1, updated_at = ?2 WHERE id = ?3")?
            .execute(params![uuid, now(), photo_id])?;
        self.carry_photo_keyed_settings(previous, uuid)?;
        let copies: Vec<(i64, String)> = self
            .conn
            .prepare_cached("SELECT volume_id, relative_path FROM photo_locations WHERE photo_id = ?1")?
            .query_map(params![photo_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (volume_id, relative_path) in copies {
            self.record_sidecar_field_target(
                photo_id,
                SidecarField::Identifier,
                volume_id,
                &relative_path,
                outcome,
            )?;
        }
        Ok(())
    }

    /// Move the settings keyed by a photo's uuid from `previous` to `uuid`, when a migration
    /// changes the photo's identity (#146 review F8). Runs inside the migration's transaction.
    ///
    /// Without it, a module record a photo had — today only the Obsidian module's note record,
    /// `obsidian.note.<photo uuid>` — would no longer be found, and "create note" would make a
    /// second note for a photo that has one.
    ///
    /// When the new key already has a record, the one with content wins (#150, review N5 of
    /// #146). A blank record — what the module's "Forget" leaves — never hides a real one: a
    /// real record at the old key replaces a blank one at the new key, and a blank record at
    /// the old key is dropped beside a real one at the new key. Two real records are two
    /// notes, and which of them is this photo's is not something a migration can know: both
    /// stay where they are, the new key's is the one the module shows, and the old one is
    /// logged so it can be found. No record with content is ever deleted or overwritten.
    fn carry_photo_keyed_settings(&self, previous: &str, uuid: &str) -> Result<()> {
        if previous == uuid {
            return Ok(());
        }
        for prefix in PHOTO_KEYED_SETTING_PREFIXES {
            let (old_key, new_key) = (format!("{prefix}{previous}"), format!("{prefix}{uuid}"));
            let Some(old) = self.get_setting(&old_key)? else { continue };
            let is_blank = |record: &str| record.trim().is_empty();
            match self.get_setting(&new_key)? {
                None => {
                    self.conn.execute(
                        "UPDATE settings SET key = ?2 WHERE key = ?1",
                        params![old_key, new_key],
                    )?;
                }
                Some(new) if is_blank(&new) => {
                    self.conn.execute("DELETE FROM settings WHERE key = ?1", params![new_key])?;
                    self.conn.execute(
                        "UPDATE settings SET key = ?2 WHERE key = ?1",
                        params![old_key, new_key],
                    )?;
                }
                Some(_) if is_blank(&old) => {
                    self.conn.execute("DELETE FROM settings WHERE key = ?1", params![old_key])?;
                }
                Some(_) => eprintln!(
                    "identity migration: {new_key} and {old_key} both hold a record; the first \
                     is kept as the photo's and the second left in place"
                ),
            }
        }
        Ok(())
    }

    /// Drop the sidecar debt recorded for the primary copy a re-homed photo is leaving:
    /// `new_location` is where the file is now, and a queue row for the path it moved from
    /// names a file that is no longer there, so it could only ever report it unreachable.
    /// The debt the new path owes is recorded by the caller's binding, as for any scan.
    pub(super) fn forget_identity_debt_left_behind(
        &self,
        photo_id: i64,
        new_location: &Path,
    ) -> Result<()> {
        let (volume_id, relative_path) = self.volume_for_path(new_location)?;
        self.conn.execute(
            "DELETE FROM pending_sidecar_identity
             WHERE photo_id = ?1 AND volume_id = ?2 AND relative_path <> ?3
               AND relative_path IN (SELECT relative_path FROM photo_locations
                                     WHERE photo_id = ?1 AND volume_id = ?2 AND role = 'primary')",
            params![photo_id, volume_id, relative_path],
        )?;
        Ok(())
    }
}

/// Queue rows planned per round trip to SQLite.
///
/// Bounds what the pass holds at once — 74,488 rows with their absolute paths is tens of
/// megabytes, and the un-paged predecessor built exactly that before its first byte of IO.
/// Large enough that the planning query is noise beside a page's worth of sidecar parses and
/// writes (a network round trip each on a NAS), small enough that a page is a fraction of a
/// second of allocation.
const REPAIR_PAGE_SIZE: i64 = 256;

/// The exact SQL [`Catalog::plan_identity_repairs_page`] runs, with or without its keyset
/// cursor predicate — two statements rather than one `?1 = 0 OR …`, so SQLite can *seek* the
/// primary-key index for a later page instead of re-scanning the rows already done.
///
/// A function rather than two consts because the shared 6 lines would otherwise be written
/// twice and could drift; `EXPLAIN QUERY PLAN` in the tests runs whatever this returns, so
/// the plan is pinned against the SQL that actually ships either way.
fn identity_repair_page_sql(with_cursor: bool) -> String {
    let cursor = if with_cursor {
        "AND (q.photo_id, q.field, q.volume_id, q.relative_path) > (?2, ?3, ?4, ?5)"
    } else {
        ""
    };
    format!(
        "SELECT q.photo_id, p.uuid, q.field, q.volume_id, q.relative_path, v.base_path,
                b.uuid, q.attempts, q.last_attempt_at
         FROM pending_sidecar_identity q
         JOIN photos p ON p.id = q.photo_id
         JOIN volumes v ON v.id = q.volume_id
         LEFT JOIN import_batches b ON b.id = p.import_batch_id
         WHERE q.dismissed_at = 0 {cursor}
         ORDER BY q.photo_id, q.field, q.volume_id, q.relative_path
         LIMIT ?1"
    )
}

/// True when the file a row records at `recorded` is still there as a file of its own, beside
/// `scanned` (#184).
///
/// `exists()` alone answers the wrong question on a case-insensitive filesystem (APFS and
/// HFS+ by default, exFAT and vfat drives, a casefold ext4 or tmpfs directory): after
/// `IMG.ARW` → `img.arw` the old name still opens the renamed file, so the file looked as if
/// it had been left behind and got a second row. So when `recorded` opens the very file being
/// scanned (one device and inode — the identity itself, not a name), it counts as still there
/// only if its own names are still in their folders' listings: a second hard link is, and a
/// name the filesystem merely folds onto the renamed file is not. A listing that cannot be
/// read counts as listed, which keeps the copy apart — when uncertain, preserve.
fn copy_still_in_place(recorded: &Path, scanned: &Path) -> bool {
    if !recorded.exists() {
        return false;
    }
    !(same_file(recorded, scanned) && reached_only_by_folding(recorded, scanned))
}

/// Whether `a` and `b` open one file: the same device and inode on Unix, the same canonical
/// path elsewhere (Windows canonicalises to the name on disk). False when either is missing.
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    }
}

/// True when some component of `recorded` that it does not share with `scanned` — its file
/// name, or a folder renamed by case — is not in its parent's listing verbatim, so `recorded`
/// reaches its file only through the filesystem folding names.
///
/// Checks the differing components **folder-first, file name last** (#224 L3): building the
/// path outward from the shared prefix means a whole folder renamed by case (`DCIM` ->
/// `dcim`, N files) finds that folder missing from its own (small) parent listing on the
/// first check and returns immediately, never listing the N-entry folder once per scanned
/// file. The reverse order (file name first, as before) would `read_dir` that N-entry folder
/// for every one of its files before ever checking the folder name itself: O(N²) dirents for
/// one rename instead of O(N). Only runs when [`same_file`] is true, so an ordinary scan (no
/// case-only rename in play) pays nothing either way.
fn reached_only_by_folding(recorded: &Path, scanned: &Path) -> bool {
    let shared = recorded
        .components()
        .zip(scanned.components())
        .take_while(|(a, b)| a == b)
        .count();
    let mut path: PathBuf = recorded.components().take(shared).collect();
    for component in recorded.components().skip(shared) {
        path.push(component);
        if !listed_verbatim(&path) {
            return true;
        }
    }
    false
}

/// Whether `path`'s last component appears, byte for byte, in its parent folder's listing.
fn listed_verbatim(path: &Path) -> bool {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return true;
    };
    match std::fs::read_dir(parent) {
        Ok(entries) => entries.flatten().any(|e| e.file_name() == name),
        Err(_) => true,
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
    /// `thumbnails::tests::TestTmpDir` (see `crates/core/src/thumbnails/mod.rs`).
    ///
    /// This file previously keyed on `tag` alone (`chairphoto-identity-test-{tag}`),
    /// which every `cargo test` process on the machine shares; `remove_dir_all` on entry
    /// then deletes a directory another process is still writing into, and nothing
    /// cleans up after a panicking test either. That is the same bug #45 / commit
    /// 9cd6d83 fixed for the thumbnail tests. `crates/core/src/test_support.rs` on the
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

    /// The queue row for one (copy, `identifier`) pair, or `None` if it is gone.
    /// `(attempts, error, dismissed_at)` — the three things a race can move.
    fn queue_row(catalog: &Catalog, photo_id: i64, path: &Path) -> Option<(i64, String, i64)> {
        let (volume_id, relative_path) = copy_of(catalog, path);
        catalog
            .conn()
            .query_row(
                "SELECT attempts, error, dismissed_at FROM pending_sidecar_identity
                 WHERE photo_id = ?1 AND field = 'identifier' AND volume_id = ?2
                   AND relative_path = ?3",
                params![photo_id, volume_id, relative_path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .unwrap()
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
            seed_conflicted_copy(&catalog, &root, "adopt-me.arw", "0f11e1d0-5a1d-4e00-8f11-e1d0f11e1d00");
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
        assert_eq!(outcome.catalog_uuid, "0f11e1d0-5a1d-4e00-8f11-e1d0f11e1d00");
        assert_eq!(outcome.previous_sidecar_uuid, "0f11e1d0-5a1d-4e00-8f11-e1d0f11e1d00");
        assert_eq!(outcome.sidecar_backup, None, "Adopt writes no file, so it backs none up");
        assert_eq!(photo_uuid(&catalog, photo_id), "0f11e1d0-5a1d-4e00-8f11-e1d0f11e1d00");
        assert_ne!(catalog_uuid, "0f11e1d0-5a1d-4e00-8f11-e1d0f11e1d00", "sanity: it really changed");
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
            seed_conflicted_copy(&catalog, &root, "primary.arw", "0f11e1d0-5a1d-4e00-8f11-e1d0f11e1d00");
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
        assert_eq!(catalog.count_active_identity_repairs().unwrap(), 0);
        assert!(catalog.plan_identity_repairs_page(None, 50).unwrap().is_empty());
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
            seed_conflicted_copy(&catalog, &root, "adopt.arw", "0add0000-5a1d-4e00-8000-00000000add0");
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

    // --- the repair pass: paging, cancellation, and row ownership (#34) -----------

    /// **Forced race, by construction.** The window #33's hand-off named: a resolution that
    /// lands while the pass is between reading the file and recording what it found.
    ///
    /// Driven through `refresh` → resolution → `run` → `record` by hand rather than by
    /// timing, because that IS the interleaving — the pass has already committed to its
    /// sidecar IO and the decision arrives underneath it.
    ///
    /// Pins the **"the pass never INSERTs"** half of the rule specifically. Adopt deletes
    /// the queue row, so the compare-and-set has nothing to compare; what stops the row
    /// coming back is that a retry which cannot find its row has nothing to say. Before #34
    /// the record was an unconditional `INSERT … ON CONFLICT DO UPDATE`, which re-created as
    /// a conflict the very row the Adopt had just cleared, and the catalog went back to
    /// reporting debt for a copy a human had already settled. The compare-and-set half is
    /// `a_dismissal_landing_during_a_rows_io_is_not_undone_by_its_record` and
    /// `a_concurrent_scans_record_is_not_overwritten_by_a_pass_that_had_started` below.
    #[test]
    fn a_resolution_landing_mid_pass_does_not_get_overwritten_by_it() {
        let (catalog, root, _dir) = temp_catalog("repair-vs-resolution");
        let (photo_id, path, _) =
            seed_conflicted_copy(&catalog, &root, "contested.arw", "0f11e1d0-5a1d-4e00-8f11-e1d0f11e1d00");
        let (volume_id, relative_path) = copy_of(&catalog, &path);

        // The pass plans the row and refreshes it, exactly as `run_identity_repair` does.
        let mut plan = catalog
            .plan_identity_repairs_page(None, 10)
            .unwrap()
            .pop()
            .expect("the conflicted copy must be planned");
        assert!(catalog.refresh_identity_repair(&mut plan).unwrap());

        // The human decides, on their own connection, while the pass is mid-flight.
        let secondary = Catalog::open_secondary(catalog.db_path(), &root).unwrap();
        secondary
            .resolve_identity_conflict(
                photo_id,
                volume_id,
                &relative_path,
                IdentityConflictAction::Adopt,
            )
            .unwrap();
        assert_eq!(photo_uuid(&catalog, photo_id), "0f11e1d0-5a1d-4e00-8f11-e1d0f11e1d00");
        assert!(queue_row(&catalog, photo_id, &path).is_none(), "the Adopt cleared the row");

        // The pass finishes its IO and tries to record. It must lose.
        let outcome = plan.run();
        assert!(matches!(outcome, SidecarIdentity::Conflict(_)), "got {outcome:?}");
        assert!(
            !catalog.record_planned_repair(&plan, &outcome).unwrap(),
            "the pass recorded over a resolution that landed under it"
        );
        assert!(
            queue_row(&catalog, photo_id, &path).is_none(),
            "the Adopt's cleared row was re-queued as a conflict by the superseded pass"
        );
        assert_eq!(
            photo_uuid(&catalog, photo_id),
            "0f11e1d0-5a1d-4e00-8f11-e1d0f11e1d00",
            "the adopted identity must still stand"
        );
    }

    /// **Forced race, by construction.** The same window, for the resolution that KEEPS the
    /// row: a Dismiss lands while the pass is mid-IO on that very row.
    ///
    /// This is the case an Adopt cannot cover. Adopt deletes the row, so the pass's record
    /// finds nothing to update whatever its predicate says; a Dismiss leaves the row right
    /// where it was, so only the compare-and-set — on `dismissed_at = 0`, pinned literally
    /// rather than remembered — stops the pass from bumping `attempts` on a copy a human
    /// has just taken out of the queue, and from reporting it as a conflict it found.
    #[test]
    fn a_dismissal_landing_during_a_rows_io_is_not_undone_by_its_record() {
        let (catalog, root, _dir) = temp_catalog("repair-vs-dismiss");
        let (photo_id, path, _) =
            seed_conflicted_copy(&catalog, &root, "dismissed-mid-io.arw", "foreign-uuid");
        let (volume_id, relative_path) = copy_of(&catalog, &path);

        let mut plan = catalog
            .plan_identity_repairs_page(None, 10)
            .unwrap()
            .pop()
            .expect("the conflicted copy must be planned");
        assert!(
            catalog.refresh_identity_repair(&mut plan).unwrap(),
            "sanity: the row is live when the pass picks it up"
        );

        let secondary = Catalog::open_secondary(catalog.db_path(), &root).unwrap();
        secondary
            .resolve_identity_conflict(
                photo_id,
                volume_id,
                &relative_path,
                IdentityConflictAction::Dismiss,
            )
            .unwrap();
        let (attempts_after_dismiss, _, dismissed_at) =
            queue_row(&catalog, photo_id, &path).expect("Dismiss keeps the row");
        assert_ne!(dismissed_at, 0);

        let outcome = plan.run();
        assert!(
            !catalog.record_planned_repair(&plan, &outcome).unwrap(),
            "the pass recorded over a dismissal that landed under it"
        );
        let (attempts, _, still_dismissed) = queue_row(&catalog, photo_id, &path).unwrap();
        assert_eq!(still_dismissed, dismissed_at, "the dismissal must survive the pass");
        assert_eq!(
            attempts, attempts_after_dismiss,
            "a dismissed row must not have its attempt counter bumped by a pass that had \
             already started on it"
        );
    }

    /// **Forced race, by construction.** The version half of the compare-and-set, against
    /// the other writer of this table: a scan re-recording the same copy on its own
    /// connection (`scanner/mod.rs` opens one) while the pass is mid-IO on that row.
    ///
    /// The row still exists and is still un-dismissed, so only `attempts` /
    /// `last_attempt_at` can tell the pass its premise moved. Without them the pass's older
    /// reading overwrites the scan's fresher one, and the queue reports the wrong reason —
    /// which is what a user acts on.
    #[test]
    fn a_concurrent_scans_record_is_not_overwritten_by_a_pass_that_had_started() {
        let (catalog, root, _dir) = temp_catalog("repair-vs-scan-record");
        let (photo_id, path) = seed_photo(&catalog, &root, "rerecorded.arw");
        catalog
            .record_sidecar_identity(photo_id, &path, &SidecarIdentity::Unreachable)
            .unwrap();
        // Make the copy unreachable for real, so the pass's own outcome is `Unreachable`
        // and differs from the scan's — a same-text overwrite would prove nothing.
        std::fs::remove_file(&path).unwrap();

        let mut plan = catalog
            .plan_identity_repairs_page(None, 10)
            .unwrap()
            .pop()
            .expect("the queued copy must be planned");
        assert!(catalog.refresh_identity_repair(&mut plan).unwrap());

        let secondary = Catalog::open_secondary(catalog.db_path(), &root).unwrap();
        secondary
            .record_sidecar_identity(
                photo_id,
                &path,
                &SidecarIdentity::Unwritable("scan says the volume is read-only".to_string()),
            )
            .unwrap();

        let outcome = plan.run();
        assert_eq!(outcome, SidecarIdentity::Unreachable, "sanity: a different reading");
        assert!(
            !catalog.record_planned_repair(&plan, &outcome).unwrap(),
            "the pass recorded over a fresher write from another connection"
        );
        let (_, error, _) = queue_row(&catalog, photo_id, &path).unwrap();
        assert!(
            error.contains("scan says the volume is read-only"),
            "the newer writer's reason must stand, got: {error}"
        );
    }

    /// The other half of the same window: the decision lands before the pass reaches the
    /// row at all. The refresh must then skip it entirely — no sidecar write, no counter —
    /// and the pass must report it as `superseded` rather than as one of the four outcomes,
    /// which would claim an effect it did not have. "Dismiss changes neither the catalog
    /// nor the file" has to hold against a pass that had already planned the row.
    ///
    /// Forced from inside a real `run_identity_repair`: the progress callback dismisses the
    /// two LATER rows while the pass is still finishing the first, so the interleaving is
    /// fixed by construction rather than by thread timing. Two, because the row must be
    /// skipped however its `dismissed_at` got set — through `resolve_identity_conflict`
    /// (only conflicts can be dismissed that way) and as a plain column value. The
    /// writable one is what discriminates a genuine skip from "wrote the file, then had the
    /// record rejected": only the former leaves the sidecar alone.
    #[test]
    fn a_row_dismissed_after_it_was_planned_is_skipped_without_being_written_to() {
        let (catalog, root, _dir) = temp_catalog("repair-skips-resolved");
        // Row 1 is an ordinary repairable copy; rows 2 and 3 get dismissed mid-pass.
        // `photo_id` ascending is the pass's scan order, and the seed helpers insert in
        // call order.
        let (first_id, first_path) = seed_photo(&catalog, &root, "first.arw");
        catalog
            .record_sidecar_identity(first_id, &first_path, &SidecarIdentity::Unreachable)
            .unwrap();
        // Reachable, with no identifier in its sidecar — so a pass that got as far as the
        // IO would WRITE it.
        let (writable_id, writable_path) = seed_photo(&catalog, &root, "writable.arw");
        catalog
            .record_sidecar_identity(writable_id, &writable_path, &SidecarIdentity::Unreachable)
            .unwrap();
        let (conflicted_id, conflicted_path, _) =
            seed_conflicted_copy(&catalog, &root, "zconflicted.arw", "foreign-uuid");
        let (conflicted_volume, conflicted_relative) = copy_of(&catalog, &conflicted_path);
        assert!(
            first_id < writable_id && writable_id < conflicted_id,
            "fixture assumes ascending scan order"
        );

        let secondary = Catalog::open_secondary(catalog.db_path(), &root).unwrap();
        let dismissed = std::cell::Cell::new(false);
        let summary = catalog
            .run_identity_repair(&AtomicBool::new(false), |s| {
                if s.done() == 1 && !dismissed.get() {
                    dismissed.set(true);
                    secondary
                        .conn()
                        .execute(
                            "UPDATE pending_sidecar_identity SET dismissed_at = 1
                             WHERE photo_id = ?1",
                            params![writable_id],
                        )
                        .unwrap();
                    secondary
                        .resolve_identity_conflict(
                            conflicted_id,
                            conflicted_volume,
                            &conflicted_relative,
                            IdentityConflictAction::Dismiss,
                        )
                        .unwrap();
                }
            })
            .unwrap();

        assert!(dismissed.get(), "the fixture never got to land its decision");
        assert_eq!(
            (summary.bound, summary.unreachable, summary.conflicts, summary.failed),
            (1, 0, 0, 0),
            "only the first copy was actually acted on: {summary:?}"
        );
        assert_eq!(
            summary.superseded, 2,
            "the dismissed copies must be reported as decided-by-someone-else, not as \
             outcomes this pass produced: {summary:?}"
        );
        assert_eq!(summary.done(), 3, "every row was visited: {summary:?}");

        assert!(
            crate::xmp::read_identifier(&writable_path).is_none(),
            "the pass wrote the sidecar of a copy that had been dismissed before it got \
             there — a skip has to happen BEFORE the IO, not be undone after it"
        );
        for (id, path) in [(writable_id, &writable_path), (conflicted_id, &conflicted_path)] {
            let (attempts, _, dismissed_at) =
                queue_row(&catalog, id, path).expect("a dismissal keeps the row");
            assert_ne!(dismissed_at, 0, "photo {id}: the pass un-dismissed a dismissed copy");
            assert_eq!(attempts, 1, "photo {id}: a skipped row must not be re-stamped");
        }
    }

    /// The refresh re-reads the plan's VALUE, not just its version — and that is the part
    /// no compare-and-set can stand in for.
    ///
    /// An Adopt rewrites `photos.uuid`. It leaves the queue rows of the photo's other copies
    /// exactly as they were when those copies carry no identifier at all
    /// (`recheck_other_copies_after_adopt` deliberately does not touch them: they owe an
    /// ordinary write, not a decision). So their version is unchanged, the compare-and-set
    /// accepts the record — and a plan read before the Adopt would put the photo's PREVIOUS
    /// identity into that sidecar. Not a lost counter: a wrong identifier in a user's file,
    /// which the next pass then reports as a conflict.
    #[test]
    fn the_pass_re_reads_a_plans_value_before_writing_it() {
        let (catalog, root, _dir) = temp_catalog("repair-refreshes-value");
        let (photo_id, path, old_uuid) =
            seed_conflicted_copy(&catalog, &root, "adopted.arw", "0f11e1d0-5a1d-4e00-8f11-e1d0f11e1d00");

        // A second copy of the same photo, reachable, with nothing in its sidecar yet.
        let other_dir = root.parent().unwrap().join("second-volume-refresh");
        std::fs::create_dir_all(&other_dir).unwrap();
        let other_volume = catalog
            .add_volume("Second", &other_dir, crate::catalog::VolumeKind::Backup)
            .unwrap();
        let other_path = other_dir.join("adopted.arw");
        std::fs::write(&other_path, b"raw-bytes-2").unwrap();
        catalog
            .add_location(photo_id, other_volume, "adopted.arw", crate::catalog::LocationRole::Backup)
            .unwrap();
        catalog
            .record_sidecar_identity(photo_id, &other_path, &SidecarIdentity::Unreachable)
            .unwrap();

        // The pass plans BOTH rows, then the user adopts the first copy's identity.
        let mut plans = catalog.plan_identity_repairs_page(None, 10).unwrap();
        assert_eq!(plans.len(), 2, "both copies of the photo are queued");
        let mut other_plan = plans
            .drain(..)
            .find(|p| p.volume_id == other_volume)
            .expect("the second copy must be planned");

        let (volume_id, relative_path) = copy_of(&catalog, &path);
        let secondary = Catalog::open_secondary(catalog.db_path(), &root).unwrap();
        secondary
            .resolve_identity_conflict(
                photo_id,
                volume_id,
                &relative_path,
                IdentityConflictAction::Adopt,
            )
            .unwrap();
        assert_eq!(photo_uuid(&catalog, photo_id), "0f11e1d0-5a1d-4e00-8f11-e1d0f11e1d00");

        // The pass now reaches the second copy. Its plan predates the Adopt.
        assert!(catalog.refresh_identity_repair(&mut other_plan).unwrap());
        let outcome = other_plan.run();
        assert_eq!(outcome, SidecarIdentity::Bound);
        assert!(catalog.record_planned_repair(&other_plan, &outcome).unwrap());

        assert_eq!(
            crate::xmp::read_identifier(&other_path).as_deref(),
            Some("0f11e1d0-5a1d-4e00-8f11-e1d0f11e1d00"),
            "the pass wrote the identity the photo had when the page was planned, not the \
             one it has now — the sidecar now carries {old_uuid}, which belongs to nobody"
        );
    }

    /// A pass and a stream of resolutions running concurrently on two real connections.
    /// Whatever the interleaving, a decision that committed must still be in force at the
    /// end: a dismissed copy stays dismissed, and an adopted one stays out of the queue.
    ///
    /// **This is a contention net, not a proof of the window.** It exercises two
    /// connections writing `pending_sidecar_identity` at once — the shape `commands::storage`
    /// actually runs, one secondary connection per call — and would catch a lock/busy
    /// regression or an invariant broken across a wide interleaving. It does *not* reliably
    /// reproduce the narrow one: the window is a single row's refresh → sidecar read →
    /// record, tens of microseconds, and a resolution has to land inside it. Measured
    /// directly: with the compare-and-set replaced by the pre-#34 unconditional upsert, this
    /// test passed 15 runs out of 15, while the step-driven tests around it failed every
    /// time. So the mechanism is pinned by
    /// `a_resolution_landing_mid_pass_does_not_get_overwritten_by_it`,
    /// `a_dismissal_landing_during_a_rows_io_is_not_undone_by_its_record` and
    /// `a_concurrent_scans_record_is_not_overwritten_by_a_pass_that_had_started`, which
    /// drive the pass's own methods one step at a time and place the decision exactly in the
    /// window rather than hoping a scheduler does.
    #[test]
    fn a_pass_and_resolutions_on_two_connections_keep_every_decision_that_committed() {
        let (catalog, root, _dir) = temp_catalog("repair-race-resolutions");
        let mut copies = Vec::new();
        for i in 0..24 {
            let (id, path, _) = seed_conflicted_copy(
                &catalog,
                &root,
                &format!("contested-{i}.arw"),
                &format!("f0000000-0000-4000-8000-{i:012}"),
            );
            let (volume_id, relative_path) = copy_of(&catalog, &path);
            copies.push((id, path, volume_id, relative_path));
        }

        // Half dismissed, half adopted, so both the "row kept" and the "row deleted"
        // resolutions race the pass.
        let decisions: Vec<_> = copies
            .iter()
            .enumerate()
            .map(|(i, (id, _, volume_id, relative_path))| {
                let action = if i % 2 == 0 {
                    IdentityConflictAction::Dismiss
                } else {
                    IdentityConflictAction::Adopt
                };
                (*id, *volume_id, relative_path.clone(), action)
            })
            .collect();

        // A `Catalog` owns a `rusqlite::Connection`, which is `Send` but not `Sync`, so each
        // thread gets its own — which is what a real pass and a real resolution have anyway
        // (`commands::storage` opens a secondary connection per call).
        //
        // Each connection waits up to 120 s for the other's write lock instead of the
        // production 5 s. The two threads write in lockstep, and every write is an fsync; on
        // /home (btrfs) under the full suite's IO load one of them waited past 5 s, and the
        // pass failed with "database is locked" (SQLITE_BUSY, extended 5), at #150's base and
        // on its branch alike. The #150 review measured 23 of 24 runs failing under IO load
        // without this and 0 of 24 with it. That timeout is not what this test is about; the
        // ownership of each queue row is.
        let patient = |c: &Catalog| c.conn().busy_timeout(std::time::Duration::from_secs(120)).unwrap();
        let db_path = catalog.db_path().to_path_buf();
        let pass_root = root.clone();
        let resolver_root = root.clone();
        std::thread::scope(|s| {
            s.spawn(move || {
                let pass = Catalog::open_secondary(&db_path, &pass_root).unwrap();
                patient(&pass);
                // Several passes, so a resolution can land in any of the pass's phases
                // (planning, refresh, IO, record) rather than only in the first.
                for _ in 0..4 {
                    pass.run_identity_repair(&AtomicBool::new(false), |_| {}).unwrap();
                }
            });
            let resolver_db = catalog.db_path().to_path_buf();
            s.spawn(move || {
                let resolver = Catalog::open_secondary(&resolver_db, &resolver_root).unwrap();
                patient(&resolver);
                for (id, volume_id, relative_path, action) in &decisions {
                    // A refusal is a legitimate outcome here (the pass may have bound the
                    // copy first, making the recorded conflict stale), so failures are not
                    // asserted away — only what happened to the ones that succeeded.
                    let _ = resolver.resolve_identity_conflict(
                        *id,
                        *volume_id,
                        relative_path,
                        *action,
                    );
                }
            });
        });

        for (i, (id, path, _, _)) in copies.iter().enumerate() {
            match queue_row(&catalog, *id, path) {
                Some((_, _, dismissed_at)) if i % 2 == 0 => assert_ne!(
                    dismissed_at, 0,
                    "copy {id} was dismissed, and the pass un-dismissed it"
                ),
                Some((_, error, _)) => assert!(
                    !error.starts_with(CONFLICT_PREFIX),
                    "copy {id} was adopted, and the pass re-queued it as a conflict: {error}"
                ),
                // Dismissals keep the row, so a missing row means the Dismiss never landed
                // (the pass bound the copy first) — legitimate, nothing to check.
                None => {}
            }
        }
    }

    // --- A lock held past the busy timeout (#182) -------------------------------------------

    /// A third connection holding the write lock, as the bundle importer's index phase does
    /// across a whole import. Shared so a hook or a progress callback can release it at the
    /// exact point a test needs.
    type Holder = std::rc::Rc<std::cell::RefCell<Option<rusqlite::Connection>>>;

    fn hold_write_lock(catalog: &Catalog) -> Holder {
        let conn = rusqlite::Connection::open(catalog.db_path()).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        // Waits of 30 ms stand in for production's 5 s; the lock outlasts each of them.
        catalog.conn().busy_timeout(std::time::Duration::from_millis(30)).unwrap();
        std::rc::Rc::new(std::cell::RefCell::new(Some(conn)))
    }

    fn release(holder: &Holder) {
        if let Some(conn) = holder.borrow_mut().take() {
            conn.execute_batch("COMMIT").unwrap();
        }
    }

    /// Removes the retry hook when a test ends, panicking or not.
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            super::super::busy::hook::clear();
        }
    }

    fn unreachable_rows(catalog: &Catalog, root: &Path, tag: &str, n: usize) -> Vec<(i64, PathBuf)> {
        (0..n)
            .map(|i| {
                let (id, path) = seed_photo(catalog, root, &format!("{tag}-{i}.arw"));
                catalog.record_sidecar_identity(id, &path, &SidecarIdentity::Unreachable).unwrap();
                (id, path)
            })
            .collect()
    }

    /// A pass that meets the lock past the busy timeout waits it out and retries the row,
    /// instead of ending on "database is locked". The lock is released between the first
    /// busy attempt and its retry, so the retry — not a lucky timing — is what binds it.
    #[test]
    fn a_pass_retries_a_row_whose_record_met_a_held_lock() {
        let (catalog, root, _dir) = temp_catalog("busy-retry");
        let seeded = unreachable_rows(&catalog, &root, "retry", 3);
        let holder = hold_write_lock(&catalog);
        let retries = std::rc::Rc::new(std::cell::Cell::new(0));
        let _guard = HookGuard;
        {
            let (holder, retries) = (holder.clone(), retries.clone());
            super::super::busy::hook::set(move || {
                retries.set(retries.get() + 1);
                release(&holder);
            });
        }

        let summary = catalog.run_identity_repair(&AtomicBool::new(false), |_| {}).unwrap();

        assert_eq!(retries.get(), 1, "the first record met the lock and was retried once");
        assert_eq!((summary.bound, summary.busy, summary.aborted), (3, 0, false), "{summary:?}");
        for (id, path) in &seeded {
            assert!(queue_row(&catalog, *id, path).is_none(), "photo {id} is still queued");
        }
    }

    /// A single `resolve_identity_conflict`, unlike the pass above, has no cancel button of
    /// its own to wait for — there is no 74k-row queue behind one click — so it does not ride
    /// `run_resolve_foreign_conflicts`'s retry ladder on a held lock (#224 N3): its record
    /// write meets the lock, fails on the connection's own `busy_timeout` alone, and returns.
    /// The lock is never released, so a regression back to a never-tripped abort flag would
    /// show up here as at least one retry (and, outside a test, a real multi-pause wait)
    /// instead of zero.
    #[test]
    fn a_single_resolution_does_not_wait_out_a_held_lock() {
        let (catalog, root, _dir) = temp_catalog("single-resolve-busy-bound");
        let (id, path, _) = seed_conflicted_copy(&catalog, &root, "x.jpg", "not-my-problem-uuid");
        let (volume_id, relative_path) = copy_of(&catalog, &path);
        let holder = hold_write_lock(&catalog);
        let retries = std::rc::Rc::new(std::cell::Cell::new(0));
        let _guard = HookGuard;
        {
            let retries = retries.clone();
            super::super::busy::hook::set(move || retries.set(retries.get() + 1));
        }

        let result = catalog.resolve_identity_conflict(id, volume_id, &relative_path, IdentityConflictAction::Dismiss);

        assert!(result.as_ref().is_err_and(is_busy), "{result:?}");
        assert_eq!(retries.get(), 0, "a single resolution must not pause and retry at all");
        release(&holder);
    }

    /// A row whose record is locked through every retry is left exactly as it was, counted
    /// `busy`, and the pass carries on with the next row. The next pass finds the sidecar the
    /// first one wrote and records it bound.
    #[test]
    fn a_row_locked_through_every_retry_stays_queued_and_the_pass_carries_on() {
        let (catalog, root, _dir) = temp_catalog("busy-defer");
        let seeded = unreachable_rows(&catalog, &root, "defer", 3);
        let before = queue_row(&catalog, seeded[0].0, &seeded[0].1).unwrap();
        let holder = hold_write_lock(&catalog);

        // Released only once the first row has been given up on.
        let summary = catalog
            .run_identity_repair(&AtomicBool::new(false), |s| {
                if s.busy == 1 {
                    release(&holder);
                }
            })
            .unwrap();

        assert_eq!((summary.bound, summary.busy, summary.done()), (2, 1, 3), "{summary:?}");
        assert_eq!(
            queue_row(&catalog, seeded[0].0, &seeded[0].1),
            Some(before),
            "the locked row is left as it was, version included"
        );
        assert!(crate::xmp::read_identifier(&seeded[0].1).is_some(), "its IO ran before the lock");
        for (id, path) in &seeded[1..] {
            assert!(queue_row(&catalog, *id, path).is_none(), "photo {id} after the lock was not bound");
        }

        let next = catalog.repair_pending_identity().unwrap();
        assert_eq!((next.bound, next.busy), (1, 0), "{next:?}");
        assert!(queue_row(&catalog, seeded[0].0, &seeded[0].1).is_none());
    }

    /// The bulk resolution does the same: an Overwrite whose record stays locked is counted
    /// `busy` and the run goes on. The sidecar was written (after its backup), so a later run
    /// skips the copy as no longer in conflict and the repair pass records it bound.
    #[test]
    fn a_bulk_run_leaves_a_locked_copy_queued_and_carries_on() {
        let (catalog, root, _dir) = temp_catalog("busy-bulk");
        let copies: Vec<_> = (0..3)
            .map(|i| seed_conflicted_copy(&catalog, &root, &format!("b{i}.jpg"), &format!("dam:{i}")))
            .collect();
        let holder = hold_write_lock(&catalog);

        let summary = catalog
            .run_resolve_foreign_conflicts(ForeignConflictAction::Overwrite, &AtomicBool::new(false), |s| {
                if s.busy == 1 {
                    release(&holder);
                }
            })
            .unwrap();

        assert_eq!((summary.overwritten, summary.busy, summary.done()), (2, 1, 3), "{summary:?}");
        let (id, path, uuid) = &copies[0];
        assert!(queue_row(&catalog, *id, path).is_some_and(|(_, e, _)| e.starts_with(CONFLICT_PREFIX)));
        assert_eq!(crate::xmp::read_identifier(path).as_deref(), Some(uuid.as_str()));
        let mut backup = crate::xmp::sidecar_path(path).into_os_string();
        backup.push(".chairphoto-backup");
        assert!(PathBuf::from(&backup).exists(), "the foreign value is kept in the backup");

        let again = catalog
            .run_resolve_foreign_conflicts(ForeignConflictAction::Overwrite, &AtomicBool::new(false), |_| {})
            .unwrap();
        assert_eq!((again.overwritten, again.skipped), (0, 1), "{again:?}");
        assert_eq!(catalog.repair_pending_identity().unwrap().bound, 1);
        assert!(queue_row(&catalog, *id, path).is_none());
    }

    /// The IPTC half of the pass: a settle locked through every retry leaves the fields owed,
    /// and the next pass settles them.
    #[test]
    fn owed_iptc_locked_through_every_retry_stays_owed() {
        let (catalog, root, _dir) = temp_catalog("busy-iptc");
        let (id, _) = seed_photo(&catalog, &root, "iptc.arw");
        let f = crate::catalog::IptcFields { title: "T".into(), ..Default::default() };
        catalog.set_iptc(id, &f).unwrap();
        let holder = hold_write_lock(&catalog);

        let summary = catalog.run_identity_repair(&AtomicBool::new(false), |_| {}).unwrap();
        release(&holder);

        assert_eq!((summary.iptc_written, summary.busy), (0, 1), "{summary:?}");
        assert_eq!(catalog.owed_iptc(id).unwrap(), crate::catalog::IptcMask::TITLE);
        let next = catalog.repair_pending_identity().unwrap();
        assert_eq!((next.iptc_written, next.busy), (1, 0), "{next:?}");
        assert!(catalog.owed_iptc(id).unwrap().is_empty());
    }

    /// The pass stops at its abort flag between rows, not merely between pages: a cancel
    /// against an unmounted NAS must cost one file's timeout, not the rest of the page's.
    /// Counters stay partial and `aborted` says so — a UI that showed them as a finished
    /// result would report a queue as clean when most of it was never looked at.
    #[test]
    fn the_pass_stops_at_its_abort_flag_between_rows() {
        let (catalog, root, _dir) = temp_catalog("repair-abort");
        let mut seeded = Vec::new();
        for i in 0..5 {
            let (id, path) = seed_photo(&catalog, &root, &format!("abort-{i}.arw"));
            catalog
                .record_sidecar_identity(id, &path, &SidecarIdentity::Unreachable)
                .unwrap();
            seeded.push((id, path));
        }

        let abort = AtomicBool::new(false);
        let summary = catalog
            .run_identity_repair(&abort, |s| {
                if s.done() == 1 {
                    abort.store(true, Ordering::Relaxed);
                }
            })
            .unwrap();

        assert!(summary.aborted, "a tripped flag must be reported, not swallowed: {summary:?}");
        assert_eq!(summary.done(), 1, "the pass ran on past its abort flag: {summary:?}");
        assert_eq!(summary.total, 5, "the denominator is the queue at start: {summary:?}");
        let still_queued = seeded
            .iter()
            .filter(|(id, path)| queue_row(&catalog, *id, path).is_some())
            .count();
        assert_eq!(still_queued, 4, "the un-visited rows must be left exactly as they were");
    }

    /// The pass pages the queue instead of materializing it, and the paging is keyset —
    /// which is what makes it correct, not merely bounded. Every bound row is DELETED, so an
    /// `OFFSET` window would slide left under each page turn and skip as many rows as the
    /// previous page repaired: with one full page plus a remainder, an offset-paged pass
    /// would bind the first `REPAIR_PAGE_SIZE` copies and never see the rest.
    #[test]
    fn the_pass_pages_the_queue_without_skipping_what_it_deletes() {
        let (catalog, root, _dir) = temp_catalog("repair-paging");
        let count = REPAIR_PAGE_SIZE as usize + 4;
        let mut seeded = Vec::new();
        for i in 0..count {
            let (id, path) = seed_photo(&catalog, &root, &format!("page-{i:04}.arw"));
            catalog
                .record_sidecar_identity(id, &path, &SidecarIdentity::Unreachable)
                .unwrap();
            seeded.push((id, path));
        }
        assert_eq!(catalog.count_active_identity_repairs().unwrap() as usize, count);

        // Each page is bounded, and the pass never holds more than one at a time.
        assert_eq!(
            catalog.plan_identity_repairs_page(None, REPAIR_PAGE_SIZE).unwrap().len(),
            REPAIR_PAGE_SIZE as usize,
            "the planning query must be bounded by its limit"
        );

        let summary = catalog.repair_pending_identity().unwrap();
        assert_eq!(
            (summary.bound, summary.superseded, summary.aborted),
            (count, 0, false),
            "every queued copy must be bound, including the ones past the first page: \
             {summary:?}"
        );
        assert_eq!(catalog.count_active_identity_repairs().unwrap(), 0);
        for (id, path) in &seeded {
            assert!(
                crate::xmp::read_identifier(path).is_some(),
                "photo {id} past the first page never had its sidecar written"
            );
        }
    }

    /// Guards the planning query's shape directly, as
    /// `list_pending_identity_page_query_plan_has_no_temp_btree_sort` does for the panel's:
    /// `EXPLAIN QUERY PLAN` over the exact SQL `plan_identity_repairs_page` ships, in both
    /// its forms. Neither may sort into a temp b-tree, and the cursor form must *seek* the
    /// primary-key index rather than scan from the top — otherwise every page turn re-walks
    /// the rows already repaired and the pass is quadratic in the queue.
    #[test]
    fn identity_repair_page_query_plan_seeks_and_has_no_temp_btree_sort() {
        let (catalog, _root, _dir) = temp_catalog("repair-query-plan");
        let plan_of = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> Vec<String> {
            let mut stmt = catalog
                .conn()
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            stmt.query_map(params, |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };

        for (with_cursor, params) in [
            (false, vec![&256i64 as &dyn rusqlite::ToSql]),
            (
                true,
                vec![
                    &256i64 as &dyn rusqlite::ToSql,
                    &1i64,
                    &"identifier",
                    &1i64,
                    &"a.arw",
                ],
            ),
        ] {
            let sql = identity_repair_page_sql(with_cursor);
            let plan = plan_of(&sql, &params);
            assert!(
                !plan.iter().any(|line| line.contains("B-TREE")),
                "with_cursor={with_cursor}: the ORDER BY fell back to a temp b-tree sort, so \
                 every page sorts the whole queue: {plan:?}"
            );
            let queue_step = plan
                .iter()
                .find(|line| line.contains("pending_sidecar_identity"))
                .unwrap_or_else(|| panic!("no step reads the queue at all: {plan:?}"));
            assert!(
                queue_step.contains("USING INDEX") || queue_step.contains("USING COVERING INDEX"),
                "with_cursor={with_cursor}: the queue must be driven by its primary-key \
                 index, got: {queue_step}"
            );
            if with_cursor {
                assert!(
                    queue_step.contains('>'),
                    "the cursor form must SEEK the index (a `>` constraint), not scan it from \
                     the top and filter: {queue_step}"
                );
            }
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

    /// #146: a legacy identifier leads a scan back to its row only when that row is the
    /// only one holding it and every primary copy it records is gone — not while one is
    /// still in place, and not while one sits on a volume that is not there to look at. A
    /// backup still in place does not hold it back (review F3): it is the same row's copy.
    #[test]
    fn a_legacy_identifier_matches_only_a_row_whose_every_primary_copy_is_gone() {
        let (catalog, root, dir) = temp_catalog("legacy-identifier-match");
        // A path distinct from every recorded copy: these assertions are about which row a
        // legacy identifier matches, not about a case-only rename, so `scanned` only needs to
        // not alias any copy below (`copy_still_in_place` then reduces to `exists()`).
        let scanned = root.join("scanned-probe.jpg");
        let (id, path) = seed_photo(&catalog, &root, "x.jpg");
        let backup = dir.path().join("backup");
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::write(backup.join("x.jpg"), b"raw-bytes").unwrap();
        let backup_volume = catalog
            .add_volume("Backup", &backup, crate::catalog::VolumeKind::Backup)
            .unwrap();
        catalog
            .add_location(id, backup_volume, "x.jpg", crate::catalog::LocationRole::Backup)
            .unwrap();
        // A second photo whose primary copy lives on a NAS volume.
        let (on_nas, nas_local) = seed_photo(&catalog, &root, "z.jpg");
        std::fs::remove_file(&nas_local).unwrap();
        let nas = dir.path().join("nas");
        std::fs::create_dir_all(&nas).unwrap();
        let nas_volume = catalog
            .add_volume("NAS", &nas, crate::catalog::VolumeKind::Local)
            .unwrap();
        catalog
            .add_location(on_nas, nas_volume, "z.jpg", crate::catalog::LocationRole::Primary)
            .unwrap();
        std::fs::write(nas.join("z.jpg"), b"raw-bytes").unwrap();
        catalog
            .conn()
            .execute_batch(&format!(
                "UPDATE photos SET uuid = 'dam:1' WHERE id = {id};
                 UPDATE photos SET uuid = 'dam:3' WHERE id = {on_nas};"
            ))
            .unwrap();
        assert_eq!(catalog.remint_non_identity_photos().unwrap(), 2);
        let uuid = photo_uuid(&catalog, id);
        assert!(is_photo_identity(&uuid), "{uuid}");

        assert_eq!(catalog.scan_identity(Some("dam:1"), 9, &scanned).unwrap(), None, "its file is still there");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(catalog.scan_identity(Some("dam:1"), 10, &scanned).unwrap(), None,
            "a file of another size is not the original, whatever DAM id it carries (N1)");
        assert_eq!(catalog.scan_identity(Some("dam:1"), 9, &scanned).unwrap(), Some(uuid.clone()),
            "the backup still in place is this row's own copy, not another photo");

        let nas_uuid = photo_uuid(&catalog, on_nas);
        assert_eq!(catalog.scan_identity(Some("dam:3"), 9, &scanned).unwrap(), None, "its NAS file is there");
        std::fs::remove_dir_all(&nas).unwrap();
        assert_eq!(catalog.scan_identity(Some("dam:3"), 9, &scanned).unwrap(), None,
            "the primary copy may still exist on the volume that is not mounted");
        std::fs::create_dir_all(&nas).unwrap();
        assert_eq!(catalog.scan_identity(Some("dam:3"), 9, &scanned).unwrap(), Some(nas_uuid));

        assert_eq!(catalog.scan_identity(Some(&uuid), 9, &scanned).unwrap(), Some(uuid.clone()));
        assert_eq!(
            catalog.scan_identity(Some(&uuid.to_ascii_uppercase()), 9, &scanned).unwrap(),
            Some(uuid.clone()),
            "a UUID is answered in its canonical lowercase spelling (#146)"
        );
        assert_eq!(catalog.scan_identity(Some("dam:2"), 9, &scanned).unwrap(), None);
        assert_eq!(catalog.scan_identity(None, 9, &scanned).unwrap(), None);

        // Two rows holding the same legacy value: the file could be either, so neither.
        let (other, other_path) = seed_photo(&catalog, &root, "y.jpg");
        std::fs::remove_file(&other_path).unwrap();
        catalog
            .conn()
            .execute(
                "INSERT INTO photo_legacy_identifiers(photo_id, identifier) VALUES(?1, 'dam:1')",
                params![other],
            )
            .unwrap();
        assert_eq!(catalog.scan_identity(Some("dam:1"), 9, &scanned).unwrap(), None);
    }

    // --- The re-home guard and file identity (#184) ------------------------------------------

    /// Catalogue `name` under `root` with its identity in its sidecar, as a scan leaves it.
    fn bound_photo(catalog: &Catalog, root: &Path, name: &str) -> (i64, PathBuf, String) {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"raw-bytes").unwrap();
        let up = catalog.upsert_photo(&path, None, 1, 9).unwrap();
        crate::xmp::write_identifier(&path, &up.uuid).unwrap();
        (up.id, path, up.uuid)
    }

    /// Through an intermediate name: on a case-insensitive filesystem a direct rename to a name
    /// differing only in case is a rename of a file onto itself, which POSIX makes a no-op.
    fn rename_with_sidecar(from: &Path, to: &Path) {
        for (from, to) in [(from.to_path_buf(), to.to_path_buf()), (crate::xmp::sidecar_path(from), crate::xmp::sidecar_path(to))] {
            let via = from.with_extension("renaming");
            std::fs::rename(&from, &via).unwrap();
            std::fs::rename(&via, to).unwrap();
        }
    }

    fn scan(catalog: &Catalog, path: &Path) -> crate::catalog::UpsertResult {
        let found = crate::xmp::read_identifier(path);
        catalog.upsert_scanned_photo(path, None, 1, 9, found.as_deref()).unwrap()
    }

    fn photo_count(catalog: &Catalog) -> i64 {
        catalog.conn().query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0)).unwrap()
    }

    /// A second name for the same file (a hard link) is a copy still in place: both names are
    /// in the folder, so the new one gets a row of its own, as before #184. Comparing inodes
    /// alone would call it a move, and the two names would then take turns owning the row.
    #[cfg(unix)]
    #[test]
    fn a_hard_link_beside_the_original_is_kept_apart() {
        let (catalog, root, _dir) = temp_catalog("rehome-hardlink");
        let (id, original, _) = bound_photo(&catalog, &root, "IMG.ARW");
        let link = root.join("img.arw");
        std::fs::hard_link(&original, &link).unwrap();
        std::fs::hard_link(crate::xmp::sidecar_path(&original), crate::xmp::sidecar_path(&link)).unwrap();

        let up = scan(&catalog, &link);

        assert!(up.created && up.id != id, "the original row was moved off a name still listed");
        assert_eq!(catalog.get_photo(id).unwrap().path, "IMG.ARW");
        assert_eq!(scan(&catalog, &original).id, id, "a rescan keeps each name on its own row");
        assert_eq!(scan(&catalog, &link).id, up.id);
    }

    /// The other half of the rule, where no case folding is involved: the recorded name is gone
    /// and the file is elsewhere (a plain move), so the row re-homes.
    #[test]
    fn a_moved_file_re_homes_its_row() {
        let (catalog, root, _dir) = temp_catalog("rehome-moved");
        let (id, original, uuid) = bound_photo(&catalog, &root, "a/IMG.ARW");
        let moved = root.join("b/IMG.ARW");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        rename_with_sidecar(&original, &moved);

        let up = scan(&catalog, &moved);

        assert_eq!((up.id, up.created, up.uuid.as_str()), (id, false, uuid.as_str()));
        assert_eq!(catalog.get_photo(id).unwrap().path, "b/IMG.ARW");
    }

    /// The guard on a real case-insensitive filesystem: a casefold tmpfs, mounted in a private
    /// user and mount namespace (`unshare -rm`), where this test binary runs
    /// [`case_only_renames_on_a_casefold_mount`]. Skipped where unprivileged namespaces, tmpfs
    /// casefold (Linux 6.13+) or `chattr` are not available.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_case_only_rename_on_a_case_insensitive_filesystem_re_homes_the_row() {
        const NAME: &str = "a_case_only_rename_on_a_case_insensitive_filesystem_re_homes_the_row";
        let dir = TestTmpDir::new("rehome-casefold-mount");
        let mount = dir.path().join("mnt");
        std::fs::create_dir_all(&mount).unwrap();
        let script = r#"mount -t tmpfs -o casefold tmpfs "$1" 2>/dev/null || exit 77
mkdir "$1/cf" && chattr +F "$1/cf" 2>/dev/null || exit 77
CHAIRPHOTO_CASEFOLD_DIR="$1/cf" exec "$2" --exact "$3" --ignored --nocapture --test-threads=1"#;
        let inner = "catalog::identity::tests::case_only_renames_on_a_casefold_mount";
        let out = std::process::Command::new("unshare")
            .args(["-rm", "sh", "-c", script, "sh"])
            .arg(&mount)
            .arg(std::env::current_exe().unwrap())
            .arg(inner)
            .output();
        let out = match out {
            Ok(out) if out.status.code() == Some(77) => {
                println!("SKIPPED: {NAME} — no casefold tmpfs in a user namespace here");
                return;
            }
            Ok(out) => out,
            Err(e) => {
                println!("SKIPPED: {NAME} — unshare unavailable: {e}");
                return;
            }
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stdout.contains("unshare: ") || stderr.contains("unshare: ") {
            println!("SKIPPED: {NAME} — unshare refused: {stderr}");
            return;
        }
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "the casefold run failed:\n{stdout}\n{stderr}"
        );
    }

    /// Run by [`a_case_only_rename_on_a_case_insensitive_filesystem_re_homes_the_row`] inside its
    /// namespace, with `CHAIRPHOTO_CASEFOLD_DIR` a casefold directory. A case-only rename of the
    /// file, then of its folder, re-homes the row each time; no second row appears.
    #[test]
    #[ignore = "run inside a casefold mount by the test above"]
    fn case_only_renames_on_a_casefold_mount() {
        let base = PathBuf::from(std::env::var_os("CHAIRPHOTO_CASEFOLD_DIR").expect("run by the outer test"));
        let root = base.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let db = TestTmpDir::new("rehome-casefold");
        let catalog = Catalog::open(&db.path().join("test.chairphoto"), &root).unwrap();
        let (id, original, uuid) = bound_photo(&catalog, &root, "Trip/IMG.ARW");
        assert!(root.join("trip/img.arw").exists(), "{} is not case-insensitive", base.display());

        let renamed = root.join("Trip/img.arw");
        rename_with_sidecar(&original, &renamed);
        let up = scan(&catalog, &renamed);
        assert_eq!((up.id, up.created, up.uuid.as_str()), (id, false, uuid.as_str()), "file renamed by case");
        assert_eq!(catalog.get_photo(id).unwrap().path, "Trip/img.arw");

        std::fs::rename(root.join("Trip"), root.join("trip-renaming")).unwrap();
        std::fs::rename(root.join("trip-renaming"), root.join("trip")).unwrap();
        let moved = root.join("trip/img.arw");
        let up = scan(&catalog, &moved);
        assert_eq!((up.id, up.created), (id, false), "folder renamed by case");
        assert_eq!(catalog.get_photo(id).unwrap().path, "trip/img.arw");
        assert_eq!(photo_count(&catalog), 1);
        assert_eq!(scan(&catalog, &moved).id, id, "a rescan finds it at its new name");
    }

    /// The legacy-identifier re-home guard (`every_primary_copy_is_gone`) on the same
    /// case-insensitive filesystem as the test above (#224 L1, the half of #184 left open:
    /// that fix covered a bound UUID, not a legacy DAM id). A casefold tmpfs mounted in a
    /// private user and mount namespace, where this test binary runs
    /// [`legacy_identity_case_only_renames_on_a_casefold_mount`]. Skipped where unprivileged
    /// namespaces, tmpfs casefold (Linux 6.13+) or `chattr` are not available.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_legacy_identity_case_only_rename_on_a_case_insensitive_filesystem_re_homes_the_row() {
        const NAME: &str =
            "a_legacy_identity_case_only_rename_on_a_case_insensitive_filesystem_re_homes_the_row";
        let dir = TestTmpDir::new("rehome-legacy-casefold-mount");
        let mount = dir.path().join("mnt");
        std::fs::create_dir_all(&mount).unwrap();
        let script = r#"mount -t tmpfs -o casefold tmpfs "$1" 2>/dev/null || exit 77
mkdir "$1/cf" && chattr +F "$1/cf" 2>/dev/null || exit 77
CHAIRPHOTO_CASEFOLD_DIR="$1/cf" exec "$2" --exact "$3" --ignored --nocapture --test-threads=1"#;
        let inner = "catalog::identity::tests::legacy_identity_case_only_renames_on_a_casefold_mount";
        let out = std::process::Command::new("unshare")
            .args(["-rm", "sh", "-c", script, "sh"])
            .arg(&mount)
            .arg(std::env::current_exe().unwrap())
            .arg(inner)
            .output();
        let out = match out {
            Ok(out) if out.status.code() == Some(77) => {
                println!("SKIPPED: {NAME} — no casefold tmpfs in a user namespace here");
                return;
            }
            Ok(out) => out,
            Err(e) => {
                println!("SKIPPED: {NAME} — unshare unavailable: {e}");
                return;
            }
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stdout.contains("unshare: ") || stderr.contains("unshare: ") {
            println!("SKIPPED: {NAME} — unshare refused: {stderr}");
            return;
        }
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "the casefold run failed:\n{stdout}\n{stderr}"
        );
    }

    /// Run by the test above inside its namespace, with `CHAIRPHOTO_CASEFOLD_DIR` a casefold
    /// directory. A file whose sidecar carries a legacy (non-UUID) identifier, renamed by case
    /// only (file, then its folder), re-homes the row that identifier belongs to each time.
    /// Before #224 L1's fix, `every_primary_copy_is_gone` tested the recorded path with
    /// `exists()`, which the casefold mount folds onto the renamed file, so the renamed file
    /// looked like a different photo and got a second row (confirmed by the #224 review's
    /// probe: "first=1 up.id=2 created=true rows=2").
    #[test]
    #[ignore = "run inside a casefold mount by the test above"]
    fn legacy_identity_case_only_renames_on_a_casefold_mount() {
        let base = PathBuf::from(std::env::var_os("CHAIRPHOTO_CASEFOLD_DIR").expect("run by the outer test"));
        let root = base.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let db = TestTmpDir::new("rehome-legacy-casefold");
        let catalog = Catalog::open(&db.path().join("test.chairphoto"), &root).unwrap();

        let path = root.join("Trip/IMG.ARW");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"raw-bytes").unwrap();
        crate::xmp::write_identifier(&path, "dam:7").unwrap();
        let first = scan(&catalog, &path);
        assert!(first.created);
        assert!(!is_photo_identity("dam:7"), "a DAM id is not a UUID");

        assert!(root.join("trip/img.arw").exists(), "{} is not case-insensitive", base.display());

        let renamed = root.join("Trip/img.arw");
        rename_with_sidecar(&path, &renamed);
        let up = scan(&catalog, &renamed);
        assert_eq!((up.id, up.created), (first.id, false), "file renamed by case");
        assert_eq!(catalog.get_photo(first.id).unwrap().path, "Trip/img.arw");

        std::fs::rename(root.join("Trip"), root.join("trip-renaming")).unwrap();
        std::fs::rename(root.join("trip-renaming"), root.join("trip")).unwrap();
        let moved = root.join("trip/img.arw");
        let up = scan(&catalog, &moved);
        assert_eq!((up.id, up.created), (first.id, false), "folder renamed by case");
        assert_eq!(catalog.get_photo(first.id).unwrap().path, "trip/img.arw");
        assert_eq!(photo_count(&catalog), 1, "no second row for the legacy id");
    }

    /// #146 (L5): Adopt stores the sidecar's identity lowercase, and the conflict it
    /// resolved — and a copy carrying the same identity upper-case — are bound.
    #[test]
    fn adopt_stores_an_upper_case_identity_lowercase() {
        let (catalog, root, _dir) = temp_catalog("adopt-uppercase");
        const FOREIGN: &str = "6F1C1F0E-2B7A-4C3D-9E8F-0A1B2C3D4E5F";
        let (id, path, _) = seed_conflicted_copy(&catalog, &root, "x.arw", FOREIGN);
        let (volume_id, relative_path) = copy_of(&catalog, &path);
        let outcome = catalog
            .resolve_identity_conflict(id, volume_id, &relative_path, IdentityConflictAction::Adopt)
            .unwrap();
        let canonical = FOREIGN.to_ascii_lowercase();
        assert_eq!(outcome.catalog_uuid, canonical);
        assert_eq!(outcome.previous_sidecar_uuid, FOREIGN);
        assert_eq!(photo_uuid(&catalog, id), canonical);
        assert_eq!(queue_row(&catalog, id, &path), None);
        assert_eq!(crate::xmp::read_identifier(&path).as_deref(), Some(FOREIGN), "Adopt never writes");
        assert_eq!(bind_sidecar_identity(&path, &canonical, Some(FOREIGN)), SidecarIdentity::Bound);
    }

    /// #146 review F1: v23 maps a legacy value to its v5 identity, unless another row
    /// already holds that identity (a migrated catalog's bundle merged in first). Then the
    /// two rows claim one photo, so this one gets a v4 rather than failing the migration on
    /// `photos.uuid`'s UNIQUE, and its copy is queued as a conflict for a person.
    #[test]
    fn v23_keeps_a_row_apart_when_its_legacy_identity_is_already_held() {
        let (catalog, root, _dir) = temp_catalog("legacy-identity-held");
        let (held, _) = seed_photo(&catalog, &root, "held.jpg");
        let (legacy, path) = seed_photo(&catalog, &root, "legacy.jpg");
        let mapped = legacy_photo_identity("dam:9");
        catalog
            .conn()
            .execute_batch(&format!(
                "UPDATE photos SET uuid = '{mapped}' WHERE id = {held};
                 UPDATE photos SET uuid = 'dam:9' WHERE id = {legacy};"
            ))
            .unwrap();
        assert_eq!(catalog.remint_non_identity_photos().unwrap(), 1);
        assert_eq!(photo_uuid(&catalog, held), mapped);
        let reminted = photo_uuid(&catalog, legacy);
        assert!(is_photo_identity(&reminted) && reminted != mapped, "{reminted}");
        assert!(queue_row(&catalog, legacy, &path).is_some_and(|(_, e, _)| e.contains("dam:9")));
    }

    /// #146 review F7: a scan works out a legacy identifier only when the path did not
    /// match. A rescan of a legacy file still at its path pays no lookup and no stats.
    #[test]
    fn a_rescan_at_the_same_path_does_not_look_up_a_legacy_identifier() {
        let (catalog, root, _dir) = temp_catalog("legacy-lookup-after-path");
        let (id, path) = seed_photo(&catalog, &root, "x.jpg");
        catalog
            .conn()
            .execute("UPDATE photos SET uuid = 'dam:1' WHERE id = ?1", params![id])
            .unwrap();
        catalog.remint_non_identity_photos().unwrap();
        let lookups = || LEGACY_LOOKUPS.with(|n| n.get());

        let before = lookups();
        let again = catalog.upsert_scanned_photo(&path, None, 1, 9, Some("dam:1")).unwrap();
        assert_eq!((again.id, again.created), (id, false));
        assert_eq!(lookups(), before, "matched by path: no legacy lookup");
        let on_volume = catalog.upsert_scanned_photo_on_volume(&path, 1, 9, Some("dam:1")).unwrap();
        assert_eq!((on_volume.id, on_volume.created), (id, false));
        assert_eq!(lookups(), before, "matched by location: no legacy lookup");

        let elsewhere = root.join("y.jpg");
        std::fs::write(&elsewhere, b"raw-bytes").unwrap();
        let other = catalog.upsert_scanned_photo(&elsewhere, None, 1, 9, Some("dam:1")).unwrap();
        assert!(other.created, "x.jpg is still there, so y.jpg is another photo");
        assert_eq!(lookups(), before + 1, "a path miss does look it up");
    }

    /// #150 (review F4 of #146): a file catalogued after #141 whose sidecar holds a DAM id
    /// gets a minted UUID; the DAM id is recorded as the row's legacy identifier, so moving
    /// the file re-homes the row instead of cataloguing it a second time. A row catalogued
    /// before the record existed gains it at its next rescan.
    #[test]
    fn a_scan_records_the_foreign_id_of_a_file_it_mints_for() {
        let (catalog, root, _dir) = temp_catalog("legacy-recorded-at-mint");
        let path = root.join("x.jpg");
        std::fs::write(&path, b"raw-bytes").unwrap();
        let first = catalog.upsert_scanned_photo(&path, None, 1, 9, Some("dam:asset/77")).unwrap();
        assert!(first.created && is_photo_identity(&first.uuid), "{}", first.uuid);
        catalog.set_culling(first.id, Some(5), None, None).unwrap();

        std::fs::remove_file(&path).unwrap();
        let moved = root.join("y.jpg");
        std::fs::write(&moved, b"raw-bytes").unwrap();
        let again = catalog.upsert_scanned_photo(&moved, None, 2, 9, Some("dam:asset/77")).unwrap();
        assert_eq!((again.id, again.created), (first.id, false), "re-homed, not duplicated");
        assert_eq!(catalog.get_photo(first.id).unwrap().path, "y.jpg");
        assert_eq!(catalog.get_photo(first.id).unwrap().rating, 5);

        // A row from before the record existed: its next rescan records it.
        catalog.conn().execute("DELETE FROM photo_legacy_identifiers", []).unwrap();
        catalog.upsert_scanned_photo(&moved, None, 2, 9, Some("dam:asset/77")).unwrap();
        let owner: i64 = catalog
            .conn()
            .query_row(
                "SELECT photo_id FROM photo_legacy_identifiers WHERE identifier = 'dam:asset/77'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, first.id);
    }

    /// #150 (nit from the #146 re-review): a re-home within a volume indexed in place drops
    /// the debt queued for the path it left, as a root re-home does — that row could only
    /// ever report a file that is no longer there as unreachable.
    #[test]
    fn a_volume_rehome_drops_the_debt_of_the_path_it_left() {
        let (catalog, _root, dir) = temp_catalog("volume-rehome-debt");
        const KNOWN: &str = "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f";
        let nas = dir.path().join("nas");
        std::fs::create_dir_all(&nas).unwrap();
        catalog.add_volume("NAS", &nas, crate::catalog::VolumeKind::Backup).unwrap();
        let before = nas.join("x.jpg");
        std::fs::write(&before, b"raw-bytes").unwrap();
        let row = catalog.upsert_photo_on_volume(&before, 1, 9, Some(KNOWN)).unwrap();
        catalog.record_sidecar_identity(row.id, &before, &SidecarIdentity::Unwritable("ro".into())).unwrap();
        assert!(queue_row(&catalog, row.id, &before).is_some());

        std::fs::remove_file(&before).unwrap();
        let after = nas.join("y.jpg");
        std::fs::write(&after, b"raw-bytes").unwrap();
        let moved = catalog.upsert_scanned_photo_on_volume(&after, 1, 9, Some(KNOWN)).unwrap();
        assert_eq!((moved.id, moved.created), (row.id, false));
        assert_eq!(queue_row(&catalog, row.id, &before), None, "the path it left owes nothing");
    }

    /// #150 (nit from the #146 re-review): a trusted (manifest) id is not recorded on the row
    /// that merely sits at the same path — that is the file already there, which can be a
    /// different photo — while a scanned sidecar's id, which describes that very file, is.
    #[test]
    fn a_trusted_id_is_not_recorded_on_a_row_matched_by_path() {
        let (catalog, root, _dir) = temp_catalog("trusted-by-path");
        let (id, path) = seed_photo(&catalog, &root, "x.jpg");
        let legacy = |photo: i64| -> Option<String> {
            catalog
                .conn()
                .query_row(
                    "SELECT identifier FROM photo_legacy_identifiers WHERE photo_id = ?1",
                    params![photo],
                    |r| r.get(0),
                )
                .optional()
                .unwrap()
        };
        let same = catalog.upsert_photo_with_identity(&path, None, 1, 9, Some("dam:asset/9")).unwrap();
        assert_eq!((same.id, same.created), (id, false));
        let on_volume = catalog.upsert_photo_on_volume(&path, 1, 9, Some("dam:asset/9")).unwrap();
        assert_eq!(on_volume.id, id);
        assert_eq!(legacy(id), None, "the manifest's id is not this file's");
        catalog.upsert_scanned_photo(&path, None, 1, 9, Some("dam:asset/9")).unwrap();
        assert_eq!(legacy(id).as_deref(), Some("dam:asset/9"), "its own sidecar's is");
    }

    // --- bulk resolution of non-UUID conflicts (#150 L6) ------------------------------

    /// Overwrite in bulk acts on every un-dismissed copy whose recorded conflict is a non-UUID,
    /// re-reading each sidecar first: a UUID conflict is left for its own decision, a copy
    /// whose sidecar has since taken a UUID is skipped, and a dismissed copy is not touched.
    #[test]
    fn bulk_overwrite_resolves_only_non_uuid_conflicts() {
        let (catalog, root, _dir) = temp_catalog("bulk-overwrite");
        const OTHER: &str = "0d9c8b7a-6f5e-4d3c-8b2a-190807060504";
        let (a, a_path, a_uuid) = seed_conflicted_copy(&catalog, &root, "a.jpg", "dam:1");
        let (b, b_path, _) = seed_conflicted_copy(&catalog, &root, "b.jpg", OTHER);
        let (c, c_path, _) = seed_conflicted_copy(&catalog, &root, "c.jpg", "dam:3");
        let (d, d_path, _) = seed_conflicted_copy(&catalog, &root, "d.jpg", "dam:4");
        let (cv, cr) = copy_of(&catalog, &c_path);
        catalog.resolve_identity_conflict(c, cv, &cr, IdentityConflictAction::Dismiss).unwrap();
        // d's sidecar changes to another photo's UUID after its conflict was recorded.
        crate::xmp::overwrite_identifier(&d_path, OTHER).unwrap();
        assert_eq!(catalog.count_foreign_conflicts().unwrap(), 2);

        let mut seen = Vec::new();
        let summary = catalog
            .run_resolve_foreign_conflicts(ForeignConflictAction::Overwrite, &AtomicBool::new(false), |s| {
                seen.push(s.done())
            })
            .unwrap();
        assert_eq!(
            summary,
            ForeignConflictSummary { total: 2, overwritten: 1, skipped: 1, ..Default::default() }
        );
        assert_eq!(seen, [1, 2]);
        assert_eq!(crate::xmp::read_identifier(&a_path).as_deref(), Some(a_uuid.as_str()));
        assert_eq!(queue_row(&catalog, a, &a_path), None, "a is bound");
        assert!(queue_row(&catalog, b, &b_path).is_some_and(|(_, e, _)| e.contains(OTHER)));
        assert_eq!(crate::xmp::read_identifier(&b_path).as_deref(), Some(OTHER), "a UUID is never overwritten in bulk");
        assert!(queue_row(&catalog, c, &c_path).is_some_and(|(_, _, dismissed)| dismissed != 0));
        assert_eq!(crate::xmp::read_identifier(&c_path).as_deref(), Some("dam:3"));
        assert_eq!(crate::xmp::read_identifier(&d_path).as_deref(), Some(OTHER));
        assert!(queue_row(&catalog, d, &d_path).is_some());
    }

    /// #150 (review M1): Overwrite replaces every `xmp:Identifier` value, while a conflict
    /// records only the first. A sidecar whose DAM id sits beside another photo's UUID — in
    /// one Bag, or in a second Description — is skipped in bulk, never overwritten, and a
    /// sidecar with two non-UUID values is skipped too.
    #[test]
    fn bulk_overwrite_skips_a_sidecar_with_more_than_one_identifier_value() {
        let (catalog, root, _dir) = temp_catalog("bulk-overwrite-multi");
        const OTHER: &str = "0d9c8b7a-6f5e-4d3c-8b2a-190807060504";
        let head = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#""#;
        let bag = format!(
            r#"{head}><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/">
<xmp:Identifier><rdf:Bag><rdf:li>dam:asset/1</rdf:li><rdf:li>{OTHER}</rdf:li></rdf:Bag></xmp:Identifier>
</rdf:Description></rdf:RDF></x:xmpmeta>"#
        );
        let two_descriptions = format!(
            r#"{head}><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Identifier="dam:asset/2"/>
<rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/">
<xmp:Identifier><rdf:Bag><rdf:li>{OTHER}</rdf:li></rdf:Bag></xmp:Identifier></rdf:Description>
</rdf:RDF></x:xmpmeta>"#
        );
        let two_foreign = format!(
            r#"{head}><rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/">
<xmp:Identifier><rdf:Bag><rdf:li>dam:asset/3</rdf:li><rdf:li>dam:asset/33</rdf:li></rdf:Bag></xmp:Identifier>
</rdf:Description></rdf:RDF></x:xmpmeta>"#
        );
        let mut paths = Vec::new();
        for (name, xml, first) in [
            ("bag.arw", &bag, "dam:asset/1"),
            ("two.arw", &two_descriptions, "dam:asset/2"),
            ("foreign.arw", &two_foreign, "dam:asset/3"),
        ] {
            let path = root.join(name);
            std::fs::write(&path, b"raw-bytes").unwrap();
            std::fs::write(crate::xmp::sidecar_path(&path), xml).unwrap();
            let up = catalog.upsert_photo(&path, None, 1, 9).unwrap();
            let found = crate::xmp::read_identifier(&path);
            assert_eq!(found.as_deref(), Some(first), "{name}: the first value is the one recorded");
            let outcome = catalog.ensure_sidecar_identity(up.id, &path, &up.uuid, found.as_deref()).unwrap();
            assert_eq!(outcome, SidecarIdentity::Conflict(first.to_string()));
            paths.push(path);
        }
        assert_eq!(catalog.count_foreign_conflicts().unwrap(), 3);

        let summary = catalog
            .run_resolve_foreign_conflicts(ForeignConflictAction::Overwrite, &AtomicBool::new(false), |_| {})
            .unwrap();
        assert_eq!(summary, ForeignConflictSummary { total: 3, skipped: 3, ..Default::default() });
        for path in &paths[..2] {
            assert!(
                crate::xmp::read_identifiers(path).iter().any(|v| v == OTHER),
                "{}: the other photo's UUID is still in the sidecar",
                path.display()
            );
        }
        assert_eq!(crate::xmp::read_identifiers(&paths[2]), ["dam:asset/3", "dam:asset/33"]);
    }

    /// Dismiss in bulk pages through a queue longer than one page, interleaved with UUID
    /// conflicts it must leave alone, and stops at its abort flag between copies.
    #[test]
    fn bulk_dismiss_pages_past_uuid_conflicts_and_stops_at_its_abort_flag() {
        let (catalog, root, _dir) = temp_catalog("bulk-dismiss");
        let mut foreign = Vec::new();
        for i in 0..(REPAIR_PAGE_SIZE as usize + 40) {
            if i % 3 == 0 {
                let other = uuid::Uuid::new_v4().to_string();
                seed_conflicted_copy(&catalog, &root, &format!("u{i}.jpg"), &other);
            } else {
                foreign.push(seed_conflicted_copy(&catalog, &root, &format!("f{i}.jpg"), &format!("dam:{i}")));
            }
        }
        let abort = AtomicBool::new(false);
        let stopped = catalog
            .run_resolve_foreign_conflicts(ForeignConflictAction::Dismiss, &abort, |s| {
                if s.done() == 5 {
                    abort.store(true, Ordering::Relaxed);
                }
            })
            .unwrap();
        assert!(stopped.aborted);
        assert_eq!((stopped.dismissed, stopped.total), (5, foreign.len()));

        let rest = catalog
            .run_resolve_foreign_conflicts(ForeignConflictAction::Dismiss, &AtomicBool::new(false), |_| {})
            .unwrap();
        assert_eq!(rest, ForeignConflictSummary {
            total: foreign.len() - 5,
            dismissed: foreign.len() - 5,
            ..Default::default()
        });
        assert_eq!(catalog.count_foreign_conflicts().unwrap(), 0);
        for (id, path, _) in &foreign {
            assert!(queue_row(&catalog, *id, path).is_some_and(|(_, _, d)| d != 0), "{}", path.display());
        }
        let summary = catalog.summarize_pending_identity().unwrap();
        assert_eq!(summary.conflicts as usize, (REPAIR_PAGE_SIZE as usize + 40).div_ceil(3), "UUID conflicts stay");
    }

    /// #146 review N4: v23 agrees with merge that a blank `photos.uuid` names no identity. It
    /// gets a random v4 — never `legacy_photo_identity` of whitespace, which every catalog
    /// would share — and no legacy identifier is recorded for it.
    #[test]
    fn v23_gives_a_blank_identity_a_v4_and_no_legacy_record() {
        let (catalog, root, _dir) = temp_catalog("blank-identity-v23");
        let scanned = root.join("scanned-probe.jpg");
        let (id, _) = seed_photo(&catalog, &root, "x.jpg");
        catalog
            .conn()
            .execute("UPDATE photos SET uuid = '  ' WHERE id = ?1", params![id])
            .unwrap();
        assert_eq!(catalog.remint_non_identity_photos().unwrap(), 1);
        let uuid = photo_uuid(&catalog, id);
        assert!(is_photo_identity(&uuid), "{uuid}");
        assert_ne!(uuid, legacy_photo_identity("  "));
        let legacy: i64 = catalog
            .conn()
            .query_row("SELECT count(*) FROM photo_legacy_identifiers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(legacy, 0);

        // A scanned sidecar with a blank identifier is no identity either: no lookup at all.
        let before = LEGACY_LOOKUPS.with(|n| n.get());
        assert_eq!(catalog.scan_identity(Some("  "), 9, &scanned).unwrap(), None);
        assert_eq!(LEGACY_LOOKUPS.with(|n| n.get()), before);
    }
}
