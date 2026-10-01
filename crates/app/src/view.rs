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
//! The stage is the slot the Library grid ([`LibraryView`]), the loupe (#109) and the
//! Darkroom (#111) fill; a surface not ported yet says so. The side columns drag-resize from
//! 140 to 640 px; at or below 1024 px window width they become overlays behind a scrim
//! (`useNarrow.ts`). The "Stack bursts" dialog ([`StackDialog`]) opens over everything.
//!
//! The root sets the [`contexts::ROOT`] key context: the app-wide bindings and every shell
//! action (`shell::actions`) are handled here — ported ones by [`ShellState`], the rest by
//! [`AppModel::not_yet_ported`]. The grid takes focus at startup; root keys still reach
//! the root, an ancestor of the grid in the dispatch path.
//!
//! **Module slots** ([`crate::modules`]): enabled modules' main views are rail items and fill
//! the stage as `Surface::Module(id)` (the shell falls back to the Library when that module is
//! disabled); sidebar panels render under the collection browser's sections, inspector panels
//! on the inspector's tags tab; their actions are More ⋯ → Modules, their publish targets the
//! Publish dialog, their settings panels a Preferences tab each (`crate::preferences`).

use crate::keymap::{contexts, ReloadTheme};
use crate::image_store::ImageStore;
use crate::library::grid::LibraryView;
use crate::library::stacks::{Closed, StackDialog};
use crate::model::AppModel;
use crate::modules::registry::SlotView;
use crate::modules::{panel as module_panel, ModuleRegistry, PanelSlot};
use crate::storage::StorageState;
use crate::shell::actions::*;
use crate::shell::state::{ShellState, Side, Surface, NARROW_MAX_W};
use crate::shell::style::Colors;
use gpui_kit::component::slider::SliderState;
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, AnyElement, Context, CursorStyle, Entity, FocusHandle, MouseButton, MouseMoveEvent, Pixels,
    SharedString, Subscription, TestSupportExt as _, Window,
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
    pub(crate) modules: Entity<ModuleRegistry>,
    /// Storage and import (#114): its jobs and dialogs.
    pub(crate) storage: Entity<StorageState>,
    /// The open storage or Preferences dialog's close request (`crate::storage::open`).
    pub(crate) dialog_close: Option<Subscription>,
    pub(crate) focus: FocusHandle,
    pub(crate) thumb_slider: Entity<SliderState>,
    pub(crate) library: Entity<LibraryView>,
    /// The "Stack bursts" dialog while it is open, and its close subscription.
    pub(crate) stacks: Option<(Entity<StackDialog>, Subscription)>,
    /// The catalog the dialog was opened on: a switch closes it.
    catalog_epoch: u64,
    resize: Option<Resize>,
    _observers: [Subscription; 6],
}

