//! Headless tests of the Smart Tagging module (#126) through the real wiring: the model gate
//! and its download, the index job's ownership (start, progress, done, an end before the
//! start's answer, Cancel, a catalog switch, re-attaching), kNN suggestions with accept and
//! reject, the settings (model path, training, delete index) and catalog identity.
//!
//! No ONNX, no model and no network: [`FakeSmarttags`] stands in for the model check, the
//! download and the worker. Its start runs the **real** core claim
//! (`app::smarttags::begin_index_job`), so ownership is the core's; the tests play the
//! worker's part (clear the slot, then send the events). Suggestions use the real kNN engine
//! over synthetic embeddings; no test needs `SMARTTAGS_TEST_MODEL`.

use super::*;
use crate::modules::smart_tagging::state::{IndexPhase, SmarttagsBackend, SmarttagsBackendGlobal, SmarttagsState};
use crate::modules::smart_tagging::views::{SimilarTagsPanel, SmarttagsSettings};
use crate::modules::smart_tagging::SMARTTAGS_MODULE_ID;
use crate::modules::{ModuleRegistry, PanelSlot};
use crate::shell::state::InspectorTab;
use crate::storage::Runner;
use chairphoto_core::app::smarttags as core_st;
use chairphoto_core::app::{
    CatalogIdentity, JobClaim, SmarttagsDownloadProgressEvent, SmarttagsIndexDone, SmarttagsJobStatus,
    SmarttagsProgressEvent, CATALOG_CHANGED,
};
use chairphoto_core::plugins::smarttags::{store, ModelReport, ModelStatus};
use gpui_kit::SharedString;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

/// The model check, download and job start, recorded.
#[derive(Default)]
struct FakeSmarttags {
    ready: Mutex<bool>,
    custom: Mutex<bool>,
    downloads: Mutex<usize>,
    claims: Mutex<Vec<JobClaim<SmarttagsJobStatus>>>,
    starts: Mutex<Vec<Result<u64, String>>>,
}

impl SmarttagsBackend for FakeSmarttags {
    fn model_status(&self, _: &AppState) -> Result<ModelStatus, String> {
        let ready = *self.ready.lock().unwrap();
        let custom = *self.custom.lock().unwrap();
        Ok(ModelStatus {
            ready,
            model: ModelReport {
                key: "clip".into(),
                present: ready,
                path: "/models/clip.onnx".into(),
                custom,
                detail: (!ready).then(|| if custom { "configured model path is missing".into() } else { "not downloaded".into() }),
            },
        })
    }

    fn download_model(&self, app: &AppState) -> Result<ModelStatus, String> {
        *self.downloads.lock().unwrap() += 1;
        *self.ready.lock().unwrap() = true;
        self.model_status(app)
    }

    fn start_index(&self, app: &AppState, from: CatalogIdentity) -> Result<u64, String> {
        let result = core_st::begin_index_job(app, Some(from)).map(|claim| {
            let job = claim.job;
            self.claims.lock().unwrap().push(claim);
            job
        });
        self.starts.lock().unwrap().push(result.clone());
        result
    }
}

impl FakeSmarttags {
    /// The worker's end: release the slot (only while it owns it), then the terminal event.
    fn finish(&self, app: &App, job: u64, done: usize, total: usize, cx: &mut TestAppContext) {
        if let Some(claim) = self.claims.lock().unwrap().iter().find(|c| c.job == job) {
            claim.slot.clear();
        }
        send_done(app, job, done, total, cx);
    }
}

fn send_done(app: &App, job: u64, done: usize, total: usize, cx: &mut TestAppContext) {
    app.state.send(CoreEvent::SmarttagsIndexDone(SmarttagsIndexDone {
        ok: true,
        done,
        total,
        offline: 0,
        failed: 0,
        aborted: false,
        job,
        error: None,
    }));
    cx.run_until_parked();
}

fn send_progress(app: &App, job: u64, done: usize, total: usize, cx: &mut TestAppContext) {
    app.state.send(CoreEvent::SmarttagsProgress(SmarttagsProgressEvent { done, total, job }));
    cx.run_until_parked();
}

