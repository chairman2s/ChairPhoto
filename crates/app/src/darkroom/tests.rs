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
use crate::tests::{click, colliding_catalog, core_switch, open_catalog_with_photos, start_with_pool, App, TempDir};
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
    // The image layer shares the hand-driven pool: the proof sheet, duel and preset cards
    // render through it.
    let pool = Arc::new(FakePool::default());
    let app = start_with_pool(cx, pool.clone());
    let ids = open_catalog_with_photos(&app, &dir, n, cx);
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

// --- the rails (#112) -----------------------------------------------------------------------

impl Rig {
    fn press(&self, key: &str, cx: &mut TestAppContext) {
        cx.update_window(self.app.window(), |_, window, cx| {
            window.render_frame(cx);
            window.press(key, cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    /// Run `f` on the view with its window.
    fn with_view<T>(
        &self,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut DarkroomView, &mut gpui_kit::Window, &mut gpui_kit::Context<DarkroomView>) -> T,
    ) -> T {
        let view = self.view(cx);
        let out = cx.update_window(self.app.window(), |_, window, cx| view.update(cx, |v, cx| f(v, window, cx))).unwrap();
        cx.run_until_parked();
        out
    }

    fn versions(&self) -> Vec<chairphoto_core::catalog::PhotoVersion> {
        self.catalog(|c| c.list_versions(self.ids[0]).unwrap())
    }

    fn labels(&self, version: i64) -> (Vec<String>, Option<i64>) {
        let h = self.catalog(|c| c.version_history(version).unwrap());
        (h.steps.iter().map(|s| s.label.clone()).collect(), h.head)
    }

    fn saved(&self, version: i64) -> Value {
        let v = self.catalog(|c| c.get_version(version).unwrap().unwrap());
        serde_json::from_str(&v.edit_json).unwrap()
    }

    fn settle_and_save(&self, cx: &mut TestAppContext) {
        advance(cx, AUTOSAVE_QUIET);
        work(cx);
    }

    fn version_id(&self, cx: &mut TestAppContext) -> Option<i64> {
        self.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().version_id)
    }

    fn presets_setting(&self) -> Option<Value> {
        self.catalog(|c| c.get_setting(chairphoto_model::presets::USER_PRESETS_KEY).unwrap())
            .map(|t| serde_json::from_str(&t).unwrap())
    }
}

/// Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y through the real key path: undo first saves the change not
/// yet saved (so it can be redone), then steps back one step from there — to the head the
/// change was made on, not React's head − 1; the step's settings go back into the
/// version; redo walks forward; a new change replaces the undone steps.
#[gpui_kit::test]
fn undo_and_redo_walk_the_history_and_save_the_pending_change_first(cx: &mut TestAppContext) {
    let rig = rig("dk-undo", 1, cx);
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    rig.settle_and_save(cx);
    let v = rig.versions()[0].id;
    rig.slide(Control::Tone(ToneKey::Contrast), 0.3, cx); // pending, not yet saved
    rig.press("ctrl-z", cx);
    work(cx);
    let (labels, head) = rig.labels(v);
    assert_eq!(labels, ["Before", "Exposure +0.50", "Contrast +0.30"], "the pending change was saved first");
    // Deliberately not React's H−1 (0 here: React chose the target before saving the
    // pending change): one Ctrl+Z takes back the unsaved change only.
    assert_eq!(head, Some(1), "then undone, landing on the step before the pending change");
    assert_eq!(rig.working(cx)["tone"]["contrast"], json!(0));
    assert_eq!(rig.working(cx)["tone"]["ev"], json!(0.5));
    assert_eq!(rig.saved(v)["tone"]["contrast"], json!(0), "the step's settings are the version's");
    let active = rig.app.wired.shell.read_with(cx, |s, _| s.active_version().map(|v| v.edit_json.clone()));
    let active: Value = serde_json::from_str(&active.expect("the shell shows the version")).unwrap();
    assert_eq!(active["tone"]["contrast"], json!(0), "the shell's copy follows");

    rig.press("ctrl-shift-z", cx);
    work(cx);
    assert_eq!(rig.labels(v).1, Some(2));
    assert_eq!(rig.working(cx)["tone"]["contrast"], json!(0.3));
    rig.press("ctrl-y", cx);
    work(cx);
    assert_eq!(rig.labels(v).1, Some(2), "nothing to redo at the tip");

    // Back two, then a new change: the undone steps are replaced.
    rig.press("ctrl-z", cx);
    work(cx);
    rig.press("ctrl-z", cx);
    work(cx);
    assert_eq!(rig.labels(v).1, Some(0));
    assert_eq!(rig.working(cx), json!({}), "the baseline is the version before its first change");
    rig.slide(Control::Effect(chairphoto_model::darkroom::controls::EffectKey::Fade), 0.2, cx);
    rig.settle_and_save(cx);
    assert_eq!(rig.labels(v), (vec!["Before".to_string(), "Fade 0.20".to_string()], Some(1)));
    // The panel lists the steps; a click goes to one.
    assert!(rig.present("dk-step-0", cx));
    rig.darkroom(cx).update(cx, |d, cx| d.goto_step(0, cx));
    work(cx);
    assert_eq!(rig.labels(v).1, Some(0));
}

/// **Forced interleaving.** Ctrl+Z while the autosave's commit is on the worker: the undo
/// waits for the commit and steps back from the history it produced — it is not lost, and
/// it does not run against the history before the save.
#[gpui_kit::test]
fn undo_during_a_running_autosave_runs_after_it(cx: &mut TestAppContext) {
    let rig = rig("dk-undo-chain", 1, cx);
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    advance(cx, AUTOSAVE_QUIET);
    assert!(rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().saving), "the commit is running");
    rig.press("ctrl-z", cx);
    work(cx);
    let v = rig.versions()[0].id;
    assert_eq!(rig.labels(v), (vec!["Before".to_string(), "Exposure +0.50".to_string()], Some(0)));
    assert_eq!(rig.working(cx), json!({}));
    assert_eq!(rig.saved(v), json!({}));
}

/// **Forced interleaving.** Ctrl+Z is on the worker (Runner::manual holds it) when a slider
/// moves and a preset is clicked: both are refused — the rail is not editable until the step
/// lands — so the undo is not cancelled by a save of "pre-undo + change", the redo branch is
/// kept (core cuts steps past the head on a commit), and the refused preset's label does not
/// name the next, unrelated change.
#[gpui_kit::test]
fn a_change_while_an_undo_is_on_the_worker_neither_cancels_it_nor_cuts_redo(cx: &mut TestAppContext) {
    use chairphoto_model::darkroom::controls::EffectKey;
    let rig = rig("dk-undo-change", 1, cx);
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    rig.settle_and_save(cx);
    rig.slide(Control::Tone(ToneKey::Contrast), 0.3, cx);
    rig.settle_and_save(cx);
    let v = rig.versions()[0].id;
    let steps = vec!["Before".to_string(), "Exposure +0.50".to_string(), "Contrast +0.30".to_string()];
    assert_eq!(rig.labels(v), (steps.clone(), Some(2)));

    rig.press("ctrl-z", cx);
    let d = rig.darkroom(cx);
    assert!(d.read_with(cx, |d, _| !d.open.as_ref().unwrap().editable()), "the step is on the worker");
    rig.slide(Control::Effect(EffectKey::Fade), 0.2, cx);
    let preset = chairphoto_model::presets::builtin_presets().into_iter().next().unwrap();
    d.update(cx, |d, cx| d.apply_preset(&preset, cx));
    cx.run_until_parked();
    assert_eq!(rig.working(cx)["tone"]["contrast"], json!(0.3), "the record is not changed under the step");
    assert_eq!(rig.working(cx).get("fade"), None, "neither the fade nor the preset was taken");
    work(cx);
    advance(cx, AUTOSAVE_QUIET);
    work(cx);
    assert_eq!(rig.labels(v), (steps.clone(), Some(1)), "undone, the redo branch kept");
    assert_eq!(rig.working(cx)["tone"]["contrast"], json!(0), "the step's record is the working one");
    assert_eq!(rig.saved(v)["tone"]["contrast"], json!(0));
    assert!(d.read_with(cx, |d, _| d.open.as_ref().unwrap().editable()), "changes are taken again");

    // Redo still reaches the step the change would have cut.
    rig.press("ctrl-shift-z", cx);
    work(cx);
    assert_eq!(rig.labels(v).1, Some(2));
    assert_eq!(rig.working(cx)["tone"]["contrast"], json!(0.3));
    // The next change is named for itself, not for the refused preset.
    rig.slide(Control::Effect(EffectKey::Fade), 0.2, cx);
    rig.settle_and_save(cx);
    assert_eq!(rig.labels(v).0.last().unwrap(), "Fade 0.20");
}

/// The version shelf: "+ New version" copies the settings and continues there; Original shows
/// the unedited photo and the next change there starts another version; a chip switches back
/// (saving first); the cover toggles in the catalog.
#[gpui_kit::test]
fn the_version_shelf_new_version_switching_and_the_cover(cx: &mut TestAppContext) {
    let rig = rig("dk-shelf", 1, cx);
    let photo = rig.ids[0];
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    rig.settle_and_save(cx);
    let v1 = rig.versions()[0].id;
    rig.darkroom(cx).update(cx, |d, cx| d.new_version(cx));
    work(cx);
    let vs = rig.versions();
    assert_eq!(vs.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), ["Version 1", "Version 2"]);
    let v2 = vs[1].id;
    assert_eq!(rig.saved(v2)["tone"]["ev"], json!(0.5), "the settings were copied");
    assert_eq!(rig.version_id(cx), Some(v2));
    assert_eq!(rig.app.wired.shell.read_with(cx, |s, _| s.active_version().map(|v| v.id)), Some(v2));
    assert!(rig.present(&format!("dk-shelf-{v2}"), cx));

