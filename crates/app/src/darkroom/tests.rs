//! Headless tests of the Darkroom through the real wiring (`start` → `wire` → the main
//! window): the rail's Develop opens it on the active photo; a control changes the record and
//! the render requests; the settle; the autosave into the catalog; and what a catalog switch
//! does to it. Frames come from a hand-driven pool (`image_tests::FakePool`), blocking work
//! runs on `Runner::manual`, timers on GPUI's fake clock.

use super::session::AUTOSAVE_QUIET;
use super::view::Control;
use super::*;
use crate::image_tests::{pixels, FakePool};
use crate::shell::state::Surface;
use crate::storage::Runner;
use crate::tests::{click, colliding_catalog, core_switch, open_catalog_with_photos, start, App, TempDir};
use chairphoto_core::app::{CoreEvent, EventSink as _, CATALOG_CHANGED};
use chairphoto_core::catalog::Catalog;
use chairphoto_core::develop::session::DevelopSourceEvent;
use chairphoto_core::develop_source::DevelopSource;
use chairphoto_core::image_pool::{EditJob, JobKey};
use chairphoto_core::plugins::edit::SourceToken;
use chairphoto_model::darkroom::controls::ToneKey;
use chairphoto_model::darkroom::filmstrip::KeyTarget;
use chairphoto_model::editing::parse_edit;
use chairphoto_model::library::session::SelectMods;
use gpui_kit::component::slider::{SliderEvent, SliderValue};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, Entity, TestAppContext};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

struct Rig {
    app: App,
    dir: TempDir,
    ids: Vec<i64>,
    pool: Arc<FakePool>,
}

impl Rig {
    fn view(&self, cx: &mut TestAppContext) -> Entity<DarkroomView> {
        self.app.wired.root.as_ref().expect("the main window opened").read_with(cx, |r, _| r.darkroom.clone())
    }

    fn darkroom(&self, cx: &mut TestAppContext) -> Entity<Darkroom> {
        let view = self.view(cx);
        view.read_with(cx, |v, _| v.darkroom().clone())
    }

    fn catalog<T>(&self, f: impl FnOnce(&Catalog) -> T) -> T {
        f(self.app.state.catalog.lock().unwrap().as_ref().unwrap())
    }

    /// The working record of the open photo, as JSON.
    fn working(&self, cx: &mut TestAppContext) -> Value {
        let d = self.darkroom(cx);
        d.read_with(cx, |d, _| serde_json::from_str(&d.open.as_ref().expect("a photo is open").working.to_json()).unwrap())
    }

    fn open_photo(&self, cx: &mut TestAppContext) -> Option<i64> {
        let d = self.darkroom(cx);
        d.read_with(cx, |d, _| d.open.as_ref().map(|o| o.photo.id))
    }

    /// A slider moved, through its real `SliderEvent` subscription.
    fn slide(&self, control: Control, v: f32, cx: &mut TestAppContext) {
        let view = self.view(cx);
        let slider = view.read_with(cx, |v, _| v.slider(control).clone());
        slider.update(cx, |_, cx| cx.emit(SliderEvent::Change(SliderValue::from(v))));
        cx.run_until_parked();
    }

    fn edit_jobs(&self) -> Vec<EditJob> {
        self.pool
            .batches
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .filter_map(|k| match k {
                JobKey::Edit(job) => Some(job.clone()),
                _ => None,
            })
            .collect()
    }

    fn last_edit_job(&self) -> EditJob {
        self.edit_jobs().pop().expect("an edit job")
    }

    fn surface(&self, cx: &mut TestAppContext) -> Surface {
        self.app.wired.shell.read_with(cx, |s, _| s.surface.clone())
    }

