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
//!   whatever the main stage shows, has no "Back to grid", and ignores Enter/Esc and C. While
//!   a proof sheet's previewed candidate (`ShellState::set_loupe_proof_preview`, `edit`
//!   feature, #250) shows that record, rendered from the Darkroom's own source and labelled
//!   "Proof: <label> — not applied", through its 320 px cell render until the loupe-size one
//!   lands; it outranks the Darkroom's print (`ShellState::set_loupe_print`) on the same photo
//!   — the sheet is transient and modal over the Darkroom, so it wins even while "🖥 Loupe
//!   print" is on, its default. Failing both, the print alone shows its own record. When its
//!   window closes it is [released](LoupeView::release).
//!
//! Keys ([`contexts::LOUPE`]): ←/→/↑/↓ step (Shift extends), Enter/Esc back to the grid, C
//! Compare, and the culling keys, which mark the targets and advance as in the grid.

use crate::image_store::{ClaimId, ImageStore};
use crate::keymap::contexts;
use crate::library::*;
use crate::loupe::zoom::ZoomImage;
use crate::loupe::{CloseLoupe, LoupeConfirm};
#[cfg(feature = "edit")]
use crate::loupe::{ProofAdopt, ProofClose, ProofDown, ProofNext, ProofPrevious, ProofUp};
use crate::model::AppModel;
use crate::modules::{ModuleRegistry, PanelSlot};
use crate::shell::actions::OpenCompare;
use crate::shell::state::{Mark, ShellState, StageView};
use crate::shell::style::{Colors, COLOR_LABELS};
use crate::storage::ui;
use chairphoto_core::app::{with_catalog_as, CatalogIdentity};
use chairphoto_core::catalog::{Photo, PickState};
use chairphoto_core::image_pool::ImageKind;
use gpui_kit::assets::IconName;
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, AnyElement, App, Context, Entity, FocusHandle, Global, Hsla, InteractiveElement, SharedString,
    Subscription, TestSupportExt as _, WeakEntity, Window, WindowId,
};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

/// The loupe's button for a video (decision #97: poster frame + the system player).
pub const PLAY_LABEL: &str = "▶ Play in system player";

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

/// The loupe image each window shows, for loupe-slot module panels (the face overlay) that
/// draw over it: every [`LoupeView`] registers its image for the window it renders in, before
/// it builds those panels. Weak, so a closed window's entry dies with its view.
#[derive(Default)]
struct LoupeImages(HashMap<WindowId, WeakEntity<ZoomImage>>);

impl Global for LoupeImages {}

/// The loupe image `window` shows — the inline loupe in the main window, the pop-out's in the
/// pop-out — so a loupe-slot panel can follow its transform without reaching into a window's
/// root view. `None` when no loupe has rendered in `window`.
pub fn loupe_image(window: &Window, cx: &App) -> Option<Entity<ZoomImage>> {
    cx.try_global::<LoupeImages>()?.0.get(&window.window_handle().window_id())?.upgrade()
}

/// Which photo a loupe follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Follow {
    /// The inline loupe: the target only while the stage shows the loupe.
    Inline,
    /// A loupe window (#110): the target whatever the main stage shows.
    Window,
}

