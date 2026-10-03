//! Backend → frontend events, independent of any UI toolkit.
//!
//! Background work reports progress and terminal results as [`CoreEvent`]s through an
//! [`EventSink`]. The Tauri shell's sink forwards each one as the webview event of the same
//! name with the same payload; a native frontend routes them to its views.
//!
//! Every event has a stable wire name (`scan:progress`, `faces:index_done`, …) — the
//! Tauri frontend listens by that name — and a payload type that serializes exactly as the
//! frontend's DTO expects. The payload types that used to live beside their commands are
//! defined here, so the event vocabulary does not depend on the command layer.

use serde::Serialize;

/// Where background work sends its events. `send` must not block: it is called from worker
/// threads and async tasks.
pub trait EventSink: Send + Sync {
    fn send(&self, event: CoreEvent);
}

/// A sink that drops everything — for tests and callers with no UI to tell.
pub struct NoEvents;

impl EventSink for NoEvents {
    fn send(&self, _event: CoreEvent) {}
}

/// Receives an event's wire name and its concretely typed payload (see
/// [`CoreEvent::visit`]), so a transport can serialize the payload as-is.
pub trait EventVisitor {
    fn visit<T: Serialize + Clone>(&self, name: &'static str, payload: &T);
}

macro_rules! core_events {
    ($( $(#[doc = $doc:expr])* $(#[cfg($cfg:meta)])* $variant:ident($ty:ty) = $name:expr, )*) => {
        /// Every event the backend emits. The variant's wire name is [`CoreEvent::name`].
        #[derive(Clone)]
        pub enum CoreEvent {
            $( $(#[doc = $doc])* $(#[cfg($cfg)])* $variant($ty), )*
        }

        impl CoreEvent {
            /// Every wire name this build can emit.
            pub const WIRE_NAMES: &'static [&'static str] = &[ $( $(#[cfg($cfg)])* $name, )* ];

            /// The event's wire name, e.g. `"scan:progress"`.
            pub fn name(&self) -> &'static str {
                match self {
                    $( $(#[cfg($cfg)])* Self::$variant(_) => $name, )*
                }
            }

            /// Hand the wire name and the typed payload to `visitor`.
            pub fn visit<V: EventVisitor>(&self, visitor: &V) {
                match self {
                    $( $(#[cfg($cfg)])* Self::$variant(p) => visitor.visit($name, p), )*
                }
            }
        }
    };
}

core_events! {
    ScanProgress(crate::scanner::ScanProgress) = "scan:progress",
    ImportProgress(ImportProgress) = "import:progress",
    /// An export's progress (`exports`): photos to a folder, or a batch as a bundle.
    ExportProgress(ExportProgress) = "export:progress",
    CacheProgress(CacheProgress) = "cache:progress",
    /// The catalog at this path is now the open one; the UI resets against it.
    CatalogSwitched(String) = "catalog:switched",
    SharpnessProgress(SharpnessProgressEvent) = "sharpness:progress",
    SharpnessIndexDone(SharpnessIndexDone) = "sharpness:index_done",
    PhashProgress(PhashProgressEvent) = "phash:progress",
    PhashIndexDone(PhashIndexDone) = "phash:index_done",
    IdentityRepairProgress(IdentityRepairProgress) = "identity:repair_progress",
    IdentityRepairDone(IdentityRepairDone) = "identity:repair_done",
    IdentityResolveProgress(IdentityResolveProgress) = "identity:resolve_progress",
    IdentityResolveDone(IdentityResolveDone) = "identity:resolve_done",
    #[cfg(feature = "faces")]
    FacesProgress(FacesProgressEvent) = "faces:progress",
    #[cfg(feature = "faces")]
    FacesIndexDone(FacesIndexDone) = "faces:index_done",
    #[cfg(feature = "faces")]
    FacesMatchProgress(FacesMatchProgressEvent) = "faces:match_progress",
    #[cfg(feature = "faces")]
    FacesMatchDone(FacesMatchDone) = "faces:match_done",
    #[cfg(feature = "smarttags")]
    SmarttagsProgress(SmarttagsProgressEvent) = "smarttags:progress",
    #[cfg(feature = "smarttags")]
    SmarttagsIndexDone(SmarttagsIndexDone) = "smarttags:index_done",
    #[cfg(feature = "smarttags")]
    SmarttagsDownloadProgress(SmarttagsDownloadProgressEvent) = "smarttags:download_progress",
    #[cfg(feature = "map")]
    GeocodeProgress(GeocodeProgress) = "geocode:progress",
    #[cfg(feature = "localsend")]
    LocalSendProgress(LocalSendProgress) = "localsend:progress",
    #[cfg(feature = "slideshow")]
    SlideshowProgress(SlideshowProgress) = "slideshow:progress",
    #[cfg(all(feature = "raw", feature = "edit"))]
    DevelopSource(crate::develop::session::DevelopSourceEvent) = "develop:source",
    /// An external editor round-trip's phase (`external_edit`).
    DevelopProgress(crate::external_edit::DevelopProgress) = "develop:progress",
    RapidRawProgress(crate::rapidraw::RapidRawProgress) = "rapidraw:progress",
    ThemeChanged(crate::appearance::SystemThemeResult) = crate::appearance::THEME_CHANGED_EVENT,
}

// ── Payloads that used to live beside their commands ─────────────────────────────────

/// Progress event payload for batch caching, emitted as `cache:progress`. `job` is the
/// warm-up's job id (`app::cache`), so a front end drops a superseded warm-up's stragglers.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheProgress {
    pub done: usize,
    pub total: usize,
    pub job: u64,
}

/// Progress event payload for slideshow encoding, emitted as `slideshow:progress`. `done`/
/// `total` are raw ffmpeg output-frame counts (mirrors `import:progress`'s shape); `job` is
/// the render's job id (`app::slideshow`), so a front end drops a superseded render's.
#[cfg(feature = "slideshow")]
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlideshowProgress {
    pub done: u32,
    pub total: u32,
    pub job: u64,
}

/// Progress event payload for a LocalSend transfer, emitted as `localsend:progress`.
#[cfg(feature = "localsend")]
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSendProgress {
    pub done: usize,
    pub total: usize,
    /// The send this belongs to (`app::localsend::claim_send`), so a front end drops a
    /// superseded send's stragglers.
    pub job: u64,
}

/// Progress event payload for card import, emitted as `import:progress` during the copy.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportProgress {
    /// The import job this progress belongs to (`scans::claim_import`), so a front end can
    /// drop a superseded or switched-away import's stragglers. Ids start at 1; `0` is the
    /// Tauri bundle export, which reuses this event and is no import job.
    pub job: u64,
    pub done: usize,
    pub total: usize,
}

/// Which export an `export:progress` event belongs to: the two are separate job families
/// (`JobRegistry::export`, `JobRegistry::bundle_export`) with their own job ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ExportKind {
    /// Photos written to a folder (the Export dialog).
    Photos,
    /// An import batch written as a `.chairphoto` bundle.
    Bundle,
}

/// Progress event payload for an export, emitted as `export:progress`: `done` of `total`
/// steps (photos; a bundle counts two steps per photo, original and preview). Carries the
/// kind and job id so a front end drops a superseded or switched-away export's stragglers.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportProgress {
    pub kind: ExportKind,
    pub job: u64,
    pub done: usize,
    pub total: usize,
}

/// Progress event payload for `geocode_all_to_iptc`, emitted as `geocode:progress`.
#[cfg(feature = "map")]
#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GeocodeProgress {
    pub done: usize,
    pub total: usize,
    pub filled: usize,
}

/// Progress event payload for `sharpness:progress`. Carries the job id so the UI can
/// ignore stragglers from a superseded run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SharpnessProgressEvent {
    pub done: usize,
    pub total: usize,
    pub job: u64,
}

/// Terminal event for the sharpness-indexing job.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SharpnessIndexDone {
    pub ok: bool,
    pub done: usize,
    pub total: usize,
    pub failed: usize,
    pub offline: usize,
    pub aborted: bool,
    pub job: u64,
    pub error: Option<String>,
}

/// Progress event payload for `phash:progress`. Carries the job id so the UI can ignore
/// stragglers from a superseded run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PhashProgressEvent {
    pub done: usize,
    pub total: usize,
    pub job: u64,
}

/// Terminal event for the perceptual-hash indexing job.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PhashIndexDone {
    pub ok: bool,
    pub done: usize,
    pub total: usize,
    pub failed: usize,
    pub offline: usize,
    pub aborted: bool,
    pub job: u64,
    pub error: Option<String>,
}

/// Download progress event payload for `smarttags:download_progress`. Sent approximately
/// every 1 MiB (or 1% of total) so the UI can render a live progress bar without event
/// spam. `total` is `None` when the server omitted `Content-Length`.
#[cfg(feature = "smarttags")]
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmarttagsDownloadProgressEvent {
    pub done: u64,
    pub total: Option<u64>,
}

/// Progress event payload for `smarttags:progress`. Carries the job id so the UI can
/// ignore stragglers from a superseded run.
#[cfg(feature = "smarttags")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct SmarttagsProgressEvent {
    pub done: usize,
    pub total: usize,
    pub job: u64,
}

