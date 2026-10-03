//! The cull session (`CullSession.tsx`): one photo, full screen, keyboard only, resumable.
//!
//! - **A frozen list.** The rows to cull (the selection, else the whole view) are copied when
//!   the session starts, with the catalog they came from. Culling changes the very fields the
//!   view is filtered by; a live list would pull photos out from under the cursor.
//! - **Decisions land at once on screen, in the background in the catalog.** Each decision is
//!   recorded in the session and the cursor moves on; the write goes through the shell's one
//!   culling queue ([`ShellState::apply_mark_reported`]), bound to the session's catalog. A
//!   write that fails takes the decision back off the photo and says so ("Not saved — …").
//!   The grid's rows are re-read once, when the session ends — not per keystroke.
//! - **Resumable.** The cursor is saved as a photo id in the catalog's settings
//!   (`cull.cursor.photo_id`, 400 ms after the last move, and at the end); the next session
//!   over a list holding that photo resumes there.
//! - **Preload.** Each move asks for the photo's preview first, then N+1, N−1, N+2…N+5 —
//!   further ahead than behind, because culling moves forward.
//!
//! [`CullState`] is the session as plain data; [`CullView`] draws it and owns its I/O.

use crate::image_store::{ImageState, ImageStore};
use crate::keymap::contexts;
use crate::library::*;
use crate::loupe::view::file_name;
use crate::loupe::zoom::fitted;
use crate::loupe::*;
use crate::shell::state::{Mark, ShellState};
use crate::shell::style::{Colors, COLOR_LABELS};
use crate::storage::ui;
use chairphoto_core::app::{with_catalog_as, AppState, CatalogIdentity};
use chairphoto_core::catalog::{Photo, PickState};
use chairphoto_core::image_pool::ImageKind;
use gpui_kit::assets::IconName;
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, relative, AnyElement, Context, Entity, EventEmitter, FocusHandle, ObjectFit, Subscription, Task,
    TestSupportExt as _, Window,
};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// Where the cursor is kept: catalog settings, since a photo id means nothing outside its
/// catalog.
pub const CURSOR_KEY: &str = "cull.cursor.photo_id";
/// The cursor is saved this long after the last move.
pub const CURSOR_DEBOUNCE: Duration = Duration::from_millis(400);
/// Preload window (React prefetched 5 ahead and 1 behind).
pub const PRELOAD_AHEAD: usize = 5;
pub const PRELOAD_BEHIND: usize = 1;

/// What this session did to one photo. Absent fields: left as they were.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Decision {
    pub rating: Option<i64>,
    pub pick: Option<PickState>,
    pub label: Option<String>,
}

impl Decision {
    fn merge(&mut self, mark: &Mark) {
        match mark {
            Mark::Rating(r) => self.rating = Some(*r),
            Mark::Pick(p) => self.pick = Some(*p),
            Mark::Label(l) => self.label = Some(l.clone()),
        }
    }
}

/// The session's account, for the summary and the status line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CullStats {
    /// Photos the cursor stopped on.
    pub visited: usize,
    /// Of those, how many got any decision.
    pub decided: usize,
    pub picked: usize,
    pub rejected: usize,
    pub rated: usize,
    pub labelled: usize,
    /// Photos the cursor never reached.
    pub remaining: usize,
    pub elapsed_secs: u64,
}

impl CullStats {
    /// The status line the grid shows afterwards (App.tsx's `onExit`).
    pub fn status_line(&self) -> String {
        format!(
            "Cull session: {} reviewed, {} picked, {} rejected, {} left.",
            self.visited, self.picked, self.rejected, self.remaining
        )
    }
}

/// The session as data. See the module docs.
#[derive(Debug, Clone)]
pub struct CullState {
    photos: Vec<Photo>,
    /// `None` until the saved cursor has been read.
    at: Option<usize>,
    decisions: HashMap<i64, Decision>,
    /// Each photo's latest decision write: a failure of an older one does not undo a newer.
    decision_seq: HashMap<i64, u64>,
    seq: u64,
    visited: HashSet<i64>,
    pub help: bool,
    pub resume_note: Option<String>,
    pub failure: Option<String>,
    pub summary: Option<CullStats>,
}