fn work(app: &App, cx: &mut TestAppContext) {
    for _ in 0..30 {
        let ran = cx.update(|cx| Runner::get(cx).run_pending());
        cx.run_until_parked();
        cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
        cx.run_until_parked();
        if ran == 0 && cx.update(|cx| Runner::get(cx).pending()) == 0 {
            return;
        }
    }
}

fn with_cat<R>(app: &App, f: impl FnOnce(&Catalog) -> R) -> R {
    f(app.state.catalog.lock().unwrap().as_ref().unwrap())
}

struct St {
    app: App,
    fake: Arc<FakeSmarttags>,
    ids: Vec<i64>,
    dir: TempDir,
}

fn open_st(n: usize, ready: bool, tag: &str, cx: &mut TestAppContext) -> St {
    let fake = Arc::new(FakeSmarttags::default());
    *fake.ready.lock().unwrap() = ready;
    cx.update(|cx| cx.set_global(SmarttagsBackendGlobal(fake.clone())));
    let dir = TempDir::new(tag);
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, n, cx);
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, SMARTTAGS_MODULE_ID, cx));
    app.wired.shell.update(cx, |s, cx| s.set_inspector_tab(InspectorTab::Tags, cx));
    work(&app, cx);
    St { app, fake, ids, dir }
}

impl St {
    fn window(&self) -> AnyWindowHandle {
        self.app.window()
    }

    fn settings(&self, cx: &mut TestAppContext) -> Entity<SmarttagsSettings> {
        let modules = self.app.wired.modules.clone();
        cx.update_window(self.window(), |_, window, cx| {
            ModuleRegistry::settings_views(&modules, &SMARTTAGS_MODULE_ID.into(), window, cx)
                .into_iter()
                .next()
                .expect("the settings panel")
                .downcast::<SmarttagsSettings>()
                .expect("a SmarttagsSettings")
        })
        .unwrap()
    }

    fn state(&self, cx: &mut TestAppContext) -> Entity<SmarttagsState> {
        let modules = self.app.wired.modules.clone();
        let panel = cx
            .update_window(self.window(), |_, window, cx| {
                ModuleRegistry::panel_views(&modules, PanelSlot::Inspector, window, cx)
                    .into_iter()
                    .find(|p| p.id.as_ref() == "smarttags-similar")
                    .expect("the panel")
                    .view
                    .downcast::<SimilarTagsPanel>()
                    .ok()
                    .expect("a SimilarTagsPanel")
            })
            .unwrap();
        panel.read_with(cx, |p, _| p.state.clone())
    }

    fn phase(&self, cx: &mut TestAppContext) -> IndexPhase {
        let state = self.state(cx);
        state.read_with(cx, |s, _| s.index.phase)
    }

    fn select(&self, id: i64, cx: &mut TestAppContext) {
        self.app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_single(id)));
        work(&self.app, cx);
    }

    fn click(&self, id: &str, cx: &mut TestAppContext) {
        let id = SharedString::from(id.to_string());
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .unwrap();
        work(&self.app, cx);
    }

    fn label(&self, id: &str, cx: &mut TestAppContext) -> Option<String> {
        let id = SharedString::from(id.to_string());
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).and_then(|e| e.label().map(str::to_string))
        })
        .unwrap()
    }

    fn present(&self, id: &str, cx: &mut TestAppContext) -> bool {
        let id = SharedString::from(id.to_string());
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .unwrap()
    }

    fn photo_tags(&self, photo: i64) -> Vec<String> {
        with_cat(&self.app, |c| {
            let mut stmt = c
                .conn()
                .prepare("SELECT t.full_path FROM photo_tags pt JOIN tags t ON t.id = pt.tag_id WHERE pt.photo_id = ?1 ORDER BY 1")
                .unwrap();
            stmt.query_map([photo], |r| r.get(0)).unwrap().collect::<Result<Vec<String>, _>>().unwrap()
        })
    }
}

