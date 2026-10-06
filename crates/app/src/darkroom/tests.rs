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
use chairphoto_core::app::{CoreEvent, CATALOG_CHANGED};
#[cfg(feature = "raw")]
use chairphoto_core::app::EventSink as _;
use chairphoto_core::catalog::{Catalog, CoverPin};
#[cfg(feature = "raw")]
use chairphoto_core::develop::session::DevelopSourceEvent;
use chairphoto_core::develop_source::DevelopSource;
use chairphoto_core::image_pool::{EditJob, JobKey};
use chairphoto_core::plugins::edit::SourceToken;
use chairphoto_model::darkroom::controls::ToneKey;
use crate::image_store::{ImageState, Look};
use chairphoto_core::image_pool::ImageKind;
use chairphoto_model::darkroom::filmstrip::{cover_look, CoverLook, KeyTarget, STRIP_LAYOUT};
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
    assert_eq!(job.catalog, from, "bound to the catalog its row was read from");
    // Nothing selected: nothing to develop.
    rig.app.wired.shell.update(cx, |s, cx| {
        s.show_library(cx);
        s.clear_selection(cx);
        s.open_develop(cx);
    });
    cx.run_until_parked();
    assert_eq!(rig.surface(cx), Surface::Library);
}

// --- frames across a catalog switch (#251) ----------------------------------------------

/// A frame asked for the open photo of catalog A, on a worker when the core switches to B
/// (whose photo has the same id): the real worker body renders nothing for it
/// (`CATALOG_CHANGED`) and the stage drops that answer — no frame of B's photo, and no
/// failure reported. Without `catalog:switched`, the stage's next frame (the settle) is still
/// A's and refused alike; with it, the Darkroom closes and the answer has no stage to reach.
fn frames_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let rig = rig(if delivered { "dk-frame-switch-ev" } else { "dk-frame-switch" }, 1, cx);
    let (photo, a) = rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().map(|o| (o.photo.id, o.from)).unwrap());
    let stage = rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().stage.clone());
    let job = rig.last_edit_job();
    assert_eq!((job.photo_id, job.catalog), (photo, a), "the open's frame, bound to its row's catalog");
    let key = JobKey::Edit(job);
    rig.pool.start(key.clone()); // on a worker: it cannot be cancelled

    let (b, b_ids) = colliding_catalog(&rig.dir, "b", 1);
    assert_eq!(b_ids, rig.ids, "the ids collide");
    core_switch(&rig.app, b);
    if delivered {
        crate::tests::deliver_switch(&rig.app, cx);
        assert_eq!(rig.open_photo(cx), None, "the switch closed the Darkroom");
    }
    let run = crate::image_store::runner(rig.app.state.clone());
    let answer = run(key.clone());
    assert_eq!(answer.as_ref().map(|_| ()).map_err(String::as_str), Err(CATALOG_CHANGED), "rendered in B");
    rig.pool.finish(&key, answer);
    cx.run_until_parked();
    let (frame, failure, failed) = stage.read_with(cx, |s, _| (s.frame().is_some(), s.failure().cloned(), s.stats().failed));
    assert!(!frame, "delivered={delivered}: no frame of B's photo");
    assert_eq!((failure, failed), (None, 0), "delivered={delivered}: dropped, not a failure");
    if !delivered {
        advance(cx, SETTLE * 2); // the open's settle: a full frame, still A's
        let full = rig.last_edit_job();
        assert_eq!((full.max_edge, full.catalog), (FULL_EDGE, a));
        let key = JobKey::Edit(full);
        rig.pool.finish(&key, run(key.clone()));
        cx.run_until_parked();
        let (frame, failure) = stage.read_with(cx, |s, _| (s.frame().is_some(), s.failure().cloned()));
        assert!(!frame && failure.is_none(), "the settle refused alike");
    }
}

#[gpui_kit::test]
fn a_frame_asked_before_an_unannounced_switch_draws_nothing_of_the_new_catalog(cx: &mut TestAppContext) {
    frames_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn a_frame_asked_before_an_announced_switch_draws_nothing_of_the_new_catalog(cx: &mut TestAppContext) {
    frames_across_a_switch(true, cx);
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

/// **Forced interleaving** (#189). The photo is left with its commit on the worker and opened
/// again before that commit lands; the Runner is a pool, so the re-open's reads run first
/// here (the commit held, then released). The versions are not read until the commit has
/// landed: meanwhile the record is not editable (a change is refused), and then the photo
/// shows what the commit wrote. The next change goes into that version, on top of the
/// committed record: one "Version 1", nothing lost.
///
/// `creating`: the commit creates "Version 1", and the photo is stepped away from and back
/// to (→ ←) — not the Original. Else the commit writes Contrast into "Version 1", and the
/// photo is left for the Library and developed again with the shell holding that version as
/// read before the commit landed (as the inspector's list would) — not that stale copy.
fn reopen_during_a_commit(creating: bool, cx: &mut TestAppContext) {
    let rig = rig(if creating { "dk-reopen-create" } else { "dk-reopen-write" }, 2, cx);
    let order = rig.app.wired.shell.read_with(cx, |s, _| s.library.photo_ids());
    let p = order[0];
    if !creating {
        rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
        rig.settle_and_save(cx);
        assert_eq!(rig.catalog(|c| c.list_versions(p).unwrap()).len(), 1);
    }
    let (control, value, key, want) =
        if creating { (ToneKey::Ev, 0.5, "ev", json!(0.5)) } else { (ToneKey::Contrast, 0.3, "contrast", json!(0.3)) };
    rig.slide(Control::Tone(control), value, cx);
    advance(cx, AUTOSAVE_QUIET);
    assert!(rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().saving), "the commit is running");
    let held = cx.update(|cx| Runner::get(cx).hold_pending());
    if creating {
        assert!(rig.view(cx).update(cx, |v, cx| v.step(1, None, cx)));
        cx.run_until_parked();
        assert!(rig.view(cx).update(cx, |v, cx| v.step(-1, None, cx)));
        cx.run_until_parked();
    } else {
        rig.app.wired.shell.update(cx, |s, cx| s.show_library(cx));
        cx.run_until_parked();
        work(cx); // the Library's re-read
        let stale = rig.catalog(|c| c.list_versions(p).unwrap()).remove(0);
        assert_ne!(serde_json::from_str::<Value>(&stale.edit_json).unwrap()["tone"]["contrast"], want, "read before the commit");
        rig.app.wired.shell.update(cx, |s, cx| {
            s.set_active_version(Some(stale), cx);
            s.open_develop(cx);
        });
        cx.run_until_parked();
    }
    assert_eq!(rig.open_photo(cx), Some(p));
    work(cx); // whatever the re-open queued runs before the held commit
    let state = |cx: &mut TestAppContext| {
        rig.darkroom(cx).read_with(cx, |d, _| {
            let o = d.open.as_ref().unwrap();
            (o.loaded, o.editable())
        })
    };
    assert_eq!(state(cx), (false, false), "the versions wait for the commit; changes are refused");
    let shown = rig.working(cx);
    rig.slide(Control::Effect(chairphoto_model::darkroom::controls::EffectKey::Fade), 0.2, cx);
    assert_eq!(rig.working(cx), shown, "refused, not made on a record about to be replaced");

    cx.update(|cx| Runner::get(cx).release(held));
    work(cx); // the commit lands, then the versions are read
    assert_eq!(state(cx), (true, true));
    let versions = rig.catalog(|c| c.list_versions(p).unwrap());
    assert_eq!(versions.len(), 1);
    assert_eq!(rig.version_id(cx), Some(versions[0].id), "the version the commit wrote");
    assert_eq!(rig.working(cx)["tone"][key], want, "its record after the commit");

    rig.slide(Control::Effect(chairphoto_model::darkroom::controls::EffectKey::Fade), 0.2, cx);
    rig.settle_and_save(cx);
    let versions = rig.catalog(|c| c.list_versions(p).unwrap());
    let names: Vec<&str> = versions.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, ["Version 1"], "no second \"Version 1\"");
    let saved: Value = serde_json::from_str(&versions[0].edit_json).unwrap();
    assert_eq!((saved["tone"]["ev"].clone(), saved["tone"][key].clone()), (json!(0.5), want), "{saved}");
    assert_eq!(saved["fade"], json!(0.2), "{saved}");
}

#[gpui_kit::test]
fn reopening_during_the_commit_that_creates_the_version_waits_for_it(cx: &mut TestAppContext) {
    reopen_during_a_commit(true, cx);
}

