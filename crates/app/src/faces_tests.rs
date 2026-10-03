//! Headless tests of the Faces module through the real wiring: the settings panel, the
//! indexing and matching jobs' ownership (start, progress, done, Cancel, a catalog switch,
//! re-attaching) (#129, #130), the inspector's verbs and the person picker, and the loupe
//! overlay's geometry (orientation, zoom, pan) with its keys and draw mode, and the People view
//! (#130): the wall and filter-by-person, naming, merging and splitting clusters, the review
//! queue, catalog identity, and the avatars' image claims.
//!
//! No ONNX and no network: [`FakeFaces`] stands in for the model check, the download and the
//! worker. Its start runs the **real** core claim (`app::faces::begin_index_job`), so ownership
//! is the core's; the tests then play the worker's part (clear the slot, send the events).
//! Catalog work runs on `Runner::manual` ([`work`]); images on a hand-answered `FakePool`.

use super::*;
use crate::image_tests::{pixels, FakePool};
use crate::loupe::zoom::{ZoomImage, ZoomView};
use crate::machine_prefs::MachinePrefs;
use crate::modules::faces::inspector::FacesInspector;
use crate::modules::faces::overlay::{FaceOverlay, SHOW_BOXES_PREF};
use crate::modules::faces::settings::FacesSettings;
use crate::modules::faces::state::{FacesBackend, FacesBackendGlobal, FacesState, IndexPhase, MatchPhase};
use crate::modules::faces::FACES_MODULE_ID;
use crate::modules::{ModuleRegistry, PanelSlot};
use crate::shell::state::InspectorTab;
use crate::storage::Runner;
use chairphoto_core::app::faces::{self as core_faces, FaceBboxJson};
use chairphoto_core::app::{
    CatalogIdentity, FacesIndexDone, FacesJobStatus, FacesMatchDone, FacesMatchJobStatus, FacesMatchProgressEvent,
    FacesProgressEvent, JobClaim, CATALOG_CHANGED,
};
use chairphoto_core::image_pool::{ImageKind, JobKey};
use chairphoto_core::plugins::faces::models::{ModelReport, ModelStatus};
use chairphoto_core::plugins::faces::store;
use gpui_kit::{point, px, InputEvent as _, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ScrollDelta,
    ScrollWheelEvent, TouchPhase};
use gpui_kit::{Focusable as _, SharedString};
use std::sync::Mutex;

/// The model check, download and job start, recorded.
#[derive(Default)]
struct FakeFaces {
    ready: Mutex<bool>,
    downloads: Mutex<usize>,
    claims: Mutex<Vec<JobClaim<FacesJobStatus>>>,
    starts: Mutex<Vec<Result<u64, String>>>,
    match_claims: Mutex<Vec<JobClaim<FacesMatchJobStatus>>>,
    match_starts: Mutex<Vec<Result<u64, String>>>,
}

impl FacesBackend for FakeFaces {
    fn models_status(&self) -> ModelStatus {
        let ready = *self.ready.lock().unwrap();
        let report = |key: &str| ModelReport {
            key: key.into(),
            filename: format!("{key}.onnx"),
            present: ready,
            path: String::new(),
            detail: (!ready).then(|| "not downloaded".to_string()),
        };
        ModelStatus { ready, models: vec![report("yunet"), report("auraface")] }
    }

    fn download_models(&self) -> ModelStatus {
        *self.downloads.lock().unwrap() += 1;
        *self.ready.lock().unwrap() = true;
        self.models_status()
    }

    fn start_index(&self, app: &AppState, from: CatalogIdentity) -> Result<u64, String> {
        let result = core_faces::begin_index_job(app, Some(from)).map(|claim| {
            let job = claim.job;
            self.claims.lock().unwrap().push(claim);
            job
        });
        self.starts.lock().unwrap().push(result.clone());
        result
    }

    /// The real core claim (`begin_match_job`); the test plays the worker.
    fn start_match(&self, app: &AppState, from: CatalogIdentity) -> Result<u64, String> {
        let result = core_faces::begin_match_job(app, Some(from)).map(|claim| {
            let job = claim.job;
            self.match_claims.lock().unwrap().push(claim);
            job
        });
        self.match_starts.lock().unwrap().push(result.clone());
        result
    }
}

impl FakeFaces {
    /// The worker's end: release the slot (only while it owns it), then `faces:index_done`.
    fn finish(&self, app: &App, job: u64, done: usize, total: usize, cx: &mut TestAppContext) {
        if let Some(claim) = self.claims.lock().unwrap().iter().find(|c| c.job == job) {
            claim.slot.clear();
        }
        send_done(app, job, done, total, cx);
    }
}

fn send_done(app: &App, job: u64, done: usize, total: usize, cx: &mut TestAppContext) {
    app.state.send(CoreEvent::FacesIndexDone(FacesIndexDone {
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

fn send_match_progress(app: &App, job: u64, done: usize, total: usize, cx: &mut TestAppContext) {
    app.state.send(CoreEvent::FacesMatchProgress(FacesMatchProgressEvent { done, total, phase: "clustering unknowns", job }));
    cx.run_until_parked();
}

fn send_match_done(app: &App, job: u64, cx: &mut TestAppContext) {
    let outcome = chairphoto_core::plugins::faces::MatchOutcome { seeded: 1, constrained: 1, open: 1, clustered: 2, people: 3 };
    app.state.send(CoreEvent::FacesMatchDone(FacesMatchDone { ok: true, outcome: Some(outcome), aborted: false, job, error: None }));
    cx.run_until_parked();
}

fn send_progress(app: &App, job: u64, done: usize, total: usize, cx: &mut TestAppContext) {
    app.state.send(CoreEvent::FacesProgress(FacesProgressEvent { done, total, job }));
    cx.run_until_parked();
}

/// Run queued catalog work and repaint until nothing more happens.
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

struct Faces {
    app: App,
    fake: Arc<FakeFaces>,
    pool: Arc<FakePool>,
    ids: Vec<i64>,
    _dir: TempDir,
}

/// The app with `n` photos, the fake backend (models `ready`) and the Faces module enabled.
fn open_faces(n: usize, ready: bool, tag: &str, cx: &mut TestAppContext) -> Faces {
    let fake = Arc::new(FakeFaces::default());
    *fake.ready.lock().unwrap() = ready;
    cx.update(|cx| cx.set_global(FacesBackendGlobal(fake.clone())));
    let dir = TempDir::new(tag);
    let pool = Arc::new(FakePool::default());
    let app = start_with_pool(cx, pool.clone());
    let ids = open_catalog_with_photos(&app, &dir, n, cx);
    with_cat(&app, |c| store::ensure_schema(c.conn()).unwrap());
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, FACES_MODULE_ID, cx));
    work(&app, cx);
    Faces { app, fake, pool, ids, _dir: dir }
}

fn with_cat<R>(app: &App, f: impl FnOnce(&Catalog) -> R) -> R {
    f(app.state.catalog.lock().unwrap().as_ref().unwrap())
}

fn add_face(app: &App, photo: i64, bbox: &str) -> i64 {
    with_cat(app, |c| store::insert_face(c.conn(), photo, bbox, "[]", 0.99, None, "detect", 0).unwrap())
}

fn suggest(app: &App, face: i64, tag: i64) {
    with_cat(app, |c| {
        c.conn()
            .execute(
                "UPDATE faces__faces SET person_tag_id = ?2, state = 'suggested', match_confidence = 0.87 WHERE id = ?1",
                [face, tag],
            )
            .unwrap()
    });
}

fn face_row(app: &App, face: i64) -> (String, Option<i64>) {
    with_cat(app, |c| {
        c.conn()
            .query_row("SELECT state, person_tag_id FROM faces__faces WHERE id = ?1", [face], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
    })
}

impl Faces {
    fn window(&self) -> AnyWindowHandle {
        self.app.window()
    }

    /// The module's settings view (and through it, its state).
    fn settings(&self, cx: &mut TestAppContext) -> Entity<FacesSettings> {
        let modules = self.app.wired.modules.clone();
        cx.update_window(self.window(), |_, window, cx| {
            ModuleRegistry::settings_views(&modules, &FACES_MODULE_ID.into(), window, cx)
                .into_iter()
                .next()
                .expect("the faces settings panel")
                .downcast::<FacesSettings>()
                .expect("a FacesSettings")
        })
        .unwrap()
    }

    fn state(&self, cx: &mut TestAppContext) -> Entity<FacesState> {
        let settings = self.settings(cx);
        settings.read_with(cx, |s, _| s.state.clone())
    }

    fn phase(&self, cx: &mut TestAppContext) -> IndexPhase {
        let state = self.state(cx);
        state.read_with(cx, |s, _| s.index.phase)
    }

    fn slot_view<V: 'static>(&self, slot: PanelSlot, id: &str, cx: &mut TestAppContext) -> Entity<V> {
        let modules = self.app.wired.modules.clone();
        let id = id.to_string();
        cx.update_window(self.window(), |_, window, cx| {
            ModuleRegistry::panel_views(&modules, slot, window, cx)
                .into_iter()
                .find(|p| p.id.as_ref() == id)
                .expect("the panel")
                .view
                .downcast::<V>()
                .ok()
                .expect("its view type")
        })
        .unwrap()
    }

    fn select(&self, id: i64, cx: &mut TestAppContext) {
        self.app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_single(id)));
        work(&self.app, cx);
    }

    fn present(&self, id: &str, cx: &mut TestAppContext) -> bool {
        let id = SharedString::from(id.to_string());
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .unwrap()
    }

    fn label(&self, id: &str, cx: &mut TestAppContext) -> Option<String> {
        let id = SharedString::from(id.to_string());
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).and_then(|e| e.label().map(str::to_string))
        })
        .unwrap()
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

    fn bounds(&self, id: &str, cx: &mut TestAppContext) -> gpui_kit::Bounds<gpui_kit::Pixels> {
        let id = SharedString::from(id.to_string());
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.find(id).bounds()
        })
        .unwrap()
    }

    fn press(&self, key: &str, cx: &mut TestAppContext) {
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.press(key, cx);
        })
        .unwrap();
        work(&self.app, cx);
    }

    /// Open the loupe on `photo` with a `w`×`h` preview drawn.
    fn loupe(&self, photo: i64, w: u32, h: u32, cx: &mut TestAppContext) {
        self.select(photo, cx);
        self.press("enter", cx);
        self.pool.finish(&JobKey::photo(photo, ImageKind::Preview), Ok(pixels(w, h)));
        work(&self.app, cx);
    }

    fn zoom(&self, cx: &mut TestAppContext) -> Entity<ZoomImage> {
        let root = self.app.wired.root.clone().expect("the main window");
        root.read_with(cx, |r, cx| r.loupe().read(cx).zoom().clone())
    }
}