/// What [`LoupeView::record_source`] picked: the proof sheet's previewed candidate, or the
/// Darkroom's print — never both at once, and never the active version (that is `None`,
/// [`LoupeView::sync_version`]'s own fallback).
#[cfg(feature = "edit")]
enum RecordSource {
    Proof(crate::shell::state::LoupeProofPreview),
    Print(crate::shell::state::LoupePrint),
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
        #[cfg(feature = "edit")]
        if let Some(print) = self.print(cx) {
            return Some(print.photo.clone());
        }
        let shell = self.shell.read(cx);
        let showing = match self.follow {
            Follow::Inline => shell.stage_view() == StageView::Loupe,
            Follow::Window => true,
        };
        showing.then(|| shell.loupe_target().cloned()).flatten()
    }

    /// The Darkroom's proof sheet to route this pop-out's ←/→/↑/↓, Enter and Esc to instead of
    /// stepping the library selection or doing nothing (#250 review follow-up) — `None` for
    /// the inline loupe, which never sits over a Darkroom overlay, and whenever no sheet is
    /// dealt. The sheet lives in the Darkroom's own window, a different one from the
    /// pop-out's: its focus/row navigation (`ProofSheet::cycle`, `move_row`) moves that
    /// window's own `FocusHandle` state, which is meaningless anywhere else, so callers
    /// re-dispatch the matching action into `.window` (`AppContext::update_window` +
    /// `Window::dispatch_action`) rather than driving the sheet's own, private methods with
    /// the wrong window.
    #[cfg(feature = "edit")]
    fn proof_sheet_route(&self, cx: &App) -> Option<crate::shell::state::LoupeProofSheetHandle> {
        if self.follow != Follow::Window {
            return None;
        }
        // `sheet` is a `WeakEntity` (#250 second review): a handle whose sheet already
        // dropped without going through `ProofSheet::end` (`open_duel` replacing a live
        // sheet; a window torn down) must read as no route, not a stale one pointing at an
        // orphan.
        self.shell.read(cx).loupe_proof_sheet().filter(|h| h.sheet.upgrade().is_some()).cloned()
    }

    /// The Darkroom's print, which the pop-out shows in place of the target while it is up.
    #[cfg(feature = "edit")]
    fn print<'a>(&self, cx: &'a App) -> Option<&'a crate::shell::state::LoupePrint> {
        match self.follow {
            Follow::Window => self.shell.read(cx).loupe_print(),
            Follow::Inline => None,
        }
    }

    /// Which record the pop-out shows in place of the active version, and why, for `photo_id`
    /// (#250 review): a proof sheet's previewed candidate outranks the Darkroom's print — the
    /// sheet is transient and modal over the Darkroom, so it should win even while "🖥 Loupe
    /// print" is on, its default (`Darkroom::session.rs` `print_on_loupe: true`). `None` outside
    /// `Follow::Window` (the inline loupe shows neither) or when neither is up for this photo.
    /// [`Self::sync_version`] (the image) and `render_bar` (the bar's label) both ask this, so
    /// the two can never disagree.
    #[cfg(feature = "edit")]
    fn record_source(&self, photo_id: i64, cx: &App) -> Option<RecordSource> {
        if self.follow != Follow::Window {
            return None;
        }
        if let Some(p) = self.shell.read(cx).loupe_proof_preview().filter(|p| p.photo_id == photo_id).cloned() {
            return Some(RecordSource::Proof(p));
        }
        self.print(cx).filter(|p| p.photo.id == photo_id).cloned().map(RecordSource::Print)
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
        use chairphoto_core::plugins::edit::SourceToken;
        use crate::loupe::zoom::Override;
        /// React's loupe render size (`renderForLoupe`, `edit://` at 2560 px).
        const LOUPE_EDGE: u32 = 2560;
        let target = self.zoom.read(cx).photo();
        let epoch = self.model.read(cx).catalog_epoch;
        // The record to render and its pixels, and why (`Self::record_source`, shared with
        // `render_bar`'s label so the two can never disagree, #250 review): a proof sheet's
        // previewed candidate, else the Darkroom's print, else the active version — each only
        // on its own photo (the trailing filter, below). The proof case also keeps its 320 px
        // cell render, a placeholder until its own loupe-size render lands.
        let chosen = target.and_then(|id| self.record_source(id, cx));
        let (record, placeholder) = match chosen {
            Some(RecordSource::Proof(p)) => {
                let edit_json = (p.source.encode)(&p.candidate.record);
                (Some((p.photo_id, edit_json, p.source.source)), Some(p.cell))
            }
            Some(RecordSource::Print(print)) => (Some((print.photo.id, print.edit_json, print.source)), None),
            None => {
                let shell = self.shell.read(cx);
                (shell.active_version().map(|v| (v.photo_id, v.edit_json.clone(), SourceToken::Preview)), None)
            }
        };
        let record = record.filter(|(photo, _, _)| Some(*photo) == target);
        let placeholder = if record.is_some() { placeholder } else { None };
        let Some((photo, edit_json, source)) = record else {
            self.renders.update(cx, |r, cx| r.want(&[], cx));
            self.zoom.update(cx, |z, cx| z.set_override(None, cx));
            return;
        };
        let mut lo = preview_job(photo, &edit_json, LOUPE_EDGE, false, epoch);
        let mut hi = preview_job(photo, &edit_json, 0, true, epoch);
        lo.source = source.clone();
        hi.source = source;
        let wants_hi = self.zoom.read(cx).wants_hi();
        let mut jobs = if wants_hi { vec![lo.clone(), hi.clone()] } else { vec![lo.clone()] };
        // Keep the Darkroom's print rendered even while a proof sheet's candidate is the one
        // shown: `EditRenders::want` drops whatever is not in the wanted set, so without this
        // the print's texture (and its full-res render) is evicted the moment a proof takes
        // over, and hovering off — or crossing the gap between cells, which also momentarily
        // has nothing hovered or focused — re-renders it from scratch, blanking the pop-out
        // until it lands (#250 review). Only for this photo; a print for another photo is
        // never wanted here regardless of what wins below.
        let print_lo = self.print(cx).filter(|p| p.photo.id == photo).map(|p| {
            let mut j = preview_job(p.photo.id, &p.edit_json, LOUPE_EDGE, false, epoch);
            j.source = p.source.clone();
            j
        });
        let print_hi = wants_hi
            .then(|| self.print(cx).filter(|p| p.photo.id == photo))
            .flatten()
            .map(|p| {
                let mut j = preview_job(p.photo.id, &p.edit_json, 0, true, epoch);
                j.source = p.source.clone();
                j
            });
        for j in [&print_lo, &print_hi].into_iter().flatten() {
            if !jobs.contains(j) {
                jobs.push(j.clone());
            }
        }
        self.renders.update(cx, |r, cx| r.want(&jobs, cx));
        let renders = self.renders.read(cx);
        let mut over = Override::default();
        // Whether `over.lo` ended up being the print's fallback texture rather than the
        // chosen record's own (#250 review, probe P3): only then is the print's full-res a
        // valid stand-in for `over.hi` below — otherwise, zoomed on a proof whose own hi is
        // still pending or failed, it would show the PRINT's full-res pixels under the
        // proof's label. The chosen record's own lo (even scaled up, past `max_scale`) is the
        // correct placeholder for its own hi; the print's lo is not.
        let mut lo_from_print = false;
        match renders.get(&lo) {
            RenderState::Ready(image) => over.lo = Some(image),
            RenderState::Failed(e) => over.failed = Some(e),
            _ => {}
        }
        // The proof sheet's own 320 px render, until the loupe-size one above lands (#250).
        if over.lo.is_none() {
            if let Some(RenderState::Ready(image)) = placeholder {
                over.lo = Some(image);
            }
        }
        // Never go blank while a replacement renders: the print, kept warm above, if it is
        // already in (#250 review).
        if over.lo.is_none() {
            if let Some(j) = &print_lo {
                if let RenderState::Ready(image) = renders.get(j) {
                    over.lo = Some(image);
                    lo_from_print = true;
                }
            }
        }
        match renders.get(&hi) {
            RenderState::Ready(image) => over.hi = Some(image),
            RenderState::Failed(_) => over.hi_settled = true,
            _ => {}
        }
        if over.hi.is_none() && lo_from_print {
            if let Some(j) = &print_hi {
                if let RenderState::Ready(image) = renders.get(j) {
                    over.hi = Some(image);
                }
            }
        }
        self.zoom.update(cx, |z, cx| z.set_override(Some(over), cx));
    }

    /// ↺ / ↻: a non-destructive rotation (`rotate_photo`) of `photo`, bound to `from`, the
    /// catalog its row was read from — both as the bar was drawn, never looked up at the click
    /// (#207); the photo's cached tiers are dropped so every view re-renders it.
    pub fn rotate(&mut self, photo: i64, from: Option<CatalogIdentity>, delta: i64, cx: &mut Context<Self>) {
        let Some(from) = from else { return };
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

    /// "▶ Play in system player": resolve `photo`'s file in `from` (both as the button was
    /// drawn, #207; never from `photos.path` directly) off the UI thread, then hand it to the
    /// desktop.
    pub fn play(&mut self, photo: i64, from: Option<CatalogIdentity>, cx: &mut Context<Self>) {
        let Some(from) = from else { return };
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
        let faces = faces_enabled(&self.modules, cx);
        let (hint_prefix, hint) = (loupe_hint_prefix(faces), loupe_hint(faces));
        // The row and the catalog it was read from, as drawn: a click acts on these (#207).
        let (id, from) = (photo.id, shell.rows_from());
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
        // A proof sheet's previewed candidate (pop-out only, #250) stands in for the version
        // tag when it is the thing actually shown (`record_source`, the same choice the image
        // makes) — it is a candidate record, not what the photo actually holds.
        match proof_label(self, photo.id, cx) {
            Some(label) => tags = tags.child(tag("loupe-tag-proof", format!("Proof: {label} — not applied"), colors.accent)),
            None => {
                if let Some(name) = version {
                    tags = tags.child(tag("loupe-tag-version", format!("· {name}"), colors.accent));
                }
            }
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
            // App.tsx's ↺ / ↻ as Lucide's rotate arrows: the UI font has no U+21BA/U+21BB, and
            // the fallback drew them as tiny marks (#172).
            .child(ui::clickable(
                ui::icon_chip("loupe-rotate-left", IconName::RotateCcw, "Rotate left", true, colors)
                    .tooltip(crate::shell::title_bar::tooltip("Rotate left (non-destructive)")),
                true,
                cx.listener(move |this, _, _, cx| this.rotate(id, from, -90, cx)),
            ))
            .child(ui::clickable(
                ui::icon_chip("loupe-rotate-right", IconName::RotateCw, "Rotate right", true, colors)
                    .tooltip(crate::shell::title_bar::tooltip("Rotate right (non-destructive)")),
                true,
                cx.listener(move |this, _, _, cx| this.rotate(id, from, 90, cx)),
            ))
            .child(tags)
            .child(div().flex_1())
            .child(
                div()
                    .id("loupe-hint")
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .text_size(px(11.))
                    .text_color(colors.mute)
                    .whitespace_nowrap()
                    .child(hint_prefix)
                    // "← →" drawn as text fell back to a font that renders U+2190/U+2192 tiny;
                    // Lucide's arrows at the app's 13 px stroke-icon size read the same way the
                    // rotate chips do (#172, #197).
                    .child(ui::sized_icon("loupe-hint-arrow-left", IconName::ArrowLeft))
                    .child(ui::sized_icon("loupe-hint-arrow-right", IconName::ArrowRight))
                    .aria_label(hint)
                    .test_support(),
            )
    }

    fn step(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| s.select_with(cx, |l| l.step_active(delta, extend)));
    }
}