#[gpui_kit::test]
fn developing_again_during_a_commit_reads_the_version_it_wrote(cx: &mut TestAppContext) {
    reopen_during_a_commit(false, cx);
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

/// A version operation queued behind a running autosave (a cover toggle, here) and then
/// left with the photo (→) is not done — and the status line says so (#204 N2), instead of
/// it vanishing with the view. The autosave itself still lands.
#[gpui_kit::test]
fn a_queued_version_operation_dropped_by_leaving_is_reported(cx: &mut TestAppContext) {
    let rig = rig("dk-ops-dropped", 2, cx);
    let p = rig.open_photo(cx).unwrap();
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    advance(cx, AUTOSAVE_QUIET);
    assert!(rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().saving), "the commit is running");
    rig.darkroom(cx).update(cx, |d, cx| d.toggle_cover(cx));
    let line_before = crate::tests::status(&rig.app, cx);
    assert!(rig.view(cx).update(cx, |v, cx| v.step(1, None, cx)));
    cx.run_until_parked();
    let line = crate::tests::status(&rig.app, cx);
    assert_ne!(line, line_before);
    assert!(line.starts_with("Darkroom: ") && line.ends_with("was left before a version operation could run — not done"), "{line}");
    work(cx);
    let versions = rig.catalog(|c| c.list_versions(p).unwrap());
    assert_eq!(versions.len(), 1, "the autosave landed");
    assert_eq!(rig.catalog(|c| c.cover_pin(p).unwrap()), CoverPin::Auto, "the cover was not pinned");
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

// --- the develop session's order (#203, #225) -------------------------------------------------

/// What the develop session's calls did, in the order the worker ran them, and the photo the
/// session is claimed for afterwards — core's rule: an open claims, a close trips the claim,
/// and either takes effect only if no call made after it already has (`DevelopOrder`, the
/// real one on the rig's state). `calls` lists what took effect; `ran` everything that ran.
#[derive(Default)]
struct SessionLog {
    ran: Vec<String>,
    calls: Vec<String>,
    claimed: Option<i64>,
}

/// What a recorded open answers for `photo_id`: its RAW, resident — so the stage moving to
/// `w:{photo_id}:1` shows the answer reached the Darkroom.
fn resident_raw(photo_id: i64) -> DevelopSource {
    DevelopSource::Raw {
        camera: "Test".into(),
        megapixels: 1.0,
        bits: 16,
        decoder: "test".into(),
        token: Some(format!("w:{photo_id}:1")),
        camera_ev: None,
        as_shot_wb: None,
        lens: None,
    }
}

fn record_session_calls(rig: &Rig, cx: &mut TestAppContext) -> Arc<std::sync::Mutex<SessionLog>> {
    let log = Arc::new(std::sync::Mutex::new(SessionLog::default()));
    let (on_open, on_close) = (log.clone(), log.clone());
    let calls = super::session::DevelopCalls {
        open: Arc::new(move |state, ticket, _, photo_id, _| {
            on_open.lock().unwrap().ran.push(format!("open {photo_id}"));
            let claimed = state.jobs.develop_order.apply(ticket, || {
                let mut l = on_open.lock().unwrap();
                l.calls.push(format!("open {photo_id}"));
                l.claimed = Some(photo_id);
                Ok(())
            })?;
            match claimed {
                Some(()) => Ok(resident_raw(photo_id)),
                None => Err(chairphoto_core::app::editing::DEVELOP_SUPERSEDED.into()),
            }
        }),
        close: Arc::new(move |state, ticket| {
            on_close.lock().unwrap().ran.push("close".into());
            state.jobs.develop_order.apply(ticket, || {
                let mut l = on_close.lock().unwrap();
                l.calls.push("close".into());
                l.claimed = None;
                Ok(())
            })?;
            Ok(())
        }),
    };
    rig.darkroom(cx).update(cx, |d, _| d.set_develop_calls(calls));
    log
}

/// The Runner's workers may take what is queued now newest first: run it so, then the rest.
fn work_newest_first_then_rest(cx: &mut TestAppContext) {
    cx.update(|cx| Runner::get(cx).run_pending_reversed());
    cx.run_until_parked();
    work(cx);
}

/// **Forced order** (#203). ← Library and straight back to Develop: the close of the session
/// left and the open of the new one are both on the pool, and the pool runs the newer first.
/// The close, made first, takes no effect after the open, so it never trips the claim the
/// open made (core's `prepare` then stops without a word: the RAW stays "preparing" and
/// nothing autosaves).
#[gpui_kit::test]
fn a_quick_return_to_develop_opens_the_session_after_the_close(cx: &mut TestAppContext) {
    let rig = rig("dk-session-reopen", 2, cx);
    let p = rig.open_photo(cx).unwrap();
    let log = record_session_calls(&rig, cx);
    rig.app.wired.shell.update(cx, |s, cx| s.show_library(cx));
    cx.run_until_parked();
    rig.app.wired.shell.update(cx, |s, cx| s.open_develop(cx));
    cx.run_until_parked();
    assert_eq!(rig.open_photo(cx), Some(p));
    work_newest_first_then_rest(cx);
    let log = log.lock().unwrap();
    assert_eq!(log.ran, [format!("open {p}"), "close".to_string()], "the pool ran the open first");
    assert_eq!(log.calls, [format!("open {p}")], "the late close took no effect");
    assert_eq!(log.claimed, Some(p), "the session is held for the photo on the stage");
}

/// **Forced order** (#203). Two quick steps (→ →): the first step's open is overtaken on the
/// pool by the second's. The older open takes no effect after the newer one, so the session
/// ends up claimed for the photo on the stage, not the one stepped past.
#[gpui_kit::test]
fn quick_steps_claim_the_session_for_the_last_photo(cx: &mut TestAppContext) {
    let rig = rig("dk-session-steps", 3, cx);
    let order = rig.app.wired.shell.read_with(cx, |s, _| s.library.photo_ids());
    let log = record_session_calls(&rig, cx);
    for _ in 0..2 {
        assert!(rig.view(cx).update(cx, |v, cx| v.step(1, None, cx)));
        cx.run_until_parked();
    }
    assert_eq!(rig.open_photo(cx), Some(order[2]));
    work_newest_first_then_rest(cx);
    let log = log.lock().unwrap();
    assert_eq!(log.calls.last(), Some(&format!("open {}", order[2])), "{:?}", log.calls);
    assert_eq!(log.claimed, Some(order[2]), "{:?}", log.calls);
}

/// **Forced order** (#225, review probe P6). The open of photo 2 stalls on the worker (a RAW
/// header on a share that stopped answering); the user steps on to photo 3. Photo 3's open
/// runs at once — it does not wait behind the stuck one — claims the session, and its RAW
/// reaches the stage. When photo 2's open finally answers it takes nothing: the session
/// stays photo 3's, and the stage stays on photo 3's RAW.
#[gpui_kit::test]
fn a_stuck_open_does_not_hold_back_the_next_photos_session(cx: &mut TestAppContext) {
    let rig = rig("dk-session-stuck", 3, cx);
    let order = rig.app.wired.shell.read_with(cx, |s, _| s.library.photo_ids());
    work(cx);
    let log = record_session_calls(&rig, cx);
    assert!(rig.view(cx).update(cx, |v, cx| v.step(1, None, cx)));
    cx.run_until_parked();
    let stuck = cx.update(|cx| Runner::get(cx).hold_pending());
    assert!(!stuck.is_empty(), "photo 2's open is on the worker");
    assert!(rig.view(cx).update(cx, |v, cx| v.step(1, None, cx)));
    cx.run_until_parked();
    assert_eq!(rig.open_photo(cx), Some(order[2]));
    work(cx);
    {
        let log = log.lock().unwrap();
        assert_eq!(log.ran, [format!("open {}", order[2])], "photo 3's open ran while photo 2's is stuck");
        assert_eq!(log.claimed, Some(order[2]));
    }
    let working = SourceToken::Working { photo_id: order[2], generation: 1 };
    assert_eq!(rig.last_edit_job().source, working, "photo 3's RAW is on the stage");

    cx.update(|cx| Runner::get(cx).release(stuck));
    work(cx);
    let log = log.lock().unwrap();
    assert_eq!(log.ran, [format!("open {}", order[2]), format!("open {}", order[1])], "photo 2's open answered late");
    assert_eq!(log.calls, [format!("open {}", order[2])], "and took nothing");
    assert_eq!(log.claimed, Some(order[2]), "the session is still photo 3's");
    assert_eq!(rig.last_edit_job().source, working, "the stage did not move");
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
///
/// `CoreEvent::DevelopSource` exists only with `raw` (#239), so this test — which simulates
/// that event — needs it too. `darkroom` itself only needs `edit`.
#[cfg(feature = "raw")]
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

/// **Forced interleaving** (map #92, Catalog identity; #204). A photo left (→) with its commit
/// A on the worker and a newer change B to follow it (`commit_again`); A's write runs in the
/// catalog it was made for, then the core switches to a catalog whose photo and version carry
/// the same ids before A's answer reaches the UI — with and without `catalog:switched`. The
/// chained commit B, started by that answer, is refused by the new catalog: nothing is
/// written there, and the status line names the photo and why.
fn a_chained_commit_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let rig = rig(if delivered { "dk-chain-switch-ev" } else { "dk-chain-switch" }, 2, cx);
    let p = rig.app.wired.shell.read_with(cx, |s, _| s.library.photo_ids())[0];
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    rig.settle_and_save(cx);
    let v = rig.catalog(|c| c.list_versions(p).unwrap())[0].id;
    rig.slide(Control::Tone(ToneKey::Contrast), 0.3, cx);
    advance(cx, AUTOSAVE_QUIET);
    assert!(rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().saving), "commit A is on the worker");
    rig.slide(Control::Tone(ToneKey::Shadows), 0.2, cx); // B, while A runs
    assert!(rig.view(cx).update(cx, |v, cx| v.step(1, None, cx)));
    cx.run_until_parked();
    // A's write runs in its own catalog; its answer waits on the UI.
    cx.update(|cx| Runner::get(cx).run_pending());
    assert_eq!(rig.saved(v)["tone"]["contrast"], json!(0.3), "A landed in the catalog it was made for");
    let (b, b_ids) = colliding_catalog(&rig.dir, "b", 2);
    let b_version = b.create_version(b_ids[0], "B's").unwrap();
    assert_eq!((b_ids[0], b_version), (p, v), "the ids collide");
    core_switch(&rig.app, b);
    if delivered {
        let event = CoreEvent::CatalogSwitched("switched.chairphoto".into());
        rig.app.wired.model.update(cx, |m, cx| m.on_core_event(&event, cx));
    }
    cx.run_until_parked(); // A's answer: B goes to the worker
    work(cx);
    advance(cx, AUTOSAVE_QUIET * 3);
    work(cx);
    rig.catalog(|c| {
        let vs = c.list_versions(p).unwrap();
        assert_eq!(vs.iter().map(|v| (v.name.as_str(), v.edit_json.as_str())).collect::<Vec<_>>(), [("B's", "{}")]);
        assert!(c.version_history(v).unwrap().steps.is_empty(), "no step in the new catalog");
    });
    let line = crate::tests::status(&rig.app, cx);
    assert!(line.starts_with("Autosave failed for ") && line.ends_with(CATALOG_CHANGED), "{line}");
    assert_eq!(cx.update(|cx| Runner::get(cx).pending()), 0, "no retry loop");
}