    fn render(&self, cx: &mut TestAppContext) {
        cx.update_window(self.app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
        cx.run_until_parked();
    }

    fn present(&self, id: &str, cx: &mut TestAppContext) -> bool {
        let id = gpui_kit::SharedString::from(id.to_string());
        cx.update_window(self.app.window(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .unwrap()
    }
}

/// Run every queued worker job (and what they queue), letting the UI take each result.
fn work(cx: &mut TestAppContext) {
    loop {
        let ran = cx.update(|cx| Runner::get(cx).run_pending());
        cx.run_until_parked();
        if ran == 0 {
            return;
        }
    }
}

fn advance(cx: &mut TestAppContext, d: Duration) {
    cx.executor().advance_clock(d);
    cx.run_until_parked();
}

/// A catalog of `n` photos, the first active, the Darkroom opened on it with the rail's
/// Develop, and its first reads answered.
fn rig(tag: &str, n: usize, cx: &mut TestAppContext) -> Rig {
    let dir = TempDir::new(tag);
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, n, cx);
    let pool = Arc::new(FakePool::default());
    let rig = Rig { app, dir, ids, pool };
    let luts = rig.dir.0.join("luts");
    std::fs::create_dir_all(&luts).unwrap();
    std::fs::write(luts.join("film.cube"), "").unwrap();
    let d = rig.darkroom(cx);
    let pool: Arc<dyn crate::image_store::Submit> = rig.pool.clone();
    d.update(cx, |d, _| d.set_backends(pool, Arc::new(move || Ok(luts.clone()))));
    let first = rig.ids[0];
    rig.app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select(first, SelectMods::default())));
    cx.run_until_parked();
    click(&rig.app, "rail-develop", cx);
    work(cx);
    rig
}

#[gpui_kit::test]
fn develop_opens_on_the_active_photo_bound_to_its_catalog(cx: &mut TestAppContext) {
    let rig = rig("dk-open", 2, cx);
    assert_eq!(rig.surface(cx), Surface::Develop);
    assert_eq!(rig.open_photo(cx), Some(rig.ids[0]));
    let d = rig.darkroom(cx);
    let (loaded, luts, from) = d.read_with(cx, |d, _| {
        let o = d.open.as_ref().unwrap();
        (o.loaded, d.luts.clone(), o.from)
    });
    assert!(loaded, "the version was resolved");
    assert_eq!(luts, ["film.cube"], "the LUT folder was listed");
    assert_eq!(Some(from), rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()));
    // The open renders the record at once: a fast frame for this photo and catalog.
    let job = rig.last_edit_job();
    assert_eq!((job.photo_id, job.max_edge, job.source.clone()), (rig.ids[0], FAST_EDGE, SourceToken::Preview));
    assert_eq!(job.catalog_epoch, rig.app.wired.model.read_with(cx, |m, _| m.catalog_epoch));
    // Nothing selected: nothing to develop.
    rig.app.wired.shell.update(cx, |s, cx| {
        s.show_library(cx);
        s.clear_selection(cx);
        s.open_develop(cx);
    });
    cx.run_until_parked();
    assert_eq!(rig.surface(cx), Surface::Library);
}