    // A change on version 2, then Original: the change is saved to version 2 first.
    rig.slide(Control::Tone(ToneKey::Contrast), 0.3, cx);
    rig.darkroom(cx).update(cx, |d, cx| d.switch_version(None, cx));
    work(cx);
    assert_eq!(rig.saved(v2)["tone"]["contrast"], json!(0.3));
    assert_eq!(rig.saved(v1)["tone"]["contrast"], json!(0), "version 1 untouched");
    assert_eq!(rig.version_id(cx), None);
    assert_eq!(rig.working(cx), json!({}));
    // A change on the Original starts "Version 3".
    rig.slide(Control::Effect(chairphoto_model::darkroom::controls::EffectKey::Fade), 0.2, cx);
    rig.settle_and_save(cx);
    assert_eq!(rig.versions().last().unwrap().name, "Version 3");

    // Back to version 1.
    rig.darkroom(cx).update(cx, |d, cx| d.switch_version(Some(v1), cx));
    work(cx);
    assert_eq!(rig.version_id(cx), Some(v1));
    assert_eq!(rig.working(cx)["tone"]["ev"], json!(0.5));
    let shown = rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().history.as_ref().map(|h| h.version_id));
    assert_eq!(shown, Some(v1), "its history is shown");

    // The cover.
    rig.darkroom(cx).update(cx, |d, cx| d.toggle_cover(cx));
    work(cx);
    assert_eq!(rig.catalog(|c| c.cover_of(photo).unwrap().map(|c| c.0)), Some(v1));
    assert!(rig.present("dk-cover", cx));
    rig.darkroom(cx).update(cx, |d, cx| d.toggle_cover(cx));
    work(cx);
    assert_eq!(rig.catalog(|c| c.cover_of(photo).unwrap()), None);
}

