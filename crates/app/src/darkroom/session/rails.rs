//! The Darkroom's rails (#112) on the session: history steps, the version shelf, "+ New
//! version", "Develop with the new engine", the cover, a duel's ⑂, presets, the proof
//! sheet's auto-tone fragment and the framing modes. See the parent's module docs ("The
//! rails") for how version operations are ordered after the autosave.

use super::*;
use crate::loupe::duel::VariantSource;
use chairphoto_model::darkroom::controls::apply_preset;
use chairphoto_model::darkroom::geometry;
use chairphoto_model::darkroom::history::step_by;
use chairphoto_model::darkroom::spreads::{proof_spread, DuelDim, ProofCandidate, ProofGroup};
use chairphoto_model::editing::for_linear_engine;
use chairphoto_model::presets::{
    add_user_preset, all_presets, delete_user_preset, rename_user_preset, serialize_user_presets,
};
use gpui_kit::EventEmitter;
use std::rc::Rc;

/// What the Darkroom tells its view.
#[derive(Clone, Debug, PartialEq)]
pub enum DarkroomEvent {
    /// A duel variant was banked as the version named here.
    Kept(String),
}

impl EventEmitter<DarkroomEvent> for Darkroom {}

/// How long a notice ("Saved preset …") stays.
pub const NOTICE: Duration = Duration::from_secs(3);

/// A new version's row and the photo's versions after it, from a worker.
type Created = (i64, Vec<PhotoVersion>);

impl Darkroom {
    // --- ordering ----------------------------------------------------------------------------

    /// Run `op` once what is pending is saved: now, or after the commit on the worker — or,
    /// the version not resolved yet, once it is.
    fn after_save(&mut self, op: Op, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        if !open.loaded {
            // Nothing to step, switch or fork from yet (a duel's ⑂, a key, pressed while the
            // photo opens): the operation waits for the version, as React's waited on its
            // load. If the versions cannot be read it is dropped and the banner says why.
            open.ops.push_back(op);
            return;
        }
        let seq = open.seq;
        self.commit(seq, cx);
        let open = self.open.as_mut().expect("open");
        if open.committing {
            open.ops.push_back(op);
        } else {
            op(self, seq, cx);
        }
    }