/// The control → record → render request flow, and the settle: a slider writes the record,
/// a fast frame of that record goes out at once, the full frame only SETTLE after the last
/// change, and the frame lands on the stage.
#[gpui_kit::test]
fn a_slider_writes_the_record_and_asks_for_fast_then_full_frames(cx: &mut TestAppContext) {
    let rig = rig("dk-slider", 1, cx);
    advance(cx, SETTLE * 2); // the open's own settle
    let before = rig.edit_jobs().len();
    rig.slide(Control::Tone(ToneKey::Ev), 0.5000000074505806_f32, cx);
    assert_eq!(rig.working(cx)["tone"]["ev"], json!(0.5), "snapped as the range input reports it");
    let jobs = rig.edit_jobs();
    assert_eq!(jobs.len(), before + 1, "one fast frame at once");
    let fast = jobs.last().unwrap();
    assert_eq!(fast.max_edge, FAST_EDGE);
    assert_eq!(parse_edit(Some(&fast.edit_json)).tone.value().unwrap().ev.get(), Some(0.5));

    advance(cx, SETTLE - Duration::from_millis(1));
    assert_eq!(rig.edit_jobs().len(), before + 1, "no full frame before the settle");
    advance(cx, Duration::from_millis(1));
    let full = rig.last_edit_job();
    assert_eq!((full.max_edge, full.edit_json.clone()), (FULL_EDGE, fast.edit_json.clone()));

    // The full frame lands and is drawn.
    let key = rig.pool.last_batch()[0].clone();
    rig.pool.finish(&key, Ok(pixels(6, 4)));
    cx.run_until_parked();
    let view = rig.view(cx);
    let stage = rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().stage.clone());
    assert_eq!(stage.read_with(cx, |s, _| s.frame().map(|f| f.tier)), Some(FrameTier::Full));
    assert!(rig.present("dk-frame", cx));
    // Its timing sample was stamped.
    let summary = stage.read_with(cx, |s, _| s.timing_summary());
    assert!(summary.latency_ms.p50 >= 0.0, "{summary:?}");

    // A double-click resets, and the slider follows the record.
    view.update(cx, |v, cx| v.reset_control(Control::Tone(ToneKey::Ev), cx));
    cx.run_until_parked();
    assert_eq!(rig.working(cx)["tone"]["ev"], json!(0));
    rig.render(cx);
    let slider = view.read_with(cx, |v, _| v.slider(Control::Tone(ToneKey::Ev)).clone());
    assert_eq!(slider.read_with(cx, |s, _| s.value().start()), 0.0);
}

/// The autosave: AUTOSAVE_QUIET after the last change, one history step into a new
/// "Version 1" through core; the same control again within the amend window amends it; the
/// shell's active version follows.
#[gpui_kit::test]
fn changes_autosave_as_history_steps_after_the_quiet(cx: &mut TestAppContext) {
    let rig = rig("dk-autosave", 1, cx);
    let photo = rig.ids[0];
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    advance(cx, AUTOSAVE_QUIET - Duration::from_millis(1));
    work(cx); // the zone masses, measured after the settle
    assert!(rig.catalog(|c| c.list_versions(photo).unwrap()).is_empty(), "nothing saved before the quiet");
    advance(cx, Duration::from_millis(1));
    work(cx);
    let versions = rig.catalog(|c| c.list_versions(photo).unwrap());
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].name, "Version 1");
    assert_eq!(serde_json::from_str::<Value>(&versions[0].edit_json).unwrap()["tone"]["ev"], json!(0.5));
    let history = rig.catalog(|c| c.version_history(versions[0].id).unwrap());
    assert_eq!(history.steps.last().unwrap().label, "Exposure +0.50");
    let active = rig.app.wired.shell.read_with(cx, |s, _| s.active_version().cloned());
    assert_eq!(active.map(|v| (v.id, v.edit_json)), Some((versions[0].id, versions[0].edit_json.clone())));

    // Same control within the window: the step is amended, not added.
    let steps = history.steps.len();
    rig.slide(Control::Tone(ToneKey::Ev), 0.75, cx);
    advance(cx, AUTOSAVE_QUIET);
    work(cx);
    let history = rig.catalog(|c| c.version_history(versions[0].id).unwrap());
    assert_eq!(history.steps.len(), steps, "amended");
    assert_eq!(history.steps.last().unwrap().label, "Exposure +0.75");
    // Another control: a new step.
    rig.slide(Control::Effect(chairphoto_model::darkroom::controls::EffectKey::Fade), 0.2, cx);
    advance(cx, AUTOSAVE_QUIET);
    work(cx);
    assert_eq!(rig.catalog(|c| c.version_history(versions[0].id).unwrap()).steps.len(), steps + 1);
    // Reset is a named step.
    rig.darkroom(cx).update(cx, |d, cx| d.reset(cx));
    advance(cx, AUTOSAVE_QUIET);
    work(cx);
    let h = rig.catalog(|c| c.version_history(versions[0].id).unwrap());
    assert_eq!(h.steps.last().unwrap().label, "Reset");
    assert_eq!(rig.catalog(|c| c.list_versions(photo).unwrap())[0].edit_json, "{}");
}