// --- the settings panel ---------------------------------------------------------------------

/// The settings read from the catalog, save into it (a blank threshold is the default), and
/// the indexing speed is the host's `indexing.speed`; a save after a catalog switch the UI has
/// not heard of is refused and touches neither catalog's keys.
#[gpui_kit::test]
fn settings_save_into_the_catalog_they_were_read_from(cx: &mut TestAppContext) {
    let f = open_faces(1, true, "faces-settings", cx);
    with_cat(&f.app, |c| c.set_setting("faces.people_root", "Family").unwrap());
    f.app.wired.model.update(cx, |m, cx| m.refresh(cx));
    work(&f.app, cx);
    let settings = f.settings(cx);
    let (root, threshold) = settings.read_with(cx, |s, _| (s.people_root.clone(), s.threshold.clone()));
    // Preferences → Faces: the panel renders, and its inputs fill from the catalog.
    cx.update_window(f.window(), |_, window, cx| window.dispatch_action(Box::new(crate::shell::actions::OpenPreferences), cx))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(450)); // the dialog's open animation
    cx.run_until_parked();
    f.click("prefs-tab-module-faces", cx);
    assert!(f.present("faces-settings", cx), "the module's tab shows its settings panel");
    // "Run matching" is on the panel (below the fold: started through the state); its run
    // shows its step until its end.
    assert!(f.present("faces-match", cx));
    let state = f.state(cx);
    state.update(cx, |s, cx| s.run_matching(cx));
    work(&f.app, cx);
    let job = f.fake.match_starts.lock().unwrap()[0].clone().unwrap();
    send_match_progress(&f.app, job, 3, 6, cx);
    assert_eq!(f.label("faces-match-progress", cx).as_deref(), Some("Matching (clustering unknowns): 3 / 6 (50%)"));
    assert!(f.present("faces-match-cancel", cx));
    f.fake.match_claims.lock().unwrap()[0].slot.clear();
    send_match_done(&f.app, job, cx);
    work(&f.app, cx);
    assert_eq!(
        f.label("faces-match-result", cx).as_deref(),
        Some("Matching: 1 seeded, 2 suggested, 2 clustered (3 known people).")
    );
    assert_eq!(f.label("faces-models", cx).as_deref(), Some("YuNet + AuraFace ready"));
    assert_eq!(root.read_with(cx, |i, _| i.value().to_string()), "Family");
    assert_eq!(threshold.read_with(cx, |i, _| i.value().to_string()), "0.45", "the default when unset");

    cx.update_window(f.window(), |_, window, cx| {
        root.update(cx, |i, cx| i.set_value("People", window, cx));
        threshold.update(cx, |i, cx| i.set_value("  ", window, cx));
    })
    .unwrap();
    f.click("faces-save", cx);
    with_cat(&f.app, |c| {
        assert_eq!(c.get_setting("faces.people_root").unwrap().as_deref(), Some("People"));
        assert_eq!(c.get_setting("faces.match_threshold").unwrap().as_deref(), Some("0.45"));
    });

    let state = f.state(cx);
    state.read_with(cx, |s, _| assert_eq!(s.inference.as_ref().map(|i| i.speed.as_str()), Some("background")));
    f.click("faces-speed-full", cx);
    with_cat(&f.app, |c| assert_eq!(c.get_setting("indexing.speed").unwrap().as_deref(), Some("full")));
    state.read_with(cx, |s, _| {
        assert_eq!(s.inference.as_ref().map(|i| i.speed.as_str()), Some("full"));
        assert!(s.speed_note.as_deref().unwrap().contains("next indexing run"));
    });

    // Another catalog is open, `catalog:switched` not delivered: the save is refused.
    let (other, _) = colliding_catalog(&f._dir, "other", 1);
    core_switch(&f.app, other);
    cx.update_window(f.window(), |_, window, cx| root.update(cx, |i, cx| i.set_value("Strangers", window, cx))).unwrap();
    settings.update(cx, |s, cx| s.save(cx));
    work(&f.app, cx);
    assert!(status(&f.app, cx).contains(CATALOG_CHANGED), "{}", status(&f.app, cx));
    with_cat(&f.app, |c| assert_eq!(c.get_setting("faces.people_root").unwrap(), None, "the new catalog is untouched"));
}

/// Missing models: "Index faces" is disabled and the model line says what is missing;
/// "Download models" is the user's click, and readiness enables indexing.
#[gpui_kit::test]
fn indexing_waits_for_the_models_the_user_downloads(cx: &mut TestAppContext) {
    let f = open_faces(1, false, "faces-models", cx);
    let state = f.state(cx);
    state.read_with(cx, |s, _| {
        assert!(!s.models_ready());
        assert!(!s.can_index(), "no models, no index");
    });
    assert_eq!(*f.fake.downloads.lock().unwrap(), 0, "nothing is downloaded unasked");
    state.update(cx, |s, cx| s.index_faces(cx));
    work(&f.app, cx);
    assert!(f.fake.starts.lock().unwrap().is_empty(), "a click on the disabled button starts nothing");

    state.update(cx, |s, cx| s.download_models(cx));
    work(&f.app, cx);
    assert_eq!(*f.fake.downloads.lock().unwrap(), 1);
    state.read_with(cx, |s, _| assert!(s.models_ready() && s.can_index()));
}

// --- the indexing job -----------------------------------------------------------------------

/// A run is followed by its own id: a superseded run's progress changes nothing, its own
/// progress shows, and its `faces:index_done` ends it with the result line — after the slot
/// was released, so the job ends exactly once.
#[gpui_kit::test]
fn the_index_follows_only_its_own_events(cx: &mut TestAppContext) {
    let f = open_faces(1, true, "faces-job", cx);
    let state = f.state(cx);
    state.update(cx, |s, cx| s.index_faces(cx));
    assert_eq!(f.phase(cx), IndexPhase::Starting);
    work(&f.app, cx);
    let job = f.fake.starts.lock().unwrap()[0].clone().unwrap();
    assert_eq!(f.phase(cx), IndexPhase::Running { job, done: 0, total: 0, progress: false });

    send_progress(&f.app, job + 100, 5, 9, cx);
    assert_eq!(f.phase(cx), IndexPhase::Running { job, done: 0, total: 0, progress: false }, "another run's straggler");
    send_progress(&f.app, job, 2, 4, cx);
    assert_eq!(f.phase(cx), IndexPhase::Running { job, done: 2, total: 4, progress: true });
    send_done(&f.app, job + 100, 9, 9, cx);
    assert!(f.phase(cx) != IndexPhase::Idle, "another run's end does not end ours");

    f.fake.finish(&f.app, job, 4, 4, cx);
    work(&f.app, cx);
    state.read_with(cx, |s, _| {
        assert_eq!(s.index.phase, IndexPhase::Idle);
        assert_eq!(s.index.last_result.as_deref(), Some("Indexing complete: 4 photos processed."));
    });
    assert_eq!(status(&f.app, cx), "Indexing complete: 4 photos processed.");
    assert!(core_faces::index_status(&f.app.state).unwrap().is_none());
}

/// A run's end can arrive before the start's answer (a tiny library): it is held and
/// replayed once the id is known, so the panel does not sit in "Indexing…" forever.
#[gpui_kit::test]
fn an_end_that_beats_the_start_answer_still_ends_the_run(cx: &mut TestAppContext) {
    let f = open_faces(1, true, "faces-early", cx);
    let state = f.state(cx);
    let next = f.app.state.jobs.faces.abort().job_ids_issued() + 1;
    state.update(cx, |s, cx| s.index_faces(cx));
    // The worker ran and finished before the start's answer reached the UI.
    send_progress(&f.app, next, 1, 1, cx);
    send_done(&f.app, next, 1, 1, cx);
    assert_eq!(f.phase(cx), IndexPhase::Starting);
    work(&f.app, cx);
    assert_eq!(f.fake.starts.lock().unwrap()[0], Ok(next));
    state.read_with(cx, |s, _| {
        assert_eq!(s.index.phase, IndexPhase::Idle);
        assert_eq!(s.index.last_result.as_deref(), Some("Indexing complete: 1 photo processed."));
    });
}

/// Cancel names its job: once another start superseded ours, our Cancel stops nothing;
/// otherwise it trips our run's flag.
#[gpui_kit::test]
fn cancel_stops_only_the_followed_run(cx: &mut TestAppContext) {
    let f = open_faces(1, true, "faces-cancel", cx);
    let state = f.state(cx);
    state.update(cx, |s, cx| s.index_faces(cx));
    work(&f.app, cx);
    let ours = f.fake.claims.lock().unwrap()[0].abort.clone();

    // A start from elsewhere (the Tauri shell, a second window) supersedes it.
    let newer = core_faces::begin_index_job(&f.app.state, None).unwrap();
    state.update(cx, |s, cx| s.cancel_index(cx));
    work(&f.app, cx);
    assert!(!newer.abort.load(std::sync::atomic::Ordering::Relaxed), "our Cancel must not stop the newer run");
    assert!(ours.load(std::sync::atomic::Ordering::Relaxed), "ours was superseded (tripped by the newer start)");
    newer.slot.clear();

    // Our own run, still the owner: Cancel trips it, and says so.
    send_done(&f.app, state.read_with(cx, |s, _| s.index.job().unwrap()), 0, 0, cx);
    state.update(cx, |s, cx| s.index_faces(cx));
    work(&f.app, cx);
    let claim_abort = f.fake.claims.lock().unwrap()[1].abort.clone();
    state.update(cx, |s, cx| s.cancel_index(cx));
    work(&f.app, cx);
    assert!(claim_abort.load(std::sync::atomic::Ordering::Relaxed));
    state.read_with(cx, |s, _| assert_eq!(s.index.last_result.as_deref(), Some("Cancelling — stops after the current photo…")));
}

