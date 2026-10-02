//! Headless tests of the Faces module's first half (#129) through the real wiring: the
//! settings panel, the indexing job's ownership (start, progress, done, Cancel, a catalog
//! switch, re-attaching), the inspector's verbs and the person picker, and the loupe
//! overlay's geometry (orientation, zoom, pan) with its keys and draw mode.
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
use crate::modules::faces::state::{FacesBackend, FacesBackendGlobal, FacesState, IndexPhase};
use crate::modules::faces::FACES_MODULE_ID;
use crate::modules::{ModuleRegistry, PanelSlot};
use crate::shell::state::InspectorTab;
use crate::storage::Runner;
use chairphoto_core::app::faces::{self as core_faces, FaceBboxJson};
use chairphoto_core::app::{
    CatalogIdentity, FacesIndexDone, FacesJobStatus, FacesMatchDone, FacesMatchProgressEvent, FacesProgressEvent,
    JobClaim, CATALOG_CHANGED,
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

    f.app.state.send(CoreEvent::FacesMatchProgress(FacesMatchProgressEvent { done: 1, total: 5, phase: "seed", job: 1 }));
    cx.run_until_parked();
    state.read_with(cx, |s, _| assert!(s.index.match_busy && !s.can_index()));
    f.app.state.send(CoreEvent::FacesMatchDone(FacesMatchDone { ok: true, outcome: None, aborted: false, job: 1, error: None }));
    work(&f.app, cx);
    state.read_with(cx, |s, _| assert!(!s.index.match_busy && s.can_index()));
}

// --- the inspector --------------------------------------------------------------------------

/// The inspector lists the active photo's faces; ✓ confirms (tagging the photo), the picker
/// assigns an existing person by Enter and creates a new one under the people root.
#[gpui_kit::test]
fn the_inspector_confirms_and_names_faces(cx: &mut TestAppContext) {
    let f = open_faces(2, true, "faces-inspector", cx);
    let photo = f.ids[0];
    let alice = with_cat(&f.app, |c| c.create_tag("People/Alice").unwrap());
    with_cat(&f.app, |c| c.set_setting("faces.people_root", "People").unwrap());
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
