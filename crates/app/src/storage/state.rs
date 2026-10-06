//! [`StorageState`]: the storage jobs the app runs in the background and follows — card and
//! bundle imports, the library rescan, the back-up drain (reconcile) and the identity repair
//! pass — and who owns each one's result.
//!
//! **Ownership.** Every job start takes a sequence number, and every start also records the
//! catalog epoch (bumped by `catalog:switched`). A result lands only when both are still
//! current: a newer start of the same job or a catalog switch makes an older worker's result
//! unreachable, as AGENTS.md requires. The core does the other half — the import, reconcile
//! and identity generations are tripped by a newer start, by Cancel and by the switch itself,
//! so the old worker stops at its next file, op or copy rather than running on.
//!
//! **Cache warm-up.** A rescan's result starts `app::cache` (`cacheImages(cachePreviews)`,
//! deliberately not matching App.tsx's `onScan` here, #195) **only while** Import ▾ → "Cache
//! previews on import" is on — when it is off the warm-up is skipped entirely, not run for
//! thumbnails alone, so B&W flags and the monochrome auto-tag (which the warm-up's own
//! `invalidate` would otherwise refresh) then catch up only once a preview is next generated
//! for a photo. When it does run, it is claimed on the UI thread (`cache::claim_cache`, one
//! abort lock), so a newer rescan's warm-up or a catalog switch trips it in the core; its
//! `cache:progress` events move the status line on the bench only while their job id is the
//! one followed, and only its own result ends it ("Cache ready", or "Cache failed: …").
//!
//! **Identity repair.** The pass's events carry its job id; this entity follows exactly one
//! job. `identity:repair_done` is the required terminal signal. The job id reaches the UI
//! thread by one channel and the events by another, so a terminal event can arrive before the
//! id is adopted: such an event is buffered while a start or re-attach is in flight and
//! replayed on adoption (React's `terminalBuffer`), so the panel never waits forever for a
//! pass that already ended.

use super::runner::Runner;
use crate::model::{AppModel, AppModelEvent};
use crate::shell::ShellState;
use chairphoto_core::app::{cache, scans, storage, AppState, CatalogIdentity, CoreEvent, IdentityRepairDone};
use chairphoto_core::bundle::importer::BundleImportResult;
use chairphoto_core::catalog::IdentityRepairSummary;
use chairphoto_core::scanner::ScanResult;
use gpui_kit::{Context, Entity, EventEmitter, Subscription};
use std::collections::HashSet;
use std::path::PathBuf;

/// Which import runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportKind {
    Card,
    Bundle,
}

/// The import this entity follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportJob {
    seq: u64,
    epoch: u64,
    pub kind: ImportKind,
}

/// The cache warm-up this entity follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheJob {
    /// The core job id its `cache:progress` events carry.
    pub job: u64,
    epoch: u64,
    /// Whether it warms previews too ("Cache previews on import" when it started).
    pub previews: bool,
}

/// The identity repair pass as the debt panel shows it.
#[derive(Debug, Clone, Default)]
pub struct RepairState {
    /// A pass is starting, re-attaching or running.
    pub running: bool,
    /// The followed pass's job id, once adopted.
    pub job: Option<u64>,
    pub progress: Option<(usize, usize)>,
    /// The last pass's summary (finished or stopped).
    pub result: Option<IdentityRepairSummary>,
    pub error: Option<String>,
    /// Bumped by every start/re-attach; a superseded attempt's job id is not adopted.
    attempt: u64,
    /// Terminal events that arrived while no job was adopted yet.
    early_done: Vec<IdentityRepairDone>,
}

/// What [`StorageState`] tells the dialogs.
#[derive(Debug, Clone)]
pub enum StorageEvent {
    /// A card import ended (its status line is already set).
    ImportEnded(ImportKind),
    /// A bundle import ended, with what it did.
    BundleImported(Result<BundleImportResult, String>),
    /// The identity repair pass this entity followed ended: the queue changed.
    RepairEnded,
    /// `catalog:switched` arrived and [`StorageState::epoch`] has moved on: whatever a dialog
    /// read from the old catalog names other photos now.
    CatalogSwitched,
}