/// A catalog switch: before `catalog:switched` reaches the UI, a start bound to the old
/// catalog is refused (nothing tripped in the new one); after it, the old run's end changes
/// nothing and the new catalog's running index is re-attached.
#[gpui_kit::test]
fn a_switch_drops_the_old_run_and_adopts_the_new_catalogs(cx: &mut TestAppContext) {
    let f = open_faces(1, true, "faces-switch", cx);
    let state = f.state(cx);
    state.update(cx, |s, cx| s.index_faces(cx));
    work(&f.app, cx);
    let old = state.read_with(cx, |s, _| s.index.job().unwrap());

    let (b, _) = colliding_catalog(&f._dir, "b", 1);
    core_switch(&f.app, b);
    let theirs = core_faces::begin_index_job(&f.app.state, None).unwrap(); // a run in the new catalog

    // Not delivered yet: our run's end is still ours to hear, but a new start is refused.
    send_done(&f.app, old, 0, 0, cx);
    state.update(cx, |s, cx| s.index_faces(cx));
    work(&f.app, cx);
    assert_eq!(f.fake.starts.lock().unwrap().last().cloned(), Some(Err(CATALOG_CHANGED.to_string())));
    assert!(!theirs.abort.load(std::sync::atomic::Ordering::Relaxed), "the refused start tripped nothing");
    state.read_with(cx, |s, _| assert!(s.index.error.as_deref().unwrap().contains(CATALOG_CHANGED)));

    deliver_switch(&f.app, cx);
    work(&f.app, cx);
    assert_eq!(
        f.phase(cx),
        IndexPhase::Running { job: theirs.job, done: 0, total: 0, progress: true },
        "the new catalog's run is followed"
    );
    send_done(&f.app, old, 7, 7, cx);
    assert_eq!(f.phase(cx), IndexPhase::Running { job: theirs.job, done: 0, total: 0, progress: true }, "the old end is ignored");
}

/// A run whose end was already heard is never adopted from a status slot read afterwards,
/// and a running match disables "Index faces" until its end.
#[gpui_kit::test]
fn reattach_never_adopts_a_finished_run_and_waits_for_a_match(cx: &mut TestAppContext) {
    let f = open_faces(1, true, "faces-reattach", cx);
    let state = f.state(cx);
    let stale = core_faces::begin_index_job(&f.app.state, None).unwrap();
    // Its end is heard while its slot still reads as running (the read raced the end).
    send_done(&f.app, stale.job, 1, 1, cx);
    f.app.wired.model.update(cx, |m, cx| m.refresh(cx));
    work(&f.app, cx);
    assert_eq!(f.phase(cx), IndexPhase::Idle, "a finished run is not re-adopted");
    stale.slot.clear();

    // A match started elsewhere: its progress makes the panel read its slot and adopt it.
    let elsewhere = core_faces::begin_match_job(&f.app.state, None).unwrap();
    send_match_progress(&f.app, elsewhere.job, 1, 5, cx);
    work(&f.app, cx);
    state.read_with(cx, |s, _| assert!(s.matching.busy() && !s.can_index() && !s.can_match()));
    elsewhere.slot.clear();
    send_match_done(&f.app, elsewhere.job, cx);
    work(&f.app, cx);
    state.read_with(cx, |s, _| assert!(!s.matching.busy() && s.can_index()));
}

// --- the matching job (#130) ----------------------------------------------------------------

fn match_phase(f: &Faces, cx: &mut TestAppContext) -> MatchPhase {
    let state = f.state(cx);
    state.read_with(cx, |s, _| s.matching.phase)
}

/// "Run matching" follows its own run by id: a superseded run's progress and end change
/// nothing, its own progress shows the step, its `faces:match_done` ends it with the result
/// line — and an end that beats the start's answer is replayed. While it runs, neither job
/// can start.
#[gpui_kit::test]
fn the_match_follows_only_its_own_events(cx: &mut TestAppContext) {
    let f = open_faces(1, true, "faces-match", cx);
    let state = f.state(cx);
    state.update(cx, |s, cx| s.run_matching(cx));
    assert_eq!(match_phase(&f, cx), MatchPhase::Starting);
    work(&f.app, cx);
    let job = f.fake.match_starts.lock().unwrap()[0].clone().unwrap();
    assert_eq!(match_phase(&f, cx), MatchPhase::Running { job, done: 0, total: 0, step: "", progress: false });
    state.read_with(cx, |s, _| assert!(!s.can_index() && !s.can_match(), "one job at a time"));
    state.update(cx, |s, cx| s.index_faces(cx));
    work(&f.app, cx);
    assert!(f.fake.starts.lock().unwrap().is_empty(), "no index starts while matching");

    send_match_progress(&f.app, job + 100, 5, 9, cx);
    send_match_done(&f.app, job + 100, cx);
    work(&f.app, cx);
    assert_eq!(match_phase(&f, cx), MatchPhase::Running { job, done: 0, total: 0, step: "", progress: false });
    send_match_progress(&f.app, job, 2, 4, cx);
    assert_eq!(match_phase(&f, cx), MatchPhase::Running { job, done: 2, total: 4, step: "clustering unknowns", progress: true });

    f.fake.match_claims.lock().unwrap()[0].slot.clear();
    send_match_done(&f.app, job, cx);
    work(&f.app, cx);
    let line = "Matching: 1 seeded, 2 suggested, 2 clustered (3 known people).";
    state.read_with(cx, |s, _| {
        assert_eq!(s.matching.phase, MatchPhase::Idle);
        assert_eq!(s.matching.last_result.as_deref(), Some(line));
        assert!(s.can_index() && s.can_match());
    });
    assert_eq!(status(&f.app, cx), line);

    // A tiny run: its end arrives before the start's answer, and is replayed.
    let next = f.app.state.jobs.faces_match.abort().job_ids_issued() + 1;
    state.update(cx, |s, cx| s.run_matching(cx));
    send_match_done(&f.app, next, cx);
    assert_eq!(match_phase(&f, cx), MatchPhase::Starting);
    work(&f.app, cx);
    assert_eq!(f.fake.match_starts.lock().unwrap()[1], Ok(next));
    // Its slot still reads as running (the claim landed after the end was sent): a finished
    // run is never re-adopted from it.
    assert_eq!(match_phase(&f, cx), MatchPhase::Idle, "the early end ended the run");
    f.fake.match_claims.lock().unwrap()[1].slot.clear();
}

/// Cancel names its run: once another start superseded ours, our Cancel stops nothing; our
/// own run's flag is tripped and the panel says so until the end arrives.
#[gpui_kit::test]
fn match_cancel_stops_only_the_followed_run(cx: &mut TestAppContext) {
    let f = open_faces(1, true, "faces-match-cancel", cx);
    let state = f.state(cx);
    state.update(cx, |s, cx| s.run_matching(cx));
    work(&f.app, cx);
    let ours = f.fake.match_claims.lock().unwrap()[0].abort.clone();
    let newer = core_faces::begin_match_job(&f.app.state, None).unwrap();
    state.update(cx, |s, cx| s.cancel_match(cx));
    work(&f.app, cx);
    assert!(!newer.abort.load(std::sync::atomic::Ordering::Relaxed), "our Cancel must not stop the newer run");
    newer.slot.clear();
    send_match_done(&f.app, state.read_with(cx, |s, _| s.matching.job().unwrap()), cx);
    send_match_done(&f.app, newer.job, cx);
    work(&f.app, cx);

    state.update(cx, |s, cx| s.run_matching(cx));
    work(&f.app, cx);
    let claim_abort = f.fake.match_claims.lock().unwrap()[1].abort.clone();
    state.update(cx, |s, cx| s.cancel_match(cx));
    work(&f.app, cx);
    assert!(claim_abort.load(std::sync::atomic::Ordering::Relaxed));
    assert!(ours.load(std::sync::atomic::Ordering::Relaxed), "the first was superseded");
    state.read_with(cx, |s, _| assert_eq!(s.matching.last_result.as_deref(), Some("Cancelling — stops at the next face…")));
}

/// A catalog switch: before `catalog:switched` reaches the UI a start bound to the old
/// catalog is refused (the new catalog's run is not tripped); after it, the old run's end
/// changes nothing and the new catalog's running match is adopted from its slot — and a
/// straggler of the old run does not resurrect it.
#[gpui_kit::test]
fn a_switch_drops_the_old_match_and_adopts_the_new_catalogs(cx: &mut TestAppContext) {
    let f = open_faces(1, true, "faces-match-switch", cx);
    let state = f.state(cx);
    state.update(cx, |s, cx| s.run_matching(cx));
    work(&f.app, cx);
    let old = state.read_with(cx, |s, _| s.matching.job().unwrap());
    send_match_done(&f.app, old, cx); // ours ends
    work(&f.app, cx);

    let (b, _) = colliding_catalog(&f._dir, "b", 1);
    core_switch(&f.app, b);
    let theirs = core_faces::begin_match_job(&f.app.state, None).unwrap();
    state.update(cx, |s, cx| s.run_matching(cx));
    work(&f.app, cx);
    assert_eq!(f.fake.match_starts.lock().unwrap().last().cloned(), Some(Err(CATALOG_CHANGED.to_string())));
    assert!(!theirs.abort.load(std::sync::atomic::Ordering::Relaxed), "the refused start tripped nothing");
    state.read_with(cx, |s, _| assert!(s.matching.error.as_deref().unwrap().contains(CATALOG_CHANGED)));

    deliver_switch(&f.app, cx);
    work(&f.app, cx);
    assert!(matches!(match_phase(&f, cx), MatchPhase::Running { job, .. } if job == theirs.job), "the new catalog's run");
    send_match_progress(&f.app, old, 9, 9, cx);
    send_match_done(&f.app, old, cx);
    work(&f.app, cx);
    assert!(matches!(match_phase(&f, cx), MatchPhase::Running { job, .. } if job == theirs.job), "old events are ignored");

    // Its end; afterwards an old straggler finds no slot and adopts nothing.
    theirs.slot.clear();
    send_match_done(&f.app, theirs.job, cx);
    work(&f.app, cx);
    send_match_progress(&f.app, theirs.job + 50, 1, 2, cx);
    work(&f.app, cx);
    assert_eq!(match_phase(&f, cx), MatchPhase::Idle);
}

// --- the inspector --------------------------------------------------------------------------