/// Whether the Faces module is on. Without the `faces` feature it does not exist (nor does
/// its id, `FACES_MODULE_ID`).
fn faces_enabled(modules: &Entity<ModuleRegistry>, cx: &App) -> bool {
    #[cfg(feature = "faces")]
    {
        modules.read(cx).is_enabled(crate::modules::faces::FACES_MODULE_ID)
    }
    #[cfg(not(feature = "faces"))]
    {
        let _ = (modules, cx);
        false
    }
}

/// The proof sheet's previewed candidate's label for `photo_id`, shown on the pop-out's bar in
/// place of the active version's name while that candidate is the thing actually rendered —
/// `view.record_source`, so the label can never name a candidate the image is not showing
/// (#250 review: the Darkroom's print outranks a proof it shares a photo with). `None` without
/// the `edit` feature (there is no proof sheet) or in the inline loupe.
fn proof_label(view: &LoupeView, photo_id: i64, cx: &App) -> Option<String> {
    #[cfg(feature = "edit")]
    {
        match view.record_source(photo_id, cx) {
            Some(RecordSource::Proof(p)) => Some(p.candidate.label.clone()),
            _ => None,
        }
    }
    #[cfg(not(feature = "edit"))]
    {
        let _ = (view, photo_id, cx);
        None
    }
}

