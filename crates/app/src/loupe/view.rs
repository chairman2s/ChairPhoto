//! [`LoupeView`]: the inline loupe on the Library stage (App.tsx's `loupeInline` branch).
//!
//! - **What it shows.** [`ShellState::loupe_target`] — the active photo (or a stacked child
//!   viewed off-grid). A video shows its poster frame and "▶ Play in system player", which
//!   hands the file to the desktop's default player (map #92, "Video playback in the GPUI
//!   app"). An edited version chosen in the inspector shows its render instead of the
//!   camera preview (`edit` feature), with its hi-res render fetched on the first zoom-in.
//! - **Navigation order.** When the target changes the image layer is asked for the target's
//!   preview first, then N+1 and N−1, then N+2…N+5 and N−2 — one pool batch
//!   (`ImageStore::navigate_window_as`), superseding the old window's queued requests. A zoom
//!   tier wanted for the previous photo is released. Each loupe holds its own claim on what
//!   it navigated to, so it releases only what no other loupe still wants: the inline loupe
//!   closing leaves the pop-out's preloads alone, and the other way round (#110).
//! - **Stale frames.** The image shows only tiers keyed by the target's id (the image layer
//!   drops answers for released keys, and clears on a catalog switch); while the target's
//!   preview is on its way its own thumbnail stands in, never the previous photo.
//! - **A second window.** The pop-out loupe ([`crate::loupe::window`], #110) is another
//!   `LoupeView`, with [`Follow::Window`], over the same entities: it follows the target
//!   whatever the main stage shows, has no "Back to grid", and ignores Enter/Esc and C. When
//!   its window closes it is [released](LoupeView::release).
//!
//! Keys ([`contexts::LOUPE`]): ←/→/↑/↓ step (Shift extends), Enter/Esc back to the grid, C
//! Compare, and the culling keys, which mark the targets and advance as in the grid.

use crate::image_store::{ClaimId, ImageStore};
use crate::keymap::contexts;
use crate::library::*;
use crate::loupe::zoom::ZoomImage;
use crate::loupe::CloseLoupe;
use crate::model::AppModel;
use crate::modules::{ModuleRegistry, PanelSlot};
use crate::shell::actions::OpenCompare;
use crate::shell::state::{Mark, ShellState, StageView};
use crate::shell::style::{Colors, COLOR_LABELS};
use crate::storage::ui;
use chairphoto_core::app::with_catalog_as;
use chairphoto_core::catalog::{Photo, PickState};
use chairphoto_core::image_pool::ImageKind;
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, AnyElement, App, Context, Entity, FocusHandle, Global, Hsla, InteractiveElement, SharedString,
    Subscription, TestSupportExt as _, Window,
};
use std::path::Path;
use std::rc::Rc;

/// How far the loupe preloads ahead and behind (React prefetched +1…+5, −1, −2).
pub const PRELOAD_AHEAD: usize = 5;
pub const PRELOAD_BEHIND: usize = 2;

/// Hands a file to the desktop's default application: `App::open_with_system` in the app; a
/// recorder in tests (GPUI's test platform does not implement it).
#[derive(Clone)]
pub struct SystemOpener(pub Rc<dyn Fn(&Path, &mut App)>);

impl Global for SystemOpener {}

fn open_with_system(path: &Path, cx: &mut App) {
    match cx.try_global::<SystemOpener>().cloned() {
        Some(opener) => (opener.0)(path, cx),
        None => cx.open_with_system(path),
    }
}

/// Which photo a loupe follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Follow {
    /// The inline loupe: the target only while the stage shows the loupe.
    Inline,
    /// A loupe window (#110): the target whatever the main stage shows.
    Window,
}

/// See the module docs.
pub struct LoupeView {
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    images: Entity<ImageStore>,
    modules: Entity<ModuleRegistry>,
    zoom: Entity<ZoomImage>,
    focus: FocusHandle,
    follow: Follow,
    /// The target the last navigation was for.
    navigated: Option<i64>,
    /// This view's hold on the images it navigated to (`ImageStore::set_claim`): another
    /// loupe over the same store — the inline one and the pop-out — keeps its own.
    claim: ClaimId,
    /// Released ([`Self::release`]): its window closed, so it shows and wants nothing more.
    released: bool,
    #[cfg(feature = "edit")]
    renders: Entity<crate::loupe::edit_renders::EditRenders>,
    _observers: Vec<Subscription>,
}

