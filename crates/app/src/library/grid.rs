//! [`LibraryView`]: the virtualised photo grid (`CatalogGrid.tsx` + `Thumbnail.tsx`).
//!
//! - **Virtualised.** A `uniform_list` of rows of `cols` tiles; GPUI lays out and builds only
//!   the rows on screen. The column count comes from the list's measured width and the
//!   command pill's size slider ([`layout::columns`]); a width change re-renders with the
//!   new count on the next frame.
//! - **Thumbnails through the image layer.** Each frame asks the [`ImageStore`] for the
//!   visible rows' thumbnails plus [`layout::OVERSCAN_ROWS`] each side, most urgent first,
//!   and releases the ones it asked for earlier that scrolled out of that window (still
//!   queued ones are cancelled in the pool; finished ones stay in the LRU). Nothing decodes
//!   on the UI thread: a tile shows what the store has.
//! - **Cover looks** (#151). Each request carries the cover look its row names and the
//!   catalog the rows were read from (`ImageStore::request_look_batch`), as `Thumbnail.tsx`
//!   put the cover token in the URL: a row re-read with a new cover, or a new revision of it,
//!   renders that tile's thumbnail again and drops a late answer for the earlier look.
//! - **Storage badges per window.** The same window is reported to the session
//!   (`ShellState::set_visible_range`), which fetches only those rows' statuses.
//! - **Position.** Opens scrolled to the newest photo (the rows are oldest first) unless a
//!   photo is active; scrolls the active photo into view whenever it changes.
//! - **Keys** ([`crate::library::bindings`]) and clicks act on the session through the
//!   shell: selection verbs, and culling marks through `ShellState::apply_mark`. A
//!   right-click opens the context menu ([`crate::library::grid_menu`]).

use crate::image_store::{ImageState, ImageStore};
use crate::keymap::contexts;
use crate::library::grid_menu::GridMenu;
use crate::library::layout::{self, GAP, NAME_H, OVERSCAN_ROWS};
use crate::library::*;
use crate::shell::actions::{OpenCompare, ToggleLoupe};
use crate::shell::sidebar::RAIL_W;
use crate::shell::state::{Mark, ShellState};
use crate::shell::style::{Colors, COLOR_LABELS};
use chairphoto_core::catalog::{Photo, PickState, StorageStatus};
use chairphoto_core::image_pool::ImageKind;
use chairphoto_model::darkroom::filmstrip::{cover_look, CoverLook};
use chairphoto_model::library::session::{LibrarySession, SelectMods};
use gpui_kit::prelude::*;
use gpui_kit::{
    div, img, px, uniform_list, AnyElement, App, Bounds, ClickEvent, Context, Entity, FocusHandle, Hsla,
    MouseButton, MouseDownEvent, ObjectFit, Pixels, Point, ScrollStrategy, SharedString, Subscription, TestSupportExt as _,
    UniformListDecoration, UniformListScrollHandle, WeakEntity, Window,
};
use std::collections::HashSet;
use std::ops::Range;

/// The grid. See the module docs.
pub struct LibraryView {
    pub(super) shell: Entity<ShellState>,
    images: Entity<ImageStore>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    /// The column count the last render used.
    cols: usize,
    /// The thumbnails the grid asked for last frame: the ones it may release.
    requested: HashSet<i64>,
    /// The selection as a set, rebuilt only when the selection changes (select-all over a
    /// big catalog must not cost a set per frame).
    selected: (Vec<i64>, HashSet<i64>),
    /// Whether the grid has made its one jump to the newest photo for these rows.
    opened_at_bottom: bool,
    /// The active photo (and column count) last scrolled into view.
    scrolled_for: Option<(i64, usize)>,
    /// Rows visible in the last frame, for Page Up/Down.
    visible_rows: usize,
    /// The right-click menu while it is open ([`crate::library::grid_menu`]).
    pub(super) menu: Option<GridMenu>,
    _observers: [Subscription; 2],
}