    /// One version operation on a worker. Photo `seq` is busy until it answers (a change
    /// meanwhile waits, as for a commit) — or, for an operation that `replaces` the working
    /// record, refuses changes until it answers ([`Darkroom::apply`]); `done` runs only while
    /// `seq` is the open photo. A left photo's failure goes to the status line.
    fn run_op<T: Send + 'static>(
        &mut self,
        seq: u64,
        replaces: bool,
        work: impl FnOnce(&AppState) -> Result<T, String> + Send + 'static,
        done: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.photo_mut(seq) else { return };
        open.committing = true;
        open.saving = true;
        open.replacing = replaces;
        let state = self.app.clone();
        let rx = Runner::get(cx).run(move || work(&state));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the worker stopped".into()));
            this.update(cx, |this, cx| {
                let Some(open) = this.photo_mut(seq) else { return };
                open.committing = false;
                open.saving = false;
                open.replacing = false;
                if this.open.as_ref().is_some_and(|o| o.seq == seq) {
                    done(this, result, cx);
                } else if let Err(e) = result {
                    this.model.update(cx, |m, cx| m.set_status(format!("Darkroom: {e}"), cx));
                }
                this.idle(seq, false, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// `v`, as the shell's active version now holds it.
    fn show_version(&mut self, v: Option<PhotoVersion>, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| s.set_active_version(v, cx));
    }

    /// Take `record` as what the version holds now (a load, a step): nothing to commit.
    fn adopt_committed(open: &mut OpenPhoto, record: &VersionEdit) {
        open.committed_json = record.to_json();
        open.committed = record.clone();
        open.last_step = None;
    }

    // --- history ---------------------------------------------------------------------------

    /// A step in the History panel: it becomes current and its settings go back into the
    /// version (pending changes are saved first, so nothing is lost).
    pub fn goto_step(&mut self, step: i64, cx: &mut Context<Self>) {
        self.after_save(Box::new(move |this, seq, cx| this.goto_now(seq, Some(step), 0, cx)), cx);
    }

    /// Ctrl+Z: one step back, counted from the history as it stands once what is pending is
    /// saved — so undo first takes back the change not yet saved.
    ///
    /// **Deliberately not React.** With a change pending at head H, React's `stepBy` picked
    /// its target from the history *before* `gotoStep` flushed the change, so it saved the
    /// change as H+1 and then went to H−1: one Ctrl+Z took back the unsaved change *and* the
    /// step before it. Here the target is counted after the save, so it lands on H: only the
    /// unsaved change is taken back (and Ctrl+Shift+Z brings it back).
    pub fn undo(&mut self, cx: &mut Context<Self>) {
        self.after_save(Box::new(|this, seq, cx| this.goto_now(seq, None, -1, cx)), cx);
    }

    /// Ctrl+Shift+Z / Ctrl+Y: one step forward.
    pub fn redo(&mut self, cx: &mut Context<Self>) {
        self.after_save(Box::new(|this, seq, cx| this.goto_now(seq, None, 1, cx)), cx);
    }

    /// Go to `step`, or `delta` steps from the head.
    fn goto_now(&mut self, seq: u64, step: Option<i64>, delta: i64, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let Some(vid) = open.version_id else { return };
        let Some(step) = step.or_else(|| step_by(open.history.as_ref(), delta)) else { return };
        let from = open.from;
        self.run_op(
            seq,
            true,
            move |state| {
                editing::write_version_then_refresh_monochrome(state, Some(from), vid, |c| c.goto_version_step(vid, step))
            },
            move |this, result, cx| match result {
                Ok((json, history)) => {
                    let open = this.open.as_mut().expect("open");
                    if open.version_id != Some(vid) {
                        return;
                    }
                    let before = (open.source_token().map(str::to_string), open.stage_json());
                    let record = parse_edit(Some(&json));
                    Self::adopt_committed(open, &record);
                    open.history = Some(history);
                    // The step's record is what the version holds now. No change was made
                    // while the step ran (changes were refused): one saved on top of it
                    // would have cut the redo branch the step just left.
                    open.working = record;
                    let mut shown = None;
                    if let Some(v) = open.versions.iter_mut().find(|v| v.id == vid) {
                        v.edit_json = json;
                        shown = Some(v.clone());
                    }
                    if shown.is_some() {
                        this.show_version(shown, cx);
                    }
                    this.restage(before, cx);
                }
                Err(e) => this.error = Some(format!("Could not go to that step: {e}")),
            },
            cx,
        );
    }

    // --- the version shelf -------------------------------------------------------------------

    /// A shelf chip: edit `target` (`None`: the Original — the next change starts a new
    /// version). Pending changes are saved first.
    pub fn switch_version(&mut self, target: Option<i64>, cx: &mut Context<Self>) {
        self.after_save(Box::new(move |this, seq, cx| this.switch_now(seq, target, cx)), cx);
    }

    fn switch_now(&mut self, seq: u64, target: Option<i64>, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let (from, photo_id) = (open.from, open.photo.id);
        // Replaces the record: changes are refused until the other version is on screen.
        self.run_op(
            seq,
            true,
            move |state| {
                with_catalog_as(state, from, |c| {
                    let versions = c.list_versions(photo_id)?;
                    let v = target.and_then(|t| versions.iter().find(|v| v.id == t).cloned());
                    let history = match &v {
                        Some(v) => Some(c.version_history(v.id)?),
                        None => None,
                    };
                    Ok((versions, v, history))
                })
            },
            |this, result, cx| match result {
                Ok((versions, v, history)) => {
                    let open = this.open.as_mut().expect("open");
                    let before = (open.source_token().map(str::to_string), open.stage_json());
                    let record = parse_edit(v.as_ref().map(|v| v.edit_json.as_str()));
                    Self::adopt_committed(open, &record);
                    // The other version's record replaces the screen. No change was made on
                    // the old one while this ran: changes were refused (React made them and
                    // then dropped them here).
                    open.working = record.clone();
                    open.commit_again = false;
                    open.engine1_version = v.is_some() && is_engine1_version(&record);
                    open.version_id = v.as_ref().map(|v| v.id);
                    open.version_unlisted = false;
                    open.versions_len = versions.len();
                    open.versions = versions;
                    open.history = history;
                    open.adopted_label = None;
                    this.show_version(v, cx);
                    this.restage(before, cx);
                }
                Err(e) => this.error = Some(format!("Could not switch versions: {e}")),
            },
            cx,
        );
    }

    /// Create a version named `name` holding `json`, with the monochrome refresh its first
    /// record owes; answers its id and the photo's versions.
    fn create_with(
        from: CatalogIdentity,
        photo_id: i64,
        name: String,
        json: String,
    ) -> impl FnOnce(&AppState) -> Result<Created, String> + Send + 'static {
        move |state| {
            let id = with_catalog_as(state, from, |c| c.create_version(photo_id, &name))?;
            editing::write_version_then_refresh_monochrome(state, Some(from), id, |c| c.set_version_edit(id, &json))?;
            Ok((id, with_catalog_as(state, from, |c| c.list_versions(photo_id))?))
        }
    }

    /// "+ New version": the current settings copied into a new version, which is edited
    /// from here on. Named after the proof last adopted, else "Version N".
    pub fn new_version(&mut self, cx: &mut Context<Self>) {
        self.after_save(Box::new(|this, seq, cx| this.fork_now(seq, false, cx)), cx);
    }

    /// "Develop with the new engine": an engine-1 version's framing as a fresh engine-2
    /// version "<name> (RAW)" — tone and look start over (an old EV is not a new EV) — which
    /// the stage moves to. The engine-1 version stays as it was.
    pub fn develop_with_new_engine(&mut self, cx: &mut Context<Self>) {
        self.after_save(Box::new(|this, seq, cx| this.fork_now(seq, true, cx)), cx);
    }

    fn fork_now(&mut self, seq: u64, new_engine: bool, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let (from, photo_id) = (open.from, open.photo.id);
        let (name, record) = if new_engine {
            let base = open.version().map_or("Version".to_string(), |v| v.name.clone());
            (format!("{base} (RAW)"), for_linear_engine(&open.working, open.source.camera_ev))
        } else {
            let name = open.adopted_label.clone().unwrap_or_else(|| format!("Version {}", open.versions_len + 1));
            (name, open.working.clone())
        };
        let json = if new_engine { record.to_json() } else { open.stamped(&record).to_json() };
        let saved = json.clone();
        // The new-engine fork replaces the record (tone and look start over): changes are
        // refused until it lands. "+ New version" copies the record, so a change made while
        // it is written is kept and saved into the new version.
        self.run_op(
            seq,
            new_engine,
            Self::create_with(from, photo_id, name, json),
            move |this, result, cx| match result {
                Ok((id, versions)) => {
                    let open = this.open.as_mut().expect("open");
                    let before = (open.source_token().map(str::to_string), open.stage_json());
                    if new_engine {
                        // Nothing was changed meanwhile (refused): the fork's record is shown.
                        open.working = record.clone();
                        open.commit_again = false;
                        open.engine1_version = false;
                    }
                    // "+ New version": a change made while the copy was written stays on
                    // screen and is saved into the new version.
                    Self::adopt_committed(open, &record);
                    open.version_id = Some(id);
                    open.version_unlisted = false;
                    open.history = None;
                    open.adopted_label = None;
                    open.versions_len = versions.len();
                    let created = versions.iter().find(|v| v.id == id).map(|v| PhotoVersion { edit_json: saved, ..v.clone() });
                    open.versions = versions;
                    this.show_version(created, cx);
                    this.restage(before, cx);
                    this.shell.update(cx, |s, cx| s.refresh_rows(cx));
                }
                Err(e) => this.error = Some(format!("Could not create the version: {e}")),
            },
            cx,
        );
    }

    /// A duel's ⑂: bank `record` as "What-if — <dimension>" without leaving the version being
    /// edited; the view tells the duel ([`DarkroomEvent::Kept`]).
    pub fn fork_variant(&mut self, record: VersionEdit, dim: DuelDim, cx: &mut Context<Self>) {
        self.after_save(
            Box::new(move |this, seq, cx| {
                let Some(open) = this.open.as_ref() else { return };
                let (from, photo_id) = (open.from, open.photo.id);
                let name = format!("What-if — {}", dim.label().to_lowercase());
                let json = open.stamped(&record).to_json();
                let kept = name.clone();
                this.run_op(
                    seq,
                    false,
                    Self::create_with(from, photo_id, name, json),
                    move |this, result, cx| match result {
                        Ok((_, versions)) => {
                            let open = this.open.as_mut().expect("open");
                            open.versions_len = versions.len();
                            open.versions = versions;
                            cx.emit(DarkroomEvent::Kept(kept));
                            this.shell.update(cx, |s, cx| s.refresh_rows(cx));
                        }
                        Err(e) => this.error = Some(format!("Could not keep the variant: {e}")),
                    },
                    cx,
                );
            }),
            cx,
        );
    }

    /// "☆ Use as cover" / "★ Cover": the version being edited becomes the photo's face in
    /// the Library, or stops being it. Pending changes are saved first.
    pub fn toggle_cover(&mut self, cx: &mut Context<Self>) {
        self.after_save(
            Box::new(|this, seq, cx| {
                let Some(open) = this.open.as_ref() else { return };
                let Some(vid) = open.version_id else { return };
                let next = if open.cover == Some(vid) { None } else { Some(vid) };
                let (from, photo_id) = (open.from, open.photo.id);
                this.run_op(
                    seq,
                    false,
                    move |state| with_catalog_as(state, from, |c| c.set_cover_version(photo_id, next)),
                    move |this, result, cx| match result {
                        Ok(_) => {
                            this.open.as_mut().expect("open").cover = next;
                            this.shell.update(cx, |s, cx| s.refresh_rows(cx));
                        }
                        Err(e) => this.error = Some(format!("Could not set the cover: {e}")),
                    },
                    cx,
                );
            }),
            cx,
        );
    }

    // --- presets -----------------------------------------------------------------------------

    /// A preset card: tone and look replaced, the strip flattened, the framing kept — one
    /// step named "Preset: <name>".
    pub fn apply_preset(&mut self, preset: &DevelopPreset, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let next = apply_preset(&open.working, preset);
        self.apply(next, Some(&format!("Preset: {}", preset.name)), cx);
    }

    /// Every preset, built-ins first (the proof sheet and the browser deal from it).
    pub fn presets(&self) -> Vec<DevelopPreset> {
        all_presets(self.user_presets.clone())
    }

    /// Run `start` — which writes the catalog setting `key` on a worker and calls
    /// [`setting_written`](Self::setting_written) when that write answers — once every
    /// earlier write of `key` has answered. The Runner's workers take tasks in any order, so
    /// two quick writes of one setting (a save then a delete; Golden then None) could
    /// otherwise land oldest last: in the catalog, and — the answers arriving in that order —
    /// on screen. Chained, not numbered as Preferences' `Ctx::write_setting` does: a preset
    /// edit is a read-modify-write, and skipping a stale one would lose its change.
    fn write_in_order(
        &mut self,
        key: &'static str,
        start: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let chain = self.setting_writes.entry(key).or_default();
        if chain.running {
            chain.queued.push_back(Box::new(start));
            return;
        }
        chain.running = true;
        start(self, cx);
    }

    /// A write of `key` answered: the next one made, if any, goes to the worker.
    fn setting_written(&mut self, key: &'static str, cx: &mut Context<Self>) {
        let Some(chain) = self.setting_writes.get_mut(key) else { return };
        match chain.queued.pop_front() {
            Some(next) => next(self, cx),
            None => chain.running = false,
        }
    }

    /// Read-modify-write the stored user presets on a worker, under one catalog lock bound
    /// to the open photo's catalog, after any edit of them still on the worker; the list
    /// shown follows while that catalog is open.
    fn edit_presets(
        &mut self,
        change: impl FnOnce(Option<&str>) -> Vec<DevelopPreset> + Send + 'static,
        done: impl FnOnce(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(from) = self.open.as_ref().map(|o| o.from) else { return };
        self.write_in_order(
            USER_PRESETS_KEY,
            move |this, cx| {
                let state = this.app.clone();
                let rx = Runner::get(cx).run(move || {
                    with_catalog_as(&state, from, |c| {
                        let stored = c.get_setting(USER_PRESETS_KEY)?;
                        let list = change(stored.as_deref());
                        c.set_setting(USER_PRESETS_KEY, &serialize_user_presets(&list))?;
                        Ok(list)
                    })
                });
                cx.spawn(async move |this, cx| {
                    let result = rx.await.unwrap_or_else(|_| Err("the worker stopped".into()));
                    this.update(cx, |this, cx| {
                        this.setting_written(USER_PRESETS_KEY, cx);
                        if !this.open.as_ref().is_some_and(|o| o.from == from) {
                            return;
                        }
                        match result {
                            Ok(list) => {
                                this.user_presets = list;
                                done(this, cx);
                            }
                            Err(e) => this.error = Some(format!("Could not save the presets: {e}")),
                        }
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            },
            cx,
        );
    }

    /// "☆ Save as preset": the current look (never the framing) under `name`.
    pub fn save_preset(&mut self, name: &str, cx: &mut Context<Self>) {
        let name = name.trim().to_string();
        let Some(record) = self.open.as_ref().map(|o| o.working.clone()) else { return };
        if name.is_empty() {
            return;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let shown = name.clone();
        self.edit_presets(
            move |stored| add_user_preset(stored, id, &name, &record),
            move |this, cx| this.set_notice(format!("Saved preset “{shown}”"), cx),
            cx,
        );
    }

    /// The browser's ✎.
    pub fn rename_preset(&mut self, id: String, name: String, cx: &mut Context<Self>) {
        if name.trim().is_empty() {
            return;
        }
        self.edit_presets(move |stored| rename_user_preset(stored, &id, &name), |_, _| {}, cx);
    }

    /// The browser's × (no confirm, as in React).
    pub fn delete_preset(&mut self, id: String, cx: &mut Context<Self>) {
        self.edit_presets(move |stored| delete_user_preset(stored, &id), |_, _| {}, cx);
    }

    fn set_notice(&mut self, text: String, cx: &mut Context<Self>) {
        self.notice = Some(text);
        let timer = cx.background_executor().timer(NOTICE);
        self.notice_timer = Some(cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, cx| {
                this.notice = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // --- proofs and duels --------------------------------------------------------------------

    /// What a variant (proof, duel pane, preset card) renders from: the stage's photo,
    /// catalog, source and engine stamp.
    pub fn variant_source(&self) -> Option<VariantSource> {
        let open = self.open.as_ref()?;
        let token = open.source_token().and_then(SourceToken::parse).unwrap_or(SourceToken::Preview);
        let mut source = VariantSource::new(open.photo.id, open.epoch, token);
        let (linear, camera_ev) = (open.engine() == 2, open.source.camera_ev);
        source.encode = Rc::new(move |r: &VersionEdit| if linear { as_linear_record(r, camera_ev) } else { r.clone() }.to_json());
        Some(source)
    }

    /// Measure the auto-tone fragment for the stage's source (the open, a new RAW token).
    pub(super) fn measure_auto_tone(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        open.auto_seq += 1;
        open.auto_fragment = None;
        let (seq, auto_seq, from, photo_id) = (open.seq, open.auto_seq, open.from, open.photo.id);
        let token = open.source_token().map(str::to_string);
        // On the RAW the fragment is measured on the working image as shot.
        let base = token.as_ref().map(|_| open.stamped(&VersionEdit::default()).to_json());
        self.run(
            seq,
            move |state| editing::suggest_auto_tone(state, Some(from), photo_id, token.as_deref(), base.as_deref()),
            move |this, result, cx| {
                let Some(open) = this.open.as_mut() else { return };
                if open.auto_seq == auto_seq {
                    // A failure deals the sheet without the Auto fix (an empty fragment).
                    open.auto_fragment = Some(result.map(|j| parse_edit(Some(&j))).unwrap_or_default());
                    cx.notify();
                }
            },
            cx,
        );
    }

    /// "▦ Deal a proof sheet": the spread for the working state, once the fragment is in.
    pub fn proof_candidates(&self) -> Option<Vec<ProofCandidate>> {
        let open = self.open.as_ref()?;
        let auto = open.auto_fragment.as_ref()?;
        Some(proof_spread(&open.working, auto, &self.presets(), self.kelvin().as_ref()))
    }

    /// A proof clicked: its record becomes the working state, named "Proof: <label>" (the
    /// as-shot cell is a plain change), and its label is what "+ New version" will be called.
    pub fn adopt_proof(&mut self, candidate: ProofCandidate, cx: &mut Context<Self>) {
        if !self.open.as_ref().is_some_and(|o| o.editable()) {
            return;
        }
        let label = (candidate.group != ProofGroup::AsShot).then(|| candidate.label.clone());
        if let (Some(open), Some(l)) = (self.open.as_mut(), &label) {
            open.adopted_label = Some(l.clone());
        }
        let step = label.map(|l| format!("Proof: {l}"));
        self.apply(candidate.record, step.as_deref(), cx);
    }

    // --- framing -----------------------------------------------------------------------------

    /// The perspective handles up (the stage un-warps) or down.
    pub fn set_perspective_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_mut() else { return };
        if open.perspective_mode != on {
            open.perspective_mode = on;
            self.rendered_changed(cx);
            cx.notify();
        }
    }

    /// "Correct perspective" / "Adjust corners": the quad (the default one if none) and the
    /// handles up; the crop goes.
    pub fn start_perspective(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref().filter(|o| o.editable()) else { return };
        let next = geometry::start_perspective(&open.working);
        self.set_perspective_mode(true, cx);
        self.apply(next, None, cx);
    }

    /// Perspective "Reset": quad and crop go, the handles down.
    pub fn clear_perspective(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref().filter(|o| o.editable()) else { return };
        let next = geometry::clear_perspective(&open.working);
        self.set_perspective_mode(false, cx);
        self.apply(next, None, cx);
    }

    /// An overlay chip: drawn in the crop box from now on, and remembered
    /// (`editor.crop_overlay`) after any earlier choice still being written.
    pub fn set_overlay(&mut self, overlay: CropOverlay, cx: &mut Context<Self>) {
        self.overlay = overlay;
        if let Some(from) = self.open.as_ref().map(|o| o.from) {
            self.write_in_order(
                OVERLAY_KEY,
                move |this, cx| {
                    let state = this.app.clone();
                    let rx = Runner::get(cx).run(move || with_catalog_as(&state, from, |c| c.set_setting(OVERLAY_KEY, overlay.key())));
                    cx.spawn(async move |this, cx| {
                        // Cosmetic: a failed write leaves the choice for this session only.
                        let _ = rx.await;
                        this.update(cx, |this, cx| this.setting_written(OVERLAY_KEY, cx)).ok();
                    })
                    .detach();
                },
                cx,
            );
        }
        cx.notify();
    }

    // --- the loupe print ---------------------------------------------------------------------

    /// "🖥 Loupe print" (DarkroomView.tsx's `togglePrintOnLoupe`): remembered
    /// (`basic-editor.printOnLoupe`) after any earlier click still being written. On opens
    /// (or raises) the pop-out loupe and puts the working print up at once; off takes it
    /// down — the pop-out follows its target again.
    pub fn set_print_on_loupe(&mut self, on: bool, cx: &mut Context<Self>) {
        self.print_on_loupe = on;
        self.print_clicks += 1;
        if let Some(from) = self.open.as_ref().map(|o| o.from) {
            self.write_in_order(
                PRINT_ON_LOUPE_KEY,
                move |this, cx| {
                    let state = this.app.clone();
                    let value = if on { "1" } else { "0" };
                    let rx = Runner::get(cx).run(move || with_catalog_as(&state, from, |c| c.set_setting(PRINT_ON_LOUPE_KEY, value)));
                    cx.spawn(async move |this, cx| {
                        // Cosmetic: a failed write leaves the choice for this session only.
                        let _ = rx.await;
                        this.update(cx, |this, cx| this.setting_written(PRINT_ON_LOUPE_KEY, cx)).ok();
                    })
                    .detach();
                },
                cx,
            );
        }
        if on {
            crate::loupe::window::open(cx);
            self.publish_print(cx);
        } else {
            if let Some(open) = self.open.as_mut() {
                open.print_timer = None;
            }
            self.clear_print(cx);
        }
        cx.notify();
    }

    /// The stored choice arrived (with the photo's settings): follow it without writing it
    /// back or opening the pop-out (React: the loupe ignores a print while it is closed).
    pub(super) fn print_on_loupe_read(&mut self, on: bool, cx: &mut Context<Self>) {
        if on == self.print_on_loupe {
            return;
        }
        self.print_on_loupe = on;
        if on {
            self.publish_print(cx);
        } else {
            self.clear_print(cx);
        }
        cx.notify();
    }

    /// The print follows the record on the settle, as React's broadcast rode its settle.
    pub(super) fn schedule_print(&mut self, cx: &mut Context<Self>) {
        let on = self.print_on_loupe;
        let Some(open) = self.open.as_mut() else { return };
        if !on {
            open.print_timer = None;
            return;
        }
        let seq = open.seq;
        let timer = cx.background_executor().timer(SETTLE);
        open.print_timer = Some(cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, cx| {
                if this.open.as_ref().is_some_and(|o| o.seq == seq) {
                    this.publish_print(cx);
                }
            })
            .ok();
        }));
    }

    /// Put the open photo's working print up: the full record (crop included — the print,
    /// not the stage), stamped with the engine that renders it, from the stage's pixels.
    fn publish_print(&mut self, cx: &mut Context<Self>) {
        use crate::shell::state::LoupePrint;
        if !self.print_on_loupe {
            return;
        }
        let Some(open) = self.open.as_ref() else { return };
        let edit_json = open.stamped(&open.working).to_json();
        let source = open.source_token().and_then(SourceToken::parse).unwrap_or(SourceToken::Preview);
        let same = self
            .shell
            .read(cx)
            .loupe_print()
            .is_some_and(|p| p.photo.id == open.photo.id && p.edit_json == edit_json && p.source == source);
        if same {
            return;
        }
        let print = LoupePrint { photo: open.photo.clone(), edit_json, source };
        self.shell.update(cx, |s, cx| s.set_loupe_print(Some(print), cx));
    }

    /// Take the print down, if one is up (the Darkroom is its only author).
    pub(super) fn clear_print(&mut self, cx: &mut Context<Self>) {
        if self.shell.read(cx).loupe_print().is_some() {
            self.shell.update(cx, |s, cx| s.set_loupe_print(None, cx));
        }
    }
}
