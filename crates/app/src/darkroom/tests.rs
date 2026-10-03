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
use crate::image_store::{ImageState, Look};
use chairphoto_core::image_pool::ImageKind;
use chairphoto_model::darkroom::filmstrip::{CoverLook, KeyTarget, STRIP_LAYOUT};
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
    rig_with(tag, n, |_, _| {}, cx)
}

/// [`rig`], with `before` run on the Library before Develop opens.
fn rig_with(tag: &str, n: usize, before: impl FnOnce(&Rig, &mut TestAppContext), cx: &mut TestAppContext) -> Rig {
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
    before(&rig, cx);
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

/// The LUT picker has no `.cube` filter (GPUI's portal picker takes none; React's `pickFile`
/// set none either): a picked file that is not a `.cube` is refused by the import with a
/// message, copies nothing and selects nothing. (#161)
#[gpui_kit::test]
fn a_picked_file_that_is_not_a_cube_lut_is_refused(cx: &mut TestAppContext) {
    let rig = rig("dk-lut-refused", 1, cx);
    let before = rig.working(cx)["lut"].clone();
    let src = rig.dir.0.join("notes.txt");
    std::fs::write(&src, "not a LUT").unwrap();
    rig.darkroom(cx).update(cx, |d, cx| d.import_lut(src, cx));
    work(cx);
    assert_eq!(rig.darkroom(cx).read_with(cx, |d, _| d.error.clone()).as_deref(), Some("only .cube LUTs are supported"));
    assert!(!rig.dir.0.join("luts/notes.txt").exists(), "nothing was copied");
    assert_eq!(rig.working(cx)["lut"], before, "nothing was selected");
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

/// #180: a LUT whose name is wider than the rail is a chip cut to the rail's width (its label
/// ellipsised, the whole name in the tooltip), not one running past the rail's edge.
///
/// Mutation-checked: with the plain `chip` the long LUT's chip is wider than the rail.
#[gpui_kit::test]
fn a_long_lut_name_stays_inside_the_rail(cx: &mut TestAppContext) {
    const LONG: &str = "Kodak_2383_Base_Lut_Rec.709_2.4_IG_ashikulisl_extended_contrast_v3";
    let rig = rig_with(
        "dk-lut-long",
        1,
        |rig, _| std::fs::write(rig.dir.0.join("luts").join(format!("{LONG}.cube")), "").unwrap(),
        cx,
    );
    assert_eq!(rig.darkroom(cx).read_with(cx, |d, _| d.luts.clone()), [format!("{LONG}.cube"), "film.cube".into()]);
    rig.render(cx);
    let (row, chip, short) = cx
        .update_window(rig.app.window(), |_, window, _| {
            (window.find("dk-luts").bounds(), window.find("dk-lut-0").bounds(), window.find("dk-lut-1").bounds())
        })
        .unwrap();
    // The rail is 280 px with 12 px padding: its content is 256 px wide.
    assert!(row.size.width <= gpui_kit::px(256.), "the LUT row fits the rail: {row:?}");
    assert!(chip.right() <= row.right(), "the long LUT's chip ends inside the rail: {chip:?} in {row:?}");
    assert!(chip.left() >= row.left(), "{chip:?} in {row:?}");
    assert!(short.size.width < chip.size.width, "a short name keeps its own width: {short:?}");
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

/// **Forced interleaving.** A duel's ⑂ pressed while the photo's versions are still being
/// read (the read held on the worker) is not dropped: it waits for the version and banks the
/// variant once it is resolved, and the duel says it was kept. If the versions cannot be
/// read, the operation is not run and the banner says so.
#[gpui_kit::test]
fn a_duel_fork_before_the_version_resolves_waits_for_it(cx: &mut TestAppContext) {
    use super::view::Overlay;
    let rig = rig("dk-fork-early", 3, cx);
    let order = rig.app.wired.shell.read_with(cx, |s, _| s.library.photo_ids());
    let d = rig.darkroom(cx);
    let loaded = |cx: &mut TestAppContext| d.read_with(cx, |d, _| d.open.as_ref().unwrap().loaded);
    rig.press("right", cx); // the next photo opens; its version read waits on the worker
    assert_eq!(rig.open_photo(cx), Some(order[1]));
    assert!(!loaded(cx), "the version is not resolved yet");
    rig.with_view(cx, |v, window, cx| v.open_duel(window, cx));
    let duel = rig.view(cx).read_with(cx, |v, _| match v.overlay() {
        Some(Overlay::Duel(d)) => d.clone(),
        _ => panic!("the duel is mounted"),
    });
    duel.update(cx, |d, cx| d.fork(1, cx));
    cx.run_until_parked();
    assert!(!loaded(cx));
    assert!(rig.catalog(|c| c.list_versions(order[1]).unwrap()).is_empty(), "nothing written before the version is known");
    work(cx);
    assert!(loaded(cx));
    let names: Vec<String> = rig.catalog(|c| c.list_versions(order[1]).unwrap()).into_iter().map(|v| v.name).collect();
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(names[0].starts_with("What-if — "), "the variant was banked once the version resolved: {names:?}");
    assert!(rig.present("duel-note", cx), "the duel says it was kept");
    assert_eq!(rig.darkroom(cx).read_with(cx, |d, _| d.error.clone()), None);

    // The versions cannot be read (the catalog switched, the event withheld): the queued
    // operation does not run, and the banner says why.
    rig.press("escape", cx);
    rig.press("right", cx);
    assert_eq!(rig.open_photo(cx), Some(order[2]));
    assert!(!loaded(cx));
    rig.darkroom(cx).update(cx, |d, cx| d.toggle_cover(cx));
    let (b, _) = colliding_catalog(&rig.dir, "b", 3);
    core_switch(&rig.app, b);
    work(cx);
    let error = rig.darkroom(cx).read_with(cx, |d, _| d.error.clone()).unwrap_or_default();
    assert!(error.contains("was not done"), "{error}");
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
        // Enter with no proof focused is a no-op of the sheet's own (`loupe::proof_sheet`).
        assert_eq!(overlay(cx), "proof", "{key}: the sheet stays up");
    }
    rig.press("escape", cx);
    assert_eq!(overlay(cx), "none");

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

/// "🖥 Loupe print" (DarkroomView.tsx's `printOnLoupe`, `basic-editor.printOnLoupe`, default
/// on): the working print goes up on the pop-out after the settle and follows the record;
/// the toggle remembers the choice, turning it on opens the pop-out, whose loupe then renders
/// the print; off, a step, leaving and a catalog switch take it down.
#[gpui_kit::test]
fn the_loupe_print_follows_the_record_onto_the_pop_out(cx: &mut TestAppContext) {
    use crate::loupe::window;
    use super::session::PRINT_ON_LOUPE_KEY;
    let rig = rig("dk-print", 3, cx);
    let order = rig.app.wired.shell.read_with(cx, |s, _| s.library.photo_ids());
    let print = |cx: &mut TestAppContext| {
        rig.app.wired.shell.read_with(cx, |s, _| s.loupe_print().map(|p| (p.photo.id, p.edit_json.clone(), p.source.clone())))
    };
    let stored = |rig: &Rig| rig.catalog(|c| c.get_setting(PRINT_ON_LOUPE_KEY).unwrap());
    let ev_of = |json: &str| serde_json::from_str::<Value>(json).unwrap()["tone"]["ev"].clone();
    // On by default: up after the open's settle.
    advance(cx, SETTLE);
    assert_eq!(print(cx), Some((order[0], "{}".to_string(), SourceToken::Preview)));

    click(&rig.app, "dk-loupe-print", cx);
    work(cx);
    assert_eq!(print(cx), None, "off takes it down");
    assert_eq!(stored(&rig).as_deref(), Some("0"));
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    advance(cx, SETTLE);
    assert_eq!(print(cx), None, "off, a settle puts nothing up");
    assert!(cx.update(|cx| window::handle(cx)).is_none());

    click(&rig.app, "dk-loupe-print", cx);
    work(cx);
    assert_eq!(stored(&rig).as_deref(), Some("1"));
    let h = cx.update(|cx| window::handle(cx)).expect("turning the print on opens the pop-out");
    let (photo, json, _) = print(cx).expect("on puts the print up at once");
    assert_eq!((photo, ev_of(&json)), (order[0], json!(0.5)));
    // The pop-out's loupe renders that record (React's 2560 px loupe render).
    cx.update_window(h, |_, window, cx| window.render_frame(cx)).unwrap();
    cx.run_until_parked();
    let zoom = cx.update(|cx| window::view(cx)).unwrap().read_with(cx, |v, cx| v.loupe().read(cx).zoom().clone());
    assert_eq!(zoom.read_with(cx, |z, _| z.photo()), Some(order[0]));
    let job = rig.edit_jobs().into_iter().rev().find(|j| j.max_edge == 2560).expect("the pop-out asked for the print");
    assert_eq!((job.photo_id, job.edit_json.as_str()), (order[0], json.as_str()));

    // It follows the record on the settle, not before.
    rig.slide(Control::Tone(ToneKey::Ev), 1.0, cx);
    assert_eq!(print(cx).map(|p| ev_of(&p.1)), Some(json!(0.5)), "not before the settle");
    advance(cx, SETTLE);
    assert_eq!(print(cx).map(|p| ev_of(&p.1)), Some(json!(1)));

    // A step takes the photo left's print down; the next photo's follows its settle.
    let view = rig.view(cx);
    assert!(view.update(cx, |v, cx| v.step(1, None, cx)));
    cx.run_until_parked();
    assert_eq!(rig.open_photo(cx), Some(order[1]));
    assert_eq!(print(cx), None, "the photo left's print is down");
    work(cx);
    advance(cx, SETTLE);
    assert_eq!(print(cx).map(|p| p.0), Some(order[1]));

    // Leaving the Darkroom takes it down, and nothing late puts it back.
    click(&rig.app, "dk-back", cx);
    work(cx);
    assert_eq!(rig.surface(cx), Surface::Library);
    assert_eq!(print(cx), None, "leaving takes it down");
    advance(cx, SETTLE * 2);
    assert_eq!(print(cx), None);

    // The stored choice is read with the next open: off stays off.
    rig.catalog(|c| c.set_setting(PRINT_ON_LOUPE_KEY, "0").unwrap());
    rig.app.wired.shell.update(cx, |s, cx| s.open_develop(cx));
    work(cx);
    advance(cx, SETTLE);
    assert_eq!(print(cx), None, "stored off");
    let d = rig.darkroom(cx);
    assert!(!d.read_with(cx, |d, _| d.print_on_loupe));
    // A click while that read is on the worker is newer than what it reads.
    click(&rig.app, "dk-back", cx);
    work(cx);
    rig.app.wired.shell.update(cx, |s, cx| s.open_develop(cx));
    cx.run_until_parked();
    click(&rig.app, "dk-loupe-print", cx);
    work(cx);
    assert!(d.read_with(cx, |d, _| d.print_on_loupe), "the stale read did not undo the click");
    assert_eq!(stored(&rig).as_deref(), Some("1"));
    advance(cx, SETTLE);
    assert_eq!(print(cx).map(|p| p.0), Some(order[1]));
    // A catalog switch takes it down.
    let (b, _) = colliding_catalog(&rig.dir, "b", 3);
    core_switch(&rig.app, b);
    let event = CoreEvent::CatalogSwitched("switched.chairphoto".into());
    rig.app.wired.model.update(cx, |m, cx| m.on_core_event(&event, cx));
    cx.run_until_parked();
    assert_eq!(print(cx), None, "a switch takes it down");
    advance(cx, SETTLE * 2);
    work(cx);
    assert_eq!(print(cx), None);
}

/// The proof sheet's keys, as React's proof cells were buttons: Enter on the sheet as dealt
/// (the backdrop focused, no proof) neither declines nor adopts; Tab / Shift+Tab move focus
/// through the proofs, wrapping; Enter adopts the focused proof. A pointer click on the
/// backdrop still declines.
#[gpui_kit::test]
fn enter_on_the_proof_sheet_adopts_the_focused_proof_and_the_backdrop_click_declines(cx: &mut TestAppContext) {
    use super::view::Overlay;
    use crate::loupe::proof_sheet::ProofSheet;
    let rig = rig("dk-proof-keys", 1, cx);
    work(cx);
    let sheet_of = |rig: &Rig, cx: &mut TestAppContext| {
        rig.view(cx).read_with(cx, |v, _| match v.overlay() {
            Some(Overlay::Proof(p)) => Some(p.clone()),
            _ => None,
        })
    };
    let focused = |rig: &Rig, sheet: &Entity<ProofSheet>, cx: &mut TestAppContext| {
        cx.update_window(rig.app.window(), |_, window, cx| sheet.read(cx).focused(window)).unwrap()
    };
    rig.with_view(cx, |v, window, cx| v.open_proof_sheet(window, cx));
    let sheet = sheet_of(&rig, cx).expect("the proof sheet is mounted");
    let n = sheet.read_with(cx, |s, _| s.candidates().len());
    assert!(n > 2);
    let before = rig.working(cx);

    rig.press("enter", cx);
    assert!(sheet_of(&rig, cx).is_some(), "Enter with no proof focused does not decline");
    assert_eq!(rig.working(cx), before, "… nor adopt");
    assert_eq!(focused(&rig, &sheet, cx), None);

    rig.press("tab", cx);
    assert_eq!(focused(&rig, &sheet, cx), Some(0));
    rig.press("tab", cx);
    rig.press("tab", cx);
    assert_eq!(focused(&rig, &sheet, cx), Some(2));
    rig.press("shift-tab", cx);
    assert_eq!(focused(&rig, &sheet, cx), Some(1));
    rig.press("shift-tab", cx);
    rig.press("shift-tab", cx);
    assert_eq!(focused(&rig, &sheet, cx), Some(n - 1), "wraps");
    rig.press("tab", cx);
    rig.press("tab", cx);
    assert_eq!(focused(&rig, &sheet, cx), Some(1));
    // On to the first film proof (a change from the record, so it is saved as a step).
    let k = sheet.read_with(cx, |s, _| {
        s.candidates().iter().position(|c| c.group == chairphoto_model::darkroom::spreads::ProofGroup::Film).unwrap()
    });
    assert!(k > 1);
    while focused(&rig, &sheet, cx) != Some(k) {
        rig.press("tab", cx);
    }
    let (label, record) = sheet.read_with(cx, |s, _| (s.candidates()[k].label.clone(), s.candidates()[k].record.clone()));
    rig.press("enter", cx);
    assert!(sheet_of(&rig, cx).is_none(), "Enter adopted and closed");
    assert_eq!(rig.working(cx), serde_json::from_str::<Value>(&record.to_json()).unwrap(), "the focused proof's record");
    rig.settle_and_save(cx);
    let v1 = rig.versions()[0].id;
    assert_eq!(rig.labels(v1).0.last().unwrap(), &format!("Proof: {label}"));

    // The backdrop's pointer click declines.
    rig.with_view(cx, |v, window, cx| v.open_proof_sheet(window, cx));
    assert!(sheet_of(&rig, cx).is_some());
    let before = rig.working(cx);
    let corner = gpui_kit::point(gpui_kit::px(4.), gpui_kit::px(4.));
    cx.update_window(rig.app.window(), |_, window, cx| window.click_at("proof-backdrop", corner, cx)).unwrap();
    cx.run_until_parked();
    assert!(sheet_of(&rig, cx).is_none(), "the backdrop click declines");
    assert_eq!(rig.working(cx), before);
}

// --- the filmstrip: centring and cover looks (#134) -------------------------------------------

/// The Library's order of the rig's photos.
fn order(rig: &Rig, cx: &mut TestAppContext) -> Vec<i64> {
    rig.app.wired.shell.read_with(cx, |s, _| s.library.photo_ids())
}

/// Make `photo` the active one from outside the Darkroom (the Library's selection), as a
/// pop-out loupe or any other surface would.
fn select(rig: &Rig, photo: i64, cx: &mut TestAppContext) {
    rig.app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_single(photo)));
    cx.run_until_parked();
    work(cx);
}

/// Two frames: the first lays the strip out (its width is known from then on), the second
/// is what the user sees.
fn draw(rig: &Rig, cx: &mut TestAppContext) {
    rig.render(cx);
    cx.update_window(rig.app.window(), |_, window, cx| {
        window.simulate_next_frame(cx);
    })
    .unwrap();
    rig.render(cx);
}

/// Where the strip stands: its scroll offset (≤ 0), its left edge and width, how far it can
/// scroll, and frame `index`'s laid-out left edge and width (unscrolled).
struct StripAt {
    offset: f32,
    left: f32,
    viewport: f32,
    max: f32,
    frame_left: f32,
    frame_width: f32,
}

fn strip_at(rig: &Rig, index: usize, cx: &mut TestAppContext) -> StripAt {
    let handle = rig.view(cx).read_with(cx, |v, _| v.strip_scroll().clone());
    let frame = handle.bounds_for_item(index).expect("the frame was laid out");
    StripAt {
        offset: f32::from(handle.offset().x),
        left: f32::from(handle.bounds().origin.x),
        viewport: f32::from(handle.bounds().size.width),
        max: f32::from(handle.max_offset().x),
        frame_left: f32::from(frame.origin.x),
        frame_width: f32::from(frame.size.width),
    }
}

/// The current frame's centre sits on the strip's centre, as laid out and as scrolled.
fn assert_centred(rig: &Rig, index: usize, count: usize, why: &str, cx: &mut TestAppContext) {
    let at = strip_at(rig, index, cx);
    assert!(at.max > 0.0, "the strip overflows (else nothing scrolls): {why}");
    let centre = at.frame_left + at.offset + at.frame_width / 2.0;
    let middle = at.left + at.viewport / 2.0;
    assert!((centre - middle).abs() < 0.5, "{why}: frame centre {centre}, strip centre {middle}");
    assert_eq!(-at.offset, STRIP_LAYOUT.centre(index, count, at.viewport), "{why}: the model's position");
}

/// Centring (`scrollIntoView({ inline: "center" })` per change of the current photo): the
/// open photo's frame is in the strip's middle after an outside selection change, ← / →,
/// and a click on a frame — and a strip scrolled by hand stays put until the photo changes.
#[gpui_kit::test]
fn the_strip_centres_the_open_photo_as_it_changes(cx: &mut TestAppContext) {
    let rig = rig("dk-centre", 30, cx);
    let order = order(&rig, cx);
    let n = order.len();

    select(&rig, order[15], cx);
    draw(&rig, cx);
    assert_eq!(rig.open_photo(cx), Some(order[15]));
    assert_centred(&rig, 15, n, "an outside selection change", cx);

    cx.update_window(rig.app.window(), |_, window, cx| window.press("right", cx)).unwrap();
    cx.run_until_parked();
    work(cx);
    draw(&rig, cx);
    assert_eq!(rig.open_photo(cx), Some(order[16]));
    assert_centred(&rig, 16, n, "→", cx);

    let id = gpui_kit::SharedString::from(format!("dk-strip-{}", order[14]));
    cx.update_window(rig.app.window(), |_, window, cx| window.click(id, cx)).unwrap();
    cx.run_until_parked();
    work(cx);
    draw(&rig, cx);
    assert_eq!(rig.open_photo(cx), Some(order[14]));
    assert_centred(&rig, 14, n, "a click on a frame", cx);

    // Scrolled by hand: a redraw with the same photo leaves it there.
    let handle = rig.view(cx).read_with(cx, |v, _| v.strip_scroll().clone());
    handle.set_offset(gpui_kit::point(gpui_kit::px(-10.), gpui_kit::px(0.)));
    draw(&rig, cx);
    assert_eq!(strip_at(&rig, 14, cx).offset, -10.0, "the same photo does not re-centre");
    select(&rig, order[20], cx);
    draw(&rig, cx);
    assert_centred(&rig, 20, n, "the next change centres again", cx);
}

/// At the strip's ends the position is clamped: the last photo leaves the strip scrolled to
/// its end, the second to its start (neither centred past the content).
#[gpui_kit::test]
fn the_strip_is_clamped_at_its_ends(cx: &mut TestAppContext) {
    let rig = rig("dk-clamp", 30, cx);
    let order = order(&rig, cx);
    let n = order.len();

    select(&rig, order[n - 1], cx);
    draw(&rig, cx);
    let at = strip_at(&rig, n - 1, cx);
    assert!(at.max > 0.0);
    assert_eq!(-at.offset, at.max, "the last photo: scrolled to the end");
    assert!(at.frame_left + at.offset + at.frame_width / 2.0 > at.left + at.viewport / 2.0, "not centred: clamped");
    assert_eq!(STRIP_LAYOUT.centre(n - 1, n, at.viewport), at.max, "the model clamps to the same end");

    select(&rig, order[1], cx);
    draw(&rig, cx);
    let at = strip_at(&rig, 1, cx);
    assert_eq!(at.offset, 0.0, "the second photo: at the start");
    assert!(at.frame_left + at.frame_width / 2.0 < at.left + at.viewport / 2.0, "not centred: clamped");
}

/// The strip's width is the last frame it was laid out in. Leave the Darkroom, make the
/// window narrower, come back on another photo: it is centred in the new width, not the
/// last visit's (review rv134 M2).
#[gpui_kit::test]
fn the_strip_centres_in_a_width_changed_outside_the_darkroom(cx: &mut TestAppContext) {
    let rig = rig("dk-centre-resize", 60, cx);
    let order = order(&rig, cx);
    let n = order.len();
    select(&rig, order[30], cx);
    draw(&rig, cx);
    assert_centred(&rig, 30, n, "before", cx);
    let wide = strip_at(&rig, 30, cx).viewport;

    rig.app.wired.shell.update(cx, |s, cx| s.show_library(cx));
    cx.run_until_parked();
    work(cx);
    rig.render(cx);
    cx.simulate_window_resize(rig.app.window(), gpui_kit::size(gpui_kit::px(900.), gpui_kit::px(700.)));
    cx.run_until_parked();
    rig.render(cx);
    select(&rig, order[31], cx);
    rig.app.wired.shell.update(cx, |s, cx| s.open_develop(cx));
    cx.run_until_parked();
    work(cx);
    draw(&rig, cx);
    let narrow = strip_at(&rig, 31, cx).viewport;
    assert!(narrow < wide, "the window is narrower: {narrow} < {wide}");
    assert_centred(&rig, 31, n, "after a resize outside the Darkroom", cx);
}

/// The strip's frames are asked for nearest the open photo first — it, then +1, −1, +2,
/// −2 … — as one batch whose first job ends on top of the pool's LIFO stack (review rv134
/// L2; the navigation rule: the requested photo first, then N±1).
#[gpui_kit::test]
fn the_strips_frames_are_asked_for_nearest_the_open_photo_first(cx: &mut TestAppContext) {
    let rig = rig_with(
        "dk-strip-order",
        7,
        |rig, cx| {
            // The Library's thumbnails have landed (pending ones would be cancelled, and the
            // strip's requests held back until each cancellation answers).
            rig.render(cx);
            for photo in order(rig, cx) {
                rig.pool.finish(&JobKey::photo(photo, ImageKind::Thumb), Ok(pixels(4, 4)));
            }
            cx.run_until_parked();
            let middle = order(rig, cx)[3];
            rig.app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_single(middle)));
            cx.run_until_parked();
        },
        cx,
    );
    let order = order(&rig, cx);
    assert_eq!(rig.open_photo(cx), Some(order[3]));
    // The grid's tiles already show the looks the strip names (#151), so the strip asked
    // for nothing. Drop them (an eviction, a rotation of each): the strip asks for its frames.
    let images = rig.app.wired.images.clone();
    for &photo in &order {
        images.update(cx, |s, cx| s.invalidate(photo, cx));
    }
    rig.app.wired.shell.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    let thumb = |i: usize| JobKey::photo(order[i], ImageKind::Thumb);
    let want: Vec<JobKey> = [3, 4, 2, 5, 1, 6, 0].into_iter().map(thumb).collect();
    let batches = rig.pool.batches.lock().unwrap().clone();
    let strip = batches
        .iter()
        .rev()
        .map(|b| b.iter().filter(|k| want.contains(k)).cloned().collect::<Vec<_>>())
        .find(|b| b.len() == want.len())
        .expect("the strip's batch");
    assert_eq!(strip, want);
}