impl LibraryView {
    pub fn new(shell: Entity<ShellState>, images: Entity<ImageStore>, cx: &mut Context<Self>) -> Self {
        let _observers = [
            cx.observe(&shell, |this, _, cx| {
                this.check_menu(cx);
                cx.notify()
            }),
            cx.observe(&images, |_, _, cx| cx.notify()),
        ];
        Self {
            shell,
            images,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            cols: 1,
            requested: HashSet::new(),
            selected: (Vec::new(), HashSet::new()),
            opened_at_bottom: false,
            scrolled_for: None,
            visible_rows: 1,
            menu: None,
            _observers,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// The scroll handle — the frame bench drives it.
    pub fn scroll_handle(&self) -> &UniformListScrollHandle {
        &self.scroll
    }

    /// The column count the last render used.
    pub fn columns(&self) -> usize {
        self.cols
    }

    /// The list's size as the last layout measured it (width, height).
    fn measured(&self) -> Option<(f32, f32)> {
        let state = self.scroll.0.borrow();
        state.last_item_size.map(|s| (f32::from(s.item.width), f32::from(s.item.height)))
    }

    /// Before the first layout: the window minus the rail, the visible side columns and the
    /// stage padding. Corrected by the measurement a frame later.
    fn estimate_width(&self, window: &Window, shell: &ShellState) -> f32 {
        let mut w = f32::from(window.viewport_size().width) - RAIL_W - 24.;
        if !shell.narrow && !shell.layout.left_hidden {
            w -= shell.layout.left_w;
        }
        if !shell.narrow && !shell.layout.right_hidden {
            w -= shell.layout.right_w;
        }
        w.max(1.)
    }

    // --- input -------------------------------------------------------------------------

    fn on_tile_click(&mut self, id: i64, event: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        let m = event.modifiers();
        if event.click_count() >= 2 {
            // Double-click opens (React `onOpen`: select, then the inline loupe).
            self.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select(id, SelectMods::default())));
            window.dispatch_action(Box::new(ToggleLoupe), cx);
            return;
        }
        let mods = SelectMods { ctrl: m.control || m.platform, shift: m.shift };
        self.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select(id, mods)));
    }

    /// Right-click opens the context menu on the photo ([`crate::library::grid_menu`]).
    fn on_tile_right_click(&mut self, id: i64, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        self.open_menu(id, event.position, cx);
    }

    fn has_active(&self, cx: &Context<Self>) -> bool {
        self.shell.read(cx).library.selection().active_id.is_some()
    }

    fn select(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut LibrarySession)) {
        self.shell.update(cx, |s, cx| s.select_with(cx, f));
    }

    /// Arrows: React stepped only with an active photo.
    fn step(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        self.select(cx, |l| l.step_active(delta, extend));
    }

    /// Home/End/Page keys: move the active photo to `photos[target(len, active index)]`.
    fn jump(&mut self, cx: &mut Context<Self>, target: impl FnOnce(usize, Option<usize>) -> Option<usize>) {
        self.select(cx, |l| {
            let active = l.selection().active_id;
            let from = active.and_then(|a| l.photos().iter().position(|p| p.id == a));
            if let Some(ix) = target(l.photos().len(), from) {
                let id = l.photos()[ix].id;
                l.select_single(id);
            }
        });
    }

    fn page(&mut self, direction: isize, cx: &mut Context<Self>) {
        let step = (self.visible_rows.max(1) * self.cols.max(1)) as isize * direction;
        self.jump(cx, |len, from| layout::page_target(len, from, step));
    }

    /// A culling key: with an active photo, mark the targets and advance (one write path,
    /// `ShellState::apply_mark`).
    fn mark(&mut self, mark: Mark, cx: &mut Context<Self>) {
        if self.has_active(cx) {
            self.shell.update(cx, |s, cx| s.apply_mark(mark, true, cx));
        }
    }

    // --- rendering ---------------------------------------------------------------------

    /// The rows on screen, once per frame, from the list's prepaint ([`VisibleRows`]):
    /// request their thumbnails plus the overscan, release the ones that scrolled out of
    /// that window, and report the window for the storage badges.
    ///
    /// Not done in [`Self::render_rows`]: `uniform_list` also calls that for row 0 to
    /// measure the row height, every frame, and a release keyed on it would cancel and
    /// re-request the visible thumbnails each frame.
    fn on_visible(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let cols = self.cols.max(1);
        let shell = self.shell.read(cx);
        let photos = shell.library.photos();
        let n = photos.len();
        let row_count = n.div_ceil(cols);
        let from = shell.rows_from();
        // Most urgent first: the visible rows, then the overscan (`layout::wanted_rows`), each
        // with the cover look its row names.
        let wanted: Vec<(i64, Option<CoverLook>)> = layout::wanted_rows(range.clone(), row_count, OVERSCAN_ROWS)
            .into_iter()
            .flat_map(|r| photos[layout::cells(r..r + 1, cols, n)].iter().map(|p| (p.id, cover_look(p.cover_token.as_deref()))))
            .collect();
        let span = layout::cells(layout::wanted_span(range, row_count, OVERSCAN_ROWS), cols, n);
        self.shell.update(cx, |s, cx| s.set_visible_range(span.start, span.end, cx));
        let keep: HashSet<i64> = wanted.iter().map(|&(id, _)| id).collect();
        let dropped: HashSet<i64> = self.requested.difference(&keep).copied().collect();
        self.images.update(cx, |store, cx| {
            match from {
                // The rows' cover tokens, as `Thumbnail.tsx` put them in the URL (#151).
                Some(from) => store.request_look_batch(from, &wanted, cx),
                None => {
                    let batch: Vec<(i64, ImageKind)> = wanted.iter().map(|&(id, _)| (id, ImageKind::Thumb)).collect();
                    store.request_batch(&batch);
                }
            }
            if !dropped.is_empty() {
                store.release_pending(|k| k.kind != ImageKind::Thumb || !dropped.contains(&k.photo));
            }
        });
        self.requested = keep;
    }

    /// Release every thumbnail the grid still has asked for — the window is gone (an empty
    /// grid draws no list). A no-op once released, so it is cheap on every empty frame.
    fn release_requested(&mut self, cx: &mut Context<Self>) {
        if self.requested.is_empty() {
            return;
        }
        let dropped = std::mem::take(&mut self.requested);
        self.images.update(cx, |store, _| {
            store.release_pending(|k| k.kind != ImageKind::Thumb || !dropped.contains(&k.photo));
        });
    }

    /// The rows `range` of the grid, as `uniform_list` asks for them (during its layout and
    /// prepaint). Builds elements from what the session and the image store hold; loads
    /// nothing (see [`Self::on_visible`]).
    fn render_rows(
        &mut self,
        range: Range<usize>,
        tile_min: f32,
        row_h: f32,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let cols = self.cols.max(1);
        if let Some((w, h)) = self.measured() {
            if layout::columns(w, tile_min) != cols {
                cx.notify(); // the width changed: re-render with the new column count
            }
            self.visible_rows = ((h / row_h).floor() as usize).max(1);
        }

        let shell = self.shell.read(cx);
        let library = &shell.library;
        let photos = library.photos();
        let n = photos.len();
        let selection = library.selection();
        if self.selected.0 != selection.ids {
            self.selected = (selection.ids.to_vec(), selection.ids.iter().copied().collect());
        }
        let active = selection.active_id;
        let soft = shell.soft_threshold;
        let statuses = library.statuses();
        let tiles: Vec<Tile> = photos[layout::cells(range.clone(), cols, n)]
            .iter()
            .map(|p| Tile::new(p, statuses.get(&p.id).copied(), self.selected.1.contains(&p.id), active == Some(p.id), soft))
            .collect();
        let images: Vec<ImageState> =
            self.images.update(cx, |store, _| tiles.iter().map(|t| store.get(t.id, ImageKind::Thumb)).collect());

        let mut tiles = tiles.into_iter().zip(images);
        range
            .map(|row| {
                let in_row = layout::cells(row..row + 1, cols, n).len();
                let mut el = div()
                    .id(("grid-row", row as u64))
                    .flex()
                    .flex_row()
                    .gap(px(GAP))
                    .w_full()
                    .h(px(row_h))
                    .pb(px(GAP));
                for (tile, image) in tiles.by_ref().take(in_row) {
                    el = el.child(self.render_tile(tile, image, colors, cx));
                }
                // Keep a short last row's tiles the same width as the others.
                for _ in in_row..cols {
                    el = el.child(div().flex_1().min_w_0());
                }
                el.into_any_element()
            })
            .collect()
    }

    fn render_tile(&self, t: Tile, image: ImageState, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let id = t.id;
        let thumb = match image {
            ImageState::Ready(loaded) => img(loaded.image).size_full().object_fit(ObjectFit::Contain).into_any_element(),
            ImageState::Failed(_) => {
                let (icon, label) = layout::failed_label(t.status);
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(4.))
                    .text_size(px(11.))
                    .text_color(colors.mute)
                    .child(div().text_size(px(18.)).child(icon))
                    .child(label)
                    .into_any_element()
            }
            // Not extracted yet (Phase A): a placeholder, as Thumbnail.tsx's spinner.
            _ if !t.metadata_ready => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(colors.mute)
                .child("…")
                .into_any_element(),
            ImageState::Loading | ImageState::Absent => div().size_full().into_any_element(),
        };

        let mut badges = div().absolute().top(px(5.)).left(px(5.)).flex().flex_wrap().gap(px(3.));
        if t.rating > 0 {
            badges = badges.child(badge("★".repeat(t.rating as usize), colors.rating, colors));
        }
        match t.pick {
            PickState::Pick => badges = badges.child(badge("P", colors.ok, colors)),
            PickState::Reject => badges = badges.child(badge("X", colors.danger, colors)),
            PickState::None => {}
        }
        if let Some(c) = t.label {
            badges = badges.child(div().size(px(10.)).mt(px(3.)).rounded_full().bg(c));
        }
        if t.versions > 0 {
            badges = badges.child(
                badge(format!("⧉ {}", t.versions), colors.txt, colors)
                    .id(("badge-versions", id as u64))
                    .tooltip(tip(format!("{} version(s)", t.versions))),
            );
        }
        if t.stack > 0 {
            badges = badges.child(
                badge(format!("▤ {}", t.stack), colors.txt, colors).id(("badge-stack", id as u64)).tooltip(tip(format!(
                    "{} stacked file(s) — e.g. the camera JPEG. Open the inspector's Stack section.",
                    t.stack
                ))),
            );
        }
        if let Some((score, method)) = &t.soft {
            badges = badges.child(
                badge("~", colors.danger, colors)
                    .id(("badge-soft", id as u64))
                    .tooltip(tip(format!("Soft — sharpness score {score:.1} (method: {method})"))),
            );
        }
        match t.burst {
            Some(Burst::SoftInBurst) => {
                badges = badges.child(badge("~B", colors.danger, colors).id(("badge-soft-burst", id as u64)).tooltip(tip(
                    "Soft in burst — dimmer than the rest of its cluster. The inspector's Culling signals section shows the cluster, the median and the exact cutoff.".into(),
                )))
            }
            Some(Burst::Sharpest) => {
                badges = badges.child(badge("♛", colors.rating, colors).id(("badge-sharpest", id as u64)).tooltip(tip(
                    "Sharpest of burst — the highest-scoring frame in its cluster. The inspector's Culling signals section shows the cluster and the scores.".into(),
                )))
            }
            None => {}
        }

        let (local, remote) = layout::storage_icons(t.status);
        let mut storage = div().absolute().bottom(px(4.)).right(px(5.)).flex().gap(px(3.)).text_size(px(11.));
        if local {
            storage = storage.child(
                div().id(("store-local", id as u64)).text_color(colors.dim).child("▣").tooltip(tip("On local disk".into())),
            );
        }
        if let Some(online) = remote {
            storage = storage.child(
                div()
                    .id(("store-remote", id as u64))
                    .text_color(if online { colors.dim } else { colors.danger })
                    .child("☁")
                    .tooltip(tip(if online { "On NAS / backup" } else { "On NAS (offline)" }.into())),
            );
        }

        let border = if t.active {
            colors.accent
        } else if t.selected {
            colors.accent_border()
        } else {
            gpui_kit::transparent_black()
        };
        div()
            .id(("tile", id as u64))
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .rounded(px(4.))
            .border_2()
            .border_color(border)
            .when(t.selected, |d| d.bg(colors.sel))
            .when(t.pick == PickState::Reject, |d| d.opacity(0.45))
            .cursor_pointer()
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .rounded(px(3.))
                    .bg(colors.well)
                    .child(thumb)
                    .child(badges)
                    .when(t.video, |d| {
                        d.child(
                            div()
                                .id(("badge-video", id as u64))
                                .absolute()
                                .top(px(5.))
                                .right(px(6.))
                                .text_size(px(12.))
                                .text_color(colors.txt)
                                .child("▶")
                                .tooltip(tip(video_tip())),
                        )
                    })
                    .child(storage),
            )
            .child(
                div()
                    .flex_none()
                    .h(px(NAME_H - 4.))
                    .px(px(4.))
                    .flex()
                    .items_center()
                    .text_size(px(10.5))
                    .text_color(if t.active { colors.txt } else { colors.dim })
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(t.name),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| this.on_tile_click(id, event, window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| this.on_tile_right_click(id, event, window, cx)),
            )
            .test_support()
            .into_any_element()
    }
}

