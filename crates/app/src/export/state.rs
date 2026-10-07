//! [`ExportState`]: the two export jobs the app runs in the background — photos to a folder
//! and an import batch as a `.chairphoto` bundle — and who owns each one's result.
//!
//! **Ownership.** A start claims the core's generation for its kind where the user pressed
//! Export (`exports::claim_export` / `claim_bundle_export`: one abort lock), so a Cancel or a
//! catalog switch before the worker runs still stops it, and a newer start of the same kind
//! trips the older. The worker resolves its ids under the identity of the catalog they were
//! read from (`CATALOG_CHANGED` otherwise) and writes off the catalog lock on the storage
//! [`Runner`]. Here, every start takes a sequence number and the catalog epoch (bumped by
//! `catalog:switched`): a result lands only while both are current, so a superseded or
//! switched-away export's report never reaches the status line or a dialog. The bench follows
//! the job id the claim numbered (`Jobs::export_photos` / `export_bundle`); progress of any
//! other job is a straggler. `export:progress` never ends a job — the worker's return does.

use crate::model::{AppModel, AppModelEvent};
use crate::shell::state::ExportTrack;
use crate::shell::ShellState;
use crate::storage::Runner;
use chairphoto_core::app::exports::{self, ExportRequest, EXPORT_CANCELLED};
use chairphoto_core::app::{AppState, CatalogIdentity, CoreEvent, ExportKind};
use chairphoto_core::bundle::writer::{BundleWriteResult, BUNDLE_EXPORT_CANCELLED};
use chairphoto_core::export::ExportResult;
use gpui_kit::{Context, Entity, EventEmitter, Subscription};
use std::path::PathBuf;

/// The export of one kind this entity follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportJob {
    seq: u64,
    epoch: u64,
    /// The core job id its progress carries.
    pub job: u64,
}

/// What [`ExportState`] tells the dialogs.
#[derive(Debug, Clone)]
pub enum ExportEvent {
    /// The followed photo export ended, with what it did (or why it stopped).
    PhotosEnded(Result<ExportResult, String>),
    /// The followed bundle export ended.
    BundleEnded(Result<BundleWriteResult, String>),
    /// `catalog:switched` arrived: whatever a dialog read names another catalog's rows.
    CatalogSwitched,
}

pub struct ExportState {
    app: AppState,
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    epoch: u64,
    seq: u64,
    pub photos: Option<ExportJob>,
    pub bundle: Option<ExportJob>,
    /// The export dialog opened last (tests drive it through this).
    pub last_dialog: Option<super::open::ExportDialog>,
    _model_events: Subscription,
}

impl EventEmitter<ExportEvent> for ExportState {}

impl ExportState {
    pub fn new(model: &Entity<AppModel>, shell: &Entity<ShellState>, cx: &mut Context<Self>) -> Self {
        let app = model.read(cx).state().clone();
        let _model_events = cx.subscribe(model, |this, _, event: &AppModelEvent, cx| {
            if let AppModelEvent::Core(CoreEvent::CatalogSwitched(_)) = event {
                this.on_catalog_switched(cx);
            }
        });
        ExportState {
            app,
            model: model.clone(),
            shell: shell.clone(),
            epoch: 0,
            seq: 0,
            photos: None,
            bundle: None,
            last_dialog: None,
            _model_events,
        }
    }

    pub fn app_state(&self) -> &AppState {
        &self.app
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The switch itself tripped both generations in the core; their results are unreachable
    /// now, and the shell's `Jobs` reset clears the bench.
    pub(crate) fn on_catalog_switched(&mut self, cx: &mut Context<Self>) {
        self.epoch += 1;
        self.photos = None;
        self.bundle = None;
        cx.emit(ExportEvent::CatalogSwitched);
        cx.notify();
    }

    fn status(&self, line: String, cx: &mut Context<Self>) {
        eprintln!("export: {line}");
        self.model.update(cx, |m, cx| m.set_status(line, cx));
    }

    fn set_bench(&self, kind: ExportKind, track: Option<ExportTrack>, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| {
            match kind {
                ExportKind::Photos => s.jobs.export_photos = track,
                ExportKind::Bundle => s.jobs.export_bundle = track,
            }
            cx.notify();
        });
    }

    fn next(&mut self, job: u64) -> ExportJob {
        self.seq += 1;
        ExportJob { seq: self.seq, epoch: self.epoch, job }
    }