impl CullState {
    pub fn new(photos: Vec<Photo>) -> Self {
        CullState {
            photos,
            at: None,
            decisions: HashMap::new(),
            decision_seq: HashMap::new(),
            seq: 0,
            visited: HashSet::new(),
            help: false,
            resume_note: None,
            failure: None,
            summary: None,
        }
    }

    pub fn photos(&self) -> &[Photo] {
        &self.photos
    }

    pub fn at(&self) -> Option<usize> {
        self.at
    }

    pub fn current(&self) -> Option<&Photo> {
        self.at.and_then(|i| self.photos.get(i))
    }

    /// Start at the saved photo when this list holds it, else at the beginning (saying why
    /// when a saved one was not found).
    pub fn resume(&mut self, saved: Option<&str>) {
        let saved_id = saved.and_then(|s| s.trim().parse::<i64>().ok());
        match saved_id.and_then(|id| self.photos.iter().position(|p| p.id == id)) {
            Some(i) => {
                self.resume_note =
                    Some(format!("Resumed where you left off — photo {} of {}.", i + 1, self.photos.len()));
                self.move_to(i);
            }
            None => {
                if saved.is_some_and(|s| !s.is_empty()) {
                    self.resume_note =
                        Some("The photo you left off at isn't in this set — starting from the beginning.".into());
                }
                self.move_to(0);
            }
        }
    }

    fn move_to(&mut self, i: usize) {
        if let Some(p) = self.photos.get(i) {
            self.visited.insert(p.id);
            self.at = Some(i);
        }
    }

    /// Move by `delta`, stopping at the ends — a session that wraps makes "did I see
    /// everything?" unanswerable. Returns whether it moved.
    pub fn step(&mut self, delta: isize) -> bool {
        let Some(i) = self.at else { return false };
        let last = self.photos.len().saturating_sub(1) as isize;
        let next = (i as isize + delta).clamp(0, last) as usize;
        if next == i {
            return false;
        }
        self.move_to(next);
        true
    }

    /// Record `mark` on the current photo and move on. Returns the photo and the decision's
    /// sequence number, for [`rollback`](Self::rollback) if its write fails.
    pub fn decide(&mut self, mark: &Mark) -> Option<(i64, u64)> {
        let id = self.current()?.id;
        self.seq += 1;
        self.decisions.entry(id).or_default().merge(mark);
        self.decision_seq.insert(id, self.seq);
        self.step(1);
        Some((id, self.seq))
    }

    /// A decision's write failed: take the photo's decisions back off it (unless a newer one
    /// has been made since) and say so.
    pub fn rollback(&mut self, photo: i64, seq: u64, error: &str) {
        if self.decision_seq.get(&photo) == Some(&seq) {
            self.decisions.remove(&photo);
            self.decision_seq.remove(&photo);
        }
        let name = self.photos.iter().find(|p| p.id == photo).map(|p| file_name(&p.path)).unwrap_or_default();
        self.failure = Some(format!("{name}: {error}"));
    }

    /// The current photo as it now stands: its frozen row overlaid with this session's
    /// decisions (rating, pick, label).
    pub fn shown(&self) -> Option<(i64, PickState, String)> {
        let p = self.current()?;
        let d = self.decisions.get(&p.id).cloned().unwrap_or_default();
        Some((d.rating.unwrap_or(p.rating), d.pick.unwrap_or(p.pick_state), d.label.unwrap_or_else(|| p.label.clone())))
    }

    pub fn decision(&self, photo: i64) -> Option<&Decision> {
        self.decisions.get(&photo)
    }

    pub fn stats(&self, elapsed: Duration) -> CullStats {
        let ds: Vec<&Decision> = self.decisions.values().collect();
        CullStats {
            visited: self.visited.len(),
            decided: ds.len(),
            picked: ds.iter().filter(|d| d.pick == Some(PickState::Pick)).count(),
            rejected: ds.iter().filter(|d| d.pick == Some(PickState::Reject)).count(),
            rated: ds.iter().filter(|d| d.rating.is_some_and(|r| r > 0)).count(),
            labelled: ds.iter().filter(|d| d.label.as_deref().is_some_and(|l| !l.is_empty())).count(),
            remaining: self.photos.len() - self.visited.len(),
            elapsed_secs: elapsed.as_secs_f64().round() as u64,
        }
    }
}