/// **Forced interleaving.** A version switch and "Develop with the new engine" replace the
/// record: a change made while either is on the worker is refused (the rail is not
/// editable), not made on screen and then silently dropped as React did. The version left
/// keeps what it held; the one arrived at shows its own record. "+ New version" copies the
/// record instead, so a change made while it is written is kept and saved into it.
#[gpui_kit::test]
fn changes_during_a_switch_or_a_new_engine_fork_are_refused_not_dropped(cx: &mut TestAppContext) {
    use chairphoto_model::darkroom::controls::EffectKey;
    let rig = rig("dk-switch-refuse", 1, cx);
    let photo = rig.ids[0];
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    rig.settle_and_save(cx);
    let v1 = rig.versions()[0].id;
    let old = r#"{"tone":{"ev":1},"crop":{"x":0.1,"y":0.1,"w":0.8,"h":0.8}}"#;
    let mine = rig.catalog(|c| {
        let id = c.create_version(photo, "Mine").unwrap();
        c.set_version_edit(id, old).unwrap();
        id
    });
    let d = rig.darkroom(cx);
    let editable = |cx: &mut TestAppContext| d.read_with(cx, |d, _| d.open.as_ref().unwrap().editable());

    // The switch on the worker: a slider is refused.
    d.update(cx, |d, cx| d.switch_version(Some(mine), cx));
    assert!(!editable(cx), "the switch is on the worker");
    rig.slide(Control::Effect(EffectKey::Fade), 0.2, cx);
    assert_eq!(rig.working(cx).get("fade"), None, "refused: not shown as if it would be kept");
    assert_eq!(rig.working(cx)["tone"]["ev"], json!(0.5), "still version 1's record");
    work(cx);
    assert!(editable(cx));
    assert_eq!(rig.version_id(cx), Some(mine));
    assert_eq!(rig.working(cx)["tone"]["ev"], json!(1), "the version switched to");
    assert_eq!(rig.saved(v1).get("fade"), None, "nothing written to the version left");
    assert_eq!(rig.saved(mine)["tone"]["ev"], json!(1));

    // The new-engine fork on the worker: a slider is refused.
    let source = DevelopSource::Raw {
        camera: "Sony".into(),
        megapixels: 61.0,
        bits: 16,
        decoder: "0.22".into(),
        token: Some(format!("w:{photo}:3")),
        camera_ev: Some(-0.5),
        as_shot_wb: None,
        lens: None,
    };
    rig.app.state.send(CoreEvent::DevelopSource(DevelopSourceEvent { photo_id: photo, job: 3, source }));
    cx.run_until_parked();
    assert!(d.read_with(cx, |d, _| d.open.as_ref().unwrap().engine1_version));
    d.update(cx, |d, cx| d.develop_with_new_engine(cx));
    assert!(!editable(cx), "the fork is on the worker");
    rig.slide(Control::Effect(EffectKey::Fade), 0.2, cx);
    assert_eq!(rig.working(cx).get("fade"), None, "refused during the fork");
    work(cx);
    advance(cx, AUTOSAVE_QUIET);
    work(cx);
    assert!(editable(cx));
    let fork = rig.versions().into_iter().find(|v| v.name == "Mine (RAW)").expect("the fork");
    assert_eq!(rig.version_id(cx), Some(fork.id));
    assert_eq!(rig.working(cx).get("fade"), None);
    assert_eq!(rig.working(cx).get("tone"), None, "tone starts over on the new engine");
    assert_eq!(rig.catalog(|c| c.get_version(mine).unwrap().unwrap().edit_json), old, "the engine-1 version is untouched");

    // "+ New version" keeps a change made while it is written.
    d.update(cx, |d, cx| d.new_version(cx));
    assert!(editable(cx), "a copy does not replace the record");
    rig.slide(Control::Effect(EffectKey::Fade), 0.2, cx);
    work(cx);
    advance(cx, AUTOSAVE_QUIET);
    work(cx);
    let copy = rig.version_id(cx).unwrap();
    assert_ne!(copy, fork.id);
    assert_eq!(rig.saved(copy)["fade"], json!(0.2), "saved into the new version");
}