/// Terminal event payload for `smarttags:index_done`. Honest breakdown: `offline` /
/// `failed` / `aborted` say why `done < total` instead of leaving the UI to guess.
#[cfg(feature = "smarttags")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct SmarttagsIndexDone {
    pub ok: bool,
    pub done: usize,
    pub total: usize,
    pub offline: usize,
    pub failed: usize,
    pub aborted: bool,
    pub job: u64,
    pub error: Option<String>,
}

/// Terminal event payload for `faces_index_photos` (`faces:index_done`). Progress events
/// alone can't signal completion unambiguously (a run with nothing to index emits only
/// `{0, 0}`, which is indistinguishable from a failure), so completion gets its own event.
/// `job` identifies which run finished — starting a new run aborts the previous one, and
/// the UI must not mistake the superseded run's done-event for its own job completing.
/// `offline`/`failed`/`aborted` say *why* `done < total` instead of leaving the UI to guess.
#[cfg(feature = "faces")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct FacesIndexDone {
    pub ok: bool,
    pub done: usize,
    pub total: usize,
    pub offline: usize,
    pub failed: usize,
    pub aborted: bool,
    pub job: u64,
    pub error: Option<String>,
}

/// Progress event payload (`faces:progress`) for the indexing job — the indexer's
/// `{done, total}` plus the job id, so a superseded job's stragglers can be ignored.
#[cfg(feature = "faces")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct FacesProgressEvent {
    pub done: usize,
    pub total: usize,
    pub job: u64,
}