#[gpui_kit::test]
fn a_chained_commit_never_reaches_the_new_catalog_before_the_switch_event(cx: &mut TestAppContext) {
    a_chained_commit_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn a_chained_commit_never_reaches_the_new_catalog_after_the_switch_event(cx: &mut TestAppContext) {
    a_chained_commit_across_a_switch(true, cx);
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

    // The cover: pinned, then unpinned — the face is then the version changed last (#252).
    rig.darkroom(cx).update(cx, |d, cx| d.toggle_cover(cx));
    work(cx);
    assert_eq!(rig.catalog(|c| c.cover_pin(photo).unwrap()), CoverPin::Version(v1));
    assert_eq!(rig.catalog(|c| c.cover_of(photo).unwrap().map(|c| c.0)), Some(v1));
    assert!(rig.present("dk-cover", cx));
    rig.darkroom(cx).update(cx, |d, cx| d.toggle_cover(cx));
    work(cx);
    assert_eq!(rig.catalog(|c| c.cover_pin(photo).unwrap()), CoverPin::Auto);
    let v3 = rig.versions().last().unwrap().id;
    assert_eq!(rig.catalog(|c| c.cover_of(photo).unwrap().map(|c| c.0)), Some(v3));
}

// --- the Library face (#252) ------------------------------------------------------------------

/// The face follows the version changed last, the bar saying so; "☆ Use as cover" pins the
/// version shown, or the Original when that is shown; "★ Cover" unpins.
#[gpui_kit::test]
fn the_face_follows_the_latest_edit_until_a_version_or_the_original_is_pinned(cx: &mut TestAppContext) {
    let rig = rig("dk-face", 1, cx);
    let photo = rig.ids[0];
    let face = |rig: &Rig| rig.catalog(|c| c.cover_of(photo).unwrap().map(|f| f.0));
    let pin = |rig: &Rig, cx: &mut TestAppContext| rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().pin);
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    rig.settle_and_save(cx);
    let v1 = rig.versions()[0].id;
    assert_eq!(face(&rig), Some(v1), "the first change's version is the face");
    assert!(rig.present("dk-face-auto", cx), "the bar says the face follows the latest edit");
    rig.darkroom(cx).update(cx, |d, cx| d.new_version(cx));
    work(cx);
    let v2 = rig.version_id(cx).unwrap();
    assert_eq!(face(&rig), Some(v2));
    // Back on version 1, a change: the face follows it.
    rig.darkroom(cx).update(cx, |d, cx| d.switch_version(Some(v1), cx));
    work(cx);
    assert_eq!(face(&rig), Some(v2), "opening a version is not a change");
    rig.slide(Control::Tone(ToneKey::Contrast), 0.3, cx);
    rig.settle_and_save(cx);
    assert_eq!(face(&rig), Some(v1));

    // Pinned: version 2's later change leaves the face on version 1.
    rig.darkroom(cx).update(cx, |d, cx| d.toggle_cover(cx));
    work(cx);
    assert_eq!(pin(&rig, cx), CoverPin::Version(v1));
    assert!(!rig.present("dk-face-auto", cx));
    rig.darkroom(cx).update(cx, |d, cx| d.switch_version(Some(v2), cx));
    work(cx);
    rig.slide(Control::Tone(ToneKey::Contrast), -0.2, cx);
    rig.settle_and_save(cx);
    assert_eq!(face(&rig), Some(v1), "pinned beats the later change");

    // On the Original, the toggle pins the original.
    rig.darkroom(cx).update(cx, |d, cx| d.switch_version(None, cx));
    work(cx);
    rig.darkroom(cx).update(cx, |d, cx| d.toggle_cover(cx));
    work(cx);
    assert_eq!(rig.catalog(|c| c.cover_pin(photo).unwrap()), CoverPin::Original);
    assert_eq!(pin(&rig, cx), CoverPin::Original);
    assert_eq!(face(&rig), None, "the original is the face");
    let row = rig.app.wired.shell.read_with(cx, |s, _| s.library.photos().iter().find(|p| p.id == photo).cloned()).unwrap();
    assert_eq!((row.cover_pin, row.cover_token), (CoverPin::Original, None), "the rows were re-read");
    // "★ Cover" on the Original unpins: the face is the version changed last again.
    rig.darkroom(cx).update(cx, |d, cx| d.toggle_cover(cx));
    work(cx);
    assert_eq!(rig.catalog(|c| c.cover_pin(photo).unwrap()), CoverPin::Auto);
    assert_eq!(face(&rig), Some(v2));
    assert!(rig.present("dk-face-auto", cx));
}

/// A duel's ⑂ banks a "What-if" version beside the one edited, and the face stays where it
/// was (#252 decision, 2026-10-06); "+ New version" still moves it.
#[gpui_kit::test]
fn a_banked_what_if_does_not_move_the_face(cx: &mut TestAppContext) {
    use super::view::Overlay;
    let rig = rig("dk-face-whatif", 1, cx);
    let photo = rig.ids[0];
    let token = |rig: &Rig| rig.catalog(|c| c.get_photo(photo).unwrap().cover_token);
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    rig.settle_and_save(cx);
    let v1 = rig.versions()[0].id;
    let before = token(&rig);
    assert!(before.as_deref().is_some_and(|t| t.starts_with(&format!("{v1}:"))), "{before:?}");
    rig.with_view(cx, |v, window, cx| v.open_duel(window, cx));
    let duel = rig.view(cx).read_with(cx, |v, _| match v.overlay() {
        Some(Overlay::Duel(d)) => d.clone(),
        _ => panic!("the duel is mounted"),
    });
    duel.update(cx, |d, cx| d.fork(1, cx));
    work(cx);
    let versions = rig.versions();
    assert_eq!(versions.len(), 2, "the variant was banked");
    assert!(versions[1].name.starts_with("What-if — "), "{:?}", versions[1].name);
    assert_eq!(token(&rig), before, "the face and its look are unchanged");
    assert_eq!(rig.version_id(cx), Some(v1), "the version edited stays the same");
    rig.press("escape", cx);
    rig.darkroom(cx).update(cx, |d, cx| d.new_version(cx));
    work(cx);
    let v3 = rig.version_id(cx).unwrap();
    assert!(token(&rig).is_some_and(|t| t.starts_with(&format!("{v3}:"))), "+ New version moves the face");
}

/// Review of #252, N3: the pinned version deleted elsewhere (the Inspector's ✕, which then
/// re-reads the rows) while Develop shows the photo lifts the pin, and the bar's ★ follows
/// once the rows land — not only when the photo is opened again.
#[gpui_kit::test]
fn the_pin_follows_a_pinned_version_deleted_elsewhere(cx: &mut TestAppContext) {
    let rig = rig("dk-pin-deleted", 1, cx);
    let photo = rig.ids[0];
    let pin = |rig: &Rig, cx: &mut TestAppContext| rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().pin);
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    rig.settle_and_save(cx);
    let v1 = rig.versions()[0].id;
    rig.darkroom(cx).update(cx, |d, cx| d.new_version(cx));
    work(cx);
    let v2 = rig.version_id(cx).unwrap();
    rig.darkroom(cx).update(cx, |d, cx| d.switch_version(Some(v1), cx));
    work(cx);
    rig.darkroom(cx).update(cx, |d, cx| d.toggle_cover(cx));
    work(cx);
    assert_eq!(pin(&rig, cx), CoverPin::Version(v1));
    rig.darkroom(cx).update(cx, |d, cx| d.switch_version(Some(v2), cx));
    work(cx);
    assert!(!rig.present("dk-face-auto", cx));
    // Deleted from the Inspector: the catalog lifts the pin, and the rows are read again.
    rig.catalog(|c| c.delete_version(v1).unwrap());
    refresh_rows(&rig, cx);
    assert_eq!(rig.catalog(|c| c.cover_pin(photo).unwrap()), CoverPin::Auto);
    assert_eq!(pin(&rig, cx), CoverPin::Auto, "the pin follows the row");
    assert!(rig.present("dk-face-auto", cx), "the bar says the face follows the latest edit again");
}

/// **Catalog identity.** Pinning the Original after the core switched to a catalog with
/// colliding ids writes nothing there: refused with the event withheld; with it delivered,
/// Develop has closed and there is nothing to pin.
fn pin_original_across_a_switch(delivered: bool, cx: &mut TestAppContext) {
    let rig = rig(if delivered { "dk-pin-switch-ev" } else { "dk-pin-switch" }, 1, cx);
    let photo = rig.ids[0];
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    rig.settle_and_save(cx);
    rig.darkroom(cx).update(cx, |d, cx| d.switch_version(None, cx));
    work(cx);
    let (b, b_ids) = colliding_catalog(&rig.dir, "b", 1);
    let b_version = b.create_version(b_ids[0], "B's").unwrap();
    assert_eq!((b_ids[0], b_version), (photo, rig.versions()[0].id), "the ids collide");
    let b_token = b.get_photo(photo).unwrap().cover_token;
    core_switch(&rig.app, b);
    if delivered {
        crate::tests::deliver_switch(&rig.app, cx);
        work(cx);
    }
    rig.darkroom(cx).update(cx, |d, cx| d.toggle_cover(cx));
    work(cx);
    if !delivered {
        let error = rig.darkroom(cx).read_with(cx, |d, _| d.error.clone()).unwrap_or_default();
        assert!(error.contains(CATALOG_CHANGED), "{error}");
    } else {
        assert!(rig.darkroom(cx).read_with(cx, |d, _| d.open.is_none()), "Develop closed on the switch");
    }
    rig.catalog(|c| {
        assert_eq!(c.cover_pin(photo).unwrap(), CoverPin::Auto, "nothing pinned in the new catalog");
        assert_eq!(c.get_photo(photo).unwrap().cover_token, b_token, "its face untouched");
    });
}