/// The inspector lists the active photo's faces; ✓ confirms (tagging the photo), the picker
/// assigns an existing person by Enter and creates a new one under the people root — with no
/// `faces.people_root` set, the matcher's and People view's default, `People`.
#[gpui_kit::test]
fn the_inspector_confirms_and_names_faces(cx: &mut TestAppContext) {
    let f = open_faces(2, true, "faces-inspector", cx);
    let photo = f.ids[0];
    let alice = with_cat(&f.app, |c| c.create_tag("People/Alice").unwrap());
    with_cat(&f.app, |c| assert_eq!(c.get_setting("faces.people_root").unwrap(), None, "no people root set"));
    let a = add_face(&f.app, photo, "[0.1,0.1,0.2,0.2]");
    suggest(&f.app, a, alice);
    let b = add_face(&f.app, photo, "[0.5,0.5,0.2,0.2]");
    let c = add_face(&f.app, photo, "[0.7,0.1,0.2,0.2]");
    f.app.wired.shell.update(cx, |s, cx| s.set_inspector_tab(InspectorTab::Tags, cx));
    f.select(photo, cx);

    assert_eq!(f.label(&format!("faces-insp-name-{a}"), cx).as_deref(), Some("Alice"));
    assert_eq!(f.label(&format!("faces-insp-name-{b}"), cx).as_deref(), Some("Unknown"));
    assert!(!f.present(&format!("faces-insp-confirm-all-{a}"), cx), "one photo selected: no confirm on N");

    f.click(&format!("faces-insp-confirm-{a}"), cx);
    assert_eq!(face_row(&f.app, a), ("confirmed".to_string(), Some(alice)));
    assert!(with_cat(&f.app, |c| c.get_photo_tags(photo).unwrap().iter().any(|t| t.id == alice)), "the photo is tagged");

    // ⇄ on b: the picker; "ali" + Enter picks Alice (the first row).
    f.click(&format!("faces-insp-reassign-{b}"), cx);
    let insp: Entity<FacesInspector> = f.slot_view(PanelSlot::Inspector, "faces-inspector", cx);
    let picker = insp.read_with(cx, |i, _| i.picker().cloned()).expect("the picker opened");
    let input = picker.read_with(cx, |p, _| p.input.clone());
    cx.update_window(f.window(), |_, window, cx| {
        window.render_frame(cx);
        assert!(input.read(cx).focus_handle(cx).is_focused(window), "the picker's field has focus");
        window.input("ali", cx);
    })
    .unwrap();
    f.press("enter", cx);
    assert_eq!(face_row(&f.app, b), ("confirmed".to_string(), Some(alice)));
    assert!(insp.read_with(cx, |i, _| i.picker().is_none()), "the picker closed");

    // ⇄ on c: a new name → "＋ Create" (the only row) → People/Dana.
    f.click(&format!("faces-insp-reassign-{c}"), cx);
    cx.update_window(f.window(), |_, window, cx| {
        window.render_frame(cx);
        window.input("Dana", cx);
    })
    .unwrap();
    f.press("enter", cx);
    let dana: i64 = with_cat(&f.app, |c| c.conn().query_row("SELECT id FROM tags WHERE full_path = 'People/Dana'", [], |r| r.get(0)).unwrap());
    assert_eq!(face_row(&f.app, c), ("confirmed".to_string(), Some(dana)));
    assert_eq!(f.label(&format!("faces-insp-name-{c}"), cx).as_deref(), Some("Dana"));
    assert!(f.app.wired.shell.read_with(cx, |s, _| s.stage_view() == crate::shell::state::StageView::Grid), "Enter did not open the loupe");
}

/// "✓✓ confirm on N" confirms the person across the selection where they were suggested and
/// says what it did; a face write bound to the old catalog refuses the new one's colliding ids.
#[gpui_kit::test]
fn confirm_on_selection_and_writes_bound_to_their_catalog(cx: &mut TestAppContext) {
    let f = open_faces(3, true, "faces-batch", cx);
    let alice = with_cat(&f.app, |c| c.create_tag("People/Alice").unwrap());
    let faces: Vec<i64> = f.ids[..2].iter().map(|&p| add_face(&f.app, p, "[0.1,0.1,0.2,0.2]")).collect();
    for &face in &faces {
        suggest(&f.app, face, alice);
    }
    f.app.wired.shell.update(cx, |s, cx| s.set_inspector_tab(InspectorTab::Tags, cx));
    f.select(f.ids[0], cx);
    f.app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_all()));
    work(&f.app, cx);
    let active = f.app.wired.shell.read_with(cx, |s, _| s.library.selection().active_id).unwrap();
    let face = faces[f.ids.iter().position(|&p| p == active).unwrap()];
    assert!(f.present(&format!("faces-insp-confirm-all-{face}"), cx), "three selected: confirm on 3");
    let state = f.state(cx);
    assert_eq!(state.read_with(cx, |s, cx| s.selection_targets(cx).len()), 3);
    f.click(&format!("faces-insp-confirm-all-{face}"), cx);
    for &face in &faces {
        assert_eq!(face_row(&f.app, face).0, "confirmed");
    }
    assert_eq!(status(&f.app, cx), "Confirmed Alice on 2 of 3 selected photos — 1 had no suggestion for Alice.");

    // The UI still shows catalog A's faces; catalog B (colliding ids) is open.
    let (b, b_ids) = colliding_catalog(&f._dir, "b", 3);
    store::ensure_schema(b.conn()).unwrap();
    let b_face = store::insert_face(b.conn(), b_ids[0], "[0.1,0.1,0.2,0.2]", "[]", 0.9, None, "detect", 0).unwrap();
    let shown = state.read_with(cx, |s, _| s.faces().unwrap().faces[0].id);
    assert_eq!(shown, b_face, "the ids collide");
    core_switch(&f.app, b);
    state.update(cx, |s, cx| s.ignore(shown, cx));
    work(&f.app, cx);
    assert!(status(&f.app, cx).contains(CATALOG_CHANGED), "{}", status(&f.app, cx));
    assert_eq!(face_row(&f.app, b_face).0, "unassigned", "the new catalog's face is untouched");
}

// --- the loupe overlay ----------------------------------------------------------------------

fn bb(x: f32, y: f32, w: f32, h: f32) -> FaceBboxJson {
    FaceBboxJson { x, y, w, h }
}

/// Where a face box must land, derived here from first principles (not through the module's
/// own maths): the picture contained and centred in the container, scaled about its centre by
/// the view, moved by its pan; the box at its normalized place in that picture.
fn expected(b: FaceBboxJson, natural: (f32, f32), container: (f32, f32), view: ZoomView) -> crate::modules::faces::logic::ScreenRect {
    let fit = (container.0 / natural.0).min(container.1 / natural.1);
    let (w, h) = (natural.0 * fit * view.scale, natural.1 * fit * view.scale);
    let (l, t) = ((container.0 - w) / 2. + view.tx, (container.1 - h) / 2. + view.ty);
    crate::modules::faces::logic::ScreenRect { left: l + b.x * w, top: t + b.y * h, width: b.w * w, height: b.h * h }
}

fn assert_rect(actual: gpui_kit::Bounds<gpui_kit::Pixels>, origin: gpui_kit::Point<gpui_kit::Pixels>, want: crate::modules::faces::logic::ScreenRect) {
    let (l, t) = (f32::from(actual.origin.x - origin.x), f32::from(actual.origin.y - origin.y));
    let (w, h) = (f32::from(actual.size.width), f32::from(actual.size.height));
    let close = |a: f32, b: f32| (a - b).abs() < 1.0;
    assert!(
        close(l, want.left) && close(t, want.top) && close(w, want.width) && close(h, want.height),
        "drawn ({l}, {t}, {w}, {h}) want {want:?}"
    );
}