// --- the model ------------------------------------------------------------------------------

/// Missing default model: the panel offers the download and Index is refused; nothing is
/// downloaded unasked; the click downloads (its progress moves the label) and enables Index.
/// A broken custom path shows its error instead.
#[gpui_kit::test]
fn the_model_gates_the_module_and_downloads_only_on_click(cx: &mut TestAppContext) {
    let s = open_st(1, false, "st-model", cx);
    s.select(s.ids[0], cx);
    assert!(s.present("smarttags-download", cx));
    assert!(!s.present("smarttags-index", cx), "no Index without the model");
    let state = s.state(cx);
    state.update(cx, |st, cx| st.index_photos(cx));
    work(&s.app, cx);
    assert!(s.fake.starts.lock().unwrap().is_empty(), "an index started without the model");
    assert_eq!(*s.fake.downloads.lock().unwrap(), 0, "nothing is downloaded unasked");

    // Progress (cosmetic) moves the label only while our download runs.
    s.app.state.send(CoreEvent::SmarttagsDownloadProgress(SmarttagsDownloadProgressEvent { done: 1, total: Some(4) }));
    cx.run_until_parked();
    state.read_with(cx, |st, _| assert_eq!(st.download_progress, None, "not ours: no download is running"));
    state.update(cx, |st, cx| st.download(cx));
    s.app.state.send(CoreEvent::SmarttagsDownloadProgress(SmarttagsDownloadProgressEvent { done: 50 << 20, total: Some(200 << 20) }));
    cx.run_until_parked();
    state.read_with(cx, |st, _| assert_eq!(st.download_progress, Some((50 << 20, Some(200 << 20)))));
    work(&s.app, cx);
    assert_eq!(*s.fake.downloads.lock().unwrap(), 1);
    state.read_with(cx, |st, _| assert!(st.model_ready() && st.can_index()));
    assert_eq!(status(&s.app, cx), "Smart Tagging model downloaded.");
    assert!(s.present("smarttags-index", cx));

    *s.fake.ready.lock().unwrap() = false;
    *s.fake.custom.lock().unwrap() = true;
    state.update(cx, |st, cx| st.check_model(cx));
    work(&s.app, cx);
    assert!(s.present("smarttags-model-error", cx), "a broken custom path names the problem");
    assert!(!s.present("smarttags-download", cx), "a custom path is never fetched");
}

// --- the index job --------------------------------------------------------------------------

/// A run is followed by its own id: a superseded run's progress changes nothing; its own
/// progress shows; its `smarttags:index_done` — after the slot was released — ends it.
///
/// Mutation-checked: dropping the job-id filter on `smarttags:progress` fails it.
#[gpui_kit::test]
fn the_index_follows_only_its_own_events(cx: &mut TestAppContext) {
    let s = open_st(1, true, "st-job", cx);
    s.select(s.ids[0], cx);
    s.click("smarttags-index", cx);
    let job = s.fake.starts.lock().unwrap()[0].clone().unwrap();
    assert_eq!(s.phase(cx), IndexPhase::Running { job, done: 0, total: 0, progress: false });
    assert_eq!(s.label("smarttags-index", cx).as_deref(), Some("Indexing…"));

    send_progress(&s.app, job + 100, 5, 9, cx);
    assert_eq!(s.phase(cx), IndexPhase::Running { job, done: 0, total: 0, progress: false }, "another run's straggler");
    send_progress(&s.app, job, 1, 4, cx);
    assert_eq!(s.label("smarttags-index", cx).as_deref(), Some("Indexing… 25%"));
    send_done(&s.app, job + 100, 9, 9, cx);
    assert!(s.phase(cx) != IndexPhase::Idle, "another run's end does not end ours");

    s.fake.finish(&s.app, job, 4, 4, cx);
    work(&s.app, cx);
    assert_eq!(s.phase(cx), IndexPhase::Idle);
    assert_eq!(s.label("smarttags-result", cx).as_deref(), Some("Indexing complete: 4 photos processed."));
    assert!(core_st::index_status(&s.app.state).unwrap().is_none());
}