impl LoupeView {
    pub fn new(
        model: Entity<AppModel>,
        shell: Entity<ShellState>,
        images: Entity<ImageStore>,
        modules: Entity<ModuleRegistry>,
        follow: Follow,
        cx: &mut Context<Self>,
    ) -> Self {
        let zoom = cx.new(|cx| {
            let mut z = ZoomImage::new(images.clone(), "loupe-image", cx);
            // Relocate / Retrieve / Remove are the main window's actions.
            z.set_unavailable_actions(follow == Follow::Inline);
            z
        });
        #[allow(unused_mut)]
        let mut observers = vec![
            cx.observe(&shell, |this, _, cx| {
                this.sync(cx);
                cx.notify();
            }),
            cx.observe(&modules, |_, _, cx| cx.notify()),
        ];
        #[cfg(feature = "edit")]
        let renders = {
            let pool = images.read(cx).pool();
            let renders = cx.new(|cx| crate::loupe::edit_renders::EditRenders::new(pool, cx));
            observers.push(cx.observe(&renders, |this, _, cx| this.sync_version(cx)));
            observers.push(cx.observe(&zoom, |this, _, cx| this.sync_version(cx)));
            renders
        };
        let claim = images.update(cx, |s, _| s.new_claim());
        let mut view = LoupeView {
            model,
            shell,
            images,
            modules,
            zoom,
            focus: cx.focus_handle(),
            follow,
            navigated: None,
            claim,
            released: false,
            #[cfg(feature = "edit")]
            renders,
            _observers: observers,
        };
        view.sync(cx);
        view
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn zoom(&self) -> &Entity<ZoomImage> {
        &self.zoom
    }

    fn target(&self, cx: &App) -> Option<Photo> {
        if self.released {
            return None;
        }
        let shell = self.shell.read(cx);
        let showing = match self.follow {
            Follow::Inline => shell.stage_view() == StageView::Loupe,
            Follow::Window => true,
        };
        showing.then(|| shell.loupe_target().cloned()).flatten()
    }

    /// Follow the target: show it, and on a change ask for it first, then its neighbours. The
    /// photo left gives up its full-resolution tier — pending or loaded; the target keeps its.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let target = self.target(cx).map(|p| p.id);
        self.zoom.update(cx, |z, cx| z.set_photo(target, cx));
        if target != self.navigated {
            let left = std::mem::replace(&mut self.navigated, target);
            let rows = self.shell.read(cx).library.photo_ids();
            let claim = self.claim;
            self.images.update(cx, |store, cx| {
                match target {
                    // This view's claim becomes the new window and the target's zoom tier:
                    // what it held before is released unless another view holds it too.
                    Some(id) => {
                        let zoom = [(id, ImageKind::Zoom)];
                        match rows.iter().position(|&r| r == id) {
                            Some(index) => store.navigate_window_as(
                                claim,
                                &rows,
                                index,
                                ImageKind::Preview,
                                PRELOAD_AHEAD,
                                PRELOAD_BEHIND,
                                &zoom,
                            ),
                            // Off-grid (a stacked child): just this one.
                            None => store.navigate_window_as(claim, &[id], 0, ImageKind::Preview, 0, 0, &zoom),
                        }
                    }
                    // Closed: nothing of this view's is wanted any more.
                    None => store.set_claim(claim, []),
                }
                if let Some(left) = left {
                    store.evict(|k| k.kind == ImageKind::Zoom && k.photo == left, cx);
                }
            });
        }
        #[cfg(feature = "edit")]
        self.sync_version(cx);
    }

    /// Which photo this loupe follows.
    pub fn follow(&self) -> Follow {
        self.follow
    }

    /// The window this view is in is closing: show nothing more, release what this view alone
    /// wanted (its preload window, its target's full-resolution tier, its version renders) and
    /// give up its image claim. The view's module panels go with the window
    /// (`ModuleRegistry`'s per-window cache).
    pub fn release(&mut self, cx: &mut Context<Self>) {
        if self.released {
            return;
        }
        self.released = true;
        self.sync(cx);
        let claim = self.claim;
        self.images.update(cx, |s, _| s.drop_claim(claim));
    }