/// Leaving Develop saves what is pending at once, releases the session and re-reads the
/// Library's rows.
#[gpui_kit::test]
fn leaving_saves_what_is_pending(cx: &mut TestAppContext) {
    let rig = rig("dk-leave", 1, cx);
    let view = rig.view(cx);
    let darkroom_focus = view.read_with(cx, |v, _| v.focus_handle().clone());
    let focused = |h: &gpui_kit::FocusHandle, cx: &mut TestAppContext| {
        cx.update_window(rig.app.window(), |_, window, _| h.is_focused(window)).unwrap()
    };
    assert!(focused(&darkroom_focus, cx), "Develop takes the keys");
    rig.slide(Control::Tone(ToneKey::Contrast), 0.3, cx);
    rig.app.wired.shell.update(cx, |s, cx| s.show_library(cx));
    cx.run_until_parked();
    assert_eq!(rig.open_photo(cx), None);
    let grid = rig.app.wired.root.as_ref().unwrap().read_with(cx, |r, cx| r.library().read(cx).focus_handle().clone());
    assert!(focused(&grid, cx), "the grid has the keys again");
    work(cx);
    let versions = rig.catalog(|c| c.list_versions(rig.ids[0]).unwrap());
    assert_eq!(versions.len(), 1, "saved on the way out, without waiting for the quiet");
    assert_eq!(serde_json::from_str::<Value>(&versions[0].edit_json).unwrap()["tone"]["contrast"], json!(0.3));
}

/// **Forced interleaving.** A change made while an autosave commit is on the worker, then
/// the photo is left — by a step (→) or ← Library — with that commit still held running: the
/// newer record is committed after it, into the version it created, for the photo it was
/// made on; the next photo's version is untouched.
fn a_change_during_a_running_commit_survives(leave: bool, cx: &mut TestAppContext) {
    let rig = rig(if leave { "dk-chain-leave" } else { "dk-chain-step" }, 2, cx);
    let order = rig.app.wired.shell.read_with(cx, |s, _| s.library.photo_ids());
    let photo = order[0];
    assert_eq!(rig.open_photo(cx), Some(photo));
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    advance(cx, AUTOSAVE_QUIET);
    // The quiet started the commit; the worker has not run it (Runner::manual holds it).
    assert!(rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().saving), "the commit is running");
    assert!(rig.catalog(|c| c.list_versions(photo).unwrap()).is_empty());
    rig.slide(Control::Tone(ToneKey::Contrast), 0.3, cx);
    if leave {
        rig.app.wired.shell.update(cx, |s, cx| s.show_library(cx));
        cx.run_until_parked();
        assert_eq!(rig.surface(cx), Surface::Library);
    } else {
        assert!(rig.view(cx).update(cx, |v, cx| v.step(1, None, cx)));
        cx.run_until_parked();
        assert_eq!(rig.open_photo(cx), Some(order[1]));
    }
    work(cx);
    let versions = rig.catalog(|c| c.list_versions(photo).unwrap());
    assert_eq!(versions.len(), 1, "one version: the follow-up commit wrote the one the first created");
    assert_eq!(versions[0].name, "Version 1");
    let saved: Value = serde_json::from_str(&versions[0].edit_json).unwrap();
    assert_eq!(
        (saved["tone"]["ev"].clone(), saved["tone"]["contrast"].clone()),
        (json!(0.5), json!(0.3)),
        "the latest record wins"
    );
    let steps = rig.catalog(|c| c.version_history(versions[0].id).unwrap()).steps;
    let labels: Vec<&str> = steps.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(labels[labels.len() - 2..], ["Exposure +0.50", "Contrast +0.30"], "{labels:?}");
    assert!(rig.catalog(|c| c.list_versions(order[1]).unwrap()).is_empty(), "nothing written to the next photo");
    if !leave {
        assert_eq!(rig.working(cx), json!({}), "the next photo keeps its own record");
    }
    assert_eq!(rig.darkroom(cx).read_with(cx, |d, _| d.error.clone()), None);
}