/// "Develop with the new engine": an engine-1 version's framing as a fresh engine-2 version
/// "<name> (RAW)"; the engine-1 version is left as it was.
#[gpui_kit::test]
fn develop_with_the_new_engine_forks_the_framing_onto_the_raw(cx: &mut TestAppContext) {
    let rig = rig("dk-engine", 1, cx);
    let photo = rig.ids[0];
    let old = r#"{"tone":{"ev":1},"crop":{"x":0.1,"y":0.1,"w":0.8,"h":0.8},"future":1}"#;
    let v1 = rig.catalog(|c| {
        let id = c.create_version(photo, "Mine").unwrap();
        c.set_version_edit(id, old).unwrap();
        id
    });
    rig.darkroom(cx).update(cx, |d, cx| d.switch_version(Some(v1), cx));
    work(cx);
    let source = DevelopSource::Raw {
        camera: "Sony".into(),
        megapixels: 61.0,
        bits: 16,
        decoder: "0.22".into(),
        token: Some(format!("w:{photo}:3")),
        camera_ev: Some(-0.5),
        as_shot_wb: None,
        lens: None,
    };
    rig.app.state.send(CoreEvent::DevelopSource(DevelopSourceEvent { photo_id: photo, job: 3, source }));
    cx.run_until_parked();
    assert!(rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().engine1_version));
    assert!(rig.present("dk-new-engine", cx));
    rig.darkroom(cx).update(cx, |d, cx| d.develop_with_new_engine(cx));
    work(cx);
    let vs = rig.versions();
    let fork = vs.iter().find(|v| v.name == "Mine (RAW)").expect("the fork");
    let saved: Value = serde_json::from_str(&fork.edit_json).unwrap();
    assert_eq!(saved, json!({"crop": {"x": 0.1, "y": 0.1, "w": 0.8, "h": 0.8}, "engine": 2, "display": "camera.2", "cameraEv": -0.5}));
    assert_eq!(rig.catalog(|c| c.get_version(v1).unwrap().unwrap().edit_json), old, "the engine-1 version is untouched");
    assert_eq!(rig.version_id(cx), Some(fork.id));
    assert!(!rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().engine1_version));
    assert_eq!(rig.last_edit_job().source, SourceToken::Working { photo_id: photo, generation: 3 }, "the stage renders the RAW");
}