/// Progress payload for `faces:match_progress`: the pipeline step label plus counts, and
/// the job id so the UI can drop stragglers from a superseded run.
///
/// Matching used to share `faces:progress` with indexing and be told apart by the *absence*
/// of a `job` field. Now that matching is a real job it has a job id of its own, so it also
/// has an event of its own — discriminating two job families by a missing field does not
/// survive both of them having one.
#[cfg(feature = "faces")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct FacesMatchProgressEvent {
    pub done: usize,
    pub total: usize,
    pub phase: &'static str,
    pub job: u64,
}

/// Terminal event payload for `faces:match_done`.
///
/// The pipeline counters live here rather than in the command's return value: the command
/// returns as soon as the job has *started*, so this event is the run's actual result, not
/// a progress notification. `aborted` says why `outcome` may be short.
#[cfg(feature = "faces")]
#[derive(Debug, Clone, serde::Serialize)]
pub struct FacesMatchDone {
    pub ok: bool,
    pub outcome: Option<crate::plugins::faces::MatchOutcome>,
    pub aborted: bool,
    pub job: u64,
    pub error: Option<String>,
}

/// Progress event for `identity:repair_progress`. Carries the job id so the UI can drop a
/// superseded pass's stragglers.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityRepairProgress {
    pub done: usize,
    pub total: usize,
    pub job: u64,
}