/// A tiny library's end can arrive before the start's answer: held and replayed, so the
/// panel does not sit in "Indexing…" forever.
#[gpui_kit::test]
fn an_end_that_beats_the_start_answer_still_ends_the_run(cx: &mut TestAppContext) {
    let s = open_st(1, true, "st-early", cx);
    let state = s.state(cx);
    let next = s.app.state.jobs.smarttags.abort().job_ids_issued() + 1;
    state.update(cx, |st, cx| st.index_photos(cx));
    send_progress(&s.app, next, 1, 1, cx);
    send_done(&s.app, next, 1, 1, cx);
    assert_eq!(s.phase(cx), IndexPhase::Starting);
    work(&s.app, cx);
    assert_eq!(s.fake.starts.lock().unwrap()[0], Ok(next));
    state.read_with(cx, |st, _| {
        assert_eq!(st.index.phase, IndexPhase::Idle);
        assert_eq!(st.index.last_result.as_deref(), Some("Indexing complete: 1 photo processed."));
    });
}

/// Cancel names its job: once a newer start superseded ours, our Cancel stops nothing;
/// otherwise it trips our run.
#[gpui_kit::test]
fn cancel_stops_only_the_followed_run(cx: &mut TestAppContext) {
    let s = open_st(1, true, "st-cancel", cx);
    let state = s.state(cx);
    state.update(cx, |st, cx| st.index_photos(cx));
    work(&s.app, cx);
    let newer = core_st::begin_index_job(&s.app.state, None).unwrap();
    state.update(cx, |st, cx| st.cancel_index(cx));
    work(&s.app, cx);
    assert!(!newer.abort.load(Ordering::Relaxed), "our Cancel stopped a newer run");
    newer.slot.clear();

    send_done(&s.app, state.read_with(cx, |st, _| st.index.job().unwrap()), 0, 0, cx);
    state.update(cx, |st, cx| st.index_photos(cx));
    work(&s.app, cx);
    let ours = s.fake.claims.lock().unwrap()[1].abort.clone();
    state.update(cx, |st, cx| st.cancel_index(cx));
    work(&s.app, cx);
    assert!(ours.load(Ordering::Relaxed), "Cancel did not trip our run");
}

/// A catalog switch: before `catalog:switched` reaches the UI, a start bound to the old
/// catalog is refused (nothing tripped); after it, the old run's end changes nothing and the
/// new catalog's running index is adopted.
#[gpui_kit::test]
fn a_switch_drops_the_old_run_and_adopts_the_new_catalogs(cx: &mut TestAppContext) {
    let s = open_st(1, true, "st-switch", cx);
    let state = s.state(cx);
    state.update(cx, |st, cx| st.index_photos(cx));
    work(&s.app, cx);
    let old = state.read_with(cx, |st, _| st.index.job().unwrap());

    let (b, _) = colliding_catalog(&s.dir, "b", 1);
    core_switch(&s.app, b);
    let theirs = core_st::begin_index_job(&s.app.state, None).unwrap();

    send_done(&s.app, old, 0, 0, cx);
    state.update(cx, |st, cx| st.index_photos(cx));
    work(&s.app, cx);
    assert_eq!(s.fake.starts.lock().unwrap().last().cloned(), Some(Err(CATALOG_CHANGED.to_string())));
    assert!(!theirs.abort.load(Ordering::Relaxed), "the refused start tripped the new catalog's run");

    deliver_switch(&s.app, cx);
    work(&s.app, cx);
    assert_eq!(s.phase(cx), IndexPhase::Running { job: theirs.job, done: 0, total: 0, progress: true });
    send_done(&s.app, old, 7, 7, cx);
    assert_eq!(s.phase(cx), IndexPhase::Running { job: theirs.job, done: 0, total: 0, progress: true }, "the old end is ignored");
}