/// Hands the list's true visible range to [`LibraryView::on_visible`] once per frame. A
/// `uniform_list` decoration is computed in prepaint with that range, after the rows.
struct VisibleRows(WeakEntity<LibraryView>);

impl UniformListDecoration for VisibleRows {
    fn compute(
        &self,
        visible_range: Range<usize>,
        _bounds: Bounds<Pixels>,
        _scroll_offset: Point<Pixels>,
        _item_height: Pixels,
        _item_count: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        self.0.update(cx, |view, cx| view.on_visible(visible_range, cx)).ok();
        div().into_any_element()
    }
}

/// One tile's data, copied out of the session for the visible rows only.
struct Tile {
    id: i64,
    name: SharedString,
    rating: i64,
    pick: PickState,
    label: Option<Hsla>,
    versions: i64,
    stack: i64,
    /// Below the soft threshold: the score and its method.
    soft: Option<(f64, String)>,
    burst: Option<Burst>,
    video: bool,
    metadata_ready: bool,
    status: Option<StorageStatus>,
    selected: bool,
    active: bool,
}

#[derive(Clone, Copy)]
enum Burst {
    SoftInBurst,
    Sharpest,
}

impl Tile {
    fn new(p: &Photo, status: Option<StorageStatus>, selected: bool, active: bool, soft_threshold: f64) -> Self {
        Tile {
            id: p.id,
            name: p.path.rsplit('/').next().unwrap_or(&p.path).to_string().into(),
            rating: p.rating,
            pick: p.pick_state,
            label: COLOR_LABELS.iter().find(|l| l.name == p.label).map(|l| l.color()),
            versions: p.version_count,
            stack: p.stack_count,
            soft: p
                .sharpness
                .filter(|s| *s < soft_threshold)
                .map(|s| (s, p.sharpness_method.clone().unwrap_or_else(|| "tile".into()))),
            burst: match p.burst_flag.as_deref() {
                Some("soft-in-burst") => Some(Burst::SoftInBurst),
                Some("sharpest-of-burst") => Some(Burst::Sharpest),
                _ => None,
            },
            video: chairphoto_core::scanner::is_video(std::path::Path::new(&p.path)),
            metadata_ready: p.metadata_ready != 0,
            status,
            selected,
            active,
        }
    }
}