#[gpui_kit::test]
fn pinning_the_original_never_writes_the_new_catalog_before_the_switch_event(cx: &mut TestAppContext) {
    pin_original_across_a_switch(false, cx);
}

#[gpui_kit::test]
fn pinning_the_original_never_writes_the_new_catalog_after_the_switch_event(cx: &mut TestAppContext) {
    pin_original_across_a_switch(true, cx);
}

/// **Forced interleaving** (#202). "+ New version" on the worker (held by `Runner::manual`),
/// a change made meanwhile (kept: the fork copies the record), then the photo is left — by a
/// step (→) or ← Library — before the fork lands: the change is saved into the new version,
/// as it is when the photo stays open, and the version it was copied from keeps what it
/// held.
fn a_change_during_new_version_then_leaving(leave: bool, cx: &mut TestAppContext) {
    let rig = rig(if leave { "dk-fork-leave" } else { "dk-fork-step" }, 2, cx);
    let p = rig.app.wired.shell.read_with(cx, |s, _| s.library.photo_ids())[0];
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    rig.settle_and_save(cx);
    let v1 = rig.catalog(|c| c.list_versions(p).unwrap())[0].id;
    rig.darkroom(cx).update(cx, |d, cx| d.new_version(cx));
    cx.run_until_parked();
    assert!(rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().unwrap().busy()), "the fork is on the worker");
    rig.slide(Control::Tone(ToneKey::Contrast), 0.3, cx);
    assert_eq!(rig.working(cx)["tone"]["contrast"], json!(0.3), "kept: the fork does not replace the record");
    if leave {
        rig.app.wired.shell.update(cx, |s, cx| s.show_library(cx));
        cx.run_until_parked();
        assert_eq!(rig.surface(cx), Surface::Library);
    } else {
        assert!(rig.view(cx).update(cx, |v, cx| v.step(1, None, cx)));
        cx.run_until_parked();
        assert_ne!(rig.open_photo(cx), Some(p));
    }
    work(cx);
    let versions = rig.catalog(|c| c.list_versions(p).unwrap());
    let names: Vec<&str> = versions.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, ["Version 1", "Version 2"]);
    let v2 = versions[1].id;
    assert_eq!(rig.saved(v1)["tone"]["contrast"], json!(0), "the version copied from is untouched");
    assert_eq!(rig.saved(v1)["tone"]["ev"], json!(0.5));
    assert_eq!(rig.saved(v2)["tone"]["contrast"], json!(0.3), "the change belongs to the new version");
    assert_eq!(rig.saved(v2)["tone"]["ev"], json!(0.5), "the copy");
    assert_eq!(rig.labels(v2).0.last().map(String::as_str), Some("Contrast +0.30"));
    assert_eq!(rig.darkroom(cx).read_with(cx, |d, _| d.error.clone()), None);
}

#[gpui_kit::test]
fn a_change_during_new_version_then_a_step_is_saved_into_the_new_version(cx: &mut TestAppContext) {
    a_change_during_new_version_then_leaving(false, cx);
}

#[gpui_kit::test]
fn a_change_during_new_version_then_leaving_is_saved_into_the_new_version(cx: &mut TestAppContext) {
    a_change_during_new_version_then_leaving(true, cx);
}

/// **Forced interleaving.** A version switch and "Develop with the new engine" replace the
/// record: a change made while either is on the worker is refused (the rail is not
/// editable), not made on screen and then silently dropped as React did. The version left
/// keeps what it held; the one arrived at shows its own record. "+ New version" copies the
/// record instead, so a change made while it is written is kept and saved into it.
///
/// The new-engine-fork half simulates `CoreEvent::DevelopSource`, which exists only with
/// `raw` (#239); `darkroom` itself only needs `edit`.
#[cfg(feature = "raw")]
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
///
/// Simulates `CoreEvent::DevelopSource`, which exists only with `raw` (#239); `darkroom`
/// itself only needs `edit`.
#[cfg(feature = "raw")]
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

