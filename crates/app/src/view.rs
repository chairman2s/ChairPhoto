//! The root view: the shell chrome around the main-area slot.
//!
//! ```text
//! ┌ title bar ─────────────────────────────────────────────────────────┐
//! │ rail │ collection browser │ command pill            │ inspector   │
//! │      │  (left column)     │ stage — the main slot   │ (right col) │
//! │      │                    │ bench                   │             │
//! └──────┴────────────────────┴─────────────────────────┴─────────────┘
//! ```
//!
//! The stage is the slot the Library view (#106), the loupe (#109) and the Darkroom (#111)
//! fill; until they land it shows which surface is active and the last core event (the
//! event bridge's live proof). The side columns drag-resize from 140 to 640 px; at or below
//! 1024 px window width they become overlays behind a scrim (`useNarrow.ts`).
//!
//! The root owns focus and the [`contexts::ROOT`] key context: the app-wide bindings and every
//! shell action (`shell::actions`) are handled here — ported ones by [`ShellState`], the rest by
//! [`AppModel::not_yet_ported`].

use crate::keymap::{contexts, ReloadTheme};
use crate::image_store::{ImageState, ImageStore};
use crate::model::AppModel;
use chairphoto_core::image_pool::ImageKind;
use crate::shell::actions::*;
use crate::shell::state::{ShellState, Side, Surface, NARROW_MAX_W};
use crate::shell::style::Colors;
use gpui_kit::component::slider::SliderState;
use gpui_kit::prelude::*;
use gpui_kit::{
    div, img, px, AnyElement, Context, CursorStyle, Entity, FocusHandle, MouseButton, MouseMoveEvent, ObjectFit,
    Pixels, Subscription, TestSupportExt as _, Window,
};

/// A column-edge drag in progress: which column, where the pointer started, the width then.
#[derive(Debug, Clone, Copy)]
struct Resize {
    side: Side,
    start_x: Pixels,
    start_w: f32,
}

pub struct RootView {
    pub(crate) model: Entity<AppModel>,
    pub(crate) shell: Entity<ShellState>,
    pub(crate) images: Entity<ImageStore>,
    pub(crate) focus: FocusHandle,
    pub(crate) thumb_slider: Entity<SliderState>,
    resize: Option<Resize>,
    _observers: [Subscription; 4],
}

/// One cell of the stage's thumbnail strip — the image layer's on-screen proof (#101) until
/// the Library view (#106) replaces the stage.
const STRIP_CELL: (f32, f32) = (132., 96.);