/// Terminal event for `identity:repair_done`.
///
/// The summary carries `aborted` and `total` itself, so a pass stopped by a Cancel or a
/// catalog switch reports partial counters that say they are partial rather than reading as
/// a finished result. `ok: false` with an `error` is the other terminal shape — a pass that
/// could not run at all.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityRepairDone {
    pub ok: bool,
    pub job: u64,
    pub summary: crate::catalog::IdentityRepairSummary,
    pub error: Option<String>,
}

/// Progress event for `identity:resolve_progress`, a bulk resolution of non-UUID identity
/// conflicts (#150). Carries the job id so a front end drops a superseded run's.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityResolveProgress {
    pub done: usize,
    pub total: usize,
    pub job: u64,
}

/// Terminal event for `identity:resolve_done`. `summary.aborted` marks a run stopped by a
/// Cancel, a newer run or a catalog switch, whose counts are partial; `ok: false` with an
/// `error` is a run that could not start or stopped on a catalog error, with an empty summary
/// (the decisions it made before the error stand; the debt panel's reload shows them).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityResolveDone {
    pub ok: bool,
    pub job: u64,
    pub action: crate::catalog::ForeignConflictAction,
    pub summary: crate::catalog::ForeignConflictSummary,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records what a transport would put on the wire.
    struct Record(RefCell<Vec<(&'static str, serde_json::Value)>>);

    impl EventVisitor for Record {
        fn visit<T: Serialize + Clone>(&self, name: &'static str, payload: &T) {
            self.0.borrow_mut().push((name, serde_json::to_value(payload).unwrap()));
        }
    }

    fn wire(event: CoreEvent) -> (&'static str, serde_json::Value) {
        let rec = Record(RefCell::new(Vec::new()));
        event.visit(&rec);
        let mut v = rec.0.into_inner();
        assert_eq!(v.len(), 1);
        v.pop().unwrap()
    }

    #[test]
    fn visit_hands_over_the_wire_name_and_the_payload_as_its_own_type() {
        let (name, json) = wire(CoreEvent::ImportProgress(ImportProgress { job: 3, done: 2, total: 5 }));
        assert_eq!(name, "import:progress");
        assert_eq!(json, serde_json::json!({ "job": 3, "done": 2, "total": 5 }));

        let (name, json) = wire(CoreEvent::CatalogSwitched("/tmp/a.chairphoto".into()));
        assert_eq!(name, "catalog:switched");
        assert_eq!(json, serde_json::json!("/tmp/a.chairphoto"));

        let (name, json) = wire(CoreEvent::ScanProgress(crate::scanner::ScanProgress {
            phase: "done".into(),
            done: 0,
            total: 0,
        }));
        assert_eq!(name, "scan:progress");
        assert_eq!(json, serde_json::json!({ "phase": "done", "done": 0, "total": 0 }));
    }

    #[test]
    fn name_agrees_with_visit() {
        let e = CoreEvent::SharpnessProgress(SharpnessProgressEvent { done: 1, total: 2, job: 3 });
        assert_eq!(e.name(), wire(e.clone()).0);
    }

    /// The frontend subscribes by these names; two variants sharing one would make a
    /// listener receive a payload it was not written for.
    #[test]
    fn every_wire_name_is_unique() {
        let mut names = CoreEvent::WIRE_NAMES.to_vec();
        let n = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), n, "duplicate wire names in {:?}", CoreEvent::WIRE_NAMES);
        assert!(names.iter().all(|n| n.split_once(':').is_some_and(|(a, b)| !a.is_empty() && !b.is_empty())));
    }
}