#[gpui_kit::test]
fn a_step_during_a_running_commit_saves_the_newer_change_after_it(cx: &mut TestAppContext) {
    a_change_during_a_running_commit_survives(false, cx);
}

#[gpui_kit::test]
fn leaving_during_a_running_commit_saves_the_newer_change_after_it(cx: &mut TestAppContext) {
    a_change_during_a_running_commit_survives(true, cx);
}

/// Make every history write fail (a full disk, say) until the test drops the trigger.
fn fail_history_writes(rig: &Rig) {
    rig.catalog(|c| {
        c.conn()
            .execute_batch(
                "CREATE TRIGGER test_fail_history BEFORE INSERT ON photo_version_history
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            )
            .unwrap()
    });
}

/// A left photo's commit that fails after the view moved on is reported on the status line
/// (the Library shows no Darkroom banner), naming the photo — not dropped with the view.
#[gpui_kit::test]
fn a_left_photos_failed_commit_is_reported_on_the_status_line(cx: &mut TestAppContext) {
    let rig = rig("dk-chain-fail", 1, cx);
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    advance(cx, AUTOSAVE_QUIET);
    assert!(rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().saving), "the commit is running");
    fail_history_writes(&rig);
    rig.app.wired.shell.update(cx, |s, cx| s.show_library(cx));
    cx.run_until_parked();
    work(cx);
    let line = crate::tests::status(&rig.app, cx);
    assert!(line.starts_with("Autosave failed for ") && line.contains("disk full"), "{line}");
}

/// "Version N" is created at most once: the first commit creates the version and its write
/// fails; the retry writes into that version — no second, empty "Version 2" — and the shell
/// is given it.
#[gpui_kit::test]
fn a_failed_first_write_keeps_the_created_version_for_the_retry(cx: &mut TestAppContext) {
    let rig = rig("dk-create-once", 1, cx);
    let photo = rig.ids[0];
    fail_history_writes(&rig);
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    advance(cx, AUTOSAVE_QUIET);
    work(cx);
    let error = rig.darkroom(cx).read_with(cx, |d, _| d.error.clone()).unwrap_or_default();
    assert!(error.starts_with("Autosave failed: ") && error.contains("disk full"), "{error}");
    let created = rig.catalog(|c| c.list_versions(photo).unwrap());
    assert_eq!(created.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), ["Version 1"], "created, not written");

    rig.catalog(|c| c.conn().execute_batch("DROP TRIGGER test_fail_history;").unwrap());
    rig.darkroom(cx).update(cx, |d, cx| d.flush(cx)); // Ctrl+S, a step or leaving
    work(cx);
    let versions = rig.catalog(|c| c.list_versions(photo).unwrap());
    assert_eq!(versions.len(), 1, "the retry wrote the version the first commit created");
    assert_eq!(versions[0].id, created[0].id);
    assert_eq!(serde_json::from_str::<Value>(&versions[0].edit_json).unwrap()["tone"]["ev"], json!(0.5));
    let active = rig.app.wired.shell.read_with(cx, |s, _| s.active_version().cloned());
    assert_eq!(active.map(|v| (v.id, v.name)), Some((created[0].id, "Version 1".to_string())));
    // The next version made here would be "Version 2", counting the one created.
    assert_eq!(rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().versions_len), 1);
}