/// The boxes sit on the faces of the picture the loupe draws — landscape and portrait
/// (oriented) previews letterbox differently — and follow a zoom and a pan.
#[gpui_kit::test]
fn overlay_boxes_follow_the_orientation_zoom_and_pan(cx: &mut TestAppContext) {
    let f = open_faces(2, true, "faces-overlay", cx);
    let (land, port) = (f.ids[0], f.ids[1]);
    let a = add_face(&f.app, land, "[0.25,0.5,0.25,0.25]");
    let p = add_face(&f.app, port, "[0.5,0.25,0.25,0.125]");

    f.loupe(land, 400, 200, cx);
    let zoom = f.zoom(cx);
    let container = zoom.read_with(cx, |z, _| z.bounds().unwrap());
    let size = (f32::from(container.size.width), f32::from(container.size.height));
    let want = expected(bb(0.25, 0.5, 0.25, 0.25), (400., 200.), size, ZoomView::FIT);
    assert_rect(f.bounds(&format!("faces-box-{a}"), cx), container.origin, want);

    // Zoom in at the image's centre, then pan: the box moves with the picture.
    cx.update_window(f.window(), |_, window, cx| {
        let position = container.center();
        window.dispatch_event(
            ScrollWheelEvent { position, delta: ScrollDelta::Lines(point(0., 1.)), modifiers: Modifiers::default(), touch_phase: TouchPhase::Moved }
                .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
    })
    .unwrap();
    work(&f.app, cx);
    let view = zoom.read_with(cx, |z, cx| z.view(cx));
    assert!(view.zoomed(), "{view:?}");
    let want = expected(bb(0.25, 0.5, 0.25, 0.25), (400., 200.), size, view);
    assert_rect(f.bounds(&format!("faces-box-{a}"), cx), container.origin, want);
    let panned = view.panned(-40., 25.);
    zoom.update(cx, |z, cx| z.shared_view().update(cx, |s, cx| s.set(panned, cx)));
    work(&f.app, cx);
    let want = expected(bb(0.25, 0.5, 0.25, 0.25), (400., 200.), size, panned);
    assert_rect(f.bounds(&format!("faces-box-{a}"), cx), container.origin, want);

    // The portrait photo: bars at the sides, the box against the oriented frame.
    f.press("escape", cx);
    f.loupe(port, 200, 400, cx);
    assert!(!f.present(&format!("faces-box-{a}"), cx), "the previous photo's boxes are gone");
    let want = expected(bb(0.5, 0.25, 0.25, 0.125), (200., 400.), size, ZoomView::FIT);
    assert_rect(f.bounds(&format!("faces-box-{p}"), cx), container.origin, want);
}

/// A Darkroom visit renders a strip photo's thumbnail again for its cover look, which moves
/// the Thumb tier's version past the Preview's (#134). The boxes still show on that
/// thumbnail while the loupe draws it as the placeholder before the preview lands (review
/// rv134 L1).
#[gpui_kit::test]
fn overlay_boxes_show_on_a_thumbnail_the_darkroom_rendered_again(cx: &mut TestAppContext) {
    use crate::loupe::zoom::Drawn;
    let f = open_faces(2, true, "faces-thumb-version", cx);
    let photo = f.ids[0];
    let a = add_face(&f.app, photo, "[0.25,0.5,0.25,0.25]");
    let images = f.app.wired.images.clone();
    let thumb = JobKey::photo(photo, ImageKind::Thumb);
    images.update(cx, |s, _| s.request(photo, ImageKind::Thumb));
    f.pool.finish(&thumb, Ok(pixels(400, 200)));
    work(&f.app, cx);

    // What the strip does when a row names another look than the grid's (the rows re-read
    // in between): the thumbnail is rendered again for it; then the grid's look, again. Only
    // the Thumb tier's version moves.
    let from = f.app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
    let claim = images.update(cx, |s, _| s.new_claim());
    let other = Some(chairphoto_model::darkroom::filmstrip::CoverLook { version: 1, rev: 0 });
    images.update(cx, |s, cx| s.request_looks(claim, from, &[(photo, other)], cx));
    f.pool.finish(&thumb, Ok(pixels(400, 200)));
    work(&f.app, cx);
    images.update(cx, |s, _| s.release_looks(claim));
    images.update(cx, |s, cx| s.request_look_batch(from, &[(photo, None)], cx));
    f.pool.finish(&thumb, Ok(pixels(400, 200)));
    work(&f.app, cx);
    let (t, p) = images.read_with(cx, |s, _| (s.key(photo, ImageKind::Thumb).version, s.key(photo, ImageKind::Preview).version));
    assert_ne!(t, p, "the tiers' versions differ");

    // The loupe on it, the preview not yet rendered: the thumbnail is drawn, with the boxes.
    f.select(photo, cx);
    f.press("enter", cx);
    work(&f.app, cx);
    assert_eq!(f.zoom(cx).read_with(cx, |z, _| z.drawn()), Some((photo, Drawn::Thumb)));
    assert!(f.present(&format!("faces-box-{a}"), cx), "the boxes show on the thumbnail");
}

/// A `w`×`h` render of the photo's cover version (`Loaded::cover`): may be cropped, so not
/// the original's frame.
fn cover_pixels(w: u32, h: u32) -> crate::image_store::Loaded {
    crate::image_store::Loaded { cover: true, ..pixels(w, h) }
}

/// #152: while the loupe draws the thumbnail as the placeholder and that thumbnail is the
/// cover version's render (possibly cropped), the boxes — in the original's frame — are not
/// drawn on it; the note says why. When the preview (the original's frame) lands, they are.
#[gpui_kit::test]
fn overlay_boxes_hide_on_a_cover_thumbnail_placeholder(cx: &mut TestAppContext) {
    use crate::loupe::zoom::Drawn;
    let f = open_faces(2, true, "faces-cover-thumb", cx);
    let photo = f.ids[0];
    let a = add_face(&f.app, photo, "[0.25,0.5,0.25,0.25]");
    let images = f.app.wired.images.clone();
    images.update(cx, |s, _| s.request(photo, ImageKind::Thumb));
    f.pool.finish(&JobKey::photo(photo, ImageKind::Thumb), Ok(cover_pixels(200, 200)));
    work(&f.app, cx);

    f.select(photo, cx);
    f.press("enter", cx);
    work(&f.app, cx);
    assert_eq!(f.zoom(cx).read_with(cx, |z, _| z.drawn()), Some((photo, Drawn::Thumb)));
    assert!(!f.present(&format!("faces-box-{a}"), cx), "no box on the cover's thumbnail");
    assert!(f.present("faces-overlay-version", cx), "the note says the faces are on the original");

    f.pool.finish(&JobKey::photo(photo, ImageKind::Preview), Ok(pixels(400, 200)));
    work(&f.app, cx);
    assert_eq!(f.zoom(cx).read_with(cx, |z, _| z.drawn()), Some((photo, Drawn::Preview)));
    assert!(f.present(&format!("faces-box-{a}"), cx), "the preview is the original's frame: boxes");
    assert!(!f.present("faces-overlay-version", cx));
}

/// F toggles the boxes and remembers it on this machine; Esc in draw mode leaves draw mode,
/// not the loupe; a drag draws a box that becomes a face and opens the picker on it.
#[gpui_kit::test]
fn overlay_keys_and_drawing_a_missed_face(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(MachinePrefs::in_memory()));
    let f = open_faces(1, true, "faces-draw", cx);
    let photo = f.ids[0];
    let a = add_face(&f.app, photo, "[0.1,0.1,0.2,0.2]");
    f.loupe(photo, 400, 400, cx);
    assert!(f.present(&format!("faces-box-{a}"), cx));

    f.press("f", cx);
    assert!(!f.present(&format!("faces-box-{a}"), cx), "F hid the boxes");
    assert_eq!(cx.update(|cx| MachinePrefs::read(cx, SHOW_BOXES_PREF)).as_deref(), Some("0"));
    assert_eq!(f.label("faces-toggle", cx).as_deref(), Some("show faces"));
    f.press("f", cx);
    assert!(f.present(&format!("faces-box-{a}"), cx), "F showed them again");

    f.click("faces-draw", cx);
    let overlay: Entity<FaceOverlay> = f.slot_view(PanelSlot::Loupe, "faces-overlay", cx);
    assert!(overlay.read_with(cx, |o, _| o.draw_mode));
    f.press("escape", cx);
    assert!(!overlay.read_with(cx, |o, _| o.draw_mode), "Esc left draw mode");
    assert_eq!(f.app.wired.shell.read_with(cx, |s, _| s.stage_view()), crate::shell::state::StageView::Loupe, "…and not the loupe");

    // Draw from (60%, 60%) to (80%, 90%) of the (square, fitted) image.
    f.click("faces-draw", cx);
    let zoom = f.zoom(cx);
    let c = zoom.read_with(cx, |z, _| z.bounds().unwrap());
    let (l, t, w, h) = ZoomView::FIT.placement((400., 400.), (f32::from(c.size.width), f32::from(c.size.height)));
    let at = |x: f32, y: f32| c.origin + point(px(l + x * w), px(t + y * h));
    let (from, to) = (at(0.6, 0.6), at(0.8, 0.9));
    cx.update_window(f.window(), |_, window, cx| {
        window.render_frame(cx);
        window.dispatch_event(MouseMoveEvent { position: from, pressed_button: None, modifiers: Modifiers::default() }.to_platform_input(), cx);
        window.dispatch_event(
            MouseDownEvent { button: MouseButton::Left, position: from, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }
                .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
        window.dispatch_event(
            MouseMoveEvent { position: to, pressed_button: Some(MouseButton::Left), modifiers: Modifiers::default() }.to_platform_input(),
            cx,
        );
        window.render_frame(cx);
        window.dispatch_event(
            MouseUpEvent { button: MouseButton::Left, position: to, modifiers: Modifiers::default(), click_count: 1 }.to_platform_input(),
            cx,
        );
        window.render_frame(cx);
    })
    .unwrap();
    work(&f.app, cx);
    let drawn = with_cat(&f.app, |c| core_faces::faces_for_photo(c, photo).unwrap())
        .into_iter()
        .find(|r| r.source == "drawn")
        .expect("the drawn face");
    let close = |a: f32, b: f32| (a - b).abs() < 0.01;
    assert!(close(drawn.bbox.x, 0.6) && close(drawn.bbox.y, 0.6) && close(drawn.bbox.w, 0.2) && close(drawn.bbox.h, 0.3), "{:?}", drawn.bbox);
    assert!(zoom.read_with(cx, |z, cx| !z.view(cx).zoomed()), "the drag drew, it did not pan");
    work(&f.app, cx);
    assert!(overlay.read_with(cx, |o, _| o.picker().is_some()), "the picker opened on the drawn face");
    assert!(f.present(&format!("faces-delete-{}", drawn.id), cx), "a drawn box can be deleted");
    f.press("escape", cx);
    assert!(overlay.read_with(cx, |o, _| o.picker().is_none()), "Esc closed the picker");
    assert_eq!(f.app.wired.shell.read_with(cx, |s, _| s.stage_view()), crate::shell::state::StageView::Loupe, "…not the loupe");
    f.click(&format!("faces-delete-{}", drawn.id), cx);
    assert_eq!(with_cat(&f.app, |c| core_faces::faces_for_photo(c, photo).unwrap()).len(), 1);
}

/// #172: with the Faces module on, the loupe's key hint names F (App.tsx's hint); disabling
/// the module takes it out again.
#[gpui_kit::test]
fn the_loupe_hint_names_f_while_faces_is_on(cx: &mut TestAppContext) {
    let f = open_faces(1, true, "faces-hint", cx);
    f.loupe(f.ids[0], 400, 400, cx);
    assert_eq!(
        f.label("loupe-hint", cx).as_deref(),
        Some("scroll zoom · drag pan · dbl-click 100% · P pick · X reject · F faces · ← →")
    );
    cx.update(|cx| ModuleRegistry::disable(&f.app.wired.modules, FACES_MODULE_ID, cx));
    work(&f.app, cx);
    assert_eq!(
        f.label("loupe-hint", cx).as_deref(),
        Some("scroll zoom · drag pan · dbl-click 100% · P pick · X reject · ← →")
    );
}

// --- any window: the pop-out loupe (#110) ---------------------------------------------------

fn in_window<R: 'static>(
    h: AnyWindowHandle,
    cx: &mut TestAppContext,
    f: impl FnOnce(&mut gpui_kit::Window, &mut gpui_kit::App) -> R,
) -> R {
    cx.update_window(h, |_, window, cx| {
        window.render_frame(cx);
        f(window, cx)
    })
    .unwrap()
}

fn present_in(h: AnyWindowHandle, id: &str, cx: &mut TestAppContext) -> bool {
    let id = SharedString::from(id.to_string());
    in_window(h, cx, |window, _| window.try_find(id).is_some())
}

fn bounds_in(h: AnyWindowHandle, id: &str, cx: &mut TestAppContext) -> gpui_kit::Bounds<gpui_kit::Pixels> {
    let id = SharedString::from(id.to_string());
    in_window(h, cx, |window, _| window.find(id).bounds())
}

fn press_in(f: &Faces, h: AnyWindowHandle, key: &'static str, cx: &mut TestAppContext) {
    in_window(h, cx, |window, cx| window.press(key, cx));
    work(&f.app, cx);
}

fn click_in(f: &Faces, h: AnyWindowHandle, id: &str, cx: &mut TestAppContext) {
    let id = SharedString::from(id.to_string());
    in_window(h, cx, |window, cx| window.click(id, cx));
    work(&f.app, cx);
}

