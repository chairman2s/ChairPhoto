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
//! The stage is the slot the Library grid ([`LibraryView`]), the loupe and Compare
//! ([`crate::loupe`], `ShellState::stage_view`) and the
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
use crate::inspector::PhotoInspector;
use crate::library::grid::LibraryView;
use crate::library::grid_menu::PhotoCommand;
use crate::library::stacks::{Closed, StackDialog};
use crate::loupe::compare::{CompareMode, MODE_PREF};
use crate::loupe::compare_view::CompareView;
use crate::loupe::cull::{CullEnded, CullView};
use crate::loupe::view::{Follow, LoupeView};
use crate::machine_prefs::MachinePrefs;
use crate::model::AppModel;
use crate::modules::registry::SlotView;
use crate::modules::{panel as module_panel, ModuleRegistry, PanelSlot};
use crate::storage::StorageState;
use crate::tags::panel::TagPanel;
use crate::tags::photo_tags::{PhotoTags, TagTarget};
use crate::tags::TagsState;
use crate::shell::actions::*;
use crate::shell::state::{ShellState, Side, StageView, Surface, NARROW_MAX_W};
use crate::shell::style::Colors;
use gpui_kit::component::slider::SliderState;
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, AnyElement, Context, CursorStyle, Entity, FocusHandle, MouseButton, MouseMoveEvent, Pixels,
    SharedString, Subscription, TestSupportExt as _, WeakEntity, Window,
};