impl RootView {
    /// The root owns focus from the start, so the app-wide bindings in [`contexts::ROOT`]
    /// work before anything else takes it.
    pub fn new(
        model: Entity<AppModel>,
        shell: Entity<ShellState>,
        images: Entity<ImageStore>,
        modules: Entity<ModuleRegistry>,
        storage: Entity<StorageState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let library = cx.new(|cx| LibraryView::new(shell.clone(), images.clone(), cx));
        // The grid has focus from the start: its keys work at once, and the root's bindings
        // in [`contexts::ROOT`] still reach the root, the grid's ancestor.
        library.read(cx).focus_handle().clone().focus(window, cx);
        let thumb_slider = crate::shell::command_pill::thumb_slider(&shell, window, cx);
        let catalog_epoch = model.read(cx).catalog_epoch;
        let _observers = [
            cx.observe(&model, |this, model, cx| {
                // A catalog switch closes the dialog: its groups name the old catalog's photos.
                let epoch = model.read(cx).catalog_epoch;
                if epoch != this.catalog_epoch {
                    this.catalog_epoch = epoch;
                    this.stacks = None;
                }
                cx.notify()
            }),
            cx.observe(&shell, |_, _, cx| cx.notify()),
            cx.observe(&images, |_, _, cx| cx.notify()),
            // A module disabled while its main view is on the stage takes the view with it:
            // back to the Library, as React fell back when `activeView` vanished.
            cx.observe(&modules, |this, modules, cx| {
                let orphaned = match &this.shell.read(cx).surface {
                    Surface::Module(id) => !modules.read(cx).main_views().iter().any(|(_, v)| v.id.as_ref() == id),
                    _ => false,
                };
                if orphaned {
                    this.shell.update(cx, |s, cx| s.show_library(cx));
                }
                cx.notify();
            }),
            cx.observe(&storage, |_, _, cx| cx.notify()),
            // React re-read the back-up queue and the trash count on window focus, and backed
            // up what waited when the NAS was reachable (App.tsx `onFocus`: `checkReconcile` +
            // `refreshTrashCount`): an external change or a reconnected NAS shows on return.
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.shell.update(cx, |s, cx| s.refresh_on_focus(cx));
                    if this.model.read(cx).catalog.is_some() {
                        this.storage.update(cx, |s, cx| s.check_reconcile(cx));
                    }
                }
            }),
        ];
        Self {
            model,
            shell,
            images,
            modules,
            storage,
            dialog_close: None,
            focus,
            thumb_slider,
            library,
            stacks: None,
            catalog_epoch,
            resize: None,
            _observers,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn shell(&self) -> &Entity<ShellState> {
        &self.shell
    }

    /// The Library grid.
    pub fn library(&self) -> &Entity<LibraryView> {
        &self.library
    }

    /// Re-read the system theme off the UI thread; it paints when following.
    fn reload_theme(&mut self, cx: &mut Context<Self>) {
        crate::theme::reread_system_theme(cx);
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

    /// Open the "Stack bursts" dialog over the selection, else the whole view (App.tsx's
    /// `openStackProposals`).
    fn open_stack_proposals(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let targets = self.shell.read(cx).whole_view_targets();
        if targets.is_empty() {
            self.model.update(cx, |m, cx| m.set_status("No photos to group — scan or select some first.", cx));
            return;
        }
        let (model, shell, images) = (self.model.clone(), self.shell.clone(), self.images.clone());
        let dialog = cx.new(|cx| StackDialog::new(&model, shell, images, targets, window, cx));
        let closed = cx.subscribe_in(&dialog, window, |this, _, _: &Closed, window, cx| {
            this.stacks = None;
            this.library.read(cx).focus_handle().clone().focus(window, cx);
            cx.notify();
        });
        self.stacks = Some((dialog, closed));
        cx.notify();
    }

    fn render_stage(&self, shell: &ShellState, colors: Colors, module_view: Option<SlotView>) -> AnyElement {
        let stage = div().id("stage").relative().flex_1().min_h_0().flex().flex_col();
        if let Some(v) = module_view {
            return stage
                .child(
                    div()
                        .id(SharedString::from(format!("module-view-{}", v.id)))
                        .flex_1()
                        .min_h_0()
                        .child(v.view)
                        .test_support(),
                )
                .into_any_element();
        }
        match &shell.surface {
            Surface::Library => stage.child(self.library.clone()).into_any_element(),
            other => {
                let text = match other {
                    Surface::Module(id) => format!("Module view {id} is not available."),
                    _ => "The Darkroom comes with #111.".to_string(),
                };
                stage
                    .items_center()
                    .justify_center()
                    .child(div().text_size(px(12.)).text_color(colors.dim).child(text))
                    .into_any_element()
            }
        }
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let narrow = window.viewport_size().width <= px(NARROW_MAX_W);
        self.shell.update(cx, |s, _| s.set_narrow(narrow));
        let colors = Colors::get(cx);
        // Module contributions, built (once per window) before the shell is borrowed below.
        let module_view = match &self.shell.read(cx).surface {
            Surface::Module(id) => {
                let id = id.clone();
                ModuleRegistry::main_view(&self.modules, &id, window, cx)
            }
            _ => None,
        };
        let sidebar_panels = module_panel::render_panel_blocks(&self.modules, PanelSlot::Sidebar, colors, window, cx);
        let inspector_panels = module_panel::render_panel_blocks(&self.modules, PanelSlot::Inspector, colors, window, cx);
        let rail_views = self.modules.read(cx).main_views();
        let shell_entity = self.shell.clone();
        let model_entity = self.model.clone();
        let shell = shell_entity.read(cx);
        let model = model_entity.read(cx);

        let title_bar = self.render_title_bar(shell, model, colors, window);
        let rail = self.render_rail(shell, colors, rail_views, cx);
        let library = shell.surface == Surface::Library;
        let pill = library.then(|| self.render_command_pill(shell, colors, cx));
        let stage = self.render_stage(shell, colors, module_view);
        let bench = library.then(|| self.render_bench(shell, model, colors, cx));
        let (left_w, right_w) = (shell.layout.left_w, shell.layout.right_w);
        let show_left = !narrow && !shell.layout.left_hidden;
        let show_right = !narrow && !shell.layout.right_hidden && shell.surface != Surface::Develop;
        let overlay_left = narrow && shell.layout.overlay_left;
        let overlay_right = narrow && shell.layout.overlay_right;
        let browser =
            (show_left || overlay_left).then(|| self.render_collection_browser(shell, colors, sidebar_panels, cx));
        let inspector =
            (show_right || overlay_right).then(|| self.render_inspector(shell, colors, inspector_panels, cx));

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
            .on_action(cx.listener(|this, _: &OpenPreferences, window, cx| {
                this.open_preferences(crate::preferences::Tab::Storage, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PublishSelection, window, cx| {
                module_panel::open_publish_dialog(&this.modules, window, cx)
            }))
            // Storage and import (#114).
            .on_action(cx.listener(|this, _: &OpenCatalogs, window, cx| this.open_catalogs(window, cx)))
            .on_action(cx.listener(|this, _: &ImportFromCard, window, cx| this.open_import_card(window, cx)))
            .on_action(cx.listener(|this, _: &ImportBundle, window, cx| this.open_import_bundle(window, cx)))
            .on_action(cx.listener(|this, _: &OpenIdentityDebt, window, cx| this.open_identity_debt(window, cx)))
            .on_action(cx.listener(|this, _: &OpenTrash, window, cx| this.open_trash(window, cx)))
            .on_action(cx.listener(|this, _: &RescanLibrary, _, cx| this.storage.update(cx, |s, cx| s.rescan(cx))))
            .on_action(cx.listener(|this, _: &Reconcile, _, cx| this.storage.update(cx, |s, cx| s.run_reconcile(cx))))
            .on_action(cx.listener(|this, _: &CancelImport, _, cx| this.storage.update(cx, |s, cx| s.cancel_import(cx))))
            .on_action(cx.listener(|this, _: &BackUpSelection, _, cx| this.back_up_selection(cx)))
            .on_action(cx.listener(|this, _: &AnalyseBurst, _, cx| this.shell.update(cx, |s, cx| s.analyse_burst(cx))))
            .on_action(cx.listener(|this, _: &ProposeStacks, window, cx| this.open_stack_proposals(window, cx)))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| this.on_mouse_move(event, cx)))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.resize = None))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.canvas)
            .text_color(colors.txt)
            .child(title_bar)
            .child(body)
            .children(self.stacks.as_ref().map(|(dialog, _)| dialog.clone()));
        let model = self.model.clone();
        on_not_yet_ported(root, move |what, ticket, _, cx| {
            model.update(cx, |m, cx| m.not_yet_ported(what, ticket, cx))
        })
    }
}