/// Presets: a card applies one named step (framing kept); "Save as preset" stores the look
/// only; rename and delete rewrite the stored list keeping every other entry as it was.
#[gpui_kit::test]
fn presets_apply_save_rename_and_delete_keeping_unknown_data(cx: &mut TestAppContext) {
    let rig = rig("dk-presets", 1, cx);
    let theirs = json!({"id": "theirs", "name": "Theirs", "category": "User", "edit": "kept as is", "note": 1});
    // As TS wrote it back: `loadUserPresets` marks every stored entry `builtin: false`.
    let theirs_saved = json!({"id": "theirs", "name": "Theirs", "category": "User", "edit": "kept as is", "builtin": false, "note": 1});
    rig.catalog(|c| c.set_setting(chairphoto_model::presets::USER_PRESETS_KEY, &json!([theirs]).to_string()).unwrap());
    // Leave and come back so the settings are read again.
    rig.app.wired.shell.update(cx, |s, cx| s.show_library(cx));
    cx.run_until_parked();
    work(cx);
    rig.app.wired.shell.update(cx, |s, cx| s.open_develop(cx));
    cx.run_until_parked();
    work(cx);
    assert_eq!(rig.darkroom(cx).read_with(cx, |d, _| d.user_presets.len()), 1);

    // A crop first, then a preset: the crop stays.
    rig.darkroom(cx).update(cx, |d, cx| {
        let w = d.open.as_ref().unwrap().working.clone();
        let next = chairphoto_model::darkroom::geometry::apply_aspect(&w, "Free", None).unwrap();
        d.apply(next, None, cx)
    });
    let preset = chairphoto_model::presets::builtin_presets().into_iter().next().unwrap();
    rig.darkroom(cx).update(cx, |d, cx| d.apply_preset(&preset, cx));
    rig.settle_and_save(cx);
    let v = rig.versions()[0].id;
    assert_eq!(rig.labels(v).0.last().unwrap(), &format!("Preset: {}", preset.name));
    assert_eq!(rig.working(cx)["crop"]["aspect"], json!("Free"), "framing kept");

    // The browser: opened, the cards ask for their renders.
    let before = rig.edit_jobs().len();
    rig.with_view(cx, |v, _, cx| v.set_presets_open(true, cx));
    assert!(rig.edit_jobs().len() > before, "the cards render");
    assert!(rig.present(&format!("dk-preset-{}", preset.id), cx));

    // Save as preset, through the name field.
    rig.with_view(cx, |v, window, cx| v.start_naming(window, cx));
    let input = rig.view(cx).read_with(cx, |v, _| v.preset_name_input().clone());
    cx.update_window(rig.app.window(), |_, window, cx| input.update(cx, |i, cx| i.replace_all("  Mine ", window, cx))).unwrap();
    rig.with_view(cx, |v, window, cx| v.save_preset(window, cx));
    work(cx);
    let list = rig.presets_setting().unwrap();
    assert_eq!(list[0], theirs_saved, "the other preset as stored");
    assert_eq!(list[1]["name"], json!("Mine"));
    assert_eq!(list[1]["edit"].get("crop"), None, "a preset holds the look, not the framing");
    assert!(rig.present("dk-notice", cx));
    let mine = list[1]["id"].as_str().unwrap().to_string();

    rig.darkroom(cx).update(cx, |d, cx| d.rename_preset(mine.clone(), "Matte".into(), cx));
    work(cx);
    let list = rig.presets_setting().unwrap();
    assert_eq!((list[0].clone(), list[1]["name"].clone()), (theirs_saved.clone(), json!("Matte")));
    rig.darkroom(cx).update(cx, |d, cx| d.delete_preset(mine, cx));
    work(cx);
    assert_eq!(rig.presets_setting().unwrap(), json!([theirs_saved]));
    assert_eq!(rig.darkroom(cx).read_with(cx, |d, _| d.user_presets.len()), 1);
}

/// Run the queued worker jobs **newest first**, as a pool of workers may take them, and what
/// they queue in turn, letting the UI take each result.
fn work_newest_first(cx: &mut TestAppContext) {
    loop {
        let ran = cx.update(|cx| Runner::get(cx).run_pending_reversed());
        cx.run_until_parked();
        if ran == 0 {
            return;
        }
    }
}