/// One left-button drag from `from` to `to` (window coordinates) in `h`.
fn drag_in(
    f: &Faces,
    h: AnyWindowHandle,
    from: gpui_kit::Point<gpui_kit::Pixels>,
    to: gpui_kit::Point<gpui_kit::Pixels>,
    cx: &mut TestAppContext,
) {
    in_window(h, cx, |window, cx| {
        window.dispatch_event(MouseMoveEvent { position: from, pressed_button: None, modifiers: Modifiers::default() }.to_platform_input(), cx);
        window.dispatch_event(
            MouseDownEvent { button: MouseButton::Left, position: from, modifiers: Modifiers::default(), click_count: 1, first_mouse: false }
                .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
        window.dispatch_event(
            MouseMoveEvent { position: to, pressed_button: Some(MouseButton::Left), modifiers: Modifiers::default() }.to_platform_input(),
            cx,
        );
        window.render_frame(cx);
        window.dispatch_event(
            MouseUpEvent { button: MouseButton::Left, position: to, modifiers: Modifiers::default(), click_count: 1 }.to_platform_input(),
            cx,
        );
        window.render_frame(cx);
    });
    work(&f.app, cx);
}

fn drawn_faces(f: &Faces, photo: i64) -> Vec<core_faces::FaceForPhoto> {
    with_cat(&f.app, |c| core_faces::faces_for_photo(c, photo).unwrap()).into_iter().filter(|r| r.source == "drawn").collect()
}

fn open_popout(cx: &mut TestAppContext) -> AnyWindowHandle {
    cx.update(crate::loupe::window::open);
    cx.run_until_parked();
    cx.update(|cx| crate::loupe::window::handle(cx)).expect("the pop-out opened")
}

fn popout_zoom(cx: &mut TestAppContext) -> Entity<ZoomImage> {
    let view = cx.update(|cx| crate::loupe::window::view(cx)).expect("open");
    view.read_with(cx, |v, cx| v.loupe().read(cx).zoom().clone())
}

fn close_bbox(got: FaceBboxJson, want: (f32, f32, f32, f32)) -> bool {
    let close = |a: f32, b: f32| (a - b).abs() < 0.01;
    close(got.x, want.0) && close(got.y, want.1) && close(got.w, want.2) && close(got.h, want.3)
}

/// The overlay works in the pop-out loupe (#110) as in the inline one: the boxes sit on the
/// pop-out's own picture, F toggles them there, and "＋ face" draws a face there — with the
/// main window on the grid, so no inline loupe is drawing anything.
#[gpui_kit::test]
fn the_overlay_works_in_the_pop_out(cx: &mut TestAppContext) {
    cx.update(|cx| cx.set_global(MachinePrefs::in_memory()));
    let f = open_faces(1, true, "faces-popout", cx);
    let photo = f.ids[0];
    let a = add_face(&f.app, photo, "[0.25,0.5,0.25,0.25]");
    f.select(photo, cx);
    let h = open_popout(cx);
    f.pool.finish(&JobKey::photo(photo, ImageKind::Preview), Ok(pixels(400, 200)));
    work(&f.app, cx);
    assert_eq!(f.app.wired.shell.read_with(cx, |s, _| s.stage_view()), crate::shell::state::StageView::Grid);

    let box_id = format!("faces-box-{a}");
    assert!(present_in(h, &box_id, cx), "the pop-out draws the boxes");
    let zoom = popout_zoom(cx);
    let c = zoom.read_with(cx, |z, _| z.bounds().unwrap());
    let size = (f32::from(c.size.width), f32::from(c.size.height));
    assert_rect(bounds_in(h, &box_id, cx), c.origin, expected(bb(0.25, 0.5, 0.25, 0.25), (400., 200.), size, ZoomView::FIT));

    press_in(&f, h, "f", cx);
    assert!(!present_in(h, &box_id, cx), "F in the pop-out hid the boxes");
    press_in(&f, h, "f", cx);
    assert!(present_in(h, &box_id, cx), "…and showed them again");

    click_in(&f, h, "faces-draw", cx);
    let (l, t, w, hh) = ZoomView::FIT.placement((400., 200.), size);
    let at = |x: f32, y: f32| c.origin + point(px(l + x * w), px(t + y * hh));
    drag_in(&f, h, at(0.6, 0.2), at(0.8, 0.6), cx);
    let drawn = drawn_faces(&f, photo);
    assert_eq!(drawn.len(), 1, "one face drawn in the pop-out");
    assert!(close_bbox(drawn[0].bbox, (0.6, 0.2, 0.2, 0.4)), "{:?}", drawn[0].bbox);
}

/// The Faces block shows — and its verbs act on — the photo the rest of the inspector shows:
/// Compare's focused pane, not the Library's active photo.
#[gpui_kit::test]
fn the_inspectors_faces_follow_compares_focused_pane(cx: &mut TestAppContext) {
    let f = open_faces(4, true, "faces-compare", cx);
    let alice = with_cat(&f.app, |c| c.create_tag("People/Alice").unwrap());
    let faces: Vec<i64> = f.ids.iter().map(|&p| add_face(&f.app, p, "[0.1,0.1,0.2,0.2]")).collect();
    for &face in &faces {
        suggest(&f.app, face, alice);
    }
    f.app.wired.shell.update(cx, |s, cx| s.set_inspector_tab(InspectorTab::Tags, cx));
    f.select(f.ids[0], cx);
    f.app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_all()));
    work(&f.app, cx);
    let active = f.app.wired.shell.read_with(cx, |s, _| s.library.selection().active_id).unwrap();
    f.press("c", cx);
    let mut focused = f.app.wired.shell.read_with(cx, |s, _| s.compare_focused()).expect("Compare is open");
    for _ in 0..4 {
        if focused != active {
            break;
        }
        f.press("down", cx);
        focused = f.app.wired.shell.read_with(cx, |s, _| s.compare_focused()).unwrap();
    }
    assert_ne!(focused, active, "the focus is not the active photo");
    let face_of = |photo: i64| faces[f.ids.iter().position(|&p| p == photo).unwrap()];
    let (shown, other) = (face_of(focused), face_of(active));

    assert!(f.present(&format!("faces-insp-name-{shown}"), cx), "the focused pane's faces are listed");
    assert!(!f.present(&format!("faces-insp-name-{other}"), cx), "not the active photo's");
    f.click(&format!("faces-insp-confirm-{shown}"), cx);
    assert_eq!(face_row(&f.app, shown).0, "confirmed", "✓ confirmed the shown face");
    assert_eq!(face_row(&f.app, other).0, "suggested", "the active photo's face is untouched");
    assert!(with_cat(&f.app, |c| c.get_photo_tags(focused).unwrap().iter().any(|t| t.id == alice)), "the shown photo is tagged");
    let state = f.state(cx);
    assert!(state.read_with(cx, |s, cx| s.selection_targets(cx).contains(&focused)));
}

/// A Darkroom print in the pop-out (the edit rendered from the print's own pixels) is an
/// edited version's frame — possibly cropped, straightened or turned — so the boxes are hidden
/// there with the note, exactly as over an active version in the loupe, even though the
/// original's preview is loaded and the faces are the print's photo's.
#[cfg(feature = "edit")]
#[gpui_kit::test]
fn a_darkroom_print_in_the_pop_out_hides_the_boxes_with_a_note(cx: &mut TestAppContext) {
    use crate::loupe::zoom::Drawn;
    use crate::shell::state::LoupePrint;
    use chairphoto_core::plugins::edit::SourceToken;
    let f = open_faces(1, true, "faces-print", cx);
    let photo = f.ids[0];
    let a = add_face(&f.app, photo, "[0.25,0.5,0.25,0.25]");
    f.select(photo, cx);
    let h = open_popout(cx);
    f.pool.finish(&JobKey::photo(photo, ImageKind::Preview), Ok(pixels(400, 200)));
    work(&f.app, cx);
    assert!(present_in(h, &format!("faces-box-{a}"), cx), "the original: boxes");

    let row = f.app.wired.shell.read_with(cx, |s, _| s.library.photos().iter().find(|p| p.id == photo).cloned()).unwrap();
    let source = SourceToken::Working { photo_id: photo, generation: 1 };
    let print = LoupePrint { photo: row, edit_json: "{\"ev\":1}".into(), source };
    f.app.wired.shell.update(cx, |s, cx| s.set_loupe_print(Some(print), cx));
    work(&f.app, cx);
    in_window(h, cx, |_, _| ());
    let job = f
        .pool
        .batches
        .lock()
        .unwrap()
        .iter()
        .flatten()
        .find_map(|k| match k {
            JobKey::Edit(job) if job.photo_id == photo => Some(job.clone()),
            _ => None,
        })
        .expect("the print's render was asked for");
    f.pool.finish(&JobKey::Edit(job), Ok(pixels(400, 200)));
    work(&f.app, cx);
    in_window(h, cx, |_, _| ());
    assert!(matches!(popout_zoom(cx).read_with(cx, |z, _| z.drawn()), Some((p, Drawn::OverrideLo | Drawn::OverrideHi)) if p == photo));
    assert!(!present_in(h, &format!("faces-box-{a}"), cx), "no boxes over the print");
    assert!(present_in(h, "faces-overlay-version", cx), "the note says why");
}

/// A user-rotated photo: the loupe draws its tiers turned, the boxes stay in the canonical
/// (unturned) frame. At 0/90/180/270 a stored box lands on the turned picture where the turned
/// face is, and a box drawn on the turned picture is stored back in the canonical frame (and
/// shows where it was drawn). The turned positions are worked out by hand here (90° clockwise
/// carries a point (x, y) to (1 − y, x)); `logic::a_turned_box_follows_the_turned_pixels`
/// checks that rule against the core's own `rotate_image`.
#[gpui_kit::test]
fn overlay_boxes_turn_with_the_user_rotation(cx: &mut TestAppContext) {
    let f = open_faces(1, true, "faces-rotate", cx);
    let photo = f.ids[0];
    let a = add_face(&f.app, photo, "[0.1,0.2,0.3,0.4]");
    f.loupe(photo, 400, 200, cx);
    let zoom = f.zoom(cx);
    let state = f.state(cx);
    // (rotation, where face `a` shows on the turned picture, where the drag below is stored)
    let cases = [
        (0, (0.1, 0.2, 0.3, 0.4), (0.6, 0.2, 0.2, 0.4)),
        (90, (0.4, 0.1, 0.4, 0.3), (0.2, 0.2, 0.4, 0.2)),
        (180, (0.6, 0.4, 0.3, 0.4), (0.2, 0.4, 0.2, 0.4)),
        (270, (0.2, 0.6, 0.4, 0.3), (0.4, 0.6, 0.4, 0.2)),
    ];
    for (rotation, shown, stored) in cases {
        // Turn the photo as ↻ does: the catalog first, then its images are invalidated.
        with_cat(&f.app, |c| c.set_photo_rotation(photo, rotation).unwrap());
        f.app.wired.images.update(cx, |s, cx| s.invalidate(photo, cx));
        cx.run_until_parked();
        // The turned pixels land before the faces' re-read (still queued on the manual
        // Runner): the last read's angle is not this picture's, so no box is drawn yet.
        let natural = if rotation % 180 == 0 { (400., 200.) } else { (200., 400.) };
        assert!(!f.present(&format!("faces-box-{a}"), cx), "{rotation}°: no box while the new tier loads");
        f.pool.finish(&JobKey::photo(photo, ImageKind::Preview), Ok(pixels(natural.0 as u32, natural.1 as u32)));
        cx.run_until_parked();
        assert!(!f.present(&format!("faces-box-{a}"), cx), "{rotation}°: no box until the faces are re-read for these pixels");
        work(&f.app, cx);

        let c = zoom.read_with(cx, |z, _| z.bounds().unwrap());
        let size = (f32::from(c.size.width), f32::from(c.size.height));
        let want = expected(bb(shown.0, shown.1, shown.2, shown.3), natural, size, ZoomView::FIT);
        assert!(f.present(&format!("faces-box-{a}"), cx), "{rotation}°: the box is drawn");
        assert_rect(f.bounds(&format!("faces-box-{a}"), cx), c.origin, want);

        // Draw (60%, 20%)–(80%, 60%) of the turned picture.
        f.click("faces-draw", cx);
        let (l, t, w, h) = ZoomView::FIT.placement(natural, size);
        let at = |x: f32, y: f32| c.origin + point(px(l + x * w), px(t + y * h));
        drag_in(&f, f.window(), at(0.6, 0.2), at(0.8, 0.6), cx);
        let drawn = drawn_faces(&f, photo);
        assert_eq!(drawn.len(), 1, "{rotation}°");
        assert!(close_bbox(drawn[0].bbox, stored), "{rotation}°: stored {:?}, want {stored:?}", drawn[0].bbox);
        let want = expected(bb(0.6, 0.2, 0.2, 0.4), natural, size, ZoomView::FIT);
        assert_rect(f.bounds(&format!("faces-box-{}", drawn[0].id), cx), c.origin, want);

        f.press("escape", cx); // the picker on the drawn face
        with_cat(&f.app, |c| c.conn().execute("DELETE FROM faces__faces WHERE source = 'drawn'", []).unwrap());
        state.update(cx, |s, cx| s.follow_photo(true, cx));
        work(&f.app, cx);
    }
}