impl RootView {
    /// The root owns focus from the start, so the app-wide bindings in [`contexts::ROOT`]
    /// work before anything else takes it.
    pub fn new(
        model: Entity<AppModel>,
        shell: Entity<ShellState>,
        images: Entity<ImageStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let thumb_slider = crate::shell::command_pill::thumb_slider(&shell, window, cx);
        let _observers = [
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.observe(&shell, |_, _, cx| cx.notify()),
            cx.observe(&images, |_, _, cx| cx.notify()),
            // React re-read the back-up queue and the trash count on window focus
            // (App.tsx `onFocus`): an external change or a finished backup shows on return.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.shell.update(cx, |s, cx| s.refresh_on_focus(cx));
                }
            }),
        ];
        Self { model, shell, images, focus, thumb_slider, resize: None, _observers }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn shell(&self) -> &Entity<ShellState> {
        &self.shell
    }

    /// Re-read the system theme off the UI thread and apply it.
    fn reload_theme(&mut self, cx: &mut Context<Self>) {
        let read = cx
            .background_executor()
            .spawn(async { chairphoto_core::appearance::read_current_theme() });
        cx.spawn(async move |_, cx| {
            let result = read.await;
            cx.update(|cx| crate::theme::apply_system_theme(&result, cx));
        })
        .detach();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(resize) = self.resize else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            self.resize = None; // released outside the window
            return;
        }
        let dx = f32::from(event.position.x - resize.start_x);
        // The right column grows when dragged left.
        let delta = if resize.side == Side::Left { dx } else { -dx };
        self.shell.update(cx, |s, cx| s.set_column_width(resize.side, resize.start_w + delta, cx));
    }

    /// A column's drag handle (`.col-resizer`): 7 px wide, straddling the column's inner edge.
    fn resizer(&self, side: Side, width: f32, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id(match side {
                Side::Left => "resize-left",
                Side::Right => "resize-right",
            })
            .absolute()
            .top_0()
            .bottom_0()
            .w(px(7.))
            .when(side == Side::Left, |d| d.right(px(-4.)))
            .when(side == Side::Right, |d| d.left(px(-4.)))
            .cursor(CursorStyle::ResizeLeftRight)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui_kit::MouseDownEvent, _, cx| {
                    this.resize = Some(Resize { side, start_x: event.position.x, start_w: width });
                    cx.stop_propagation();
                }),
            )
    }

    /// The first thumbnails of the open catalog through the [`ImageStore`]: requested in display
    /// order (the pool is LIFO, so the batch keeps the first cell first), then read back —
    /// cached, loading or failed.
    fn strip_cells(&self, cx: &mut Context<Self>) -> Vec<(i64, ImageState)> {
        let ids: Vec<i64> = self.model.read(cx).catalog.as_ref().map(|c| c.first_photos.clone()).unwrap_or_default();
        self.images.update(cx, |store, _| {
            let wanted: Vec<_> = ids.iter().map(|&id| (id, ImageKind::Thumb)).collect();
            store.request_batch(&wanted);
            ids.iter().map(|&id| (id, store.get(id, ImageKind::Thumb))).collect()
        })
    }

    fn render_strip(cells: Vec<(i64, ImageState)>, colors: Colors) -> AnyElement {
        div()
            .id("thumb-strip")
            .flex()
            .flex_row()
            .flex_wrap()
            .justify_center()
            .gap_2()
            .max_w(px(8. * (STRIP_CELL.0 + 8.)))
            .children(cells.into_iter().map(|(id, state)| {
                let cell = div()
                    .id(("thumb", id as u64))
                    .w(px(STRIP_CELL.0))
                    .h(px(STRIP_CELL.1))
                    .rounded_sm()
                    .overflow_hidden()
                    .bg(colors.panel);
                match state {
                    ImageState::Ready(loaded) => {
                        cell.child(img(loaded.image).size_full().object_fit(ObjectFit::Contain)).into_any_element()
                    }
                    ImageState::Failed(_) => cell.border_1().border_color(colors.danger).into_any_element(),
                    ImageState::Loading | ImageState::Absent => cell.border_1().border_color(colors.border).into_any_element(),
                }
            }))
            .into_any_element()
    }

    fn render_stage(&self, shell: &ShellState, model: &AppModel, colors: Colors, strip: Vec<(i64, ImageState)>) -> AnyElement {
        let surface = match &shell.surface {
            Surface::Library => "The library grid comes with the Library view (#106).".to_string(),
            Surface::Develop => "The Darkroom comes with #111.".to_string(),
            Surface::Module(id) => format!("Module view {id}: Module registry (#104)."),
        };
        let last_event = model.last_event.clone().unwrap_or_else(|| "Waiting for core events…".into());
        div()
            .id("stage")
            .relative()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .justify_center()
            .items_center()
            .gap_2()
            .p(px(12.))
            .child(div().text_size(px(12.)).text_color(colors.dim).child(surface))
            .when(shell.surface == Surface::Library && !strip.is_empty(), |d| d.child(Self::render_strip(strip, colors)))
            .child(
                div()
                    .id("last-event")
                    .text_size(px(11.))
                    .text_color(colors.mute)
                    .child(format!("{} core events · last: {last_event}", model.events_seen))
                    .test_support(),
            )
            .into_any_element()
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let narrow = window.viewport_size().width <= px(NARROW_MAX_W);
        self.shell.update(cx, |s, _| s.set_narrow(narrow));
        let strip = self.strip_cells(cx);
        let colors = Colors::get(cx);
        let shell_entity = self.shell.clone();
        let model_entity = self.model.clone();
        let shell = shell_entity.read(cx);
        let model = model_entity.read(cx);

        let title_bar = self.render_title_bar(shell, model, colors, window);
        let rail = self.render_rail(shell, colors, cx);
        let library = shell.surface == Surface::Library;
        let pill = library.then(|| self.render_command_pill(shell, colors, cx));
        let stage = self.render_stage(shell, model, colors, strip);
        let bench = library.then(|| self.render_bench(shell, model, colors, cx));
        let (left_w, right_w) = (shell.layout.left_w, shell.layout.right_w);
        let show_left = !narrow && !shell.layout.left_hidden;
        let show_right = !narrow && !shell.layout.right_hidden && shell.surface != Surface::Develop;
        let overlay_left = narrow && shell.layout.overlay_left;
        let overlay_right = narrow && shell.layout.overlay_right;
        let browser = (show_left || overlay_left).then(|| self.render_collection_browser(shell, colors, cx));
        let inspector = (show_right || overlay_right).then(|| self.render_inspector(shell, colors, cx));

        let mut body = div().id("body").relative().flex().flex_row().flex_1().min_h_0().child(rail);
        let (browser_col, overlay_browser) = if show_left { (browser, None) } else { (None, browser) };
        let (inspector_col, overlay_inspector) = if show_right { (inspector, None) } else { (None, inspector) };
        if let Some(browser) = browser_col {
            body = body.child(
                div()
                    .id("left-column")
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(px(left_w))
                    .h_full()
                    .border_r_1()
                    .border_color(colors.border)
                    .child(browser)
                    .child(self.resizer(Side::Left, left_w, cx))
                    .test_support(),
            );
        }
        body = body.child(
            div()
                .id("main")
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .h_full()
                .children(pill)
                .child(stage)
                .children(bench),
        );
        if let Some(inspector) = inspector_col {
            body = body.child(
                div()
                    .id("right-column")
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(px(right_w))
                    .h_full()
                    .border_l_1()
                    .border_color(colors.border)
                    .child(inspector)
                    .child(self.resizer(Side::Right, right_w, cx))
                    .test_support(),
            );
        }
        // Narrow: the columns are overlays over the body, behind a scrim that closes them.
        for (side, panel) in [(Side::Left, overlay_browser), (Side::Right, overlay_inspector)] {
            let Some(panel) = panel else { continue };
            let w = if side == Side::Left { left_w } else { right_w };
            body = body.child(
                div()
                    .id(match side {
                        Side::Left => "overlay-left",
                        Side::Right => "overlay-right",
                    })
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(crate::shell::sidebar::RAIL_W))
                    .right_0()
                    .bg(colors.scrim)
                    .flex()
                    .flex_row()
                    .when(side == Side::Right, |d| d.justify_end())
                    .on_click(cx.listener(move |this, _, _, cx| this.shell.update(cx, |s, cx| s.hide_panel(side, cx))))
                    .child(
                        div()
                            .id(match side {
                                Side::Left => "overlay-left-panel",
                                Side::Right => "overlay-right-panel",
                            })
                            .flex()
                            .flex_col()
                            .w(px(w))
                            .h_full()
                            .bg(colors.panel)
                            // Clicks inside the panel must not reach the scrim.
                            .on_click(|_, _, cx| cx.stop_propagation())
                            .child(panel),
                    )
                    .test_support(),
            );
        }

        let root = div()
            .id("root")
            .key_context(contexts::ROOT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &ReloadTheme, _, cx| this.reload_theme(cx)))
            .on_action(cx.listener(|this, _: &ToggleLeftPanel, _, cx| {
                this.shell.update(cx, |s, cx| s.toggle_panel(Side::Left, cx))
            }))
            .on_action(cx.listener(|this, _: &ToggleRightPanel, _, cx| {
                this.shell.update(cx, |s, cx| s.toggle_panel(Side::Right, cx))
            }))
            .on_action(cx.listener(|this, _: &ToggleCachePreviews, _, cx| {
                this.shell.update(cx, |s, cx| s.toggle_cache_previews(cx))
            }))
            .on_action(cx.listener(|this, _: &ShowAllPhotos, _, cx| {
                this.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.clear_scope()))
            }))
            .on_action(cx.listener(|this, _: &ShowLibrary, _, cx| this.shell.update(cx, |s, cx| s.show_library(cx))))
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| this.shell.update(cx, |s, cx| s.clear_selection(cx))))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| this.on_mouse_move(event, cx)))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.resize = None))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.canvas)
            .text_color(colors.txt)
            .child(title_bar)
            .child(body);
        let model = self.model.clone();
        on_not_yet_ported(root, move |what, ticket, _, cx| {
            model.update(cx, |m, cx| m.not_yet_ported(what, ticket, cx))
        })
    }
}