/// **Forced interleaving.** Two quick writes of one setting, the worker taking the newer
/// first: the preset saves and the overlay choices still land in the order made — in the
/// catalog and on screen — because a key's next write waits for the one before it.
#[gpui_kit::test]
fn preset_and_overlay_writes_land_in_the_order_made(cx: &mut TestAppContext) {
    use chairphoto_model::darkroom::geometry::OVERLAY_KEY;
    use chairphoto_model::editing::CropOverlay;
    let rig = rig("dk-setting-order", 1, cx);
    let d = rig.darkroom(cx);
    let pending = |cx: &mut TestAppContext| cx.update(|cx| Runner::get(cx).pending());

    d.update(cx, |d, cx| d.set_overlay(CropOverlay::Golden, cx));
    d.update(cx, |d, cx| d.set_overlay(CropOverlay::None, cx));
    assert_eq!(pending(cx), 1, "the second choice waits for the first");
    work_newest_first(cx);
    assert_eq!(rig.catalog(|c| c.get_setting(OVERLAY_KEY).unwrap()).as_deref(), Some("none"), "the last choice is remembered");
    assert_eq!(d.read_with(cx, |d, _| d.overlay), CropOverlay::None);

    d.update(cx, |d, cx| d.save_preset("First", cx));
    d.update(cx, |d, cx| d.save_preset("Second", cx));
    let first = d.read_with(cx, |d, _| d.user_presets.len());
    assert_eq!(first, 0);
    work_newest_first(cx);
    let stored: Vec<Value> = serde_json::from_value(rig.presets_setting().unwrap()).unwrap();
    let names: Vec<&str> = stored.iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["First", "Second"], "saved in the order made");
    let shown: Vec<String> = d.read_with(cx, |d, _| d.user_presets.iter().map(|p| p.name.clone()).collect());
    assert_eq!(shown, ["First", "Second"], "the list shown is the last one written");

    // A delete then a save: the deleted preset does not come back, here or on screen.
    let id = stored[0]["id"].as_str().unwrap().to_string();
    d.update(cx, |d, cx| d.delete_preset(id, cx));
    d.update(cx, |d, cx| d.save_preset("Third", cx));
    work_newest_first(cx);
    let stored: Vec<Value> = serde_json::from_value(rig.presets_setting().unwrap()).unwrap();
    let names: Vec<&str> = stored.iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Second", "Third"]);
    let shown: Vec<String> = d.read_with(cx, |d, _| d.user_presets.iter().map(|p| p.name.clone()).collect());
    assert_eq!(shown, ["Second", "Third"]);
}

/// Crop & rotate: an aspect chip fits a crop of that aspect to the frame; the angle slider
/// straightens with the crop inset; perspective puts the quad up (the stage renders
/// un-warped), a handle moves a corner, Done renders it warped; each is a history step.
#[gpui_kit::test]
fn crop_rotate_and_perspective_change_the_record(cx: &mut TestAppContext) {
    use chairphoto_model::editing::{Field, QuadCorner};
    let rig = rig("dk-geometry", 1, cx);
    advance(cx, SETTLE);
    let key = rig.pool.last_batch()[0].clone();
    rig.pool.finish(&key, Ok(pixels(6, 4)));
    cx.run_until_parked();
    rig.with_view(cx, |v, _, cx| v.set_aspect("1:1", cx));
    let crop = rig.working(cx)["crop"].clone();
    assert_eq!(crop["aspect"], json!("1:1"));
    let (w, h) = (crop["w"].as_f64().unwrap(), crop["h"].as_f64().unwrap());
    assert!((w * 6.0 - h * 4.0).abs() < 1e-9, "square on the 6×4 frame: {crop}");
    assert!(rig.present("dk-crop", cx) && rig.present("dk-crop-se", cx), "the box and its handles are drawn");
    assert_eq!(parse_edit(Some(&rig.last_edit_job().edit_json)).crop, Field::Absent, "the stage renders without the crop");
    rig.settle_and_save(cx);

    let slider = rig.view(cx).read_with(cx, |v, _| v.straighten_slider().clone());
    slider.update(cx, |_, cx| cx.emit(SliderEvent::Change(SliderValue::from(3.0_f32))));
    cx.run_until_parked();
    let rec = rig.working(cx);
    assert_eq!(rec["straighten"], json!(3));
    assert_eq!(rec["crop"]["aspect"], json!("Original"), "the crop is inset to hide the corners");
    rig.settle_and_save(cx);

    let d = rig.darkroom(cx);
    d.update(cx, |d, cx| d.start_perspective(cx));
    cx.run_until_parked();
    assert!(d.read_with(cx, |d, _| d.open.as_ref().unwrap().perspective_mode));
    assert_eq!(rig.working(cx).get("crop"), None, "the crop goes with a new quad");
    assert!(rig.present("dk-quad-tl", cx));
    advance(cx, SETTLE);
    assert_eq!(parse_edit(Some(&rig.last_edit_job().edit_json)).perspective, Field::Absent, "un-warped while the handles are up");
    rig.with_view(cx, |v, _, cx| v.drag_quad_to(QuadCorner::Tl, 0.2, 0.1, cx));
    assert_eq!(rig.working(cx)["perspective"]["tl"], json!([0.2, 0.1]));
    d.update(cx, |d, cx| d.set_perspective_mode(false, cx));
    advance(cx, SETTLE);
    assert!(parse_edit(Some(&rig.last_edit_job().edit_json)).perspective.is_set(), "Done: rendered warped");
    rig.settle_and_save(cx);
    let v = rig.versions()[0].id;
    let labels = rig.labels(v).0;
    assert!(labels.len() >= 4, "crop, straighten and perspective are steps: {labels:?}");
    assert_eq!(rig.saved(v)["perspective"]["tl"], json!([0.2, 0.1]));
    d.update(cx, |d, cx| d.clear_perspective(cx));
    cx.run_until_parked();
    assert_eq!(rig.working(cx).get("perspective"), None);
}