/// The filmstrip: → steps to the next photo in the Library's order (saving first), arrows
/// that belong to a slider are left alone, and the ends do not wrap.
#[gpui_kit::test]
fn the_filmstrip_and_arrows_step_through_the_library_saving_first(cx: &mut TestAppContext) {
    let rig = rig("dk-strip", 3, cx);
    let order = rig.app.wired.shell.read_with(cx, |s, _| s.library.photo_ids());
    assert_eq!(rig.open_photo(cx), Some(order[0]));
    assert!(rig.present(&format!("dk-strip-{}", order[1]), cx), "the strip lists the Library's photos");
    rig.slide(Control::Tone(ToneKey::Ev), 1.0, cx);
    let view = rig.view(cx);
    assert!(!view.update(cx, |v, cx| v.step(1, Some(KeyTarget::Input), cx)), "a slider keeps its arrows");
    // The real key path: the Darkroom has focus.
    cx.update_window(rig.app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.press("right", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(rig.open_photo(cx), Some(order[1]));
    work(cx);
    assert_eq!(rig.catalog(|c| c.list_versions(order[0]).unwrap()).len(), 1, "the photo left was saved");
    assert_eq!(rig.working(cx), json!({}), "the next photo starts from its own record");
    assert!(view.update(cx, |v, cx| v.step(1, None, cx)));
    cx.run_until_parked();
    assert_eq!(rig.open_photo(cx), Some(order[2]));
    assert!(!view.update(cx, |v, cx| v.step(1, None, cx)), "no wrap at the end");
}

/// The RAW arrives (`develop:source` with a token): the stage moves to the working image,
/// Kelvin white balance appears, saves are stamped engine 2, and the clipping layer can be
/// asked for. An event for another photo changes nothing.
#[gpui_kit::test]
fn the_raw_source_moves_the_stage_and_stamps_the_record(cx: &mut TestAppContext) {
    let rig = rig("dk-source", 2, cx);
    let photo = rig.ids[0];
    let raw = |photo_id: i64, generation: u64| DevelopSource::Raw {
        camera: "Sony ILCE-7RM5".into(),
        megapixels: 61.0,
        bits: 16,
        decoder: "0.22".into(),
        token: Some(format!("w:{photo_id}:{generation}")),
        camera_ev: Some(-0.5),
        as_shot_wb: Some([5300.0, 2.0]),
        lens: None,
    };
    let send = |photo_id: i64, source: DevelopSource, cx: &mut TestAppContext| {
        rig.app.state.send(CoreEvent::DevelopSource(DevelopSourceEvent { photo_id, job: 7, source }));
        cx.run_until_parked();
    };
    send(rig.ids[1], raw(rig.ids[1], 7), cx);
    assert_eq!(rig.last_edit_job().source, SourceToken::Preview, "another photo's RAW is not this one's");
    send(photo, raw(photo, 7), cx);
    let job = rig.last_edit_job();
    assert_eq!(job.source, SourceToken::Working { photo_id: photo, generation: 7 });
    let stamped = parse_edit(Some(&job.edit_json));
    assert_eq!(stamped.engine.get(), Some(2.0), "the stage renders the engine-2 record");
    assert!(rig.present("dk-source", cx), "the badge names the source");
    assert!(rig.present("dk-kelvin", cx), "Kelvin white balance, as shot");

    rig.slide(Control::KelvinTemp, 1000.0, cx);
    assert_eq!(rig.working(cx)["tone"]["wb"], json!({"temp": 0, "tint": 2, "mode": "kelvin", "kelvin": 12000}));
    advance(cx, AUTOSAVE_QUIET);
    work(cx);
    let saved: Value = serde_json::from_str(&rig.catalog(|c| c.list_versions(photo).unwrap())[0].edit_json).unwrap();
    assert_eq!((saved["engine"].clone(), saved["cameraEv"].clone()), (json!(2), json!(-0.5)));

    let d = rig.darkroom(cx);
    d.update(cx, |d, cx| d.set_clipping(true, cx));
    cx.run_until_parked();
    let clip = rig.last_edit_job();
    assert!(clip.clip, "the clipping layer asks for the overlay");
    assert_eq!(clip.max_edge, FULL_EDGE);
}

/// A failed render of the newest record is shown, not hidden behind the older frame.
#[gpui_kit::test]
fn a_failed_render_is_marked_on_the_stage(cx: &mut TestAppContext) {
    let rig = rig("dk-fail", 1, cx);
    advance(cx, SETTLE);
    let key = rig.pool.last_batch()[0].clone();
    assert!(matches!(&key, JobKey::Edit(job) if job.max_edge == FULL_EDGE), "the newest record's full frame");
    rig.pool.finish(&key, Err("decode failed".into()));
    cx.run_until_parked();
    assert!(rig.present("dk-render-failed", cx));
}

/// **Forced interleaving** (map #92, Catalog identity). The core switches to a catalog whose
/// photo and version carry the same ids, with `catalog:switched` withheld: the pending
/// autosave fails closed and the new catalog's version is untouched. Then the event arrives:
/// the Darkroom closes without saving and the stage is the Library again.
fn autosave_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let rig = rig(if delivered { "dk-switch-ev" } else { "dk-switch" }, 1, cx);
    let photo = rig.ids[0];
    // Give the photo a version first, so the colliding catalog has the same version id.
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    advance(cx, AUTOSAVE_QUIET);
    work(cx);
    let version = rig.catalog(|c| c.list_versions(photo).unwrap())[0].id;

    let (b, b_ids) = colliding_catalog(&rig.dir, "b", 1);
    let b_version = b.create_version(b_ids[0], "B's").unwrap();
    assert_eq!((b_ids[0], b_version), (photo, version), "the ids collide");
    rig.slide(Control::Tone(ToneKey::Ev), -1.0, cx);
    core_switch(&rig.app, b);
    if delivered {
        // `catalog:switched` as the router hands it to the model, observed before anything
        // the handlers spawned has run.
        let event = CoreEvent::CatalogSwitched("switched.chairphoto".into());
        rig.app.wired.model.update(cx, |m, cx| m.on_core_event(&event, cx));
        assert_eq!(rig.open_photo(cx), None, "the switch closed the Darkroom");
        assert_eq!(rig.surface(cx), Surface::Library);
        // What the Darkroom's own close (rather than following the shell to the Library,
        // which saves and re-reads the rows) prevents: no row read of the new catalog before
        // the model has read it (`CatalogRead`) …
        let pending = rig.app.wired.shell.read_with(cx, |s, _| s.rows_pending());
        assert_eq!(pending, None, "no premature refresh_rows");
        cx.run_until_parked();
    }
    advance(cx, AUTOSAVE_QUIET);
    work(cx);
    rig.catalog(|c| {
        let v = c.list_versions(photo).unwrap();
        assert_eq!((v[0].name.as_str(), v[0].edit_json.as_str()), ("B's", "{}"), "the new catalog's version was written");
        assert!(c.version_history(version).unwrap().steps.is_empty());
    });
    if !delivered {
        let error = rig.darkroom(cx).read_with(cx, |d, _| d.error.clone());
        assert_eq!(error, Some(format!("Autosave failed: {CATALOG_CHANGED}")), "the refusal is said, not swallowed");
        assert_eq!(rig.working(cx)["tone"]["ev"], json!(-1), "the change is kept, unsaved");
        // A refused save is not retried on a timer against the catalog that refuses it.
        advance(cx, AUTOSAVE_QUIET * 3);
        assert_eq!(cx.update(|cx| Runner::get(cx).pending()), 0, "no retry loop");
    } else {
        // … and no autosave attempted against the catalog the photo is not in: nothing to
        // refuse, so nothing reported.
        assert_eq!(rig.darkroom(cx).read_with(cx, |d, _| d.error.clone()), None, "no autosave was attempted");
        let line = crate::tests::status(&rig.app, cx);
        assert!(!line.starts_with("Autosave failed"), "no autosave was attempted: {line}");
    }
}

#[gpui_kit::test]
fn autosave_never_reaches_the_new_catalog_before_the_switch_event(cx: &mut TestAppContext) {
    autosave_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn autosave_never_reaches_the_new_catalog_after_the_switch_event(cx: &mut TestAppContext) {
    autosave_across_a_switch(true, cx);
}

/// LUTs: a chip selects one from the folder, its amount slider appears, and Import copies a
/// validated `.cube` into the folder and selects it.
#[gpui_kit::test]
fn luts_are_chosen_and_imported(cx: &mut TestAppContext) {
    let rig = rig("dk-luts", 1, cx);
    assert!(rig.present("dk-lut-0", cx), "the folder's LUT is a chip");
    assert!(!rig.present("dk-lut-amount", cx));
    // (The chip is below the fold of the test window's rail, so it is not clickable here;
    // it applies `controls::set_lut`, as this does.)
    let d = rig.darkroom(cx);
    d.update(cx, |d, cx| {
        let next = chairphoto_model::darkroom::controls::set_lut(&d.open.as_ref().unwrap().working, Some("film.cube"));
        d.apply(next, None, cx)
    });
    cx.run_until_parked();
    assert_eq!(rig.working(cx)["lut"], json!({"file": "film.cube", "amount": 1}));
    assert!(rig.present("dk-lut-amount", cx));
    rig.slide(Control::LutAmount, 0.4, cx);
    assert_eq!(rig.working(cx)["lut"]["amount"], json!(0.4));

    let src = rig.dir.0.join("Portra.cube");
    std::fs::write(&src, "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n").unwrap();
    rig.darkroom(cx).update(cx, |d, cx| d.import_lut(src, cx));
    work(cx);
    assert!(rig.dir.0.join("luts/Portra.cube").exists());
    assert_eq!(rig.working(cx)["lut"], json!({"file": "Portra.cube", "amount": 1}));
    assert_eq!(rig.darkroom(cx).read_with(cx, |d, _| d.luts.clone()), ["Portra.cube", "film.cube"]);
}

/// **Forced interleaving.** A LUT import on one photo is overtaken by a step: the next
/// photo's open (and its listing of the LUT folder) runs first, then the copy lands. The list
/// — the folder's, not the photo's — still gains the import; the selection is applied to
/// neither the next photo nor anything else.
#[gpui_kit::test]
fn a_lut_import_overtaken_by_a_step_refreshes_the_list_only(cx: &mut TestAppContext) {
    let rig = rig("dk-lut-step", 2, cx);
    let order = rig.app.wired.shell.read_with(cx, |s, _| s.library.photo_ids());
    let src = rig.dir.0.join("Portra.cube");
    std::fs::write(&src, "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n").unwrap();
    rig.darkroom(cx).update(cx, |d, cx| d.import_lut(src, cx));
    let held = cx.update(|cx| Runner::get(cx).hold_pending());
    assert!(!held.is_empty(), "the import is on the worker");
    assert!(rig.view(cx).update(cx, |v, cx| v.step(1, None, cx)));
    cx.run_until_parked();
    work(cx); // the next photo opens and lists the folder before the copy
    assert_eq!(rig.open_photo(cx), Some(order[1]));
    assert_eq!(rig.darkroom(cx).read_with(cx, |d, _| d.luts.clone()), ["film.cube"]);
    cx.update(|cx| Runner::get(cx).release(held));
    work(cx);
    assert!(rig.dir.0.join("luts/Portra.cube").exists());
    assert_eq!(rig.darkroom(cx).read_with(cx, |d, _| d.luts.clone()), ["Portra.cube", "film.cube"], "the list is refreshed");
    assert_eq!(rig.working(cx), json!({}), "not selected for the photo it was not imported on");
    advance(cx, AUTOSAVE_QUIET);
    work(cx);
    assert!(rig.catalog(|c| c.list_versions(order[1]).unwrap()).is_empty());
}