    /// The active version's render in place of the preview, while one is chosen for the
    /// photo the loupe shows (App.tsx's `editedSrc` + `renderHiVersion`).
    #[cfg(feature = "edit")]
    fn sync_version(&mut self, cx: &mut Context<Self>) {
        use crate::loupe::edit_renders::{preview_job, RenderState};
        use crate::loupe::zoom::Override;
        /// React's loupe render size (`renderForLoupe`, `edit://` at 2560 px).
        const LOUPE_EDGE: u32 = 2560;
        let target = self.zoom.read(cx).photo();
        let epoch = self.model.read(cx).catalog_epoch;
        let version = self.shell.read(cx).active_version().filter(|v| Some(v.photo_id) == target).cloned();
        let Some(version) = version else {
            self.renders.update(cx, |r, cx| r.want(&[], cx));
            self.zoom.update(cx, |z, cx| z.set_override(None, cx));
            return;
        };
        let lo = preview_job(version.photo_id, &version.edit_json, LOUPE_EDGE, false, epoch);
        let hi = preview_job(version.photo_id, &version.edit_json, 0, true, epoch);
        let wants_hi = self.zoom.read(cx).wants_hi();
        let jobs = if wants_hi { vec![lo.clone(), hi.clone()] } else { vec![lo.clone()] };
        self.renders.update(cx, |r, cx| r.want(&jobs, cx));
        let renders = self.renders.read(cx);
        let mut over = Override::default();
        match renders.get(&lo) {
            RenderState::Ready(image) => over.lo = Some(image),
            RenderState::Failed(e) => over.failed = Some(e),
            _ => {}
        }
        match renders.get(&hi) {
            RenderState::Ready(image) => over.hi = Some(image),
            RenderState::Failed(_) => over.hi_settled = true,
            _ => {}
        }
        self.zoom.update(cx, |z, cx| z.set_override(Some(over), cx));
    }

    /// ↺ / ↻: a non-destructive rotation (`rotate_photo`), bound to the catalog the rows were
    /// read from; the photo's cached tiers are dropped so every view re-renders it.
    pub fn rotate(&mut self, photo: i64, delta: i64, cx: &mut Context<Self>) {
        let Some(from) = self.shell.read(cx).rows_from() else { return };
        let state = self.model.read(cx).state().clone();
        let run = cx.background_executor().spawn(async move { with_catalog_as(&state, from, |c| c.rotate_photo(photo, delta)) });
        cx.spawn(async move |this, cx| {
            let result = run.await;
            this.update(cx, |this, cx| match result {
                Ok(_) => {
                    this.images.update(cx, |s, cx| s.invalidate(photo, cx));
                    this.shell.update(cx, |s, cx| s.refresh_rows(cx));
                }
                Err(e) => this.model.update(cx, |m, cx| m.set_status(format!("Could not rotate: {e}"), cx)),
            })
            .ok();
        })
        .detach();
    }

    /// "▶ Play in system player": resolve the file (never from `photos.path` directly) off
    /// the UI thread, then hand it to the desktop.
    pub fn play(&mut self, photo: i64, cx: &mut Context<Self>) {
        let Some(from) = self.shell.read(cx).rows_from() else { return };
        let state = self.model.read(cx).state().clone();
        let run = cx.background_executor().spawn(async move { with_catalog_as(&state, from, |c| c.require_photo_path(photo)) });
        cx.spawn(async move |this, cx| {
            let result = run.await;
            this.update(cx, |this, cx| match result {
                Ok(path) => open_with_system(&path, cx),
                Err(e) => this.model.update(cx, |m, cx| m.set_status(format!("Could not play the video: {e}"), cx)),
            })
            .ok();
        })
        .detach();
    }