/// The proof sheet and the duel belong to one open (`OpenPhoto::seq`), not to a photo id
/// (#204 N1): the same photo opened again in one update — its rows now from another catalog
/// identity (a re-root) — closes the overlay, whose picks would otherwise apply to the new
/// open.
#[gpui_kit::test]
fn the_overlay_closes_when_the_same_photo_is_opened_again(cx: &mut TestAppContext) {
    use super::view::Overlay;
    let rig = rig("dk-overlay-reopen", 2, cx);
    let photo = rig.open_photo(cx).unwrap();
    work(cx); // the auto-tone fragment
    rig.with_view(cx, |v, window, cx| v.open_proof_sheet(window, cx));
    assert!(rig.view(cx).read_with(cx, |v, _| matches!(v.overlay(), Some(Overlay::Proof(_)))), "the proof sheet is up");
    let seq = |cx: &mut TestAppContext| rig.darkroom(cx).read_with(cx, |d, _| d.open.as_ref().map(|o| (o.seq, o.from)).unwrap());
    let (before, a) = seq(cx);
    chairphoto_core::app::catalogs::reroot_open_catalog_as(&rig.app.state, a, rig.dir.0.join("newroot")).unwrap();
    let b = chairphoto_core::app::catalog_identity(&rig.app.state).unwrap();
    assert_ne!(a, b);
    rig.app.wired.shell.update(cx, |s, cx| {
        s.set_rows_from_undrawn(b);
        cx.notify();
    });
    cx.run_until_parked();
    let (after, from) = seq(cx);
    assert_eq!((rig.open_photo(cx), from), (Some(photo), b), "the same photo, opened again under the new identity");
    assert_ne!(after, before);
    assert!(rig.view(cx).read_with(cx, |v, _| v.overlay().is_none()), "the overlay closed with the open it belonged to");
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

// --- the duel's layout (#178) -------------------------------------------------------------

/// The duel draws each variant whole inside its pane (`darkroom.css`: the image `flex: 1;
/// min-height: 0; object-fit: contain`) with "This one" and ⑂ below it, never over it: on a
/// photo stored landscape with a rotating EXIF orientation (its renders come upright, so
/// portrait), then landscape and tall portrait variants as later rounds render them.
#[gpui_kit::test]
fn the_duel_fits_each_variant_above_its_buttons(cx: &mut TestAppContext) {
    use super::view::Overlay;
    use crate::loupe::duel::DUEL_EDGE;
    use crate::loupe::fit_tests::{assert_fitted, drawn, store_rotated, LANDSCAPE, PORTRAIT};
    let rig = rig_with("dk-duel-fit", 1, |rig, _| store_rotated(&rig.app, rig.ids[0]), cx);
    rig.with_view(cx, |v, window, cx| v.open_duel(window, cx));
    let duel = rig.view(cx).read_with(cx, |v, _| match v.overlay() {
        Some(Overlay::Duel(d)) => d.clone(),
        _ => panic!("the duel is mounted"),
    });
    for (round, image) in [(1, PORTRAIT), (2, LANDSCAPE), (3, (100, 400))] {
        assert_eq!(duel.read_with(cx, |d, _| d.round()), round);
        for job in rig.edit_jobs().into_iter().filter(|j| j.max_edge == DUEL_EDGE) {
            rig.pool.finish(&JobKey::Edit(job), Ok(pixels(image.0, image.1)));
        }
        cx.run_until_parked();
        rig.render(cx);
        cx.update_window(rig.app.window(), |_, window, _| {
            let viewport = window.viewport_size();
            for i in 0..2u64 {
                let what = format!("round {round}, pane {i}");
                let boxed = window.find(("duel-image-box", i)).bounds();
                let picture = drawn(window.find(("duel-image", i)).bounds(), image);
                assert_fitted(&what, picture, boxed, image);
                let bar = window.find(("duel-pane-bar", i)).bounds();
                let pick = window.find(format!("duel-pick-{i}")).bounds();
                assert!(bar.origin.y >= boxed.bottom(), "{what}: the buttons start below the image ({bar:?} under {boxed:?})");
                assert!(bar.contains(&pick.center()), "{what}: \"This one\" is in the bar under the image");
                assert!(bar.bottom() <= viewport.height, "{what}: the buttons are on screen ({bar:?} in {viewport:?})");
            }
        })
        .unwrap();
        rig.press("down", cx); // "same": the next round's variants
    }
}

/// #197: the duel's "← This one" / "This one →" buttons draw Lucide's arrow icons at the
/// app's 13 px stroke-icon size, not the UI font's tiny fallback mark for U+2190/U+2192 — the
/// icon leads the text on the left pane and trails it on the right, as the arrows did.
#[gpui_kit::test]
fn the_duel_pick_buttons_draw_normal_sized_arrow_icons(cx: &mut TestAppContext) {
    let rig = rig("dk-duel-arrows", 1, cx);
    rig.with_view(cx, |v, window, cx| v.open_duel(window, cx));
    rig.render(cx);
    cx.update_window(rig.app.window(), |_, window, _| {
        for i in 0..2u64 {
            let chip = window.find(format!("duel-pick-{i}")).bounds();
            let arrow = window.find(format!("duel-pick-arrow-{i}")).bounds();
            assert_eq!(arrow.size.width, gpui_kit::px(13.), "pane {i}'s arrow is the 13 px icon, not a fallback glyph");
            assert_eq!(arrow.size.height, gpui_kit::px(13.), "pane {i}'s arrow is the 13 px icon, not a fallback glyph");
            if i == 0 {
                assert!(arrow.center().x < chip.center().x, "pane 0: the arrow leads \"This one\"");
            } else {
                assert!(arrow.center().x > chip.center().x, "pane 1: the arrow follows \"This one\"");
            }
        }
    })
    .unwrap();
}

/// #197 L5: `dk-back` ("← Library") drew its arrow as the literal "←" character — the UI
/// font lacks U+2190, so the fallback font it reaches for draws it at a fraction of the
/// surrounding text's size, the same bug #197 fixed for the duel, the cull help and the other
/// back chips (owed-prev/next, debt-prev/next, faces-sheet-back, loupe-card-back). It now
/// draws Lucide's `ArrowLeft` beside "Library", like those, with an explicit accessible name
/// that keeps the arrow semantic — matching this same follow-up's restoration of the other
/// back chips' accessible names.
/// (Mutation-checked: dropping the `.aria_label("← Library")` call leaves the chip with
/// whatever its children's own implicit accessible name derives to instead, which is not
/// "← Library"; this fails.)
#[gpui_kit::test]
fn dk_back_draws_an_icon_and_keeps_its_accessible_name(cx: &mut TestAppContext) {
    let rig = rig("dk-back-aria", 1, cx);
    rig.render(cx);
    let label =
        cx.update_window(rig.app.window(), |_, window, _| window.find("dk-back").label().map(str::to_string)).unwrap();
    assert_eq!(label.as_deref(), Some("← Library"));
}

/// The proof sheet's cells share the duel's picture helper: a portrait proof's picture element
/// is laid out as its 3:2 cell, not taller (it paints with `cover` there, as React's
/// `.dk-proof-cell img` does — the fit mode itself is not observable here).
#[gpui_kit::test]
fn a_proof_cells_picture_is_laid_out_as_its_cell(cx: &mut TestAppContext) {
    use crate::loupe::fit_tests::PORTRAIT;
    use crate::loupe::proof_sheet::PROOF_EDGE;
    let rig = rig("dk-proof-fit", 1, cx);
    rig.with_view(cx, |v, window, cx| v.open_proof_sheet(window, cx));
    for job in rig.edit_jobs().into_iter().filter(|j| j.max_edge == PROOF_EDGE) {
        rig.pool.finish(&JobKey::Edit(job), Ok(pixels(PORTRAIT.0, PORTRAIT.1)));
    }
    cx.run_until_parked();
    rig.render(cx);
    cx.update_window(rig.app.window(), |_, window, _| {
        let cell = window.find(("proof-image", 0u64)).bounds();
        assert_eq!(cell.size.height, gpui_kit::px(160.), "the picture is its cell's height: {cell:?}");
        assert!(cell.size.width > cell.size.height, "and its width, a 3:2 cell: {cell:?}");
    })
    .unwrap();
}

/// #186: the filmstrip's frames fill their cell inside its 2 px ring — portrait, landscape
/// and rotated thumbnails — not a portrait element as wide as the cell and taller than it.
#[gpui_kit::test]
fn the_filmstrips_frames_fill_their_cell(cx: &mut TestAppContext) {
    use crate::loupe::fit_tests::{assert_fills, store_rotated, LANDSCAPE, PORTRAIT};
    let rig = rig_with("dk-strip-fit", 3, |rig, _| store_rotated(&rig.app, rig.ids[2]), cx);
    let frames = [(rig.ids[0], LANDSCAPE), (rig.ids[1], PORTRAIT), (rig.ids[2], PORTRAIT)];
    rig.render(cx);
    for &(id, (w, h)) in &frames {
        rig.pool.finish(&JobKey::photo(id, ImageKind::Thumb), Ok(pixels(w, h)));
    }
    cx.run_until_parked();
    rig.render(cx);
    cx.update_window(rig.app.window(), |_, window, _| {
        for &(id, image) in &frames {
            let cell = gpui_kit::SharedString::from(format!("dk-strip-{id}"));
            assert_fills(&format!("strip frame {id} {image:?}"), window, cell, ("dk-strip-picture", id as u64), 2.);
        }
    })
    .unwrap();
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
        assert_eq!(c.cover_pin(photo).unwrap(), CoverPin::Auto, "no cover pinned in the new catalog");
        assert_eq!(c.get_setting(chairphoto_model::presets::USER_PRESETS_KEY).unwrap(), None, "no preset saved there");
        assert!(c.version_history(v).unwrap().steps.is_empty(), "no step taken there");
        assert_eq!(c.list_versions(photo).unwrap()[0].edit_json, "{}");
    });
}

/// The Darkroom's keys stand down while the proof sheet or the duel is up (React's
/// filmstrip `keysDisabled`): under the proof sheet — which now binds its own ← / → / ↑ / ↓
/// to move its own cell focus (#250 follow-up) — none of that steps the filmstrip, zooms to
/// the crop, or moves the history, and Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y do nothing either;
/// under the duel — which binds the arrows, ↓ and Esc — Ctrl+Z, Ctrl+Y and Enter do nothing
/// either. Each key is first shown to act with no overlay up. Enter's own "no-op from the
/// backdrop" is checked once the arrows' focus moves are backed out, since a focused cell's
/// Enter now adopts it by design — that is the sheet's own business, covered in
/// `loupe::tests::overlays`, not this test's.
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

    // Under the proof sheet: nothing Darkroom's own — including ← / → / ↑ / ↓, which now
    // move the sheet's own cell focus (#250 follow-up) instead of doing nothing at all, but
    // still never reach the filmstrip, the crop zoom or the history.
    rig.with_view(cx, |v, window, cx| v.open_proof_sheet(window, cx));
    assert_eq!(overlay(cx), "proof");
    for key in ["right", "left", "up", "down", "ctrl-z", "ctrl-shift-z", "ctrl-y"] {
        rig.press(key, cx);
        work(cx);
        assert_eq!(rig.open_photo(cx), photo, "{key} under the proof sheet does not step the filmstrip");
        assert!(stage_view(cx).is_fit(), "{key} under the proof sheet does not zoom");
        assert_eq!(rig.labels(v).1, head, "{key} under the proof sheet does not move the history");
        assert_eq!(overlay(cx), "proof", "{key}: the sheet stays up");
    }
    // The arrows above focused a cell of the sheet's own; refocus the backdrop so Enter's
    // own no-op (nothing of the sheet's is focused) is what this checks, not an adopt.
    rig.with_view(cx, |v, window, cx| match v.overlay() {
        Some(Overlay::Proof(p)) => p.read(cx).focus_handle().clone().focus(window, cx),
        _ => panic!("the proof sheet is still up"),
    });
    rig.press("enter", cx);
    work(cx);
    assert_eq!(rig.open_photo(cx), photo, "Enter from the backdrop does not step the filmstrip");
    assert!(stage_view(cx).is_fit(), "Enter from the backdrop does not zoom");
    assert_eq!(rig.labels(v).1, head, "Enter from the backdrop does not move the history");
    assert_eq!(overlay(cx), "proof", "Enter with no proof focused is a no-op of the sheet's own");
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

// --- the proof sheet's preview vs. the Darkroom's print (#250 review) ----------------------