pub struct StorageState {
    app: AppState,
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    /// Bumped by every `catalog:switched`.
    epoch: u64,
    seq: u64,
    pub import: Option<ImportJob>,
    /// The running rescan's `(seq, epoch)`.
    scan: Option<(u64, u64)>,
    /// The cache warm-up followed.
    pub cache: Option<CacheJob>,
    /// Its claim's abort flag: dropping the follow on `catalog:switched` trips it, so a
    /// warm-up started by a rescan result that landed between the core's switch and the
    /// event (it reads the new catalog) never runs on unfollowed.
    cache_abort: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// The epoch of the back-up drain running now: overlapping triggers in one catalog start
    /// no second one (React's `reconciling` ref). A drain from before a catalog switch does
    /// not count — the switch tripped it in the core (`storage::ReconcileClaim`), so the new
    /// catalog's launch drain must not wait for it.
    pub reconciling: Option<u64>,
    pub repair: RepairState,
    /// The storage dialog opened last (tests drive it through this).
    pub last_dialog: Option<super::open::StorageDialog>,
    /// The epoch whose launch reconcile check ran (React's on-`ready` `checkReconcile`).
    launch_checked: Option<u64>,
    _model_events: Subscription,
}

impl EventEmitter<StorageEvent> for StorageState {}

impl StorageState {
    pub fn new(model: &Entity<AppModel>, shell: &Entity<ShellState>, cx: &mut Context<Self>) -> Self {
        let app = model.read(cx).state().clone();
        let _model_events = cx.subscribe(model, |this, _, event: &AppModelEvent, cx| match event {
            AppModelEvent::Core(event) => this.on_core_event(event, cx),
            // The catalog is open (startup, or after a switch): back up what waits, once.
            AppModelEvent::CatalogRead => {
                if this.launch_checked != Some(this.epoch) {
                    this.launch_checked = Some(this.epoch);
                    this.check_reconcile(cx);
                }
            }
            AppModelEvent::DeepLink(_) => {}
        });
        StorageState {
            app,
            model: model.clone(),
            shell: shell.clone(),
            epoch: 0,
            seq: 0,
            import: None,
            scan: None,
            cache: None,
            cache_abort: None,
            reconciling: None,
            repair: RepairState::default(),
            last_dialog: None,
            launch_checked: None,
            _model_events,
        }
    }

    pub fn app_state(&self) -> &AppState {
        &self.app
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn status(&self, line: String, cx: &mut Context<Self>) {
        eprintln!("storage: {line}");
        self.model.update(cx, |m, cx| {
            m.status = line.into();
            cx.notify();
        });
    }

    /// Everything catalog-derived may have changed: the model re-reads, and its `CatalogRead`
    /// makes the shell re-read its lists and counts.
    pub(crate) fn invalidate(&self, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.refresh(cx));
    }