/// Albums and export's entities (#115), handed to the root view together.
pub struct Collections {
    pub albums: Entity<crate::albums::AlbumsState>,
    pub exports: Entity<crate::export::ExportState>,
}

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
    /// Tags (#107): the collection browser's tag panel, and the inspector tags tab's tagging
    /// block (mounted on the Photo inspector's tags tab, #108).
    pub(crate) tag_panel: Entity<TagPanel>,
    pub(crate) photo_tags: Entity<PhotoTags>,
    /// The tag tree both of those use; Preferences → Tags merges under it too.
    pub(crate) tags: Entity<TagsState>,
    /// Albums and smart albums (#115): their write path and dialogs.
    pub(crate) albums: Entity<crate::albums::AlbumsState>,
    /// The export jobs and dialogs (#115).
    pub(crate) exports: Entity<crate::export::ExportState>,
    /// The open name prompt's submission.
    pub(crate) album_prompt: Option<Subscription>,
    /// The open Export dialog's "Export as bundle…".
    pub(crate) export_batch: Option<Subscription>,
    /// This view, for menu rows that open a dialog over it.
    pub(crate) this: WeakEntity<RootView>,
    /// The open storage or Preferences dialog's close request (`crate::storage::open`).
    pub(crate) dialog_close: Option<Subscription>,
    pub(crate) focus: FocusHandle,
    pub(crate) thumb_slider: Entity<SliderState>,
    pub(crate) library: Entity<LibraryView>,
    /// The Photo inspector's tab bodies (#108), drawn inside the inspector column.
    pub(crate) inspector: Entity<PhotoInspector>,
    /// The "Stack bursts" dialog while it is open, and its close subscription.
    pub(crate) stacks: Option<(Entity<StackDialog>, Subscription)>,
    /// The inline loupe and Compare (#109), on the Library stage instead of the grid while
    /// the shell says so (`ShellState::stage_view`).
    pub(crate) loupe: Entity<LoupeView>,
    pub(crate) compare: Entity<CompareView>,
    /// The cull session while it runs (full screen, over everything), and its end.
    pub(crate) cull: Option<(Entity<CullView>, Subscription)>,
    /// What the stage showed at the last shell change: a change hands focus to the new view.
    stage_seen: StageView,
    /// The catalog the dialog was opened on: a switch closes it.
    catalog_epoch: u64,
    /// The Remove-from-catalog confirm while it is open (its serial): a switch closes it
    /// (`crate::library::photo_actions`).
    pub(crate) remove_confirm: Option<u64>,
    pub(crate) confirm_serial: u64,
    resize: Option<Resize>,
    _observers: [Subscription; 10],
    /// Develop takes the keys when it opens (its arrows step the filmstrip) and hands them
    /// back to the grid when the Library returns.
    #[cfg(feature = "edit")]
    _develop_focus: Subscription,
    /// The Darkroom (#111): the stage on `Surface::Develop`.
    #[cfg(feature = "edit")]
    pub(crate) darkroom: Entity<crate::darkroom::DarkroomView>,
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
        tags: Entity<TagsState>,
        collections: Collections,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let library = cx.new(|cx| LibraryView::new(shell.clone(), images.clone(), cx));
        // The grid has focus from the start: its keys work at once, and the root's bindings
        // in [`contexts::ROOT`] still reach the root, the grid's ancestor.
        library.read(cx).focus_handle().clone().focus(window, cx);
        let thumb_slider = crate::shell::command_pill::thumb_slider(&shell, window, cx);
        let inspector = cx.new(|cx| PhotoInspector::new(model.clone(), shell.clone(), images.clone(), window, cx));
        let tag_panel = cx.new(|cx| TagPanel::new(tags.clone(), shell.clone(), modules.clone(), window, cx));
        let target = tag_target(shell.read(cx));
        let photo_tags = cx.new(|cx| PhotoTags::new(tags.clone(), target, window, cx));
        let compare_mode = CompareMode::from_pref(MachinePrefs::read(cx, MODE_PREF).as_deref());
        shell.update(cx, |s, _| s.compare_mode = compare_mode);
        let loupe = cx.new(|cx| {
            LoupeView::new(model.clone(), shell.clone(), images.clone(), modules.clone(), Follow::Inline, cx)
        });
        let compare = cx.new(|cx| CompareView::new(shell.clone(), images.clone(), cx));
        let catalog_epoch = model.read(cx).catalog_epoch;
        #[cfg(feature = "edit")]
        let darkroom = {
            let pool: std::sync::Arc<dyn crate::image_store::Submit> = match model.read(cx).pool() {
                Some(pool) => pool.clone(),
                None => std::sync::Arc::new(crate::NoPool),
            };
            let state = cx.new(|cx| crate::darkroom::Darkroom::new(&model, shell.clone(), images.clone(), pool, cx));
            cx.new(|cx| crate::darkroom::DarkroomView::new(state, window, cx))
        };
        #[cfg(feature = "edit")]
        let _develop_focus = cx.observe_in(&shell, window, |this, shell, window, cx| {
            let focus = this.darkroom.read(cx).focus_handle().clone();
            match shell.read(cx).surface {
                Surface::Develop if !focus.contains_focused(window, cx) => focus.focus(window, cx),
                // Back in the Library: the grid takes the keys again.
                Surface::Library if focus.contains_focused(window, cx) => {
                    this.library.read(cx).focus_handle().clone().focus(window, cx)
                }
                _ => {}
            }
        });
        let _observers = [
            cx.observe_in(&model, window, |this, model, window, cx| {
                // A catalog switch closes the dialog: its groups name the old catalog's photos.
                // The dialog had the focus; the grid gets it back, as on Close.
                let epoch = model.read(cx).catalog_epoch;
                if epoch != this.catalog_epoch {
                    this.catalog_epoch = epoch;
                    // The Remove confirm names the old catalog's photo.
                    if this.remove_confirm.take().is_some() {
                        gpui_kit::component::WindowExt::close_dialog(window, cx);
                    }
                    // The cull session's list is the old catalog's photos too.
                    let cull = this.cull.take().is_some();
                    if this.stacks.take().is_some() || cull {
                        this.focus_stage(window, cx);
                    }
                }
                cx.notify()
            }),
            // The root itself focused — by a title-bar menu (its `action_context` focuses the
            // root before dispatching, and gpui-component's popup then leaves focus there), by
            // a dialog opened from such a menu restoring focus on close, or by a click on the
            // chrome: hand it to the grid, so its keys work again. Only on the Library
            // surface and with no Stack dialog open, where the grid is what takes keys.
            cx.on_focus(&focus, window, |this, window, cx| {
                if this.stacks.is_none() && this.cull.is_none() && this.shell.read(cx).surface == Surface::Library {
                    this.focus_stage(window, cx);
                }
            }),
            // The stage changed (the loupe or Compare opened or closed — by a key, a click, a
            // catalog switch): its view takes the keys.
            cx.observe_in(&shell, window, |this, shell, window, cx| {
                let stage = shell.read(cx).stage_view();
                if stage != this.stage_seen {
                    this.stage_seen = stage;
                    if this.stacks.is_none() && this.cull.is_none() && shell.read(cx).surface == Surface::Library {
                        this.focus_stage(window, cx);
                    }
                }
            }),
            cx.observe(&shell, |this, shell, cx| {
                // The tagging block follows the selection.
                let target = tag_target(shell.read(cx));
                this.photo_tags.update(cx, |p, cx| p.set_target(target, cx));
                cx.notify()
            }),
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
            // The grid's right-click menu (`crate::library::grid_menu`).
            cx.subscribe_in(&library, window, |this, _, command, window, cx| this.run_photo_command(command, window, cx)),
            cx.observe(&collections.exports, |_, _, cx| cx.notify()),
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
            tag_panel,
            photo_tags,
            tags,
            albums: collections.albums,
            exports: collections.exports,
            album_prompt: None,
            export_batch: None,
            this: cx.entity().downgrade(),
            dialog_close: None,
            focus,
            thumb_slider,
            library,
            inspector,
            stacks: None,
            loupe,
            compare,
            cull: None,
            stage_seen: StageView::Grid,
            #[cfg(feature = "edit")]
            darkroom,
            catalog_epoch,
            remove_confirm: None,
            confirm_serial: 0,
            resize: None,
            _observers,
            #[cfg(feature = "edit")]
            _develop_focus,
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

    /// The inline loupe.
    pub fn loupe(&self) -> &Entity<LoupeView> {
        &self.loupe
    }

    pub fn compare(&self) -> &Entity<CompareView> {
        &self.compare
    }

    pub fn cull(&self) -> Option<&Entity<CullView>> {
        self.cull.as_ref().map(|(v, _)| v)
    }

    /// Focus whichever view the Library stage shows: the grid, the loupe or Compare.
    pub(crate) fn focus_stage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = match self.shell.read(cx).stage_view() {
            StageView::Grid => self.library.read(cx).focus_handle().clone(),
            StageView::Loupe => self.loupe.read(cx).focus_handle().clone(),
            StageView::Compare => self.compare.read(cx).focus_handle().clone(),
        };
        handle.focus(window, cx);
    }

    /// Enter / More ⋯ → Loupe / a double-click: the inline loupe on or off.
    fn toggle_loupe(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| s.toggle_loupe(cx));
        self.focus_stage(window, cx);
    }

    /// C / the bench's Compare: open Compare on two or more selected, or close it.
    fn toggle_compare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| {
            if s.compare().is_some() {
                s.close_compare(cx);
            } else {
                s.open_compare(cx);
            }
        });
        self.focus_stage(window, cx);
    }

    /// More ⋯ → Start cull session / the bench's Cull: over the selection (in grid order),
    /// else the whole view, frozen now (App.tsx's `startCullSession`).
    fn start_cull(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shell = self.shell.read(cx);
        let selected: std::collections::HashSet<i64> = shell.library.selection().ids.iter().copied().collect();
        let rows: Vec<chairphoto_core::catalog::Photo> = shell
            .library
            .photos()
            .iter()
            .filter(|p| selected.is_empty() || selected.contains(&p.id))
            .cloned()
            .collect();
        let Some(from) = shell.rows_from().filter(|_| !rows.is_empty()) else {
            self.model.update(cx, |m, cx| m.set_status("Nothing to cull — scan or select some photos first.", cx));
            return;
        };
        let app = self.model.read(cx).state().clone();
        let (shell, images) = (self.shell.clone(), self.images.clone());
        let view = cx.new(|cx| CullView::new(app, shell, images, rows, from, cx));
        let ended = cx.subscribe_in(&view, window, |this, _, ended: &CullEnded, window, cx| {
            this.cull = None;
            let line = ended.0.status_line();
            this.model.update(cx, |m, cx| m.set_status(line, cx));
            // The session wrote without re-reading; the grid catches up once.
            this.shell.update(cx, |s, cx| s.refresh_rows(cx));
            this.focus_stage(window, cx);
            cx.notify();
        });
        view.read(cx).focus_handle().clone().focus(window, cx);
        self.cull = Some((view, ended));
        cx.notify();
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
        let Some(from) = self.shell.read(cx).rows_from().filter(|_| !targets.is_empty()) else {
            self.model.update(cx, |m, cx| m.set_status("No photos to group — scan or select some first.", cx));
            return;
        };
        let (model, shell, images) = (self.model.clone(), self.shell.clone(), self.images.clone());
        let dialog = cx.new(|cx| StackDialog::new(&model, shell, images, targets, from, window, cx));
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
            Surface::Library => match shell.stage_view() {
                StageView::Grid => stage.child(self.library.clone()).into_any_element(),
                StageView::Loupe => stage.child(self.loupe.clone()).into_any_element(),
                StageView::Compare => stage.child(self.compare.clone()).into_any_element(),
            },
            #[cfg(feature = "edit")]
            Surface::Develop => stage.child(self.darkroom.clone()).into_any_element(),
            other => {
                let text = match other {
                    Surface::Module(id) => format!("Module view {id} is not available."),
                    _ => "This build has no Darkroom (the `edit` feature is off).".to_string(),
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

/// What the inspector's tagging block edits: the active photo, and the selection's targets.
fn tag_target(shell: &ShellState) -> TagTarget {
    let selection = shell.library.selection();
    TagTarget { active: selection.active.map(|p| p.id), targets: selection.targets }
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
        // React hid the command pill in Compare: there is nothing there for it to filter.
        let pill = (library && shell.stage_view() != StageView::Compare)
            .then(|| self.render_command_pill(shell, colors, cx));
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
            .on_action(cx.listener(|this, _: &OpenDevelop, _, cx| this.shell.update(cx, |s, cx| s.open_develop(cx))))
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| this.shell.update(cx, |s, cx| s.clear_selection(cx))))
            .on_action(cx.listener(|this, _: &OpenPreferences, window, cx| {
                this.open_preferences(crate::preferences::Tab::Storage, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PublishSelection, window, cx| {
                module_panel::open_publish_dialog(&this.modules, window, cx);
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
            // Albums and export (#115).
            .on_action(cx.listener(|this, _: &ExportSelection, window, cx| this.open_export(window, cx)))
            .on_action(cx.listener(|this, _: &CancelExport, _, cx| this.exports.update(cx, |e, cx| e.cancel(cx))))
            .on_action(cx.listener(|this, _: &ToggleLoupe, window, cx| this.toggle_loupe(window, cx)))
            .on_action(cx.listener(|this, _: &OpenCompare, window, cx| this.toggle_compare(window, cx)))
            .on_action(cx.listener(|this, _: &StartCullSession, window, cx| this.start_cull(window, cx)))
            .on_action(|_: &PopOutLoupe, _, cx| crate::loupe::window::open(cx))
            // The loupe's unavailable state (#158).
            .on_action(cx.listener(|this, _: &RelocatePhoto, window, cx| {
                this.loupe_photo_command(|id, _, from| PhotoCommand::Relocate { id, from }, window, cx)
            }))
            .on_action(cx.listener(|this, _: &RetrieveFromNas, window, cx| {
                this.loupe_photo_command(|id, _, from| PhotoCommand::Retrieve { id, from }, window, cx)
            }))
            .on_action(cx.listener(|this, _: &RemoveFromCatalog, window, cx| {
                this.loupe_photo_command(|id, name, from| PhotoCommand::Remove { id, name, from }, window, cx)
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| this.on_mouse_move(event, cx)))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, _| this.resize = None))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.canvas)
            .text_color(colors.txt)
            .child(title_bar)
            .child(body)
            .children(self.stacks.as_ref().map(|(dialog, _)| dialog.clone()))
            .children(self.cull.as_ref().map(|(cull, _)| cull.clone()));
        let model = self.model.clone();
        on_not_yet_ported(root, move |what, ticket, _, cx| {
            model.update(cx, |m, cx| m.not_yet_ported(what, ticket, cx))
        })
    }
}