/// "🖥 Loupe print" is on by default (`the_loupe_print_follows_the_record_onto_the_pop_out`),
/// so it is up on the same photo a dealt proof sheet previews. The previewed candidate must
/// outrank it: the sheet is transient and modal over the Darkroom, so hovering a proof should
/// show that proof on the pop-out, labelled, not silently keep showing the print underneath
/// while the bar still claims to show the proof.
#[gpui_kit::test]
fn a_previewed_proof_outranks_the_default_loupe_print(cx: &mut TestAppContext) {
    use super::view::Overlay;
    use crate::loupe::window;
    let rig = rig("dk-proof-vs-print", 2, cx);
    work(cx); // the proof sheet's auto-tone fragment
    advance(cx, SETTLE); // the default print goes up after the open's settle
    let print_json =
        rig.app.wired.shell.read_with(cx, |s, _| s.loupe_print().expect("the print is up by default").edit_json.clone());
    cx.update(window::open);
    cx.run_until_parked();
    let h = cx.update(|cx| window::handle(cx)).expect("the pop-out opened");

    rig.with_view(cx, |v, window, cx| v.open_proof_sheet(window, cx));
    let sheet = rig.view(cx).read_with(cx, |v, _| match v.overlay() {
        Some(Overlay::Proof(p)) => p.clone(),
        _ => panic!("the proof sheet is mounted"),
    });
    // A candidate whose record differs from the print's `{}` — not the "Auto" cell, which can
    // coincidentally share it and pass either way regardless of which one wins (#250 review:
    // the first version of this test hovered "Auto" and passed by coincidence).
    let i = sheet
        .read_with(cx, |s, _| s.candidates().iter().position(|c| c.group == chairphoto_model::darkroom::spreads::ProofGroup::Film))
        .unwrap();
    let want = sheet.read_with(cx, |s, _| s.candidates()[i].record.to_json());
    let label = sheet.read_with(cx, |s, _| s.candidates()[i].label.clone());
    assert_ne!(want, print_json, "a candidate that differs from the print");

    cx.update_window(rig.app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.hover(("proof-cell", i as u64), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(h, |_, window, cx| window.render_frame(cx)).unwrap();
    cx.run_until_parked();

    let asked = rig.edit_jobs().iter().any(|j| j.max_edge == 2560 && j.edit_json == want);
    assert!(asked, "the pop-out asks for the hovered proof at loupe size, not the print's");
    let bar_label =
        cx.update_window(h, |_, window, _| window.try_find("loupe-tag-proof").and_then(|e| e.label().map(str::to_string))).unwrap();
    assert_eq!(bar_label, Some(format!("Proof: {label} — not applied")), "the bar names the proof, not the print");
}

/// Taking over from the Darkroom's print never blanks the pop-out (#250 review, probe P1):
/// the print's own render stays wanted while a proof is previewed, so hovering off, crossing
/// the gap between cells (nothing hovered or focused, same as off), or declining shows its
/// already-rendered texture again at once — no new print render; and while a just-hovered
/// candidate's own 320 px render is still on the way, the print's texture stands in rather
/// than nothing.
#[gpui_kit::test]
fn the_pop_out_never_blanks_between_the_print_and_a_proof(cx: &mut TestAppContext) {
    use super::view::Overlay;
    use crate::loupe::window;
    use crate::loupe::zoom::Drawn;
    let rig = rig("dk-proof-never-blank", 2, cx);
    work(cx);
    advance(cx, SETTLE); // the default print goes up after the open's settle
    let print_json = rig.app.wired.shell.read_with(cx, |s, _| s.loupe_print().expect("the print is up by default").edit_json.clone());
    cx.update(window::open);
    cx.run_until_parked();
    let h = cx.update(|cx| window::handle(cx)).expect("the pop-out opened");
    let zoom = cx.update(|cx| window::view(cx)).unwrap().read_with(cx, |v, cx| v.loupe().read(cx).zoom().clone());
    cx.update_window(h, |_, window, cx| window.render_frame(cx)).unwrap();
    cx.run_until_parked();

    let print_jobs = |rig: &Rig| rig.edit_jobs().into_iter().filter(|j| j.max_edge == 2560 && j.edit_json == print_json).count();
    let before = print_jobs(&rig);
    let print_job = rig.edit_jobs().into_iter().find(|j| j.max_edge == 2560 && j.edit_json == print_json).expect("the print's render was asked for");
    let l = pixels(400, 300);
    let print_image = l.image.clone();
    rig.pool.finish(&JobKey::Edit(print_job), Ok(l));
    cx.run_until_parked();
    cx.update_window(h, |_, window, cx| window.render_frame(cx)).unwrap();
    assert!(
        zoom.read_with(cx, |z, _| z.override_lo()).is_some_and(|s| Arc::ptr_eq(&s, &print_image)),
        "the pop-out shows the print once it lands"
    );

    rig.with_view(cx, |v, window, cx| v.open_proof_sheet(window, cx));
    let i = rig.view(cx).read_with(cx, |v, cx| match v.overlay() {
        Some(Overlay::Proof(p)) => {
            p.read(cx).candidates().iter().position(|c| c.group == chairphoto_model::darkroom::spreads::ProofGroup::Film).unwrap()
        }
        _ => panic!("the proof sheet is mounted"),
    });
    cx.update_window(rig.app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.hover(("proof-cell", i as u64), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(h, |_, window, cx| window.render_frame(cx)).unwrap();
    // The proof's own 320 px and 2560 px renders are both still on the way (never finished):
    // the pop-out must not go blank, so the print's texture stands in.
    assert_eq!(
        zoom.read_with(cx, |z, _| z.drawn().map(|(_, d)| d)),
        Some(Drawn::OverrideLo),
        "never blank while the proof's own render is on the way"
    );
    assert!(
        zoom.read_with(cx, |z, _| z.override_lo()).is_some_and(|s| Arc::ptr_eq(&s, &print_image)),
        "stands in with the print's own texture"
    );

    // Off the cell, onto the close button (inside the panel, not the backdrop — a decline):
    // the print shows again, with no new print render asked for.
    cx.update_window(rig.app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.hover("proof-close", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(h, |_, window, cx| window.render_frame(cx)).unwrap();
    assert_eq!(print_jobs(&rig), before, "no new print render on hover-off");
    assert!(
        zoom.read_with(cx, |z, _| z.override_lo()).is_some_and(|s| Arc::ptr_eq(&s, &print_image)),
        "the print's cached texture, not a re-render"
    );
}

/// Zoomed, the Darkroom's print full-res render must not fill a proof's own hi-res slot
/// (#250 review, probe P3): it is only a valid stand-in for `over.hi` when `over.lo` is
/// *itself* the print's fallback — otherwise, hovering a proof whose own lo has landed but
/// whose hi is still pending (or has failed) would show the PRINT's full-res pixels under
/// the proof's "Proof: <label> — not applied" bar, which is exactly where one judges a
/// film/grain proof.
#[gpui_kit::test]
fn the_pop_out_never_shows_the_prints_full_res_under_a_proof(cx: &mut TestAppContext) {
    use super::view::Overlay;
    use crate::loupe::window;
    use crate::loupe::zoom::Drawn;
    let rig = rig("dk-proof-no-print-hires", 2, cx);
    work(cx);
    advance(cx, SETTLE); // the default print goes up after the open's settle
    let print_json = rig.app.wired.shell.read_with(cx, |s, _| s.loupe_print().expect("the print is up by default").edit_json.clone());
    cx.update(window::open);
    cx.run_until_parked();
    let h = cx.update(|cx| window::handle(cx)).expect("the pop-out opened");
    let zoom = cx.update(|cx| window::view(cx)).unwrap().read_with(cx, |v, cx| v.loupe().read(cx).zoom().clone());
    cx.update_window(h, |_, window, cx| window.render_frame(cx)).unwrap();
    cx.run_until_parked();

    // Land the print's lo, zoom the pop-out in, and land the print's full-res too.
    let print_lo_job = rig.edit_jobs().into_iter().find(|j| j.max_edge == 2560 && j.edit_json == print_json).expect("the print's render was asked for");
    rig.pool.finish(&JobKey::Edit(print_lo_job), Ok(pixels(400, 300)));
    cx.run_until_parked();
    cx.update_window(h, |_, window, cx| window.double_click("loupe-image", cx)).unwrap();
    cx.run_until_parked();
    cx.update_window(h, |_, window, cx| window.render_frame(cx)).unwrap();
    let print_hi_job = rig.edit_jobs().into_iter().find(|j| j.hi_res && j.edit_json == print_json).expect("the print's full-res was asked for once zoomed");
    let l = pixels(800, 600);
    let print_hi_image = l.image.clone();
    rig.pool.finish(&JobKey::Edit(print_hi_job), Ok(l));
    cx.run_until_parked();
    cx.update_window(h, |_, window, cx| window.render_frame(cx)).unwrap();
    assert!(
        zoom.read_with(cx, |z, _| z.override_hi()).is_some_and(|s| Arc::ptr_eq(&s, &print_hi_image)),
        "the print's own full-res shows, zoomed, with nothing else up"
    );

    rig.with_view(cx, |v, window, cx| v.open_proof_sheet(window, cx));
    let i = rig.view(cx).read_with(cx, |v, cx| match v.overlay() {
        Some(Overlay::Proof(p)) => {
            p.read(cx).candidates().iter().position(|c| c.group == chairphoto_model::darkroom::spreads::ProofGroup::Film).unwrap()
        }
        _ => panic!("the proof sheet is mounted"),
    });
    cx.update_window(rig.app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.hover(("proof-cell", i as u64), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(h, |_, window, cx| window.render_frame(cx)).unwrap();
    let proof_json = rig.app.wired.shell.read_with(cx, |s, _| s.loupe_proof_preview().map(|p| (p.source.encode)(&p.candidate.record))).unwrap();
    let proof_lo_job = rig.edit_jobs().into_iter().find(|j| j.max_edge == 2560 && j.edit_json == proof_json).expect("the proof's own render was asked for");
    let proof_hi_job = rig.edit_jobs().into_iter().find(|j| j.hi_res && j.edit_json == proof_json).expect("the proof's own full-res was asked for too, zoomed");
    let l = pixels(400, 300);
    let proof_lo_image = l.image.clone();
    rig.pool.finish(&JobKey::Edit(proof_lo_job), Ok(l));
    cx.run_until_parked();
    cx.update_window(h, |_, window, cx| window.render_frame(cx)).unwrap();
    // The proof's own lo landed; its own hi is still pending: the print's full-res must not
    // stand in, and the proof's own lo shows (scaled up), not the print's.
    assert!(
        zoom.read_with(cx, |z, _| z.override_hi()).map_or(true, |s| !Arc::ptr_eq(&s, &print_hi_image)),
        "the print's full-res does not stand in for the proof's own pending hi"
    );
    assert_eq!(
        zoom.read_with(cx, |z, _| z.drawn().map(|(_, d)| d)),
        Some(Drawn::OverrideLo),
        "the proof's own lo shows, scaled up, while its own hi is pending"
    );
    assert!(
        zoom.read_with(cx, |z, _| z.override_lo()).is_some_and(|s| Arc::ptr_eq(&s, &proof_lo_image)),
        "specifically the proof's own lo, not the print's"
    );

    // The proof's own hi fails outright: still never the print's.
    rig.pool.finish(&JobKey::Edit(proof_hi_job), Err("decode failed".into()));
    cx.run_until_parked();
    cx.update_window(h, |_, window, cx| window.render_frame(cx)).unwrap();
    assert!(
        zoom.read_with(cx, |z, _| z.override_hi()).map_or(true, |s| !Arc::ptr_eq(&s, &print_hi_image)),
        "a failed proof hi does not fall back to the print's full-res either"
    );
}

/// The pop-out's own ←/→ and Enter no longer silently act on the library/main stage while a
/// proof sheet is up (#250 review follow-up, `LoupeView::proof_sheet_route`): with a sheet up,
/// → re-dispatches `ProofNext` into the Darkroom's own window instead of stepping the library
/// selection, and Enter adopts the sheet's own currently-previewed candidate directly. With no
/// sheet up, the pop-out's own arrow still steps the selection exactly as before.
#[gpui_kit::test]
fn the_pop_out_routes_arrows_and_enter_to_a_live_proof_sheet(cx: &mut TestAppContext) {
    use super::view::Overlay;
    use crate::loupe::window;
    let rig = rig("dk-popout-proof-route", 3, cx);
    work(cx);
    advance(cx, SETTLE); // the auto-tone fragment `open_proof_sheet` needs
    cx.update(window::open);
    cx.run_until_parked();
    let h = cx.update(|cx| window::handle(cx)).expect("the pop-out opened");
    let active = |cx: &mut TestAppContext| rig.app.wired.shell.read_with(cx, |s, _| s.library.selection().active_id);
    let press_in_popout = |key: &str, cx: &mut TestAppContext| {
        cx.update_window(h, |_, window, cx| {
            window.render_frame(cx);
            window.press(key, cx);
        })
        .unwrap();
        cx.run_until_parked();
    };

    // With no sheet up: the pop-out's own → still steps the library selection, as before —
    // which, with the Darkroom open, also re-targets it to the new active photo, so its own
    // auto-tone fragment needs a fresh settle before a sheet can be dealt on it below.
    let before = active(cx);
    press_in_popout("right", cx);
    let stepped = active(cx);
    assert_ne!(stepped, before, "no sheet up: → still steps the selection");
    work(cx);
    advance(cx, SETTLE);

    // Open a proof sheet on the Darkroom's own window.
    rig.with_view(cx, |v, window, cx| v.open_proof_sheet(window, cx));
    let sheet = rig.view(cx).read_with(cx, |v, _| match v.overlay() {
        Some(Overlay::Proof(p)) => p.clone(),
        _ => panic!("the proof sheet is mounted"),
    });
    let focused_in_main = |cx: &mut TestAppContext| cx.update_window(rig.app.window(), |_, window, cx| sheet.read(cx).focused(window)).unwrap();
    assert_eq!(focused_in_main(cx), None, "the backdrop is focused, no cell yet");

    // → from the pop-out moves the sheet's own focus/preview; the selection does not move.
    press_in_popout("right", cx);
    assert_eq!(active(cx), stepped, "the selection stayed put while the sheet is up");
    assert_eq!(focused_in_main(cx), Some(0), "→ reached the sheet's own first cell, like from its own backdrop");
    let candidate0 = sheet.read_with(cx, |s, _| s.candidates()[0].clone());
    assert_eq!(
        rig.app.wired.shell.read_with(cx, |s, _| s.loupe_proof_preview().map(|p| p.candidate.clone())),
        Some(candidate0),
        "the pop-out's own preview follows the sheet's new focus"
    );

    // ← wraps the sheet's own focus backward, same as if pressed on the main window.
    press_in_popout("left", cx);
    let n = sheet.read_with(cx, |s, _| s.candidates().len());
    assert_eq!(focused_in_main(cx), Some(n - 1), "← wrapped to the sheet's own last cell");
    assert_eq!(active(cx), stepped, "still untouched");

    // Enter from the pop-out adopts whichever candidate the sheet is now showing.
    let last = sheet.read_with(cx, |s, _| s.candidates()[n - 1].clone());
    press_in_popout("enter", cx);
    assert!(rig.view(cx).read_with(cx, |v, _| v.overlay().is_none()), "Enter from the pop-out adopted and closed the sheet");
    assert_eq!(
        rig.working(cx),
        serde_json::from_str::<Value>(&last.record.to_json()).unwrap(),
        "adopted exactly the cell the pop-out's Enter left focused"
    );
}

/// `DarkroomView::open_duel` replaces `rails.overlay` without closing a live proof sheet
/// first (#250 second review, F2): the sheet must still be released and its pop-out routing
/// handle cleared with it, so the pop-out's own arrows step the library selection again
/// instead of being silently swallowed by a route that now points at an orphan.
#[gpui_kit::test]
fn opening_the_duel_over_a_live_proof_sheet_releases_its_popout_route(cx: &mut TestAppContext) {
    use super::view::Overlay;
    use crate::loupe::window;
    let rig = rig("dk-duel-over-proof-route", 3, cx);
    work(cx);
    advance(cx, SETTLE);
    cx.update(window::open);
    cx.run_until_parked();
    let h = cx.update(|cx| window::handle(cx)).expect("the pop-out opened");
    let active = |cx: &mut TestAppContext| rig.app.wired.shell.read_with(cx, |s, _| s.library.selection().active_id);

    rig.with_view(cx, |v, window, cx| v.open_proof_sheet(window, cx));
    let sheet = rig.view(cx).read_with(cx, |v, _| match v.overlay() {
        Some(Overlay::Proof(s)) => s.clone(),
        _ => panic!("the proof sheet is mounted"),
    });
    let weak = sheet.downgrade();
    drop(sheet);
    assert!(rig.app.wired.shell.read_with(cx, |s, _| s.loupe_proof_sheet().is_some()), "routed");

    rig.with_view(cx, |v, window, cx| v.open_duel(window, cx));
    cx.run_until_parked();
    assert!(rig.view(cx).read_with(cx, |v, _| matches!(v.overlay(), Some(Overlay::Duel(_)))), "the duel replaced it");
    assert!(weak.upgrade().is_none(), "the replaced proof sheet is actually released");
    assert!(rig.app.wired.shell.read_with(cx, |s, _| s.loupe_proof_sheet().is_none()), "its stale route is cleared");

    let before = active(cx);
    cx.update_window(h, |_, window, cx| {
        window.render_frame(cx);
        window.press("right", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_ne!(active(cx), before, "the pop-out's own → steps the selection again, not swallowed by a stale route");
}

/// Up/Down in the pop-out move the proof sheet's own real row (#250 second review, item 2) —
/// `ProofSheet::row_target`'s own arithmetic, oracle-checked the same way `loupe::tests::
/// overlays::up_and_down_move_focus_by_row_and_preview_follows` checks it on the main window
/// directly — not `ProofNext`/`ProofPrevious`'s cycle the way Left/Right still do. While the
/// sheet is up, `contexts::POPOUT_PROOF_SHEET` replaces `LOUPE` on the pop-out's own root, so
/// Shift+→ (`ExtendNext`), Ctrl+A (`SelectAll`) and C (`CompareSelection`) — bound only in
/// `LOUPE` — are inert there instead of silently moving the active photo or opening Compare
/// out from under a dealt sheet; Esc declines it.
#[gpui_kit::test]
fn the_pop_out_gives_up_down_real_rows_and_swallows_extend_select_all_and_compare_while_a_sheet_is_up(cx: &mut TestAppContext) {
    use super::view::Overlay;
    use crate::loupe::proof_sheet::row_target;
    use crate::loupe::window;
    let rig = rig("dk-popout-rows", 3, cx);
    work(cx);
    advance(cx, SETTLE);
    cx.update(window::open);
    cx.run_until_parked();
    let h = cx.update(|cx| window::handle(cx)).expect("the pop-out opened");
    let active = |cx: &mut TestAppContext| rig.app.wired.shell.read_with(cx, |s, _| s.library.selection().active_id);
    let press_in_popout = |key: &str, cx: &mut TestAppContext| {
        cx.update_window(h, |_, window, cx| {
            window.render_frame(cx);
            window.press(key, cx);
        })
        .unwrap();
        cx.run_until_parked();
    };

    rig.with_view(cx, |v, window, cx| v.open_proof_sheet(window, cx));
    let sheet = rig.view(cx).read_with(cx, |v, _| match v.overlay() {
        Some(Overlay::Proof(s)) => s.clone(),
        _ => panic!("the proof sheet is mounted"),
    });
    let focused_in_main = |cx: &mut TestAppContext| cx.update_window(rig.app.window(), |_, window, cx| sheet.read(cx).focused(window)).unwrap();
    let n = sheet.read_with(cx, |s, _| s.candidates().len());
    let cols = cx.update_window(rig.app.window(), |_, window, cx| { window.render_frame(cx); sheet.read(cx).columns() }).unwrap();
    assert!(cols < n, "this rig's own candidates ({n}) must wrap past one row at the Darkroom's own width: {cols} columns");

    // ↓ from the backdrop starts at cell 0, the same convention as the main window's own ↓.
    press_in_popout("down", cx);
    assert_eq!(focused_in_main(cx), Some(0));

    // A second ↓ is a real row move — `row_target`, not a plain +1 cycle.
    press_in_popout("down", cx);
    let want = row_target(0, n, cols, 1);
    assert_ne!(want, Some(1), "this rig's own layout must actually exercise a row move, not coincide with +1");
    assert_eq!(focused_in_main(cx), want, "the pop-out's ↓ moved by row, not by cycling to cell 1");

    // ↑ moves back by row the same way.
    let before_up = focused_in_main(cx).unwrap();
    press_in_popout("up", cx);
    assert_eq!(focused_in_main(cx), row_target(before_up, n, cols, -1));

    // Shift+→, Ctrl+A and C are inert while the sheet is up: neither the selection nor the
    // sheet's own focus changes, and the sheet stays up.
    let a = active(cx);
    let focus_before = focused_in_main(cx);
    for key in ["shift-right", "ctrl-a", "c"] {
        press_in_popout(key, cx);
        assert_eq!(active(cx), a, "{key}: the selection does not move while a sheet is up");
        assert_eq!(focused_in_main(cx), focus_before, "{key}: the sheet's own focus is untouched");
        assert!(rig.view(cx).read_with(cx, |v, _| v.overlay().is_some()), "{key}: the sheet stays up");
    }

    // Esc declines it.
    press_in_popout("escape", cx);
    assert!(rig.view(cx).read_with(cx, |v, _| v.overlay().is_none()), "Esc from the pop-out declined the sheet");
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

/// #191 N2: a strip column stuck at zero width must not ask for a fresh frame forever. The
/// window is resized so narrow that, once the right rail's fixed width is subtracted, the
/// stage+strip column — and so the filmstrip's tracked scroll area — gets none: `centre_strip`
/// retries a bounded number of times, then gives up until something else changes the layout.
#[gpui_kit::test]
fn a_zero_width_strip_column_stops_asking_for_frames(cx: &mut TestAppContext) {
    let rig = rig("dk-zero-width", 5, cx);
    cx.simulate_window_resize(rig.app.window(), gpui_kit::size(gpui_kit::px(4.), gpui_kit::px(400.)));
    cx.run_until_parked();
    rig.render(cx);
    let viewport = strip_at(&rig, 0, cx).viewport;
    assert_eq!(viewport, 0.0, "the window is too narrow for the strip column to have any width");

    // Each iteration: run whatever `request_animation_frame` queued, then redraw — the loop
    // this would spin in, pre-fix, forever.
    let mut ran = 1;
    let mut frames = 0;
    while ran > 0 && frames < 20 {
        ran = cx.update_window(rig.app.window(), |_, window, cx| window.simulate_next_frame(cx)).unwrap();
        rig.render(cx);
        frames += 1;
    }
    assert!(frames < 20, "bounded: still asking for a fresh frame after {frames} redraws");
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

/// #191 N1: `sync` runs `request_strip_thumbs` on every shell notify while the same photo
/// stays open (it still asks the image store every time — that is what lets
/// `the_strips_frames_are_asked_for_nearest_the_open_photo_first` pick up an eviction from a
/// plain notify), but it only rebuilds the strip's window — `strip()`'s clone of up to
/// `2*STRIP_RADIUS+1` rows plus the `nearest_first` ordering — when the Library's rows were
/// actually re-read since the last build.
#[gpui_kit::test]
fn repeated_notifies_with_unchanged_rows_do_not_rebuild_the_strip_window(cx: &mut TestAppContext) {
    let rig = rig("dk-strip-rebuild", 5, cx);
    let d = rig.darkroom(cx);
    let before = d.read_with(cx, |d, _| d.strip_rebuild_count());
    assert!(before > 0, "opening built the strip once");

    // Several shell notifies that touch nothing about the rows.
    for _ in 0..5 {
        rig.app.wired.shell.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
    }
    let after = d.read_with(cx, |d, _| d.strip_rebuild_count());
    assert_eq!(after, before, "no rows change: the window is not rebuilt");

    // A genuine rows re-read still rebuilds it.
    rig.app.wired.shell.update(cx, |s, cx| {
        s.refresh_rows(cx);
        cx.notify();
    });
    cx.run_until_parked();
    work(cx);
    let after2 = d.read_with(cx, |d, _| d.strip_rebuild_count());
    assert!(after2 > after, "a real rows re-read rebuilds it: {after2} > {after}");
}

/// #191 M2: the cache key must be the rows that actually *landed*
/// ([`chairphoto_model::library::query::LibraryQuery::rows_landed`]), not
/// [`chairphoto_model::library::query::LibraryQuery::generation`], which bumps the instant a
/// refresh is requested — before its page arrives. Develop open on photo 0 of 5; a 6th photo
/// is added straight to the catalog (the Library's rows don't know about it yet); the same
/// `refresh_rows` + notify the N1 test above uses runs sync while the read is still in
/// flight — correctly, from the still-five-photo rows, since nothing new has landed. Once the
/// read lands (`cx.run_until_parked()` + `work`) a further notify must rebuild the strip's
/// window from the now-six-photo rows.
/// (Mutation-checked: keying `request_strip_thumbs` on `LibraryQuery::generation()` instead
/// reproduces the bug exactly — the key does not change between the two notifies, so the
/// second is a cache hit and the strip never picks up the 6th photo; this test then fails,
/// `wanted.len() == 5`.)
#[gpui_kit::test]
fn a_rows_landing_during_develop_rebuilds_the_strip_window(cx: &mut TestAppContext) {
    let rig = rig("dk-strip-landed-rows", 5, cx);
    let d = rig.darkroom(cx);
    let sixth = rig.catalog(|c| c.upsert_photo(&rig.dir.0.join("photos/2026/p5.ARW"), None, 0, 1).unwrap().id);

    rig.app.wired.shell.update(cx, |s, cx| {
        s.refresh_rows(cx);
        cx.notify();
    });
    cx.run_until_parked();
    work(cx);
    // A further notify, as the strip sees on any later shell change, must pick up the landed rows.
    rig.app.wired.shell.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();

    let wanted = d.read_with(cx, |d, _| d.strip_wanted_ids());
    assert!(wanted.contains(&sixth), "the strip must ask for the 6th photo once its row landed: {wanted:?}");
    assert_eq!(wanted.len(), 6, "all six rows are within the strip's radius: {wanted:?}");
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

/// #252: a saved change moves the photo's face to its version. Stepping on reads that one
/// row's face again — never the whole library (review M1) — so the strip's frame for the
/// photo left asks for its new face without leaving Develop, whether the save had landed
/// before the step or was still on the worker. The other rows are left as they were.
#[gpui_kit::test]
fn a_frame_shows_the_new_face_of_the_photo_just_edited(cx: &mut TestAppContext) {
    let rig = rig("dk-face-strip", 3, cx);
    let from = rig.app.wired.shell.read_with(cx, |s, _| s.rows_from()).unwrap();
    let face_look = |rig: &Rig, photo: i64| cover_look(rig.catalog(|c| c.get_photo(photo).unwrap().cover_token).as_deref());
    draw(&rig, cx);
    let first = rig.open_photo(cx).unwrap();
    assert_eq!(look(&rig, first, cx), Some(Look { from, cover: None }), "unedited: the original");
    let row_reads = |cx: &mut TestAppContext| rig.app.wired.shell.read_with(cx, |s, _| s.row_reads);
    let rows = |cx: &mut TestAppContext| rig.app.wired.shell.read_with(cx, |s, _| s.library.photos().to_vec());
    let reads_before = row_reads(cx);
    let rows_before = rows(cx);

    // Saved, then a step.
    rig.slide(Control::Tone(ToneKey::Ev), 0.5, cx);
    rig.settle_and_save(cx);
    let want = face_look(&rig, first);
    assert!(want.is_some());
    assert!(rig.view(cx).update(cx, |v, cx| v.step(1, None, cx)));
    cx.run_until_parked();
    work(cx);
    let second = rig.open_photo(cx).unwrap();
    assert_ne!(second, first);
    assert_eq!(look(&rig, first, cx), Some(Look { from, cover: want }), "the frame asks for the new face");

    // A step while the save is still on the worker: the rows are re-read once it lands.
    rig.slide(Control::Tone(ToneKey::Ev), -0.5, cx);
    assert!(rig.view(cx).update(cx, |v, cx| v.step(1, None, cx)));
    cx.run_until_parked();
    work(cx);
    let want = face_look(&rig, second);
    assert!(want.is_some(), "the change on the second photo was saved");
    assert_eq!(look(&rig, second, cx), Some(Look { from, cover: want }));

    assert_eq!(row_reads(cx), reads_before, "no whole-library row read on a step");
    let after = rows(cx);
    assert_eq!(after.len(), rows_before.len());
    for (b, a) in rows_before.iter().zip(&after) {
        assert_eq!(a.id, b.id);
        if a.id != first && a.id != second {
            assert_eq!(a.cover_token, b.cover_token, "photo {}: untouched", a.id);
        }
    }
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
    // Created (the automatic face, rev 0), then pinned (rev 1) before the rows were re-read.
    assert_eq!(look(&rig, photo, cx), Some(Look { from, cover: Some(CoverLook { version, rev: 1 }) }));
    assert_eq!(thumb_jobs(&rig, photo), jobs + 1, "a new cover: asked again");
    assert!(preview_ready(cx), "only the thumbnail shows the cover: the preview stays");

    refresh_rows(&rig, cx);
    assert_eq!(thumb_jobs(&rig, photo), jobs + 1, "the same look: nothing asked");

    // The cover version's settings change: its revision moves.
    rig.catalog(|c| c.set_version_edit(version, r#"{"tone":{"ev":1}}"#).unwrap());
    refresh_rows(&rig, cx);
    assert_eq!(look(&rig, photo, cx).unwrap().cover, Some(CoverLook { version, rev: 2 }));
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
    let cover = Some(CoverLook { version, rev: 1 });
    assert_eq!(look(&rig, photo, cx), Some(Look { from: a, cover }));
    let key = JobKey::photo(photo, ImageKind::Thumb);
    rig.pool.start(key.clone());

    let (b, b_ids) = colliding_catalog(&rig.dir, "b", 2);
    let b_version = b.create_version(b_ids[1], "B's").unwrap();
    let token = b.set_cover_version(b_ids[1], Some(b_version)).unwrap();
    assert_eq!((b_ids[1], token), (photo, Some(format!("{version}:1"))), "the ids and the token collide");
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