// --- the People view (#130) -----------------------------------------------------------------

use crate::modules::faces::people::{People, Tab, PEOPLE_VIEW_ID, WAIT_FOR_MATCHING};
use crate::modules::faces::people_view::PeopleView;
use crate::shell::state::Surface;

/// Put `face` in cluster `cluster` (the clustering step's output).
fn cluster(app: &App, face: i64, cluster: i64) {
    with_cat(app, |c| {
        c.conn()
            .execute("INSERT OR IGNORE INTO faces__clusters (id, centroid, size, created_at) VALUES (?1, x'00', 0, 0)", [cluster])
            .unwrap();
        c.conn().execute("UPDATE faces__faces SET cluster_id = ?2 WHERE id = ?1", [face, cluster]).unwrap();
    });
}

fn confirm_as(app: &App, face: i64, tag: i64) {
    with_cat(app, |c| {
        c.conn().execute("UPDATE faces__faces SET state = 'confirmed', person_tag_id = ?2 WHERE id = ?1", [face, tag]).unwrap()
    });
}

fn suggest_at(app: &App, face: i64, tag: i64, confidence: f64) {
    with_cat(app, |c| {
        c.conn()
            .execute(
                &format!("UPDATE faces__faces SET person_tag_id = ?2, state = 'suggested', match_confidence = {confidence} WHERE id = ?1"),
                [face, tag],
            )
            .unwrap()
    });
}

impl Faces {
    /// Put the People view on the stage; its view and state.
    fn people(&self, cx: &mut TestAppContext) -> (Entity<PeopleView>, Entity<People>) {
        self.app.wired.shell.update(cx, |s, cx| s.show_module_view(PEOPLE_VIEW_ID, cx));
        work(&self.app, cx);
        let modules = self.app.wired.modules.clone();
        let view = cx
            .update_window(self.window(), |_, window, cx| {
                ModuleRegistry::main_view(&modules, PEOPLE_VIEW_ID, window, cx)
                    .expect("the People view")
                    .view
                    .downcast::<PeopleView>()
                    .expect("a PeopleView")
            })
            .unwrap();
        work(&self.app, cx);
        let people = view.read_with(cx, |v, _| v.people.clone());
        (view, people)
    }

    fn type_name(&self, view: &Entity<PeopleView>, text: &str, cx: &mut TestAppContext) {
        let input = view.read_with(cx, |v, _| v.name_input.clone());
        cx.update_window(self.window(), |_, window, cx| input.update(cx, |i, cx| i.set_value(text.to_string(), window, cx)))
            .unwrap();
        work(&self.app, cx);
    }
}

fn tag_of(app: &App, path: &str) -> Option<i64> {
    with_cat(app, |c| c.find_tag_id_by_path(path).unwrap())
}

/// The wall lists each named person with their counts; a card filters the Library by that
/// person and puts the Library on the stage. Bound to its catalog: after a switch the UI has
/// not heard of, the click is refused and the scope stays.
#[gpui_kit::test]
fn the_wall_filters_the_library_by_person(cx: &mut TestAppContext) {
    let f = open_faces(3, true, "people-wall", cx);
    let alice = with_cat(&f.app, |c| c.create_tag("People/Alice").unwrap());
    for &p in &f.ids[..2] {
        let face = add_face(&f.app, p, "[0.1,0.1,0.2,0.2]");
        confirm_as(&f.app, face, alice);
    }
    let (_view, people) = f.people(cx);
    assert_eq!(f.label(&format!("faces-person-{alice}"), cx).as_deref(), Some("Filter Library to People/Alice"));
    people.read_with(cx, |p, _| {
        let d = p.data.as_ref().unwrap();
        assert_eq!((d.people.len(), d.people[0].photo_count, d.people[0].face_count), (1, 2, 2));
    });

    f.click(&format!("faces-person-{alice}"), cx);
    f.app.wired.shell.read_with(cx, |s, _| {
        assert_eq!(s.library.scope().tag_id, Some(alice));
        assert_eq!(s.surface, Surface::Library);
    });

    // Back on the People view; another catalog is opened under it (no event yet).
    f.app.wired.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.clear_scope()));
    let _ = f.people(cx);
    let (other, _) = colliding_catalog(&f._dir, "other", 3);
    core_switch(&f.app, other);
    f.click(&format!("faces-person-{alice}"), cx);
    assert!(status(&f.app, cx).contains(CATALOG_CHANGED), "{}", status(&f.app, cx));
    f.app.wired.shell.read_with(cx, |s, _| assert_eq!(s.library.scope().tag_id, None, "the scope is untouched"));
}

/// Name a cluster from its card (Enter saves), merge two picked clusters into the same person
/// through the type-ahead, and split a cluster from its face sheet — naming one face apart
/// and ignoring another. Esc closes the dialog without naming.
#[gpui_kit::test]
fn clusters_are_named_merged_and_split(cx: &mut TestAppContext) {
    let f = open_faces(4, true, "people-clusters", cx);
    let face = |p: usize, x: f32| add_face(&f.app, f.ids[p], &format!("[{x},0.1,0.1,0.1]"));
    let (a1, a2) = (face(0, 0.1), face(1, 0.1));
    let b1 = face(2, 0.1);
    let (c1, c2) = (face(3, 0.1), face(3, 0.4));
    let (d1, d2, d3) = (face(0, 0.5), face(1, 0.5), face(2, 0.5));
    for (fc, cl) in [(a1, 10), (a2, 10), (b1, 11), (c1, 12), (c2, 12), (d1, 13), (d2, 13), (d3, 13)] {
        cluster(&f.app, fc, cl);
    }
    let (view, people) = f.people(cx);
    f.click("faces-tab-clusters", cx);
    assert!(f.present("faces-cluster-10", cx));

    // Esc cancels.
    f.click("faces-cluster-10", cx);
    assert!(f.present("faces-name-dialog", cx));
    f.press("escape", cx);
    assert!(!f.present("faces-name-dialog", cx), "Esc closes the dialog");
    assert_eq!(face_row(&f.app, a1).0, "unassigned");

    // Name: the card, a name, Enter.
    f.click("faces-cluster-10", cx);
    f.type_name(&view, "Ann", cx);
    f.press("enter", cx);
    let ann = tag_of(&f.app, "People/Ann").expect("created under the people root");
    assert_eq!(face_row(&f.app, a1), ("confirmed".into(), Some(ann)));
    assert_eq!(face_row(&f.app, a2), ("confirmed".into(), Some(ann)));
    assert!(with_cat(&f.app, |c| c.get_photo_tags(f.ids[1]).unwrap().iter().any(|t| t.id == ann)), "the photo is tagged");
    assert!(status(&f.app, cx).starts_with("Named 2 faces on 2 photos as People/Ann."), "{}", status(&f.app, cx));
    assert!(!f.present("faces-cluster-10", cx), "the named cluster is gone after the re-read");

    // Merge: pick 11 and 12, "Name 2 together…", the type-ahead's existing person.
    f.click("faces-cluster-pick-11", cx);
    f.click("faces-cluster-pick-12", cx);
    people.read_with(cx, |p, _| assert_eq!(p.picked_clusters, vec![11, 12]));
    f.click("faces-name-together", cx);
    f.type_name(&view, "ann", cx);
    assert_eq!(f.label("faces-name-suggestion-0", cx).as_deref(), Some("People/Ann"));
    f.click("faces-name-suggestion-0", cx);
    f.click("faces-name-confirm", cx);
    for fc in [b1, c1, c2] {
        assert_eq!(face_row(&f.app, fc), ("confirmed".into(), Some(ann)), "merged into Ann");
    }
    people.read_with(cx, |p, _| {
        assert!(p.picked_clusters.is_empty() && p.naming.is_none());
        assert_eq!(p.data.as_ref().unwrap().clusters.iter().map(|c| c.cluster_id).collect::<Vec<_>>(), vec![13]);
    });

    // Split: the sheet of 13; pick d1, name it Bob; pick d2, ignore it; d3 stays.
    f.click("faces-cluster-faces-13", cx);
    assert!(f.present(&format!("faces-face-{d3}"), cx));
    f.click(&format!("faces-face-{d1}"), cx);
    f.click("faces-sheet-name", cx);
    f.type_name(&view, "Bob", cx);
    f.click("faces-name-confirm", cx);
    let bob = tag_of(&f.app, "People/Bob").unwrap();
    assert_eq!(face_row(&f.app, d1), ("confirmed".into(), Some(bob)));
    assert_eq!(face_row(&f.app, d2).0, "unassigned", "a face not picked is not named");
    assert!(!f.present(&format!("faces-face-{d1}"), cx), "the sheet re-read without the named face");
    f.click(&format!("faces-face-{d2}"), cx);
    f.click("faces-sheet-ignore", cx);
    assert_eq!(face_row(&f.app, d2).0, "ignored");
    assert_eq!(face_row(&f.app, d3).0, "unassigned");
    people.read_with(cx, |p, _| {
        let sheet = p.sheet.as_ref().expect("the sheet stays open");
        assert_eq!(sheet.faces.as_ref().unwrap().iter().map(|f| f.face_id).collect::<Vec<_>>(), vec![d3]);
    });
}