/// A run whose end was already heard is never adopted from a status slot read afterwards.
///
/// Mutation-checked: adopting any slot (no `finished` check) fails it.
#[gpui_kit::test]
fn reattach_never_adopts_a_finished_run(cx: &mut TestAppContext) {
    let s = open_st(1, true, "st-reattach", cx);
    let stale = core_st::begin_index_job(&s.app.state, None).unwrap();
    send_done(&s.app, stale.job, 1, 1, cx);
    s.app.wired.model.update(cx, |m, cx| m.refresh(cx));
    work(&s.app, cx);
    assert_eq!(s.phase(cx), IndexPhase::Idle, "a finished run was re-adopted");
    stale.slot.clear();

    // A live run started elsewhere is adopted on the next catalog read.
    let live = core_st::begin_index_job(&s.app.state, None).unwrap();
    s.app.wired.model.update(cx, |m, cx| m.refresh(cx));
    work(&s.app, cx);
    assert_eq!(s.phase(cx), IndexPhase::Running { job: live.job, done: 0, total: 0, progress: true });
}

// --- suggestions ----------------------------------------------------------------------------

/// Suggest runs the real kNN over the stored embeddings; ✓ add tags the photo; ✗ reject is
/// remembered; an accept after an unannounced switch to a catalog with colliding ids is
/// refused and tags neither.
#[gpui_kit::test]
fn suggest_accept_reject_and_identity(cx: &mut TestAppContext) {
    let s = open_st(4, true, "st-sugs", cx);
    with_cat(&s.app, |c| {
        store::ensure_schema(c.conn()).unwrap();
        let mut v = vec![0.0f32; 512];
        v[0] = 1.0;
        for id in &s.ids {
            store::upsert_embedding(c.conn(), *id, &store::embedding_to_blob(&v)).unwrap();
        }
        let gull = c.create_tag("Animals/Gull").unwrap();
        let fog = c.create_tag("Nature/Fog").unwrap();
        for id in &s.ids[1..] {
            c.assign_tag(*id, gull).unwrap();
            c.assign_tag(*id, fog).unwrap();
        }
    });
    let p0 = s.ids[0];
    s.select(p0, cx);
    s.click("smarttags-suggest", cx);
    assert!(s.present("smarttags-sug-Animals_Gull", cx), "kNN found the neighbours' tag");
    assert!(s.present("smarttags-sug-Nature_Fog", cx));

    s.click("smarttags-add-Animals_Gull", cx);
    assert_eq!(s.photo_tags(p0), vec!["Animals/Gull"]);
    assert_eq!(status(&s.app, cx), "Tagged: Animals/Gull");
    assert!(!s.present("smarttags-sug-Animals_Gull", cx));

    let (b, b_ids) = colliding_catalog(&s.dir, "b", 1);
    assert_eq!(b_ids[0], p0);
    core_switch(&s.app, b);
    s.click("smarttags-reject-Nature_Fog", cx);
    let state = s.state(cx);
    state.read_with(cx, |st, _| assert_eq!(st.error.as_deref(), Some(CATALOG_CHANGED)));
    with_cat(&s.app, |c| {
        chairphoto_core::plugins::smarttags::ensure_suggestions_schema(c.conn()).unwrap();
        let n: i64 = c.conn().query_row("SELECT COUNT(*) FROM smarttags__suggestions", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0, "the reject reached the new catalog");
    });
}