    /// Put the bench on import job `job` (its `import:progress` events move it), or clear it.
    fn set_bench_import(&self, job: Option<u64>, progress: Option<(usize, usize)>, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| {
            s.jobs.import_job = job;
            s.jobs.import = progress;
            cx.notify();
        });
    }

    // --- events -----------------------------------------------------------------------

    pub(crate) fn on_core_event(&mut self, event: &CoreEvent, cx: &mut Context<Self>) {
        match event {
            CoreEvent::CatalogSwitched(_) => {
                // Every job result from before the switch is unreachable now; the switch
                // itself tripped the import and identity generations and cleared the slot.
                self.epoch += 1;
                self.import = None;
                self.scan = None;
                self.cache = None;
                if let Some(abort) = self.cache_abort.take() {
                    abort.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                self.repair = RepairState::default();
                cx.emit(StorageEvent::CatalogSwitched);
                cx.notify();
            }
            CoreEvent::IdentityRepairProgress(p) => {
                if self.repair.running && self.repair.job == Some(p.job) {
                    self.repair.progress = Some((p.done, p.total));
                    cx.notify();
                }
            }
            // React's listener: "Caching d/t…", then "Cache ready (t)" — this warm-up's only.
            CoreEvent::CacheProgress(p) if self.cache.is_some_and(|c| c.job == p.job) => {
                let line =
                    if p.done < p.total { format!("Caching {}/{}…", p.done, p.total) } else { format!("Cache ready ({})", p.total) };
                self.status(line, cx);
            }
            CoreEvent::IdentityRepairDone(d) => match self.repair.job {
                Some(job) if self.repair.running && job == d.job => self.end_repair(d.clone(), cx),
                None if self.repair.running => self.repair.early_done.push(d.clone()),
                _ => {} // another pass's straggler
            },
            _ => {}
        }
    }

    // --- imports ----------------------------------------------------------------------

    /// Import ▾ → Import from card…'s hand-off: the background import (App.tsx
    /// `startImport`). Progress shows on the bench; the result on the status line.
    pub fn start_card_import(&mut self, source: PathBuf, name: String, selected: Vec<String>, cx: &mut Context<Self>) {
        let job = ImportJob { seq: self.next_seq(), epoch: self.epoch, kind: ImportKind::Card };
        self.import = Some(job.clone());
        self.status("Importing from card…".into(), cx);
        let state = self.app.clone();
        let name = (!name.trim().is_empty()).then(|| name.trim().to_string());
        let selected: HashSet<String> = selected.into_iter().collect();
        // Claimed here, not on the worker: a Cancel or a catalog switch before the worker
        // starts must still stop it. One abort-flag lock, never the catalog's.
        let claim = match scans::claim_import(&self.app) {
            Ok(c) => c,
            Err(e) => return self.finish_import(&job, Err(e), cx),
        };
        self.set_bench_import(Some(claim.job), Some((0, 0)), cx);
        let rx = Runner::get(cx)
            .run(move || scans::ingest_from_card_claimed(&state, &claim, &source, name.as_deref(), Some(selected)));
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| s.finish_import(&job, result.map(ImportOutcome::Card), cx)).ok();
        })
        .detach();
        cx.notify();
    }

    /// The bundle dialog's Import (BundleImportDialog `run`).
    pub fn start_bundle_import(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let job = ImportJob { seq: self.next_seq(), epoch: self.epoch, kind: ImportKind::Bundle };
        self.import = Some(job.clone());
        self.status("Importing bundle…".into(), cx);
        let state = self.app.clone();
        let claim = match scans::claim_import(&self.app) {
            Ok(c) => c,
            Err(e) => return self.finish_import(&job, Err(e), cx),
        };
        self.set_bench_import(Some(claim.job), Some((0, 0)), cx);
        let rx =
            Runner::get(cx).run(move || chairphoto_core::app::bundles::import_bundle_claimed(&state, &claim, &path));
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| s.finish_import(&job, result.map(ImportOutcome::Bundle), cx)).ok();
        })
        .detach();
        cx.notify();
    }

    fn finish_import(&mut self, job: &ImportJob, result: Result<ImportOutcome, String>, cx: &mut Context<Self>) {
        if self.import.as_ref() != Some(job) || job.epoch != self.epoch {
            return; // superseded by a newer import, or by a catalog switch
        }
        self.import = None;
        self.set_bench_import(None, None, cx);
        match (job.kind, result) {
            (_, Ok(ImportOutcome::Card(r))) => {
                self.status(card_import_line(&r), cx);
                cx.emit(StorageEvent::ImportEnded(ImportKind::Card));
            }
            (_, Ok(ImportOutcome::Bundle(r))) => {
                self.status(bundle_import_line(&r), cx);
                cx.emit(StorageEvent::BundleImported(Ok(r)));
            }
            (ImportKind::Card, Err(e)) => {
                // A cancelled import's report is its own line, not a failure.
                let line = if e.starts_with(scans::IMPORT_CANCELLED) { e } else { format!("Import failed: {e}") };
                self.status(line, cx);
                cx.emit(StorageEvent::ImportEnded(ImportKind::Card));
            }
            (ImportKind::Bundle, Err(e)) => {
                self.status(format!("Bundle import failed: {e}"), cx);
                cx.emit(StorageEvent::BundleImported(Err(e)));
            }
        }
        // Even a cancelled or failed import may have changed the catalog.
        self.invalidate(cx);
        cx.notify();
    }

    /// Stop the running import before its next file. Its result still arrives, as the
    /// cancelled import's own report.
    pub fn cancel_import(&mut self, cx: &mut Context<Self>) {
        if self.import.is_none() {
            return;
        }
        // One abort-flag store; it never waits on the catalog lock.
        if let Err(e) = scans::cancel_import(&self.app) {
            self.status(format!("Cancel failed: {e}"), cx);
        } else {
            self.status("Cancelling import…".into(), cx);
        }
    }

    // --- rescan -----------------------------------------------------------------------

    /// Import ▾ → Rescan library (App.tsx `onScan`). Phase A's result lands here; Phase B
    /// runs on, on the same worker, and reports through `scan:progress` on the bench.
    pub fn rescan(&mut self, cx: &mut Context<Self>) {
        let token = (self.next_seq(), self.epoch);
        self.scan = Some(token);
        self.status("Scanning library…".into(), cx);
        let state = self.app.clone();
        let (tx, rx) = futures::channel::oneshot::channel();
        Runner::get(cx).spawn(move || match scans::rescan_library(&state) {
            Ok((result, enrich)) => {
                let _ = tx.send(Ok(result));
                enrich.run();
            }
            Err(e) => {
                let _ = tx.send(Err(e));
            }
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| s.finish_rescan(token, result, cx)).ok();
        })
        .detach();
    }

    fn finish_rescan(&mut self, token: (u64, u64), result: Result<ScanResult, String>, cx: &mut Context<Self>) {
        if self.scan != Some(token) || token.1 != self.epoch {
            return;
        }
        self.scan = None;
        let scanned = result.is_ok();
        match result {
            Ok(r) => self.status(rescan_line(&r), cx),
            Err(e) => self.status(format!("Scan failed: {e}"), cx),
        }
        self.invalidate(cx);
        // Owner decision (#195): with "Cache previews on import" off, skip the warm-up
        // entirely rather than running it for thumbnails alone. Consequence: B&W flags and
        // the monochrome auto-tag (`finish_cache`'s invalidate) then refresh only when a
        // preview is next generated for a photo, not right after a rescan.
        if scanned && self.shell.read(cx).cache_previews {
            self.start_cache(cx);
        }
    }

    /// Pre-cache so browsing is instant (App.tsx `onScan`): thumbnails always, previews when
    /// "Cache previews on import" is on. A newer start trips the one before it.
    pub fn start_cache(&mut self, cx: &mut Context<Self>) {
        let include_previews = self.shell.read(cx).cache_previews;
        let claim = match cache::claim_cache(&self.app) {
            Ok(c) => c,
            Err(e) => return self.status(format!("Cache failed: {e}"), cx),
        };
        let token = CacheJob { job: claim.job, epoch: self.epoch, previews: include_previews };
        self.cache = Some(token);
        self.cache_abort = Some(claim.abort.clone());
        let state = self.app.clone();
        let rx = Runner::get(cx).run(move || cache::cache_images_claimed(&state, &claim, include_previews));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the cache worker stopped".into()));
            this.update(cx, |s, cx| s.finish_cache(token, result, cx)).ok();
        })
        .detach();
    }

    /// The warm-up's own result: the terminal signal, whatever its progress said.
    fn finish_cache(&mut self, token: CacheJob, result: Result<cache::CacheResult, String>, cx: &mut Context<Self>) {
        if self.cache != Some(token) || token.epoch != self.epoch {
            return; // superseded by a newer warm-up, or by a catalog switch
        }
        self.cache = None;
        self.cache_abort = None;
        match result {
            Ok(_) => {
                self.status("Cache ready".into(), cx);
                // The B&W flags and the monochrome auto-tag changed.
                self.invalidate(cx);
            }
            Err(e) => self.status(format!("Cache failed: {e}"), cx),
        }
    }

    pub fn scanning(&self) -> bool {
        self.scan.is_some()
    }

    // --- reconcile --------------------------------------------------------------------

    /// On launch and on window focus (App.tsx `checkReconcile`): when ops are pending and a
    /// backup volume is reachable, drain them in the background.
    pub fn check_reconcile(&mut self, cx: &mut Context<Self>) {
        let state = self.app.clone();
        let epoch = self.epoch;
        let rx = Runner::get(cx).run(move || storage::reconcile_due(&state));
        cx.spawn(async move |this, cx| {
            let Ok(Ok((_, due))) = rx.await else { return };
            this.update(cx, |s, cx| {
                if due && epoch == s.epoch {
                    s.run_reconcile(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// The "⤓ N waiting for the NAS" chip and More ⋯ → Back-up queue (App.tsx
    /// `runReconcile`): back up, then apply the offload policy.
    pub fn run_reconcile(&mut self, cx: &mut Context<Self>) {
        if self.reconciling == Some(self.epoch) {
            return;
        }
        let epoch = self.epoch;
        self.reconciling = Some(epoch);
        let pending = self.shell.read(cx).counts.pending;
        if pending > 0 {
            self.status(format!("Backing up {pending} to NAS…"), cx);
        }
        let state = self.app.clone();
        let rx = Runner::get(cx).run(move || {
            // One claim for both steps: a switch between the drain and the policy stops the
            // policy too, and neither ever touches a catalog other than the one claimed.
            let claim = storage::claim_reconcile(&state)?;
            let summary = claim.drain(&state)?;
            // Best-effort: never blocks the result.
            let offloaded = if summary.skipped_offline { 0 } else { claim.apply_offload_policy().unwrap_or(0) };
            Ok::<_, String>((summary, offloaded))
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await;
            this.update(cx, |s, cx| {
                // Only this drain's own mark: a newer catalog's drain may own it by now.
                if s.reconciling == Some(epoch) {
                    s.reconciling = None;
                }
                if epoch != s.epoch {
                    return;
                }
                match result {
                    Ok(Ok((summary, offloaded))) => {
                        if !summary.skipped_offline && summary.ran + summary.failed + summary.partial + summary.busy > 0 {
                            s.status(drain_status(&summary), cx);
                        }
                        if offloaded > 0 {
                            s.status(format!("Offloaded {offloaded} older photo(s) to the NAS"), cx);
                        }
                    }
                    Ok(Err(e)) => s.status(format!("Backup failed: {e}"), cx),
                    Err(_) => s.status("Backup failed: the worker stopped".into(), cx),
                }
                s.invalidate(cx);
                s.shell.update(cx, |sh, cx| sh.refresh_on_focus(cx));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The bench's Back up: queue a backup for each target and drain now (React's
    /// `enqueueOperations` + reconcile). Queued, not copied here: an offline NAS keeps them
    /// for the next drain. `from` is the catalog the ids were read from (the Library's
    /// `rows_from`): the queueing fails closed with `CATALOG_CHANGED` once another catalog is
    /// open, so the old ids never queue the new catalog's same-numbered photos.
    pub fn back_up(&mut self, photo_ids: Vec<i64>, from: Option<CatalogIdentity>, cx: &mut Context<Self>) {
        if photo_ids.is_empty() {
            self.status("Select photos to back up.".into(), cx);
            return;
        }
        let Some(from) = from else {
            self.status("The photos are still loading; try again.".into(), cx);
            return;
        };
        let state = self.app.clone();
        let epoch = self.epoch;
        let rx = Runner::get(cx).run(move || {
            chairphoto_core::app::with_catalog_as(&state, from, |c| c.enqueue_operations("backup", &photo_ids))
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                if epoch != s.epoch {
                    return;
                }
                match result {
                    Ok(n) => {
                        s.status(format!("Queued {n} for backup"), cx);
                        s.shell.update(cx, |sh, cx| sh.refresh_on_focus(cx));
                        s.run_reconcile(cx);
                    }
                    Err(e) => s.status(format!("Back up failed: {e}"), cx),
                }
            })
            .ok();
        })
        .detach();
    }

    // --- identity repair ----------------------------------------------------------------

    /// "Start repair pass". A second start while one is followed does nothing (the button
    /// is disabled then); the core would supersede the first anyway.
    pub fn start_repair(&mut self, cx: &mut Context<Self>) {
        if self.repair.running {
            return;
        }
        let attempt = self.begin_attempt(cx);
        let state = self.app.clone();
        let (tx, rx) = futures::channel::oneshot::channel();
        Runner::get(cx).spawn(move || match chairphoto_core::app::identity::claim_identity_repair(&state) {
            Ok(pass) => {
                // The id goes out before the pass sends anything.
                let _ = tx.send(Ok(Some((pass.job, 0, 0))));
                pass.run();
            }
            Err(e) => {
                let _ = tx.send(Err(e));
            }
        });
        self.adopt_when_ready(attempt, rx, cx);
    }

    /// The debt panel opened: follow a pass that is already running (re-attach), so a
    /// reopened panel never invites a second pass over the same queue. Idle costs nothing.
    pub fn reattach_repair(&mut self, cx: &mut Context<Self>) {
        if self.repair.running {
            return;
        }
        let state = self.app.clone();
        let epoch = self.epoch;
        let rx = Runner::get(cx).run(move || chairphoto_core::app::identity::identity_repair_status(&state));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(_))) = rx.await else { return };
            this.update(cx, |s, cx| {
                if epoch != s.epoch || s.repair.running {
                    return;
                }
                // Something runs: follow it, and ask again now that terminal events buffer —
                // a pass that ended in between has already cleared its slot.
                let attempt = s.begin_attempt(cx);
                let state = s.app.clone();
                let rx = Runner::get(cx).run(move || {
                    chairphoto_core::app::identity::identity_repair_status(&state)
                        .map(|st| st.map(|st| (st.job, st.done, st.total)))
                });
                s.adopt_when_ready(attempt, rx, cx);
            })
            .ok();
        })
        .detach();
    }

    fn begin_attempt(&mut self, cx: &mut Context<Self>) -> (u64, u64) {
        self.repair.attempt += 1;
        self.repair = RepairState { running: true, attempt: self.repair.attempt, ..RepairState::default() };
        cx.notify();
        (self.repair.attempt, self.epoch)
    }

    fn adopt_when_ready(
        &mut self,
        attempt: (u64, u64),
        rx: futures::channel::oneshot::Receiver<Result<Option<(u64, usize, usize)>, String>>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the repair worker stopped".into()));
            this.update(cx, |s, cx| s.adopt(attempt, result, cx)).ok();
        })
        .detach();
    }

    /// The job id of a start or re-attach arrived.
    pub(crate) fn adopt(&mut self, attempt: (u64, u64), result: Result<Option<(u64, usize, usize)>, String>, cx: &mut Context<Self>) {
        if attempt != (self.repair.attempt, self.epoch) || !self.repair.running || self.repair.job.is_some() {
            return; // superseded (a catalog switch reset the attempt)
        }
        match result {
            Ok(Some((job, done, total))) => {
                self.repair.job = Some(job);
                if total > 0 {
                    self.repair.progress = Some((done, total));
                }
                // A terminal event that beat the id is replayed now.
                let early = std::mem::take(&mut self.repair.early_done);
                if let Some(done) = early.into_iter().find(|d| d.job == job) {
                    self.end_repair(done, cx);
                    return;
                }
            }
            Ok(None) => {
                // A re-attach that found nothing running any more.
                self.repair = RepairState { attempt: self.repair.attempt, ..RepairState::default() };
                cx.emit(StorageEvent::RepairEnded);
            }
            Err(e) => {
                self.repair = RepairState { attempt: self.repair.attempt, error: Some(e), ..RepairState::default() };
            }
        }
        cx.notify();
    }

    fn end_repair(&mut self, done: IdentityRepairDone, cx: &mut Context<Self>) {
        let attempt = self.repair.attempt;
        self.repair = RepairState {
            attempt,
            result: done.error.is_none().then_some(done.summary),
            error: done.error,
            ..RepairState::default()
        };
        cx.emit(StorageEvent::RepairEnded);
        // The debt count in the title bar.
        self.invalidate(cx);
        cx.notify();
    }

    /// Cancel: the pass stops at its next copy and still sends its terminal event, whose
    /// partial, `aborted` summary is the honest report — so nothing changes here yet.
    pub fn cancel_repair(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = chairphoto_core::app::identity::cancel_identity_repair(&self.app) {
            self.repair.error = Some(e);
            cx.notify();
        }
    }
}

enum ImportOutcome {
    Card(ScanResult),
    Bundle(BundleImportResult),
}

/// React's status line after a card import.
pub fn card_import_line(r: &ScanResult) -> String {
    let mut line = format!("Imported {} new of {} on card", r.created, r.scanned);
    if r.skipped > 0 {
        line += &format!(", {} already imported", r.skipped);
    }
    if r.restored > 0 {
        line += &format!(", {}", restored_clause(r.restored, r.restored_trashed));
    }
    if r.offloaded > 0 {
        line += &format!(", {} already offloaded, not copied back", r.offloaded);
    }
    if r.name_too_long > 0 {
        line += &format!(", {} not imported (name too long for a sidecar)", r.name_too_long);
    }
    if r.errors > 0 {
        line += &format!(", {} errors", r.errors);
    }
    line
}

/// The photos an import put back onto the rows that still held their names, their files
/// gone (#247), and how many of those are in the trash — where a restored photo stays
/// hidden, so the line says so rather than leave it invisible.
fn restored_clause(restored: usize, trashed: usize) -> String {
    if restored == 1 {
        let trash = if trashed > 0 { ", in the trash" } else { "" };
        format!("1 restored to its old row{trash}")
    } else {
        let trash = if trashed > 0 { format!(", {trashed} of them in the trash") } else { String::new() };
        format!("{restored} restored to their old rows{trash}")
    }
}

/// React's status line after a rescan.
pub fn rescan_line(r: &ScanResult) -> String {
    let mut line = format!("Scanned {}, imported {} ({} new)", r.scanned, r.imported, r.created);
    if r.errors > 0 {
        line += &format!(", {} errors", r.errors);
    }
    line
}

/// The status line after a reconcile drain. Neutral about the verb, because the queue holds
/// offloads and restores as well as backups. A part-done stack op says what became of the
/// frames it left: queued to retry (pending — the drain was superseded before them) or
/// failed (they refused on their own account, and wait in the queue with the reason).
pub fn drain_status(s: &chairphoto_core::catalog::DrainSummary) -> String {
    let frames = |n: usize| if n == 1 { "1 frame".to_string() } else { format!("{n} frames") };
    let mut line = format!("Storage queue: {} done", s.ran);
    if s.failed > 0 {
        line += &format!(", {} failed", s.failed);
    }
    if s.partial > 0 {
        line += &format!(", {} part-done", s.partial);
        if s.frames_requeued > 0 {
            line += &format!(" ({} queued to retry)", frames(s.frames_requeued));
        }
        if s.frames_failed > 0 {
            line += &format!(" ({} failed)", frames(s.frames_failed));
        }
    }
    if s.busy > 0 {
        // Their photos were in use by another storage operation; still queued (#254).
        line += &format!(", {} waiting (photo in use)", s.busy);
    }
    line
}

/// The photos a bundle import kept apart, by file name (#249): `": A.ARW, B.ARW"`, the first
/// five and how many more; empty when none were named.
fn kept_apart_names(paths: &[String]) -> String {
    const SHOWN: usize = 5;
    if paths.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = paths.iter().take(SHOWN).map(|p| p.rsplit('/').next().unwrap_or(p)).collect();
    let more = paths.len().saturating_sub(SHOWN);
    let tail = if more > 0 { format!(" and {more} more") } else { String::new() };
    format!(": {}{tail}", names.join(", "))
}

/// BundleImportDialog's result line.
pub fn bundle_import_line(r: &BundleImportResult) -> String {
    let plural = |n: usize, one: &str, many: &str| if n == 1 { one.to_string() } else { many.to_string() };
    let added = r.merge.photos_added;
    let mut line = if added > 0 {
        format!("Import complete. {added} {} added.", plural(added, "photo", "photos"))
    } else {
        "Import complete. No new photos.".to_string()
    };
    if r.copied > 0 {
        line += &format!(" {} {} copied.", r.copied, plural(r.copied, "original", "originals"));
    }
    if r.skipped_duplicate > 0 {
        line += &format!(" {} already present (skipped).", r.skipped_duplicate);
    }
    if r.restored > 0 {
        line += &format!(" {}.", restored_clause(r.restored, r.restored_trashed));
    }
    if r.offloaded > 0 {
        line += &format!(" {} already offloaded, not copied back.", r.offloaded);
    }
    if r.name_too_long > 0 {
        line += &format!(" {} not unpacked (name too long for a sidecar).", r.name_too_long);
    }
    if r.merge.photos_filled > 0 {
        line += &format!(
            " {} already in the catalog filled in ({} new {}).",
            r.merge.photos_filled,
            r.merge.versions_added,
            plural(r.merge.versions_added, "version", "versions")
        );
    }
    if r.merge.photos_merged_before > 0 {
        line += &format!(
            " {} merged from this bundle before (left as they are).",
            r.merge.photos_merged_before
        );
    }
    if r.merge.photos_matched_by_capture > 0 {
        line += &format!(
            " {} matched to the same capture here under another identity (filled in, identity kept).",
            r.merge.photos_matched_by_capture
        );
    }
    if r.merge.photos_kept_apart > 0 {
        line += &format!(
            " {} kept apart (the same file is here under another identity){}.",
            r.merge.photos_kept_apart,
            kept_apart_names(&r.merge.kept_apart_names)
        );
    }
    if r.merge.tags_created > 0 {
        line += &format!(" {} {} created.", r.merge.tags_created, plural(r.merge.tags_created, "tag", "tags"));
    }
    if r.errors > 0 {
        line += &format!(" {} error(s).", r.errors);
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lines_match_reacts() {
        let r = ScanResult { scanned: 5, imported: 4, created: 3, errors: 1, skipped: 2, ..Default::default() };
        assert_eq!(card_import_line(&r), "Imported 3 new of 5 on card, 2 already imported, 1 errors");
        assert_eq!(rescan_line(&r), "Scanned 5, imported 4 (3 new), 1 errors");
        let clean = ScanResult { scanned: 2, imported: 2, created: 2, errors: 0, skipped: 0, ..Default::default() };
        assert_eq!(card_import_line(&clean), "Imported 2 new of 2 on card");
    }

    /// #247: photos put back onto the rows that held their names are counted apart from the
    /// new ones, and those in the trash are named, so a restored photo hidden there is not
    /// invisible.
    #[test]
    fn the_import_lines_count_restored_photos_and_those_in_the_trash() {
        let card = |restored, restored_trashed| ScanResult {
            scanned: 4,
            imported: 4,
            created: 4 - restored,
            restored,
            restored_trashed,
            ..Default::default()
        };
        assert_eq!(
            card_import_line(&card(3, 1)),
            "Imported 1 new of 4 on card, 3 restored to their old rows, 1 of them in the trash"
        );
        assert_eq!(card_import_line(&card(2, 0)), "Imported 2 new of 4 on card, 2 restored to their old rows");
        assert_eq!(card_import_line(&card(1, 1)), "Imported 3 new of 4 on card, 1 restored to its old row, in the trash");
        // #231 F5 and LOW-5: offloaded photos and refused names have their own counts.
        let other = ScanResult { scanned: 3, offloaded: 2, name_too_long: 1, ..Default::default() };
        assert_eq!(
            card_import_line(&other),
            "Imported 0 new of 3 on card, 2 already offloaded, not copied back, 1 not imported (name too long for a sidecar)"
        );
        let bundle = BundleImportResult {
            copied: 2,
            skipped_duplicate: 0,
            errors: 0,
            restored: 2,
            restored_trashed: 2,
            offloaded: 0,
            name_too_long: 0,
            refused: Vec::new(),
            merge: Default::default(),
        };
        assert_eq!(
            bundle_import_line(&bundle),
            "Import complete. No new photos. 2 originals copied. 2 restored to their old rows, 2 of them in the trash."
        );
    }

    /// The drain line names no verb the queue may not have run, and says "queued" only for
    /// frames that really are pending again.
    #[test]
    fn the_drain_line_says_what_is_queued_and_what_failed() {
        use chairphoto_core::catalog::DrainSummary;
        assert_eq!(drain_status(&DrainSummary { ran: 1, ..Default::default() }), "Storage queue: 1 done");
        let s = DrainSummary { ran: 2, failed: 1, partial: 2, frames_requeued: 3, frames_failed: 1, ..Default::default() };
        assert_eq!(
            drain_status(&s),
            "Storage queue: 2 done, 1 failed, 2 part-done (3 frames queued to retry) (1 frame failed)"
        );
        let failed_only = DrainSummary { partial: 1, frames_failed: 2, ..Default::default() };
        assert_eq!(drain_status(&failed_only), "Storage queue: 0 done, 1 part-done (2 frames failed)");
        // #254: an op whose photo another storage operation held is still queued.
        let busy = DrainSummary { ran: 1, busy: 2, ..Default::default() };
        assert_eq!(drain_status(&busy), "Storage queue: 1 done, 2 waiting (photo in use)");
    }

    /// #249: the bundle line names the photos kept apart (the first five, then how many more)
    /// and counts those matched to the same capture under another identity.
    #[test]
    fn the_bundle_line_names_the_photos_kept_apart() {
        let names: Vec<String> = (1..=7).map(|i| format!("2026/06/28/DSC{i}.ARW")).collect();
        let r = BundleImportResult {
            copied: 0,
            skipped_duplicate: 9,
            errors: 0,
            restored: 0,
            restored_trashed: 0,
            offloaded: 0,
            name_too_long: 0,
            refused: Vec::new(),
            merge: chairphoto_core::catalog::MergeSummary {
                photos_existing: 2,
                photos_matched_by_capture: 2,
                photos_kept_apart: 7,
                kept_apart_names: names,
                ..Default::default()
            },
        };
        assert_eq!(
            bundle_import_line(&r),
            "Import complete. No new photos. 9 already present (skipped). 2 matched to the same capture here \
             under another identity (filled in, identity kept). 7 kept apart (the same file is here under \
             another identity): DSC1.ARW, DSC2.ARW, DSC3.ARW, DSC4.ARW, DSC5.ARW and 2 more."
        );
    }

    /// #185: the bundle line says what an import filled in on photos already in the catalog,
    /// and what it kept apart.
    #[test]
    fn the_bundle_line_reports_filled_and_kept_apart_photos() {
        let r = BundleImportResult {
            copied: 0,
            skipped_duplicate: 2,
            errors: 0,
            restored: 0,
            restored_trashed: 0,
            offloaded: 0,
            name_too_long: 0,
            refused: Vec::new(),
            merge: chairphoto_core::catalog::MergeSummary {
                photos_existing: 2,
                photos_filled: 1,
                versions_added: 2,
                photos_kept_apart: 1,
                photos_merged_before: 3,
                ..Default::default()
            },
        };
        assert_eq!(
            bundle_import_line(&r),
            "Import complete. No new photos. 2 already present (skipped). 1 already in the catalog filled in \
             (2 new versions). 3 merged from this bundle before (left as they are). 1 kept apart (the same \
             file is here under another identity)."
        );
    }
}