/// The Thumb jobs for `photo` submitted so far.
fn thumb_jobs(rig: &Rig, photo: i64) -> usize {
    let key = JobKey::photo(photo, ImageKind::Thumb);
    rig.pool.batches.lock().unwrap().iter().flatten().filter(|k| **k == key).count()
}

fn look(rig: &Rig, photo: i64, cx: &mut TestAppContext) -> Option<Look> {
    rig.app.wired.images.read_with(cx, |s, _| s.look(photo))
}

fn image(rig: &Rig, photo: i64, cx: &mut TestAppContext) -> ImageState {
    rig.app.wired.images.read_with(cx, |s, _| s.peek(photo, ImageKind::Thumb))
}

fn stale_dropped(rig: &Rig, cx: &mut TestAppContext) -> u64 {
    rig.app.wired.images.read_with(cx, |s, _| s.stats().stale_dropped)
}

fn refused(rig: &Rig, cx: &mut TestAppContext) -> u64 {
    rig.app.wired.images.read_with(cx, |s, _| s.stats().refused)
}

fn refresh_rows(rig: &Rig, cx: &mut TestAppContext) {
    rig.app.wired.shell.update(cx, |s, cx| s.refresh_rows(cx));
    cx.run_until_parked();
}

/// A frame shows its photo's cover look, asked for under the row's cover token — (photo,
/// cover version, revision) in the catalog the rows came from: a new cover or a new revision
/// of it asks again, the same look asks nothing.
#[gpui_kit::test]
fn a_frame_asks_for_the_cover_look_its_row_names(cx: &mut TestAppContext) {
    let rig = rig("dk-cover", 3, cx);
    let photo = order(&rig, cx)[1];
    let from = rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
    assert_eq!(look(&rig, photo, cx), Some(Look { from, cover: None }), "no cover: the plain thumbnail");
    let jobs = thumb_jobs(&rig, photo);
    assert!(jobs >= 1, "the frame was asked for");
    // The photo's preview (the loupe's tier) is cached: a cover does not change it.
    let images = rig.app.wired.images.clone();
    images.update(cx, |s, _| s.request(photo, ImageKind::Preview));
    rig.pool.finish(&JobKey::photo(photo, ImageKind::Preview), Ok(pixels(4, 4)));
    cx.run_until_parked();
    let preview_ready = |cx: &mut TestAppContext| {
        matches!(images.read_with(cx, |s, _| s.peek(photo, ImageKind::Preview)), ImageState::Ready(_))
    };
    assert!(preview_ready(cx));

    let version = rig.catalog(|c| {
        let v = c.create_version(photo, "Warm").unwrap();
        c.set_cover_version(photo, Some(v)).unwrap();
        v
    });
    refresh_rows(&rig, cx);
    assert_eq!(look(&rig, photo, cx), Some(Look { from, cover: Some(CoverLook { version, rev: 0 }) }));
    assert_eq!(thumb_jobs(&rig, photo), jobs + 1, "a new cover: asked again");
    assert!(preview_ready(cx), "only the thumbnail shows the cover: the preview stays");

    refresh_rows(&rig, cx);
    assert_eq!(thumb_jobs(&rig, photo), jobs + 1, "the same look: nothing asked");

    // The cover version's settings change: its revision moves.
    rig.catalog(|c| c.set_version_edit(version, r#"{"tone":{"ev":1}}"#).unwrap());
    refresh_rows(&rig, cx);
    assert_eq!(look(&rig, photo, cx).unwrap().cover, Some(CoverLook { version, rev: 1 }));
    assert_eq!(thumb_jobs(&rig, photo), jobs + 2, "a new revision: asked again");
}

/// #151: "Use as cover" in the Darkroom, then back to the Library: the grid's tile shows the
/// cover's look, never the plain thumbnail it had cached; the other tiles are not rendered
/// again.
#[gpui_kit::test]
fn a_cover_set_in_the_darkroom_updates_the_grid_tile(cx: &mut TestAppContext) {
    let rig = rig_with(
        "dk-cover-grid",
        3,
        |rig, cx| {
            rig.render(cx);
            for &id in &rig.ids {
                rig.pool.finish(&JobKey::photo(id, ImageKind::Thumb), Ok(pixels(4, 4)));
            }
            cx.run_until_parked();
        },
        cx,
    );
    let photo = rig.ids[0];
    let thumb_width = |rig: &Rig, id: i64, cx: &mut TestAppContext| match image(rig, id, cx) {
        ImageState::Ready(l) => Some(l.image.size(0).width.0),
        _ => None,
    };
    for &id in &rig.ids {
        assert_eq!(thumb_width(&rig, id, cx), Some(4), "the grid cached every tile");
    }
    let others: Vec<(i64, usize)> = rig.ids[1..].iter().map(|&id| (id, thumb_jobs(&rig, id))).collect();

    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    rig.settle_and_save(cx);
    rig.darkroom(cx).update(cx, |d, cx| d.toggle_cover(cx));
    work(cx);
    assert!(rig.catalog(|c| c.cover_of(photo).unwrap()).is_some(), "the cover is set");

    rig.app.wired.shell.update(cx, |s, cx| s.show_library(cx));
    cx.run_until_parked();
    work(cx);
    rig.render(cx);
    assert_eq!(rig.surface(cx), Surface::Library);
    assert_eq!(thumb_width(&rig, photo, cx), None, "the plain thumbnail is not shown under the cover's row");
    assert!(rig.app.wired.images.read_with(cx, |s, _| s.is_pending(photo, ImageKind::Thumb)), "the grid asks for the cover's look");
    rig.pool.finish(&JobKey::photo(photo, ImageKind::Thumb), Ok(pixels(8, 4)));
    cx.run_until_parked();
    rig.render(cx);
    assert_eq!(thumb_width(&rig, photo, cx), Some(8), "the cover's look lands in the grid");
    for (id, jobs) in others {
        assert_eq!(thumb_jobs(&rig, id), jobs, "photo {id}: not rendered again");
        assert_eq!(thumb_width(&rig, id, cx), Some(4));
    }
}

/// A thumbnail cached with no look said (a plain request) may be an older cover's: it is
/// rendered again for the look its row names — by the grid already (#151), so the strip asks
/// nothing more — and the cached one is not shown.
#[gpui_kit::test]
fn a_thumbnail_cached_with_no_look_is_rendered_again(cx: &mut TestAppContext) {
    let mut jobs_before = 0;
    let rig = rig_with(
        "dk-cover-unknown",
        3,
        |rig, cx| {
            let photo = order(rig, cx)[1];
            rig.app.wired.images.update(cx, |s, _| s.request(photo, ImageKind::Thumb));
            rig.pool.finish(&JobKey::photo(photo, ImageKind::Thumb), Ok(pixels(4, 4)));
            cx.run_until_parked();
            assert!(matches!(image(rig, photo, cx), ImageState::Ready(_)), "cached by the Library");
            rig.catalog(|c| {
                let v = c.create_version(photo, "Warm").unwrap();
                c.set_cover_version(photo, Some(v)).unwrap();
            });
            let jobs = thumb_jobs(rig, photo);
            refresh_rows(rig, cx);
            rig.render(cx);
            jobs_before = thumb_jobs(rig, photo);
            assert_eq!(jobs_before, jobs + 1, "the grid rendered it again for the row's look");
        },
        cx,
    );
    let photo = order(&rig, cx)[1];
    assert!(look(&rig, photo, cx).unwrap().cover.is_some());
    assert_eq!(thumb_jobs(&rig, photo), jobs_before, "the strip names the same look: nothing more");
    assert!(!matches!(image(&rig, photo, cx), ImageState::Ready(_)), "the cached one is not shown");
    rig.pool.finish(&JobKey::photo(photo, ImageKind::Thumb), Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert!(matches!(image(&rig, photo, cx), ImageState::Ready(_)));
}

/// A render for an earlier look, already running when the row's look changed, answers
/// late: it is dropped — never shown — and the new look is rendered after it.
#[gpui_kit::test]
fn a_result_for_an_earlier_look_is_dropped(cx: &mut TestAppContext) {
    let rig = rig("dk-cover-stale", 3, cx);
    let photo = order(&rig, cx)[1];
    let key = JobKey::photo(photo, ImageKind::Thumb);
    rig.pool.start(key.clone()); // on a worker: it cannot be cancelled
    let jobs = thumb_jobs(&rig, photo);

    rig.catalog(|c| {
        let v = c.create_version(photo, "Warm").unwrap();
        c.set_cover_version(photo, Some(v)).unwrap();
    });
    refresh_rows(&rig, cx);
    assert_eq!(thumb_jobs(&rig, photo), jobs, "the new look waits for the running render");
    let dropped = stale_dropped(&rig, cx);

    rig.pool.finish(&key, Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert!(!matches!(image(&rig, photo, cx), ImageState::Ready(_)), "the earlier look is not shown");
    assert_eq!(stale_dropped(&rig, cx), dropped + 1);
    assert_eq!(thumb_jobs(&rig, photo), jobs + 1, "then the new look is asked for");

    rig.pool.finish(&key, Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert!(matches!(image(&rig, photo, cx), ImageState::Ready(_)), "the new look lands");
}

/// A photo that leaves the strip's window (±40 around the open one) is let go: its pending
/// frame is released and its late answer is dropped. Its look stays — it says what the
/// tier was asked to show, and binds no other view's request (#151).
#[gpui_kit::test]
fn a_frame_that_leaves_the_strip_is_released(cx: &mut TestAppContext) {
    let rig = rig("dk-cover-window", 43, cx);
    let order = order(&rig, cx);
    select(&rig, order[40], cx);
    let first = order[0];
    let pending = |rig: &Rig, cx: &mut TestAppContext| rig.app.wired.images.read_with(cx, |s, _| s.is_pending(first, ImageKind::Thumb));
    assert!(pending(&rig, cx), "in the window");
    let key = JobKey::photo(first, ImageKind::Thumb);
    rig.pool.start(key.clone());

    assert!(look(&rig, first, cx).is_some());
    select(&rig, order[41], cx);
    assert!(!pending(&rig, cx), "out of the window: released");
    let claimed = rig.app.wired.images.read_with(cx, |s, _| s.is_claimed(first, ImageKind::Thumb));
    assert!(!claimed, "and no longer held by the strip");
    let dropped = stale_dropped(&rig, cx);
    rig.pool.finish(&key, Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert!(!matches!(image(&rig, first, cx), ImageState::Ready(_)));
    assert_eq!(stale_dropped(&rig, cx), dropped + 1);
}

/// **Catalog identity** (map #92). Catalog B's photo and cover carry the same ids and the
/// same cover token as the frame's. The core switches while the frame's render is running:
/// - `catalog:switched` withheld: the render answers in B — refused on the worker, never
///   shown under A's row; then the event arrives and B's thumbnail lands fresh.
/// - delivered first: the store forgets A; the late answer is dropped; Develop on B asks
///   for B's look again.
fn cover_look_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let rig = rig(if delivered { "dk-cover-switch-ev" } else { "dk-cover-switch" }, 2, cx);
    let photo = order(&rig, cx)[1];
    let a = rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
    let version = rig.catalog(|c| {
        let v = c.create_version(photo, "A's").unwrap();
        c.set_cover_version(photo, Some(v)).unwrap();
        v
    });
    refresh_rows(&rig, cx);
    let cover = Some(CoverLook { version, rev: 0 });
    assert_eq!(look(&rig, photo, cx), Some(Look { from: a, cover }));
    let key = JobKey::photo(photo, ImageKind::Thumb);
    rig.pool.start(key.clone());

    let (b, b_ids) = colliding_catalog(&rig.dir, "b", 2);
    let b_version = b.create_version(b_ids[1], "B's").unwrap();
    let token = b.set_cover_version(b_ids[1], Some(b_version)).unwrap();
    assert_eq!((b_ids[1], token), (photo, Some(format!("{version}:0"))), "the ids and the token collide");
    core_switch(&rig.app, b);

    if !delivered {
        let refused_before = refused(&rig, cx);
        rig.pool.finish(&key, Ok(pixels(4, 4)));
        cx.run_until_parked();
        match image(&rig, photo, cx) {
            ImageState::Ready(_) => panic!("B's pixels were shown under A's row"),
            ImageState::Failed(_) => panic!("a refusal is not a failure: it would stick"),
            ImageState::Loading => panic!("the refused render is answered"),
            ImageState::Absent => {}
        }
        assert_eq!(refused(&rig, cx), refused_before + 1, "refused on the worker");
        crate::tests::deliver_switch(&rig.app, cx);
        work(cx);
        assert_eq!(rig.surface(cx), Surface::Library);
        assert_ne!(look(&rig, photo, cx).map(|l| l.from), Some(a), "the switch forgot A's looks");
        // The Library asks for B's thumbnail: not bound to A, so it lands.
        rig.render(cx);
        rig.pool.finish(&key, Ok(pixels(4, 4)));
        cx.run_until_parked();
        assert!(matches!(image(&rig, photo, cx), ImageState::Ready(_)), "B's thumbnail lands");
    } else {
        crate::tests::deliver_switch(&rig.app, cx);
        work(cx);
        assert_eq!(rig.surface(cx), Surface::Library);
        assert_ne!(look(&rig, photo, cx).map(|l| l.from), Some(a), "the switch forgot A's looks");
        let dropped = stale_dropped(&rig, cx);
        rig.pool.finish(&key, Ok(pixels(4, 4)));
        cx.run_until_parked();
        assert_eq!(stale_dropped(&rig, cx), dropped + 1, "A's answer dropped");
        assert!(!matches!(image(&rig, photo, cx), ImageState::Ready(_)));

        // Develop on B's photo: its frame asks for B's look, from B.
        let b = rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
        assert_ne!(a, b);
        let first = order(&rig, cx)[0];
        select(&rig, first, cx);
        rig.app.wired.shell.update(cx, |s, cx| s.open_develop(cx));
        cx.run_until_parked();
        work(cx);
        assert_eq!(look(&rig, photo, cx), Some(Look { from: b, cover }));
        rig.pool.finish(&key, Ok(pixels(4, 4)));
        cx.run_until_parked();
        assert!(matches!(image(&rig, photo, cx), ImageState::Ready(_)), "B's look lands");
    }
}

#[gpui_kit::test]
fn a_cover_look_never_shows_the_new_catalog_before_the_switch_event(cx: &mut TestAppContext) {
    cover_look_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn a_cover_look_never_shows_the_old_catalog_after_the_switch_event(cx: &mut TestAppContext) {
    cover_look_across_a_switch(true, cx);
}

/// A re-root (Preferences → Library folder) reopens the catalog under a new identity and
/// sends no `catalog:switched`. Back in the Library, with the rows re-read, a photo the strip
/// showed is not bound to the catalog the strip read: its next thumbnail (a rotation, an
/// eviction) lands (review rv134 M1). The rows from the reopened catalog replace the earlier
/// looks (#151); a plain request is never bound (`library::tests`).
#[gpui_kit::test]
fn a_re_root_leaves_no_library_thumbnail_bound_to_the_old_catalog(cx: &mut TestAppContext) {
    let rig = rig("dk-reroot", 3, cx);
    let photo = order(&rig, cx)[1];
    let a = rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
    assert_eq!(look(&rig, photo, cx).map(|l| l.from), Some(a));
    let key = JobKey::photo(photo, ImageKind::Thumb);
    rig.pool.finish(&key, Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert!(matches!(image(&rig, photo, cx), ImageState::Ready(_)));

    rig.app.wired.shell.update(cx, |s, cx| s.show_library(cx));
    cx.run_until_parked();
    work(cx);
    assert_eq!(look(&rig, photo, cx).map(|l| l.from), Some(a), "the tier's look stays with the strip gone");
    let new_root = rig.dir.0.join("newroot");
    chairphoto_core::app::catalogs::reroot_open_catalog_as(&rig.app.state, a, new_root).unwrap();
    refresh_rows(&rig, cx);
    work(cx);
    assert_ne!(rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()), Some(a), "a new identity");

    let images = rig.app.wired.images.clone();
    images.update(cx, |s, cx| s.invalidate(photo, cx));
    images.update(cx, |s, _| s.request(photo, ImageKind::Thumb));
    // The grid's own ask for the new rows' look was cancelled by the invalidate; the request
    // goes out once that answer is drained.
    cx.run_until_parked();
    rig.pool.finish(&key, Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert!(matches!(image(&rig, photo, cx), ImageState::Ready(_)), "the Library's thumbnail lands");
    assert_eq!(refused(&rig, cx), 0, "nothing refused");
}

/// A frame rendered across a re-root is refused, not failed: the frame stays empty (asked
/// nothing more while its row still names the old catalog). Rows re-read from the reopened
/// catalog close the Darkroom; the photo's thumbnail is then the Library's again, asked and
/// landing, and Develop asks for the new catalog's look (review rv134 M1).
#[gpui_kit::test]
fn a_frame_refused_across_a_re_root_is_asked_again_for_the_new_rows(cx: &mut TestAppContext) {
    let rig = rig("dk-reroot-strip", 3, cx);
    let photo = order(&rig, cx)[1];
    let a = rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
    let key = JobKey::photo(photo, ImageKind::Thumb);
    rig.pool.start(key.clone());
    let new_root = rig.dir.0.join("newroot");
    chairphoto_core::app::catalogs::reroot_open_catalog_as(&rig.app.state, a, new_root).unwrap();
    let (before, jobs) = (refused(&rig, cx), thumb_jobs(&rig, photo));

    rig.pool.finish(&key, Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert_eq!(refused(&rig, cx), before + 1, "rendered in the reopened catalog: refused");
    assert!(matches!(image(&rig, photo, cx), ImageState::Absent), "empty, not failed");
    rig.render(cx);
    rig.app.wired.shell.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    assert_eq!(thumb_jobs(&rig, photo), jobs, "not asked again under the old row");

    refresh_rows(&rig, cx);
    work(cx);
    let b = rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
    assert_ne!(a, b);
    // Rows from another catalog close the Darkroom (as a switch does), and its strip with it.
    assert_eq!(rig.surface(cx), Surface::Library);
    rig.render(cx);
    assert_eq!(thumb_jobs(&rig, photo), jobs + 1, "the Library asks for it again: the refusal did not stick");
    assert_eq!(look(&rig, photo, cx).map(|l| l.from), Some(b), "the grid's look, from the new rows");
    rig.pool.finish(&key, Ok(pixels(4, 4)));
    cx.run_until_parked();
    assert!(matches!(image(&rig, photo, cx), ImageState::Ready(_)), "it lands");

    // Develop again: the frame is asked for the reopened catalog's look.
    select(&rig, photo, cx);
    rig.app.wired.shell.update(cx, |s, cx| s.open_develop(cx));
    cx.run_until_parked();
    work(cx);
    assert_eq!(look(&rig, photo, cx).map(|l| l.from), Some(b));
}