/// The proof sheet and the duel mount over the Darkroom: a proof adopts as "Proof: <label>"
/// and names the next "+ New version"; a duel pick (← with the duel's keys) applies, ⑂ banks
/// a variant and the duel says so; Esc closes it.
#[gpui_kit::test]
fn the_proof_sheet_and_duel_are_mounted_and_feed_the_record(cx: &mut TestAppContext) {
    use super::view::Overlay;
    let rig = rig("dk-proof", 2, cx);
    let photo = rig.open_photo(cx);
    work(cx); // the auto-tone fragment (unmeasurable here: empty)
    assert!(rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().auto_fragment.is_some()));
    rig.with_view(cx, |v, window, cx| v.open_proof_sheet(window, cx));
    let sheet = rig.view(cx).read_with(cx, |v, _| match v.overlay() {
        Some(Overlay::Proof(p)) => p.clone(),
        _ => panic!("the proof sheet is mounted"),
    });
    assert!(rig.present("proof-backdrop", cx));
    let (i, label) = sheet.read_with(cx, |s, _| {
        let i = s.candidates().iter().position(|c| c.group == chairphoto_model::darkroom::spreads::ProofGroup::Film).unwrap();
        (i, s.candidates()[i].label.clone())
    });
    sheet.update(cx, |s, cx| s.adopt(i, cx));
    cx.run_until_parked();
    assert!(rig.view(cx).read_with(cx, |v, _| v.overlay().is_none()), "closed after adopting");
    rig.settle_and_save(cx);
    let v1 = rig.versions()[0].id;
    assert_eq!(rig.labels(v1).0.last().unwrap(), &format!("Proof: {label}"));
    rig.darkroom(cx).update(cx, |d, cx| d.new_version(cx));
    work(cx);
    assert_eq!(rig.versions().last().unwrap().name, label, "+ New version is named after the proof");

    rig.with_view(cx, |v, window, cx| v.open_duel(window, cx));
    let duel = rig.view(cx).read_with(cx, |v, _| match v.overlay() {
        Some(Overlay::Duel(d)) => d.clone(),
        _ => panic!("the duel is mounted"),
    });
    let ev_before = rig.working(cx)["tone"]["ev"].as_f64().unwrap_or(0.0);
    rig.press("left", cx); // the duel has the keys: the left variant wins round 1
    let ev_after = rig.working(cx)["tone"]["ev"].as_f64().unwrap();
    assert!((ev_after - (ev_before - 0.4)).abs() < 1e-9, "{ev_before} → {ev_after}");
    assert_eq!(duel.read_with(cx, |d, _| d.round()), 2);
    rig.press("right", cx);
    assert_eq!(rig.open_photo(cx), photo, "the duel's arrows do not step the filmstrip");
    assert_eq!(duel.read_with(cx, |d, _| d.round()), 3);
    duel.update(cx, |d, cx| d.fork(1, cx));
    work(cx);
    assert!(rig.versions().iter().any(|v| v.name == "What-if — contrast"), "the variant was banked");
    assert!(rig.present("duel-note", cx), "the duel says it was kept");
    rig.press("escape", cx);
    assert!(rig.view(cx).read_with(cx, |v, _| v.overlay().is_none()));
}

/// **Catalog identity.** The core switches to a catalog with colliding ids, the event
/// withheld: a version operation (the cover), a history step and a preset save fail closed
/// and touch nothing in the new catalog.
#[gpui_kit::test]
fn rails_writes_fail_closed_across_a_switch(cx: &mut TestAppContext) {
    let rig = rig("dk-rails-switch", 1, cx);
    let photo = rig.ids[0];
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    rig.settle_and_save(cx);
    let v = rig.versions()[0].id;
    let (b, b_ids) = colliding_catalog(&rig.dir, "b", 1);
    let b_version = b.create_version(b_ids[0], "B's").unwrap();
    assert_eq!((b_ids[0], b_version), (photo, v), "the ids collide");
    core_switch(&rig.app, b);
    rig.darkroom(cx).update(cx, |d, cx| d.toggle_cover(cx));
    work(cx);
    let error = rig.darkroom(cx).read_with(cx, |d, _| d.error.clone()).unwrap_or_default();
    assert!(error.contains(CATALOG_CHANGED), "{error}");
    rig.darkroom(cx).update(cx, |d, cx| d.goto_step(0, cx));
    work(cx);
    rig.darkroom(cx).update(cx, |d, cx| d.save_preset("Mine", cx));
    work(cx);
    rig.catalog(|c| {
        assert_eq!(c.cover_of(photo).unwrap(), None, "no cover set in the new catalog");
        assert_eq!(c.get_setting(chairphoto_model::presets::USER_PRESETS_KEY).unwrap(), None, "no preset saved there");
        assert!(c.version_history(v).unwrap().steps.is_empty(), "no step taken there");
        assert_eq!(c.list_versions(photo).unwrap()[0].edit_json, "{}");
    });
}