    /// The Export dialog's Export: `request.photo_ids` were read from catalog `from`.
    pub fn start_photos(&mut self, request: ExportRequest, from: CatalogIdentity, cx: &mut Context<Self>) {
        let claim = match exports::claim_export(&self.app) {
            Ok(c) => c,
            Err(e) => return cx.emit(ExportEvent::PhotosEnded(Err(e))),
        };
        let job = self.next(claim.job);
        self.photos = Some(job);
        self.set_bench(ExportKind::Photos, Some(ExportTrack::new(claim.job)), cx);
        self.status(format!("Exporting {} photo(s)…", request.photo_ids.len()), cx);
        let state = self.app.clone();
        let rx = Runner::get(cx).run(move || exports::export_photos_claimed(&state, &claim, Some(from), &request));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the export worker stopped".into()));
            this.update(cx, |s, cx| s.finish_photos(job, result, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    fn finish_photos(&mut self, job: ExportJob, result: Result<ExportResult, String>, cx: &mut Context<Self>) {
        if self.photos != Some(job) || job.epoch != self.epoch {
            return; // superseded by a newer export, or by a catalog switch
        }
        self.photos = None;
        self.set_bench(ExportKind::Photos, None, cx);
        let line = match &result {
            Ok(r) => export_line(r),
            Err(e) if e.starts_with(EXPORT_CANCELLED) => e.clone(),
            Err(e) => format!("Export failed: {e}"),
        };
        self.status(line, cx);
        cx.emit(ExportEvent::PhotosEnded(result));
        cx.notify();
    }

    /// The bundle dialog's Export bundle: `batch_id` was read from catalog `from`.
    pub fn start_bundle(&mut self, batch_id: i64, dest: PathBuf, from: CatalogIdentity, cx: &mut Context<Self>) {
        let claim = match exports::claim_bundle_export(&self.app) {
            Ok(c) => c,
            Err(e) => return cx.emit(ExportEvent::BundleEnded(Err(e))),
        };
        let job = self.next(claim.job);
        self.bundle = Some(job);
        self.set_bench(ExportKind::Bundle, Some(ExportTrack::new(claim.job)), cx);
        self.status("Writing bundle…".into(), cx);
        let state = self.app.clone();
        let rx = Runner::get(cx).run(move || exports::export_bundle_claimed(&state, &claim, Some(from), batch_id, &dest));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the export worker stopped".into()));
            this.update(cx, |s, cx| s.finish_bundle(job, result, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    fn finish_bundle(&mut self, job: ExportJob, result: Result<BundleWriteResult, String>, cx: &mut Context<Self>) {
        if self.bundle != Some(job) || job.epoch != self.epoch {
            return;
        }
        self.bundle = None;
        self.set_bench(ExportKind::Bundle, None, cx);
        let line = match &result {
            Ok(r) => bundle_line(r),
            Err(e) if e.starts_with(BUNDLE_EXPORT_CANCELLED) => e.clone(),
            Err(e) => format!("Bundle export failed: {e}"),
        };
        self.status(line, cx);
        cx.emit(ExportEvent::BundleEnded(result));
        cx.notify();
    }

    /// The bench's Cancel export (the photo export first, as the bench shows it) — one abort
    /// store; the stopped job's own report still arrives.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        let result = if self.photos.is_some() {
            exports::cancel_export(&self.app)
        } else if self.bundle.is_some() {
            exports::cancel_bundle_export(&self.app)
        } else {
            return;
        };
        match result {
            Ok(()) => self.status("Cancelling export…".into(), cx),
            Err(e) => self.status(format!("Cancel failed: {e}"), cx),
        }
    }

    /// A dialog's Cancel for one kind.
    pub fn cancel_kind(&mut self, kind: ExportKind, cx: &mut Context<Self>) {
        let result = match kind {
            ExportKind::Photos if self.photos.is_some() => exports::cancel_export(&self.app),
            ExportKind::Bundle if self.bundle.is_some() => exports::cancel_bundle_export(&self.app),
            _ => return,
        };
        if let Err(e) = result {
            self.status(format!("Cancel failed: {e}"), cx);
        }
    }
}

/// The Export dialog's result (React's): exported, skipped offline, failed.
pub fn export_line(r: &ExportResult) -> String {
    let mut line = format!("Exported {}.", r.exported);
    if r.skipped_offline > 0 {
        line += &format!(
            " {} skipped — original offline/missing (connect the NAS to include them).",
            r.skipped_offline
        );
    }
    if r.errors > 0 {
        line += &format!(" {} failed.", r.errors);
    }
    line
}

/// The bundle dialog's result (React's BundleExportDialog).
pub fn bundle_line(r: &BundleWriteResult) -> String {
    let mut line = format!("Bundle written. {} original{} exported.", r.exported, if r.exported == 1 { "" } else { "s" });
    if r.skipped_offline > 0 {
        line += &format!(
            " {} skipped — original offline/missing (connect the NAS to include them).",
            r.skipped_offline
        );
    }
    if r.errors > 0 {
        line += &format!(" {} failed.", r.errors);
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_lines_name_what_was_skipped_and_what_failed() {
        let r = ExportResult { exported: 3, skipped_offline: 0, errors: 0 };
        assert_eq!(export_line(&r), "Exported 3.");
        let r = ExportResult { exported: 1, skipped_offline: 2, errors: 1 };
        assert_eq!(
            export_line(&r),
            "Exported 1. 2 skipped — original offline/missing (connect the NAS to include them). 1 failed."
        );
        let b = BundleWriteResult { exported: 1, skipped_offline: 0, errors: 0 };
        assert_eq!(bundle_line(&b), "Bundle written. 1 original exported.");
    }
}