/// [`loupe_hint`] without its trailing "· ← →" — what the bar draws as text; the arrows are
/// Lucide icons instead (#197: the UI font has no U+2190/U+2192, and the fallback font it
/// reaches for draws them tiny).
fn loupe_hint_prefix(faces: bool) -> &'static str {
    if faces {
        "scroll zoom · drag pan · dbl-click 100% · P pick · X reject · F faces"
    } else {
        "scroll zoom · drag pan · dbl-click 100% · P pick · X reject"
    }
}

/// The loupe bar's key hint (App.tsx's `.loupe-hint`), as the accessible/test string. "F
/// faces" only while the Faces module is on: F toggles its overlay's boxes, and without it the
/// key does nothing.
pub fn loupe_hint(faces: bool) -> String {
    format!("{} · ← →", loupe_hint_prefix(faces))
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
        // Replaces `contexts::LOUPE` on this same div for as long as a proof sheet is up on
        // the pop-out (#250 second review, `contexts::POPOUT_PROOF_SHEET`'s own docs): gives
        // ↑/↓ real row moves there (not `SelectNext`/`SelectPrevious`'s ⇄ conflation) and
        // makes Shift+arrows/Ctrl+A/C — bound only in `LOUPE` — unreachable meanwhile, rather
        // than letting them silently move the active photo or open Compare out from under a
        // dealt sheet. `None` (and so plain `LOUPE`) for the inline loupe always, which never
        // sits over a Darkroom overlay.
        #[cfg(feature = "edit")]
        let route = self.proof_sheet_route(cx);
        #[cfg(not(feature = "edit"))]
        let route: Option<()> = None;
        let context = if route.is_some() { contexts::POPOUT_PROOF_SHEET } else { contexts::LOUPE };
        let root = div()
            .id("loupe")
            .key_context(context)
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
            // Inline: back to the grid, same as Esc (`CloseLoupe`). Pop-out: nothing — the
            // window manager closes the window instead (`contexts::POPOUT_PROOF_SHEET`'s own
            // `ProofAdopt` handles Enter there whenever a sheet is up; this action no longer
            // reaches the pop-out in that case at all).
            .on_action(cx.listener(|this, _: &LoupeConfirm, _, cx| {
                if this.follow == Follow::Inline {
                    this.shell.update(cx, |s, cx| s.set_loupe(false, cx));
                }
            }))
            // Inline: back to the grid. Pop-out: nothing, as `LoupeConfirm`'s own docs
            // (`contexts::POPOUT_PROOF_SHEET`'s own `ProofClose` declines the sheet instead,
            // whenever one is up).
            .on_action(cx.listener(|this, _: &CloseLoupe, _, cx| {
                if this.follow == Follow::Inline {
                    this.shell.update(cx, |s, cx| s.set_loupe(false, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &CompareSelection, window, cx| {
                if this.follow == Follow::Inline && this.shell.read(cx).library.selection().ids.len() >= 2 {
                    window.dispatch_action(Box::new(OpenCompare), cx);
                }
            }))
            .test_support();
        // Only reachable while `context` is `POPOUT_PROOF_SHEET` (the bindings above), so
        // these never fire for the inline loupe or a pop-out with no sheet up. Each re-reads
        // `Self::proof_sheet_route` itself rather than closing over `route` computed above:
        // the route can go stale between this render and whenever the key is actually
        // pressed (the sheet declined, the Darkroom left, a catalog switch), and a handler
        // must see that, not this frame's snapshot.
        #[cfg(feature = "edit")]
        let root = root
            .on_action(cx.listener(|this, _: &ProofNext, _, cx| {
                if let Some(route) = this.proof_sheet_route(cx) {
                    cx.update_window(route.window, |_, window, cx| window.dispatch_action(Box::new(ProofNext), cx)).ok();
                }
            }))
            .on_action(cx.listener(|this, _: &ProofPrevious, _, cx| {
                if let Some(route) = this.proof_sheet_route(cx) {
                    cx.update_window(route.window, |_, window, cx| window.dispatch_action(Box::new(ProofPrevious), cx)).ok();
                }
            }))
            .on_action(cx.listener(|this, _: &ProofUp, _, cx| {
                if let Some(route) = this.proof_sheet_route(cx) {
                    cx.update_window(route.window, |_, window, cx| window.dispatch_action(Box::new(ProofUp), cx)).ok();
                }
            }))
            .on_action(cx.listener(|this, _: &ProofDown, _, cx| {
                if let Some(route) = this.proof_sheet_route(cx) {
                    cx.update_window(route.window, |_, window, cx| window.dispatch_action(Box::new(ProofDown), cx)).ok();
                }
            }))
            // Adopts the pop-out's own currently previewed candidate directly (`ProofSheet::
            // adopt`, not a re-dispatched action): there is no cell of the pop-out's own for
            // the Darkroom's window to have focused, so there is nothing for an action there
            // to adopt on this one's behalf.
            .on_action(cx.listener(|this, _: &ProofAdopt, _, cx| {
                let Some(route) = this.proof_sheet_route(cx) else { return };
                let Some(sheet) = route.sheet.upgrade() else { return };
                let Some(preview) = this.shell.read(cx).loupe_proof_preview().cloned() else { return };
                let i = sheet.read(cx).candidates().iter().position(|c| *c == preview.candidate);
                if let Some(i) = i {
                    sheet.update(cx, |s, cx| s.adopt(i, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &ProofClose, _, cx| {
                if let Some(route) = this.proof_sheet_route(cx) {
                    route.sheet.update(cx, |s, cx| s.close(cx)).ok();
                }
            }));
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
        // The image the loupe-slot panels draw over, for this window ([`loupe_image`]).
        let images = &mut cx.default_global::<LoupeImages>().0;
        images.retain(|_, zoom| zoom.upgrade().is_some());
        images.insert(window.window_handle().window_id(), self.zoom.downgrade());
        let panels = ModuleRegistry::panel_views(&self.modules, PanelSlot::Loupe, window, cx);
        let (id, from) = (photo.id, self.shell.read(cx).rows_from());
        let stage: AnyElement = div()
            .id("loupe-stage")
            .relative()
            .flex_1()
            .min_h_0()
            .child(self.zoom.clone())
            .when(video, |d| {
                d.child(
                    div().absolute().bottom(px(18.)).left_0().right_0().flex().justify_center().child(ui::clickable(
                        ui::primary("loupe-play", PLAY_LABEL, true, colors),
                        true,
                        cx.listener(move |this, _, _, cx| this.play(id, from, cx)),
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