/// A Suggest that finishes after the user moved to another photo stores its rows for the photo
/// it ran on but does not replace the shown photo's list (#126 review).
///
/// Forced interleaving: the Suggest job is held on the manual runner while the next photo's
/// list loads and lands, then released. Mutation-checked: dropping the shown-photo check in
/// `SmarttagsState::suggest` makes the panel show photo 1's list on photo 2.
#[gpui_kit::test]
fn a_suggest_for_a_photo_no_longer_shown_is_dropped(cx: &mut TestAppContext) {
    let s = open_st(3, true, "st-sug-nav", cx);
    with_cat(&s.app, |c| {
        store::ensure_schema(c.conn()).unwrap();
        let mut v = vec![0.0f32; 512];
        v[0] = 1.0;
        for id in &s.ids {
            store::upsert_embedding(c.conn(), *id, &store::embedding_to_blob(&v)).unwrap();
        }
        let gull = c.create_tag("Animals/Gull").unwrap();
        for id in &s.ids[2..] {
            c.assign_tag(*id, gull).unwrap();
        }
    });
    let (p0, p1) = (s.ids[0], s.ids[1]);
    s.select(p0, cx);
    // Click Suggest, but hold its job: it has not run when the user moves on.
    cx.update_window(s.window(), |_, window, cx| {
        window.render_frame(cx);
        window.click(SharedString::from("smarttags-suggest"), cx);
    })
    .unwrap();
    let held = cx.update(|cx| Runner::get(cx).hold_pending());
    assert_eq!(held.len(), 1, "the Suggest job is queued");
    s.select(p1, cx);
    let state = s.state(cx);
    state.read_with(cx, |st, _| assert_eq!(st.suggestions().map(|p| p.photo_id), Some(p1)));

    cx.update(|cx| Runner::get(cx).release(held));
    work(&s.app, cx);
    state.read_with(cx, |st, _| {
        assert!(!st.suggesting, "the Suggest ended");
        let shown = st.suggestions().expect("photo 2's list");
        assert_eq!(shown.photo_id, p1, "photo 1's Suggest replaced the shown photo's list");
        assert!(shown.list.is_empty(), "photo 2 has no suggestions of its own");
    });
    assert!(!s.present("smarttags-sug-Animals_Gull", cx));
    // Photo 1's rows were stored: going back shows them.
    s.select(p0, cx);
    assert!(s.present("smarttags-sug-Animals_Gull", cx), "the Suggest's rows are photo 1's");
}

// --- settings -------------------------------------------------------------------------------

/// The model path saves into its catalog (and the model is checked again); training reports
/// its result; Delete index drops the plugin's tables; after an unannounced switch the save is
/// refused and touches neither catalog.
#[gpui_kit::test]
fn settings_save_train_and_delete(cx: &mut TestAppContext) {
    let s = open_st(1, true, "st-settings", cx);
    let settings = s.settings(cx);
    cx.update_window(s.window(), |_, window, cx| window.dispatch_action(Box::new(crate::shell::actions::OpenPreferences), cx))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(450)); // the dialog's open animation
    cx.run_until_parked();
    s.click("prefs-tab-module-smarttags", cx);
    assert!(s.present("smarttags-settings", cx));
    assert_eq!(s.label("smarttags-model", cx).as_deref(), Some("Ready (default): /models/clip.onnx"));

    let input = settings.read_with(cx, |v, _| v.model_path.clone());
    cx.update_window(s.window(), |_, window, cx| input.update(cx, |i, cx| i.set_value("  /opt/clip.onnx ", window, cx))).unwrap();
    s.click("smarttags-save", cx);
    with_cat(&s.app, |c| assert_eq!(c.get_setting("smarttags.model_path").unwrap().as_deref(), Some("/opt/clip.onnx")));

    s.click("smarttags-train", cx);
    assert_eq!(s.label("smarttags-train-status", cx).as_deref(), Some("No tag has enough confirmed indexed photos yet."));

    with_cat(&s.app, |c| store::ensure_schema(c.conn()).unwrap());
    s.click("smarttags-delete", cx);
    with_cat(&s.app, |c| {
        let n: i64 = c
            .conn()
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'smarttags__%'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "the index tables are dropped");
    });

    let (other, _) = colliding_catalog(&s.dir, "other", 1);
    core_switch(&s.app, other);
    settings.update(cx, |v, cx| v.save(cx));
    work(&s.app, cx);
    assert!(status(&s.app, cx).contains(CATALOG_CHANGED), "{}", status(&s.app, cx));
    with_cat(&s.app, |c| assert_eq!(c.get_setting("smarttags.model_path").unwrap(), None));
}