    fn render_bar(&self, photo: &Photo, colors: Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let shell = self.shell.read(cx);
        let selection = shell.library.selection();
        let back_to_original = selection.extra_photo.is_some() && selection.stack_origin.is_some();
        let soft = shell.soft_threshold;
        let version = shell.active_version().map(|v| v.name.clone());
        let id = photo.id;
        let name = file_name(&photo.path);
        let mut tags = div().flex().items_center().gap(px(6.)).min_w_0().overflow_hidden();
        tags = tags.child(
            div()
                .id("loupe-filename")
                .text_size(px(13.))
                .text_color(colors.txt)
                .whitespace_nowrap()
                .child(name.clone())
                .aria_label(name)
                .test_support(),
        );
        let tag = |id: &'static str, text: String, color: Hsla| {
            div().id(id).text_size(px(11.5)).text_color(color).child(text.clone()).aria_label(text).test_support()
        };
        match photo.pick_state {
            PickState::Reject => tags = tags.child(tag("loupe-tag-reject", "rejected".into(), colors.danger)),
            PickState::Pick => tags = tags.child(tag("loupe-tag-pick", "pick".into(), colors.ok)),
            PickState::None => {}
        }
        if photo.rating > 0 {
            tags = tags.child(tag("loupe-tag-stars", "★".repeat(photo.rating as usize), colors.rating));
        }
        if let Some(c) = COLOR_LABELS.iter().find(|l| l.name == photo.label) {
            tags = tags.child(div().size(px(9.)).rounded_full().bg(c.color()));
        }
        if photo.sharpness.is_some_and(|s| s < soft) {
            tags = tags.child(tag("loupe-tag-soft", "soft".into(), colors.danger));
        }
        match photo.burst_flag.as_deref() {
            Some("soft-in-burst") => tags = tags.child(tag("loupe-tag-soft-burst", "soft-in-burst".into(), colors.danger)),
            Some("sharpest-of-burst") => {
                tags = tags.child(tag("loupe-tag-sharpest", "♛ sharpest of burst".into(), colors.accent))
            }
            _ => {}
        }
        if let Some(name) = version {
            tags = tags.child(tag("loupe-tag-version", format!("· {name}"), colors.accent));
        }
        div()
            .id("loupe-bar")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .h(px(40.))
            .px(px(12.))
            .border_b_1()
            .border_color(colors.border)
            // The pop-out has no grid to go back to.
            .when(self.follow == Follow::Inline, |d| {
                let shell = self.shell.clone();
                d.child(ui::clickable(ui::chip("loupe-back", "‹ Back to grid (Esc)", true, colors), true, move |_, _, cx| {
                    shell.update(cx, |s, cx| s.set_loupe(false, cx))
                }))
            })
            .when(back_to_original && self.follow == Follow::Inline, |d| {
                let shell = self.shell.clone();
                d.child(ui::clickable(ui::chip("loupe-back-original", "‹ Back to original", true, colors), true, move |_, _, cx| {
                    shell.update(cx, |s, cx| s.select_with(cx, |l| l.back_to_original()))
                }))
            })
            .child(ui::clickable(
                ui::chip("loupe-rotate-left", "↺", true, colors),
                true,
                cx.listener(move |this, _, _, cx| this.rotate(id, -90, cx)),
            ))
            .child(ui::clickable(
                ui::chip("loupe-rotate-right", "↻", true, colors),
                true,
                cx.listener(move |this, _, _, cx| this.rotate(id, 90, cx)),
            ))
            .child(tags)
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(colors.mute)
                    .whitespace_nowrap()
                    .child("scroll zoom · drag pan · dbl-click 100% · P pick · X reject · ← →"),
            )
    }

    fn step(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| s.select_with(cx, |l| l.step_active(delta, extend)));
    }
}

/// The last path component (`path.split("/").pop()`).
pub fn file_name(path: &str) -> SharedString {
    path.rsplit('/').next().unwrap_or(path).to_string().into()
}