/// The Darkroom's keys stand down while the proof sheet or the duel is up (React's
/// filmstrip `keysDisabled`): under the proof sheet — which binds only Esc — ← / → do not
/// step the filmstrip, Enter does not zoom to the crop, and Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y
/// do not move the history; under the duel — which binds the arrows, ↓ and Esc — Ctrl+Z,
/// Ctrl+Y and Enter do nothing either. Each key is first shown to act with no overlay up.
#[gpui_kit::test]
fn the_darkroom_keys_stand_down_under_the_proof_sheet_and_the_duel(cx: &mut TestAppContext) {
    use super::view::Overlay;
    let rig = rig("dk-key-guard", 2, cx);
    let photo = rig.open_photo(cx);
    work(cx); // the auto-tone fragment
    advance(cx, SETTLE);
    let key = rig.pool.last_batch()[0].clone();
    rig.pool.finish(&key, Ok(pixels(6, 4)));
    cx.run_until_parked();
    rig.with_view(cx, |v, _, cx| v.set_aspect("1:1", cx));
    rig.settle_and_save(cx);
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    rig.settle_and_save(cx);
    let v = rig.versions()[0].id;
    let head = rig.labels(v).1;
    assert_eq!(head, Some(2), "Before, the crop, the exposure");
    let stage_view = |cx: &mut TestAppContext| rig.view(cx).read_with(cx, |v, _| v.stage_view());
    let overlay = |cx: &mut TestAppContext| {
        rig.view(cx).read_with(cx, |v, _| match v.overlay() {
            Some(Overlay::Proof(_)) => "proof",
            Some(Overlay::Duel(_)) => "duel",
            None => "none",
        })
    };

    // With no overlay up, each key acts.
    rig.press("enter", cx);
    assert!(!stage_view(cx).is_fit(), "Enter zooms to the crop");
    rig.press("escape", cx);
    assert!(stage_view(cx).is_fit());
    rig.press("ctrl-z", cx);
    work(cx);
    assert_eq!(rig.labels(v).1, Some(1), "Ctrl+Z undoes");
    rig.press("ctrl-y", cx);
    work(cx);
    assert_eq!(rig.labels(v).1, head, "Ctrl+Y redoes");

    // Under the proof sheet: nothing.
    rig.with_view(cx, |v, window, cx| v.open_proof_sheet(window, cx));
    assert_eq!(overlay(cx), "proof");
    for key in ["right", "left", "ctrl-z", "ctrl-shift-z", "ctrl-y", "enter"] {
        rig.press(key, cx);
        work(cx);
        assert_eq!(rig.open_photo(cx), photo, "{key} under the proof sheet does not step the filmstrip");
        assert!(stage_view(cx).is_fit(), "{key} under the proof sheet does not zoom");
        assert_eq!(rig.labels(v).1, head, "{key} under the proof sheet does not move the history");
        if key != "enter" {
            assert_eq!(overlay(cx), "proof", "{key}: the sheet stays up");
        }
    }
    // Enter is the focused backdrop's keyboard click: the sheet declines and closes (its own
    // behaviour, `loupe::proof_sheet`) — but the Darkroom did not also take it as "zoom".
    assert_eq!(overlay(cx), "none", "Enter declines the sheet");

    // Under the duel: no undo, redo or zoom.
    rig.with_view(cx, |v, window, cx| v.open_duel(window, cx));
    assert_eq!(overlay(cx), "duel");
    for key in ["ctrl-z", "ctrl-shift-z", "ctrl-y", "enter"] {
        rig.press(key, cx);
        work(cx);
        assert_eq!(rig.labels(v).1, head, "{key} under the duel does not move the history");
        assert!(stage_view(cx).is_fit(), "{key} under the duel does not zoom");
        assert_eq!(overlay(cx), "duel", "{key}: the duel stays up");
    }
    assert_eq!(rig.saved(v)["tone"]["ev"], json!(0.5), "the version is as it was");
}