/// The session ended: back to the grid.
#[derive(Debug, Clone, PartialEq)]
pub struct CullEnded(pub CullStats);

/// The full-screen session. See the module docs.
pub struct CullView {
    app: AppState,
    shell: Entity<ShellState>,
    images: Entity<ImageStore>,
    from: CatalogIdentity,
    pub state: CullState,
    focus: FocusHandle,
    started: Instant,
    /// The debounced cursor save; replaced by every move.
    save: Option<Task<()>>,
    /// Set by `finish`: no more decisions; the summary shows once the cursor is saved.
    finishing: bool,
    _observers: [Subscription; 1],
}

impl EventEmitter<CullEnded> for CullView {}

impl CullView {
    /// A session over `photos`, read from the catalog `from`. Reads the saved cursor off the
    /// UI thread; until it lands the view says "Opening session…".
    pub fn new(
        app: AppState,
        shell: Entity<ShellState>,
        images: Entity<ImageStore>,
        photos: Vec<Photo>,
        from: CatalogIdentity,
        cx: &mut Context<Self>,
    ) -> Self {
        let read = {
            let app = app.clone();
            cx.background_executor().spawn(async move { with_catalog_as(&app, from, |c| c.get_setting(CURSOR_KEY)) })
        };
        cx.spawn(async move |this, cx| {
            let saved = read.await.ok().flatten();
            this.update(cx, |v, cx| {
                v.state.resume(saved.as_deref());
                v.moved(false, cx);
            })
            .ok();
        })
        .detach();
        let _observers = [cx.observe(&images, |_, _, cx| cx.notify())];
        CullView {
            app,
            shell,
            images,
            from,
            state: CullState::new(photos),
            focus: cx.focus_handle(),
            started: cx.background_executor().now(),
            save: None,
            finishing: false,
            _observers,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// After every cursor move: preload around it, and (re)start the debounced save.
    fn moved(&mut self, save: bool, cx: &mut Context<Self>) {
        if let Some(at) = self.state.at() {
            let ids: Vec<i64> = self.state.photos().iter().map(|p| p.id).collect();
            self.images.update(cx, |s, _| s.navigate_window(&ids, at, ImageKind::Preview, PRELOAD_AHEAD, PRELOAD_BEHIND));
        }
        if save {
            if let Some(id) = self.state.current().map(|p| p.id) {
                let timer = cx.background_executor().timer(CURSOR_DEBOUNCE);
                let (app, from) = (self.app.clone(), self.from);
                self.save = Some(cx.spawn(async move |_, cx| {
                    timer.await;
                    let write = cx.background_executor().spawn(async move { save_cursor(&app, from, id) });
                    write.await;
                }));
            }
        }
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.blocked() {
            return;
        }
        if self.state.step(delta) {
            self.moved(true, cx);
        }
    }

    fn blocked(&self) -> bool {
        self.finishing || self.state.summary.is_some() || self.state.at().is_none()
    }

    /// Record, move on, write in the background; a failed write is rolled back.
    pub fn decide(&mut self, mark: Mark, cx: &mut Context<Self>) {
        if self.blocked() {
            return;
        }
        let Some((photo, seq)) = self.state.decide(&mark) else { return };
        let answer = self.shell.update(cx, |s, cx| s.apply_mark_reported(mark, photo, self.from, cx));
        cx.spawn(async move |this, cx| {
            let result = answer.await.unwrap_or_else(|_| Err("the write was dropped".into()));
            if let Err(e) = result {
                this.update(cx, |v, cx| {
                    v.state.rollback(photo, seq, &e);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
        self.moved(true, cx);
    }

    fn escape(&mut self, cx: &mut Context<Self>) {
        if let Some(stats) = self.state.summary.clone() {
            cx.emit(CullEnded(stats));
        } else if self.state.help {
            self.state.help = false;
            cx.notify();
        } else {
            self.finish(cx);
        }
    }

    /// End the session: save the cursor now (not on the debounce), then show the summary.
    pub fn finish(&mut self, cx: &mut Context<Self>) {
        if self.finishing || self.state.summary.is_some() {
            return;
        }
        self.finishing = true;
        self.save = None;
        let stats = self.state.stats(cx.background_executor().now().saturating_duration_since(self.started));
        let cursor = self.state.current().map(|p| p.id);
        let (app, from) = (self.app.clone(), self.from);
        let write = cx.background_executor().spawn(async move {
            if let Some(id) = cursor {
                save_cursor(&app, from, id);
            }
        });
        cx.spawn(async move |this, cx| {
            write.await;
            this.update(cx, |v, cx| {
                v.state.summary = Some(stats);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn render_summary(&self, stats: &CullStats, colors: Colors, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let total = self.state.photos().len();
        let mins = stats.elapsed_secs / 60;
        let secs = stats.elapsed_secs % 60;
        let rate = if stats.visited > 0 { stats.elapsed_secs as f64 / stats.visited as f64 } else { 0. };
        let row = |k: &'static str, v: String| {
            div()
                .flex()
                .gap(px(16.))
                .child(div().w(px(90.)).text_color(colors.mute).child(k))
                .child(div().text_color(colors.txt).child(v))
        };
        let mut lead = format!("{} of {} photo{} reviewed", stats.visited, total, if total == 1 { "" } else { "s" });
        if stats.remaining > 0 {
            lead.push_str(&format!(" · {} still to go", stats.remaining));
        }
        let mut decided = stats.decided.to_string();
        if stats.visited > stats.decided {
            decided.push_str(&format!(" · {} left as they were", stats.visited - stats.decided));
        }
        let mut time = if mins > 0 { format!("{mins}m {secs}s") } else { format!("{secs}s") };
        if rate > 0. {
            time.push_str(&format!(" · {rate:.1}s per photo"));
        }
        div()
            .id("cull-summary")
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(28.))
            .rounded(px(10.))
            .bg(colors.panel)
            .border_1()
            .border_color(colors.border)
            .text_size(px(13.))
            .child(div().text_size(px(20.)).text_color(colors.txt).child("Session over"))
            .child(div().id("cull-summary-lead").text_color(colors.dim).child(lead.clone()).aria_label(lead).test_support())
            .child(row("Decided", decided))
            .child(row("Picked", stats.picked.to_string()))
            .child(row("Rejected", stats.rejected.to_string()))
            .child(row("Rated", stats.rated.to_string()))
            .child(row("Labelled", stats.labelled.to_string()))
            .child(row("Time", time))
            .when(stats.remaining > 0, |d| {
                d.child(
                    div()
                        .text_color(colors.mute)
                        .text_size(px(12.))
                        .child("Your place is saved — starting a session again picks up here."),
                )
            })
            .child(ui::clickable(
                ui::primary("cull-done", "Back to the grid", true, colors),
                true,
                cx.listener(|this, _, _, cx| {
                    if let Some(stats) = this.state.summary.clone() {
                        cx.emit(CullEnded(stats));
                    }
                }),
            ))
            .test_support()
            .into_any_element()
    }
}

/// Persist the cursor in the session's catalog (never another one); a failure costs only the
/// resume point.
fn save_cursor(app: &AppState, from: CatalogIdentity, id: i64) {
    if let Err(e) = with_catalog_as(app, from, |c| c.set_setting(CURSOR_KEY, &id.to_string())) {
        eprintln!("cull: cursor not saved: {e}");
    }
}

impl Render for CullView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let root = div()
            .id("cull")
            .key_context(contexts::CULL)
            .track_focus(&self.focus)
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            // Full screen over the shell: nothing beneath it sees the mouse. Without this a
            // click reaches the grid, which takes the focus (and the selection) and with it
            // the culling keys.
            .occlude()
            .bg(gpui_kit::black())
            .text_color(colors.txt)
            .on_action(cx.listener(|this, _: &CullNext, _, cx| this.step(1, cx)))
            .on_action(cx.listener(|this, _: &CullPrevious, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &CullEscape, _, cx| this.escape(cx)))
            .on_action(cx.listener(|this, _: &CullConfirm, _, cx| {
                if let Some(stats) = this.state.summary.clone() {
                    cx.emit(CullEnded(stats));
                }
            }))
            .on_action(cx.listener(|this, _: &CullHelp, _, cx| {
                if !this.blocked() {
                    this.state.help = !this.state.help;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &Rate0, _, cx| this.decide(Mark::Rating(0), cx)))
            .on_action(cx.listener(|this, _: &Rate1, _, cx| this.decide(Mark::Rating(1), cx)))
            .on_action(cx.listener(|this, _: &Rate2, _, cx| this.decide(Mark::Rating(2), cx)))
            .on_action(cx.listener(|this, _: &Rate3, _, cx| this.decide(Mark::Rating(3), cx)))
            .on_action(cx.listener(|this, _: &Rate4, _, cx| this.decide(Mark::Rating(4), cx)))
            .on_action(cx.listener(|this, _: &Rate5, _, cx| this.decide(Mark::Rating(5), cx)))
            .on_action(cx.listener(|this, _: &MarkPick, _, cx| this.decide(Mark::Pick(PickState::Pick), cx)))
            .on_action(cx.listener(|this, _: &MarkReject, _, cx| this.decide(Mark::Pick(PickState::Reject), cx)))
            .on_action(cx.listener(|this, _: &MarkUnflag, _, cx| this.decide(Mark::Pick(PickState::None), cx)))
            .on_action(cx.listener(|this, _: &LabelRed, _, cx| this.decide(Mark::Label("Red".into()), cx)))
            .on_action(cx.listener(|this, _: &LabelYellow, _, cx| this.decide(Mark::Label("Yellow".into()), cx)))
            .on_action(cx.listener(|this, _: &LabelGreen, _, cx| this.decide(Mark::Label("Green".into()), cx)))
            .on_action(cx.listener(|this, _: &LabelBlue, _, cx| this.decide(Mark::Label("Blue".into()), cx)))
            .on_action(cx.listener(|this, _: &LabelPurple, _, cx| this.decide(Mark::Label("Purple".into()), cx)))
            .on_action(cx.listener(|this, _: &LabelNone, _, cx| this.decide(Mark::Label(String::new()), cx)))
            .test_support();

        if let Some(stats) = self.state.summary.clone() {
            let summary = self.render_summary(&stats, colors, cx);
            return root.flex().items_center().justify_center().child(summary).into_any_element();
        }
        let (Some(at), Some(photo)) = (self.state.at(), self.state.current().cloned()) else {
            return root
                .flex()
                .items_center()
                .justify_center()
                .child(div().id("cull-opening").text_color(colors.mute).child("Opening session…").test_support())
                .into_any_element();
        };
        let total = self.state.photos().len();
        let image = self.images.update(cx, |s, _| match s.get(photo.id, ImageKind::Preview) {
            ImageState::Ready(l) => Ok(Some(l.image)),
            ImageState::Failed(_) => Err(()),
            _ => match s.peek(photo.id, ImageKind::Thumb) {
                ImageState::Ready(l) => Ok(Some(l.image)),
                _ => Ok(None),
            },
        });
        let stage = match image {
            Ok(Some(image)) => fitted("cull-image", image, ObjectFit::Contain).into_any_element(),
            Ok(None) => div().into_any_element(),
            Err(()) => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(colors.mute)
                .child("No preview available")
                .into_any_element(),
        };
        let pos = format!("{} / {}", at + 1, total);
        let name = file_name(&photo.path);
        let top = div()
            .absolute()
            .top(px(14.))
            .left(px(18.))
            .right(px(18.))
            .flex()
            .gap(px(14.))
            .items_center()
            .text_size(px(13.))
            .child(div().id("cull-pos").child(pos.clone()).aria_label(pos).test_support())
            .child(div().id("cull-name").child(name.clone()).aria_label(name).test_support())
            .children(self.state.resume_note.clone().map(|n| {
                div().id("cull-note").text_color(colors.mute).child(n.clone()).aria_label(n).test_support()
            }));
        let (rating, pick, label) = self.state.shown().unwrap_or((0, PickState::None, String::new()));
        let mut state = div().flex().gap(px(10.)).items_center().text_size(px(13.));
        if rating > 0 {
            let stars = "★".repeat(rating as usize);
            state = state.child(
                div().id("cull-stars").text_color(colors.rating).child(stars.clone()).aria_label(stars).test_support(),
            );
        }
        match pick {
            PickState::Pick => state = state.child(div().id("cull-pick").text_color(colors.ok).child("PICK").test_support()),
            PickState::Reject => {
                state = state.child(div().id("cull-reject").text_color(colors.danger).child("REJECT").test_support())
            }
            PickState::None => {}
        }
        if let Some(c) = COLOR_LABELS.iter().find(|l| l.name == label) {
            state = state.child(div().id("cull-label").size(px(12.)).rounded_full().bg(c.color()).test_support());
        }
        if at + 1 == total {
            state = state.child(div().id("cull-end").text_color(colors.mute).child("End of set — Esc for the summary.").test_support());
        }
        if let Some(f) = self.state.failure.clone() {
            let line = format!("Not saved — {f}");
            state = state.child(
                div().id("cull-failure").text_color(colors.danger).child(line.clone()).aria_label(line).test_support(),
            );
        }
        let fill = (at + 1) as f32 / total.max(1) as f32;
        let bottom = div()
            .absolute()
            .bottom(px(14.))
            .left(px(18.))
            .right(px(18.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(state)
            .child(div().h(px(3.)).w_full().bg(colors.line).child(div().h_full().w(relative(fill)).bg(colors.accent)))
            .child(div().text_size(px(11.)).text_color(colors.mute).child("h — keys · Esc — end session"));
        let help = self.state.help.then(|| {
            let row = |k: AnyElement, v: &'static str| {
                div().flex().gap(px(16.)).child(div().w(px(90.)).text_color(colors.txt).child(k)).child(
                    div().text_color(colors.dim).child(v),
                )
            };
            // → and ← draw tiny: the UI font lacks U+2192/U+2190, and the fallback font it
            // reaches for draws them at a fraction of the surrounding text's size. ↓ and ↑ are
            // in the font and stay plain text (#197).
            let arrow_key = |id: &'static str, arrow_id: &'static str, icon: IconName, rest: &'static str, label: &'static str| {
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(ui::sized_icon(arrow_id, icon))
                    .child(rest)
                    .aria_label(label)
                    .test_support()
                    .into_any_element()
            };
            div()
                .id("cull-help")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(colors.scrim)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.state.help = false;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("cull-help-card")
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .p(px(22.))
                        .rounded(px(10.))
                        .bg(colors.panel)
                        .text_size(px(13.))
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(div().text_size(px(16.)).child("Keys"))
                        .child(row("0 – 5".into_any_element(), "rating"))
                        .child(row("p / x / u".into_any_element(), "pick · reject · clear"))
                        .child(row("r y g b v".into_any_element(), "colour label"))
                        .child(row("n".into_any_element(), "clear colour label"))
                        .child(row(
                            arrow_key(
                                "cull-help-next-key",
                                "cull-help-next-arrow",
                                IconName::ArrowRight,
                                "↓ space",
                                "→ ↓ space",
                            ),
                            "next, without deciding",
                        ))
                        .child(row(
                            arrow_key("cull-help-back-key", "cull-help-back-arrow", IconName::ArrowLeft, "↑", "← ↑"),
                            "back",
                        ))
                        .child(row("Esc".into_any_element(), "end the session"))
                        .child(
                            div()
                                .id("cull-help-note")
                                .max_w(px(380.))
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap(px(4.))
                                .text_size(px(12.))
                                .text_color(colors.mute)
                                .child("Every decision moves you on, exactly as it does in the grid — press")
                                .child(ui::sized_icon("cull-help-note-arrow", IconName::ArrowLeft))
                                .child(
                                    "to go back and change one. Where you stop is remembered, so the next session resumes here.",
                                )
                                .aria_label(
                                    "Every decision moves you on, exactly as it does in the grid — press ← to go back and change one. Where you stop is remembered, so the next session resumes here.",
                                )
                                .test_support(),
                        ),
                )
                .test_support()
        });
        root.child(div().id("cull-stage").absolute().top_0().left_0().size_full().p(px(48.)).child(stage).test_support())
            .child(top)
            .child(bottom)
            .children(help)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo(id: i64) -> Photo {
        Photo {
            id,
            uuid: format!("uuid-{id}"),
            path: format!("2026/p{id}.ARW"),
            rating: 0,
            label: String::new(),
            pick_state: PickState::None,
            capture_time: None,
            width: None,
            height: None,
            camera_model: None,
            lens: None,
            aperture: None,
            shutter_speed: None,
            iso: None,
            external_editors: String::new(),
            thumbnail_path: None,
            stack_count: 0,
            stack_parent_id: None,
            metadata_ready: 1,
            sharpness: None,
            sharpness_method: None,
            burst_flag: None,
            version_count: 0,
            cover_token: None,
        }
    }

    fn state(n: i64) -> CullState {
        CullState::new((1..=n).map(photo).collect())
    }

    #[test]
    fn resume_at_the_saved_photo_or_the_start() {
        let mut s = state(5);
        s.resume(Some("3"));
        assert_eq!(s.at(), Some(2));
        assert_eq!(s.resume_note.as_deref(), Some("Resumed where you left off — photo 3 of 5."));
        let mut s = state(5);
        s.resume(Some("99"));
        assert_eq!(s.at(), Some(0));
        assert!(s.resume_note.unwrap().contains("isn't in this set"));
        let mut s = state(5);
        s.resume(None);
        assert_eq!((s.at(), s.resume_note), (Some(0), None));
    }

    #[test]
    fn steps_stop_at_the_ends() {
        let mut s = state(3);
        s.resume(None);
        assert!(!s.step(-1));
        assert!(s.step(1) && s.step(1));
        assert!(!s.step(1), "no wrap");
        assert_eq!(s.at(), Some(2));
    }

    #[test]
    fn decisions_advance_overlay_the_row_and_count() {
        let mut s = state(4);
        s.resume(None);
        assert_eq!(s.decide(&Mark::Rating(3)), Some((1, 1)));
        assert_eq!(s.at(), Some(1));
        s.step(-1);
        assert_eq!(s.shown(), Some((3, PickState::None, String::new())), "photo 1 as decided");
        assert_eq!(s.stats(Duration::ZERO).remaining, 2, "photos 3 and 4 not reached");
        s.decide(&Mark::Pick(PickState::Pick)); // photo 1 again: rated and picked
        s.decide(&Mark::Pick(PickState::Reject)); // photo 2
        s.decide(&Mark::Label("Red".into())); // photo 3
        assert_eq!(
            s.stats(Duration::from_millis(9_400)),
            CullStats { visited: 4, decided: 3, picked: 1, rejected: 1, rated: 1, labelled: 1, remaining: 0, elapsed_secs: 9 }
        );
    }

    #[test]
    fn a_failed_write_takes_the_decision_back_unless_a_newer_one_followed() {
        let mut s = state(3);
        s.resume(None);
        let (id, seq) = s.decide(&Mark::Rating(4)).unwrap();
        s.rollback(id, seq, "disk full");
        assert!(s.decision(id).is_none());
        assert_eq!(s.failure.as_deref(), Some("p1.ARW: disk full"));
        // A newer decision on the same photo survives an older one's failure.
        s.step(-1);
        let (id, old) = s.decide(&Mark::Rating(2)).unwrap();
        s.step(-1);
        s.decide(&Mark::Rating(5)).unwrap();
        s.rollback(id, old, "late failure");
        assert_eq!(s.decision(id).and_then(|d| d.rating), Some(5));
    }
}