/// Attach the culling keys' handlers (0–5, P/X/U, labels) to `el`: each marks through
/// `ShellState::apply_mark` — the one write path — and advances as in the grid. In Compare the
/// shell marks the focused pane instead.
pub fn with_culling_actions<E: InteractiveElement>(el: E, shell: &Entity<ShellState>) -> E {
    let mark = |shell: Entity<ShellState>, mark: Mark| {
        move |cx: &mut App| {
            shell.update(cx, |s, cx| {
                if s.compare().is_some() || s.library.selection().active_id.is_some() {
                    s.apply_mark(mark.clone(), true, cx)
                }
            })
        }
    };
    macro_rules! on {
        ($el:expr, $action:ty, $mark:expr) => {{
            let f = mark(shell.clone(), $mark);
            $el.on_action(move |_: &$action, _, cx| f(cx))
        }};
    }
    let el = on!(el, Rate0, Mark::Rating(0));
    let el = on!(el, Rate1, Mark::Rating(1));
    let el = on!(el, Rate2, Mark::Rating(2));
    let el = on!(el, Rate3, Mark::Rating(3));
    let el = on!(el, Rate4, Mark::Rating(4));
    let el = on!(el, Rate5, Mark::Rating(5));
    let el = on!(el, MarkPick, Mark::Pick(PickState::Pick));
    let el = on!(el, MarkReject, Mark::Pick(PickState::Reject));
    let el = on!(el, MarkUnflag, Mark::Pick(PickState::None));
    let el = on!(el, LabelRed, Mark::Label("Red".into()));
    let el = on!(el, LabelYellow, Mark::Label("Yellow".into()));
    let el = on!(el, LabelGreen, Mark::Label("Green".into()));
    let el = on!(el, LabelBlue, Mark::Label("Blue".into()));
    let el = on!(el, LabelPurple, Mark::Label("Purple".into()));
    on!(el, LabelNone, Mark::Label(String::new()))
}

impl Render for LoupeView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let root = div()
            .id("loupe")
            .key_context(contexts::LOUPE)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .on_mouse_down(gpui_kit::MouseButton::Left, cx.listener(|this, _, window, cx| this.focus.focus(window, cx)))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.step(1, false, cx)))
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.step(-1, false, cx)))
            .on_action(cx.listener(|this, _: &ExtendNext, _, cx| this.step(1, true, cx)))
            .on_action(cx.listener(|this, _: &ExtendPrevious, _, cx| this.step(-1, true, cx)))
            // Select every photo in the view, keeping the one shown active (React's grid handler,
            // which stayed live under the inline loupe).
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_all()))
            }))
            // Enter/Esc and C act on the main window's stage; in the pop-out they do nothing (the
            // window manager closes the window).
            .on_action(cx.listener(|this, _: &CloseLoupe, _, cx| {
                if this.follow == Follow::Inline {
                    this.shell.update(cx, |s, cx| s.set_loupe(false, cx))
                }
            }))
            .on_action(cx.listener(|this, _: &CompareSelection, window, cx| {
                if this.follow == Follow::Inline && this.shell.read(cx).library.selection().ids.len() >= 2 {
                    window.dispatch_action(Box::new(OpenCompare), cx);
                }
            }))
            .test_support();
        let root = with_culling_actions(root, &self.shell);
        let Some(photo) = self.target(cx) else {
            return root
                .items_center()
                .justify_center()
                .child(div().text_color(colors.mute).child("No photo selected"))
                .into_any_element();
        };
        let video = chairphoto_core::scanner::is_video(Path::new(&photo.path));
        let bar = self.render_bar(&photo, colors, cx);
        let panels = ModuleRegistry::panel_views(&self.modules, PanelSlot::Loupe, window, cx);
        let id = photo.id;
        let stage: AnyElement = div()
            .id("loupe-stage")
            .relative()
            .flex_1()
            .min_h_0()
            .child(self.zoom.clone())
            .when(video, |d| {
                d.child(
                    div().absolute().bottom(px(18.)).left_0().right_0().flex().justify_center().child(ui::clickable(
                        ui::primary("loupe-play", "▶ Play in system player", true, colors),
                        true,
                        cx.listener(move |this, _, _, cx| this.play(id, cx)),
                    )),
                )
            })
            // Loupe-slot module panels (the face overlay) lie over the image.
            .children(panels.into_iter().map(|p| {
                div()
                    .id(SharedString::from(format!("module-panel-loupe-{}", p.id)))
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .child(p.view)
                    .test_support()
            }))
            .into_any_element();
        root.child(bar).child(stage).into_any_element()
    }
}