/// The naming dialog is bound to the catalog it opened on: confirmed after another catalog
/// was opened under it (no event yet), it is refused and the new catalog — with colliding
/// ids — is untouched; the event then closes the dialog and drops the data.
#[gpui_kit::test]
fn naming_is_bound_to_the_catalog_it_was_opened_on(cx: &mut TestAppContext) {
    let f = open_faces(1, true, "people-switch", cx);
    let a = add_face(&f.app, f.ids[0], "[0.1,0.1,0.2,0.2]");
    cluster(&f.app, a, 5);
    let (view, people) = f.people(cx);
    f.click("faces-tab-clusters", cx);
    f.click("faces-cluster-5", cx);

    let (other, ids) = colliding_catalog(&f._dir, "other", 1);
    store::ensure_schema(other.conn()).unwrap();
    let b = store::insert_face(other.conn(), ids[0], "[0.1,0.1,0.2,0.2]", "[]", 0.99, None, "detect", 0).unwrap();
    assert_eq!(a, b, "colliding ids");
    core_switch(&f.app, other);
    with_cat(&f.app, |c| {
        c.conn().execute("INSERT INTO faces__clusters (id, centroid, size, created_at) VALUES (5, x'00', 1, 0)", []).unwrap();
        c.conn().execute("UPDATE faces__faces SET cluster_id = 5 WHERE id = ?1", [b]).unwrap();
    });
    f.type_name(&view, "Eve", cx);
    f.click("faces-name-confirm", cx);
    assert!(f.present("faces-name-error", cx));
    people.read_with(cx, |p, _| assert!(p.naming.as_ref().unwrap().error.as_deref().unwrap().contains(CATALOG_CHANGED)));
    assert_eq!(face_row(&f.app, b).0, "unassigned", "the open catalog's face is untouched");
    assert_eq!(tag_of(&f.app, "People/Eve"), None);

    deliver_switch(&f.app, cx);
    work(&f.app, cx);
    people.read_with(cx, |p, _| assert!(p.naming.is_none() && p.data.is_none() && p.sheet.is_none()));
}

/// The queue: "Confirm all ≥ 80%" confirms exactly the suggestions at or above it, ✕
/// rejects one (remembered), and a ✓ on a suggestion re-matched since the read leaves it
/// alone and says so.
#[gpui_kit::test]
fn the_review_queue_confirms_above_the_threshold_and_rejects(cx: &mut TestAppContext) {
    let f = open_faces(4, true, "people-review", cx);
    let alice = with_cat(&f.app, |c| c.create_tag("People/Alice").unwrap());
    let bob = with_cat(&f.app, |c| c.create_tag("People/Bob").unwrap());
    let s: Vec<i64> = f.ids.iter().map(|&p| add_face(&f.app, p, "[0.1,0.1,0.2,0.2]")).collect();
    suggest_at(&f.app, s[0], alice, 0.95);
    suggest_at(&f.app, s[1], bob, 0.8);
    suggest_at(&f.app, s[2], alice, 0.795); // shown as 80%, but below 0.8
    suggest_at(&f.app, s[3], bob, 0.4);
    let (_view, people) = f.people(cx);
    f.click("faces-tab-suggestions", cx);
    assert_eq!(f.label("faces-threshold", cx).as_deref(), Some("80%"));
    assert!(f.present(&format!("faces-sugg-{}", s[3]), cx));

    f.click("faces-confirm-all", cx);
    assert_eq!(face_row(&f.app, s[0]), ("confirmed".into(), Some(alice)));
    assert_eq!(face_row(&f.app, s[1]), ("confirmed".into(), Some(bob)), "80% is at the threshold");
    assert_eq!(face_row(&f.app, s[2]).0, "suggested", "0.795 is not ≥ 0.8, whatever its label rounds to");
    assert_eq!(status(&f.app, cx), "Suggestions: 2 confirmed.");

    f.click(&format!("faces-sugg-reject-{}", s[2]), cx);
    assert_eq!(face_row(&f.app, s[2]), ("unassigned".into(), None));
    let remembered: i64 = with_cat(&f.app, |c| {
        c.conn().query_row("SELECT COUNT(*) FROM faces__rejections WHERE face_id = ?1", [s[2]], |r| r.get(0)).unwrap()
    });
    assert_eq!(remembered, 1);

    // Re-matched to Alice behind the queue's back: ✓ on the row that shows Bob does nothing.
    suggest_at(&f.app, s[3], alice, 0.6);
    f.click(&format!("faces-sugg-confirm-{}", s[3]), cx);
    assert_eq!(face_row(&f.app, s[3]), ("suggested".into(), Some(alice)));
    assert!(status(&f.app, cx).contains("changed since the list was read"), "{}", status(&f.app, cx));
    people.read_with(cx, |p, _| assert_eq!(p.data.as_ref().unwrap().suggestions[0].person_tag_id, alice, "re-read"));
}

/// While a matching run regroups the faces, the view's writes wait (the core's matcher does
/// not re-check a face's state before writing a suggestion); its end re-reads the view.
#[gpui_kit::test]
fn writes_wait_for_a_running_match_and_its_end_rereads(cx: &mut TestAppContext) {
    let f = open_faces(1, true, "people-matching", cx);
    let a = add_face(&f.app, f.ids[0], "[0.1,0.1,0.2,0.2]");
    cluster(&f.app, a, 7);
    let (_view, people) = f.people(cx);
    f.click("faces-tab-clusters", cx);
    let run = core_faces::begin_match_job(&f.app.state, None).unwrap();
    send_match_progress(&f.app, run.job, 1, 2, cx);
    work(&f.app, cx);
    people.update(cx, |p, cx| p.name_cluster(7, cx));
    work(&f.app, cx);
    people.read_with(cx, |p, _| assert!(p.naming.is_none()));
    assert_eq!(status(&f.app, cx), WAIT_FOR_MATCHING);

    // The run regroups: a new cluster appears with its end.
    let b = add_face(&f.app, f.ids[0], "[0.5,0.1,0.2,0.2]");
    cluster(&f.app, b, 8);
    run.slot.clear();
    send_match_done(&f.app, run.job, cx);
    work(&f.app, cx);
    assert!(f.present("faces-cluster-8", cx), "the end re-read the clusters");
    people.update(cx, |p, cx| p.name_cluster(8, cx));
    people.read_with(cx, |p, _| assert!(p.naming.is_some()));
}

/// #152, rv151 L4: a card's avatar is a face cut by the face's box, which is in the
/// original's frame. A cover version's thumbnail (possibly cropped) is not cut: the face is
/// cut from the photo's preview instead, asked for (and held) once the thumbnail turns out to
/// be a cover render, the circle empty until it lands. A plain thumbnail is cut directly.
#[gpui_kit::test]
fn an_avatar_is_not_cut_from_a_cover_thumbnail(cx: &mut TestAppContext) {
    let f = open_faces(2, true, "people-cover-avatar", cx);
    let alice = with_cat(&f.app, |c| c.create_tag("People/Alice").unwrap());
    let photo = f.ids[0];
    let face = add_face(&f.app, photo, "[0.1,0.1,0.2,0.2]");
    confirm_as(&f.app, face, alice);
    let (view, people) = f.people(cx);
    assert_eq!(people.read_with(cx, |p, _| p.data.as_ref().unwrap().people[0].avatar_photo_id), photo);
    let thumb = JobKey::photo(photo, ImageKind::Thumb);
    let preview = JobKey::photo(photo, ImageKind::Preview);
    let images = f.app.wired.images.clone();
    assert!(!images.read_with(cx, |s, _| s.is_pending(photo, ImageKind::Preview)), "no preview for a plain card");
    f.pool.finish(&thumb, Ok(cover_pixels(400, 200)));
    work(&f.app, cx);
    assert!(f.present(&format!("faces-person-{alice}"), cx), "the card is drawn");
    assert!(!f.present(&format!("faces-avatar-{photo}"), cx), "no face cut from the cover's thumbnail");
    assert!(images.read_with(cx, |s, _| s.is_pending(photo, ImageKind::Preview)), "the preview is asked for");
    assert!(view.read_with(cx, |v, cx| v.held(cx)).contains(&photo));

    // The preview (the original's frame; portrait here, to tell it from the thumbnail) lands:
    // the face is cut from it.
    f.pool.finish(&preview, Ok(pixels(200, 400)));
    work(&f.app, cx);
    let cut = f.bounds(&format!("faces-avatar-{photo}"), cx);
    let aspect = f32::from(cut.size.width) / f32::from(cut.size.height);
    assert!((aspect - 0.5).abs() < 0.01, "cut from the preview (aspect {aspect})");

    // The cover taken off: the plain thumbnail, cut by the box.
    let images = f.app.wired.images.clone();
    images.update(cx, |s, cx| s.invalidate(photo, cx));
    work(&f.app, cx);
    f.pool.finish(&thumb, Ok(pixels(400, 200)));
    work(&f.app, cx);
    assert!(f.present(&format!("faces-avatar-{photo}"), cx), "the face is cut from the original's thumbnail");
}

/// The lists are virtualised and the avatars come from the image layer under the view's
/// claim: a long cluster list holds only the visible rows' thumbnails (plus the overscan),
/// scrolling moves the claim, and leaving the view releases it.
#[gpui_kit::test]
fn avatars_are_claimed_for_the_visible_rows_and_released(cx: &mut TestAppContext) {
    let f = open_faces(400, true, "people-avatars", cx);
    for (i, &p) in f.ids.iter().enumerate() {
        let face = add_face(&f.app, p, "[0.1,0.1,0.2,0.2]");
        cluster(&f.app, face, 1000 + i as i64);
    }
    let (view, people) = f.people(cx);
    people.update(cx, |p, cx| p.set_tab(Tab::Clusters, cx));
    work(&f.app, cx);
    let held = view.read_with(cx, |v, cx| v.held(cx));
    assert!(!held.is_empty() && held.len() < 200, "only the rows on screen and the overscan: {}", held.len());
    // The first card's photo is held and asked of the pool.
    let first = people.read_with(cx, |p, _| p.data.as_ref().unwrap().clusters[0].avatar_photo_id);
    assert!(held.contains(&first));
    assert!(f.app.wired.images.read_with(cx, |s, _| s.is_pending(first, ImageKind::Thumb)));

    cx.update_window(f.window(), |_, window, cx| {
        window.render_frame(cx);
        window.scroll("faces-people-scroll", ScrollDelta::Pixels(point(px(0.), px(-4000.))), cx)
    })
    .unwrap();
    work(&f.app, cx);
    let moved = view.read_with(cx, |v, cx| v.held(cx));
    assert!(!moved.contains(&first), "scrolled away: released");
    assert!(!f.app.wired.images.read_with(cx, |s, _| s.is_pending(first, ImageKind::Thumb)), "its queued render is cancelled");

    f.app.wired.shell.update(cx, |s, cx| s.show_library(cx));
    work(&f.app, cx);
    assert!(view.read_with(cx, |v, cx| v.held(cx)).is_empty(), "off stage: nothing held");
}