/// A small overlay badge (`.badge`).
fn badge(text: impl Into<SharedString>, fg: Hsla, colors: Colors) -> gpui_kit::Div {
    div()
        .px(px(4.))
        .h(px(16.))
        .flex()
        .items_center()
        .rounded(px(3.))
        .bg(colors.canvas.opacity(0.78))
        .text_size(px(10.))
        .text_color(fg)
        .child(text.into())
}

/// The video badge's tooltip. React's said "double-click to play", which its double-click
/// did; here a double-click opens the loupe on the poster, whose button plays it (#97).
pub fn video_tip() -> String {
    let play = crate::loupe::view::PLAY_LABEL.trim_start_matches('▶').trim();
    format!("Video — double-click to open, then {play}")
}

/// A tooltip with runtime text.
fn tip(text: String) -> impl Fn(&mut Window, &mut gpui_kit::App) -> gpui_kit::AnyView + 'static {
    let text = SharedString::from(text);
    move |window, cx| gpui_kit::component::tooltip::Tooltip::new(text.clone()).build(window, cx)
}

impl Render for LibraryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let shell = self.shell.read(cx);
        let n = shell.library.photos().len();
        let tile_min = shell.layout.thumb_size;
        let rows_loaded = shell.rows_loaded;
        let message = layout::empty_message(shell.library.scope());
        let active = shell.library.selection().active_id;
        let width = self.measured().map(|(w, _)| w).unwrap_or_else(|| self.estimate_width(window, shell));
        let cols = layout::columns(width, tile_min);
        let active_index = match active {
            Some(a) if self.scrolled_for != Some((a, cols)) => shell.library.photos().iter().position(|p| p.id == a),
            _ => None,
        };
        self.cols = cols;
        let row_count = n.div_ceil(cols);
        let row_h = layout::row_height(width, cols);

        let root = div()
            .id("library")
            .key_context(contexts::LIBRARY)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.focus.focus(window, cx)))
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.step(1, false, cx)))
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.step(-1, false, cx)))
            .on_action(cx.listener(|this, _: &ExtendNext, _, cx| this.step(1, true, cx)))
            .on_action(cx.listener(|this, _: &ExtendPrevious, _, cx| this.step(-1, true, cx)))
            .on_action(cx.listener(|this, _: &SelectFirst, _, cx| this.jump(cx, |len, _| (len > 0).then_some(0))))
            .on_action(cx.listener(|this, _: &SelectLast, _, cx| this.jump(cx, |len, _| len.checked_sub(1))))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| this.page(1, cx)))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| this.page(-1, cx)))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select(cx, |l| l.select_all())))
            .on_action(cx.listener(|this, _: &CloseMenu, _, cx| this.close_menu(cx)))
            .on_action(cx.listener(|this, _: &Rate0, _, cx| this.mark(Mark::Rating(0), cx)))
            .on_action(cx.listener(|this, _: &Rate1, _, cx| this.mark(Mark::Rating(1), cx)))
            .on_action(cx.listener(|this, _: &Rate2, _, cx| this.mark(Mark::Rating(2), cx)))
            .on_action(cx.listener(|this, _: &Rate3, _, cx| this.mark(Mark::Rating(3), cx)))
            .on_action(cx.listener(|this, _: &Rate4, _, cx| this.mark(Mark::Rating(4), cx)))
            .on_action(cx.listener(|this, _: &Rate5, _, cx| this.mark(Mark::Rating(5), cx)))
            .on_action(cx.listener(|this, _: &MarkPick, _, cx| this.mark(Mark::Pick(PickState::Pick), cx)))
            .on_action(cx.listener(|this, _: &MarkReject, _, cx| this.mark(Mark::Pick(PickState::Reject), cx)))
            .on_action(cx.listener(|this, _: &MarkUnflag, _, cx| this.mark(Mark::Pick(PickState::None), cx)))
            .on_action(cx.listener(|this, _: &LabelRed, _, cx| this.mark(Mark::Label("Red".into()), cx)))
            .on_action(cx.listener(|this, _: &LabelYellow, _, cx| this.mark(Mark::Label("Yellow".into()), cx)))
            .on_action(cx.listener(|this, _: &LabelGreen, _, cx| this.mark(Mark::Label("Green".into()), cx)))
            .on_action(cx.listener(|this, _: &LabelBlue, _, cx| this.mark(Mark::Label("Blue".into()), cx)))
            .on_action(cx.listener(|this, _: &LabelPurple, _, cx| this.mark(Mark::Label("Purple".into()), cx)))
            .on_action(cx.listener(|this, _: &LabelNone, _, cx| this.mark(Mark::Label(String::new()), cx)))
            .on_action(cx.listener(|this, _: &OpenActive, window, cx| {
                if this.has_active(cx) {
                    window.dispatch_action(Box::new(ToggleLoupe), cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CompareSelection, window, cx| {
                if this.shell.read(cx).library.selection().ids.len() >= 2 {
                    window.dispatch_action(Box::new(OpenCompare), cx);
                }
            }))
            .test_support();

        if n == 0 {
            // A fresh set of rows opens at the bottom again (React remounted the grid).
            self.opened_at_bottom = false;
            self.scrolled_for = None;
            // No list is drawn, so `on_visible` never runs to release the last window: the
            // thumbnails it asked for would stay queued for tiles that are gone.
            self.release_requested(cx);
            return root
                .items_center()
                .justify_center()
                .p(px(24.))
                .when(rows_loaded, |d| {
                    d.child(
                        div()
                            .id("grid-empty")
                            .max_w(px(520.))
                            .text_size(px(12.5))
                            .text_color(colors.mute)
                            .text_center()
                            .child(message)
                            .aria_label(message)
                            .test_support(),
                    )
                });
        }

        // Open at the newest photo, once the width is known — unless a photo is active, in
        // which case the selection decides the position.
        if !self.opened_at_bottom && self.measured().is_some() {
            self.opened_at_bottom = true;
            if active.is_none() {
                self.scroll.scroll_to_item(row_count - 1, ScrollStrategy::Bottom);
            }
        }
        if let Some(a) = active {
            if self.scrolled_for != Some((a, cols)) {
                if let Some(ix) = active_index {
                    self.scroll.scroll_to_item(ix / cols, ScrollStrategy::Nearest);
                }
                self.scrolled_for = Some((a, cols));
            }
        }

        let list = uniform_list(
            "library-grid",
            row_count,
            cx.processor(move |this, range: Range<usize>, _window, cx| this.render_rows(range, tile_min, row_h, colors, cx)),
        )
        .track_scroll(&self.scroll)
        .with_decoration(VisibleRows(cx.entity().downgrade()))
        .flex_1()
        .min_h_0()
        .w_full();
        let menu = self.render_menu(colors, cx);
        root.px(px(12.)).pt(px(8.)).child(list).children(menu)
    }
}
