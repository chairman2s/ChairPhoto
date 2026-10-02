//! The pop-out loupe window (#110): `LoupeWindow.tsx` and `modules/loupe.ts`.
//!
//! React ran the pop-out as a second webview and kept it in step with events: the main
//! window broadcast `loupe:photo` on every selection or version change, a module's card
//! went out as `loupe:card`, and a window that opened mid-session announced `loupe:ready` so
//! both were sent again. Here the pop-out is a second GPUI window over the **same entities**
//! as the main window (`AppModel`, `ShellState`, `ImageStore`, `ModuleRegistry`;
//! docs/plans/gpui/shell-apis.md § 4): its [`LoupeView`] reads the shell's target on every
//! render, so there is nothing to broadcast and no ready race.
//!
//! - **One at a time.** [`open`] opens it, or brings the open one forward (React's
//!   `getByLabel` + `setFocus`). The open happens deferred, outside the caller's window
//!   update, and a second request while it is opening is a no-op.
//! - **What it shows** is the loupe's target ([`ShellState::loupe_target`]): the active
//!   photo — or Compare's focused pane — whatever the main stage shows, with the active
//!   version's render when one is chosen for that photo. Its keys are the loupe's (arrows
//!   step the shared selection, the culling keys mark), except Enter/Esc and C, which act on
//!   the main window's stage.
//! - **Closing it** releases what it alone wanted ([`LoupeView::release`]: its preload
//!   window, its full-resolution tier, its version renders; image claims keep what the
//!   inline loupe still wants) and its module panel views (the registry drops a window's
//!   views when it closes). Closing it never quits the app; closing the main window does
//!   (`crate::wire`, `QuitMode::Explicit`).
//! - **A catalog switch** leaves it open: the shell drops the selection and the image layer
//!   its cache, so it shows "No photo selected" until a photo of the new catalog is chosen.

use crate::image_store::ImageStore;
use crate::loupe::card::{self, CardView};
use crate::loupe::view::{Follow, LoupeView};
use crate::model::AppModel;
use crate::modules::ModuleRegistry;
use crate::shell::state::ShellState;
use crate::shell::style::Colors;
use crate::APP_ID;
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, size, AnyWindowHandle, App, Bounds, Context, Entity, Global, TitlebarOptions, Window,
    Subscription, WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowId, WindowOptions,
};

/// The pop-out's title (React's `WebviewWindow` title).
pub const TITLE: &str = "ChairPhoto — Loupe";

/// The entities the pop-out shares with the main window, and the pop-out while it is open.
struct PopOut {
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    images: Entity<ImageStore>,
    modules: Entity<ModuleRegistry>,
    open: Option<(AnyWindowHandle, Entity<LoupeWindowView>)>,
    opening: bool,
}

impl Global for PopOut {}

/// Make the pop-out available ([`crate::wire`], after the entities exist): [`open`] does
/// nothing before this.
pub fn install(
    model: &Entity<AppModel>,
    shell: &Entity<ShellState>,
    images: &Entity<ImageStore>,
    modules: &Entity<ModuleRegistry>,
    cx: &mut App,
) {
    cx.set_global(PopOut {
        model: model.clone(),
        shell: shell.clone(),
        images: images.clone(),
        modules: modules.clone(),
        open: None,
        opening: false,
    });
    cx.on_window_closed(|cx, closed| on_closed(closed, cx)).detach();
}

/// The pop-out's window options: 1280×800 (React's), the app's `app_id` so the compositor's
/// rules match it, server-side decorations like the main window's.
pub fn window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        app_id: Some(APP_ID.into()),
        titlebar: Some(TitlebarOptions { title: Some(TITLE.into()), ..Default::default() }),
        window_min_size: Some(size(px(320.), px(240.))),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1280.), px(800.)), cx))),
        window_background: WindowBackgroundAppearance::Opaque,
        window_decorations: Some(WindowDecorations::Server),
        ..Default::default()
    }
}

/// More ⋯ → "Open loupe in a new window", or a module's `open_loupe`: open the pop-out, or
/// bring the open one forward.
pub fn open(cx: &mut App) {
    let Some(popout) = cx.try_global::<PopOut>() else { return };
    if popout.opening {
        return;
    }
    if let Some((handle, _)) = &popout.open {
        let handle = *handle;
        if handle.update(cx, |_, window, _| window.activate_window()).is_ok() {
            return;
        }
        // Gone without the close hook having run: forget it and open a new one.
        if let Some((_, view)) = cx.global_mut::<PopOut>().open.take() {
            view.update(cx, |v, cx| v.release(cx));
        }
    }
    cx.global_mut::<PopOut>().opening = true;
    // `open_window` draws the new window at once: not inside the caller's window update.
    cx.defer(open_now);
}

fn open_now(cx: &mut App) {
    let (model, shell, images, modules) = {
        let p = cx.global::<PopOut>();
        (p.model.clone(), p.shell.clone(), p.images.clone(), p.modules.clone())
    };
    let status = model.clone();
    let opened = gpui_kit::open_window(window_options(cx), cx, move |window, cx| {
        cx.new(|cx| LoupeWindowView::new(model, shell, images, modules, window, cx))
    });
    let popout = cx.global_mut::<PopOut>();
    popout.opening = false;
    match opened {
        Ok((handle, view)) => popout.open = Some((handle, view)),
        Err(e) => {
            eprintln!("could not open the loupe window: {e}");
            status.update(cx, |m, cx| m.set_status(format!("Could not open the loupe window: {e}"), cx));
        }
    }
}

/// The open pop-out's window, if there is one.
pub fn handle(cx: &App) -> Option<AnyWindowHandle> {
    cx.try_global::<PopOut>().and_then(|p| p.open.as_ref().map(|(h, _)| *h))
}

/// The open pop-out's root view, if there is one.
pub fn view(cx: &App) -> Option<Entity<LoupeWindowView>> {
    cx.try_global::<PopOut>().and_then(|p| p.open.as_ref().map(|(_, v)| v.clone()))
}

/// Close the pop-out, if it is open.
pub fn close(cx: &mut App) {
    if let Some(handle) = handle(cx) {
        handle.update(cx, |_, window, _| window.remove_window()).ok();
    }
}

fn on_closed(closed: WindowId, cx: &mut App) {
    let Some(popout) = cx.try_global::<PopOut>() else { return };
    if !popout.open.as_ref().is_some_and(|(h, _)| h.window_id() == closed) {
        return;
    }
    if let Some((_, view)) = cx.global_mut::<PopOut>().open.take() {
        view.update(cx, |v, cx| v.release(cx));
    }
}

/// The pop-out's root: a module's card while one is up (and its module enabled), else a
/// [`LoupeView`] that follows the target whatever the main stage shows.
pub struct LoupeWindowView {
    shell: Entity<ShellState>,
    modules: Entity<ModuleRegistry>,
    loupe: Entity<LoupeView>,
    card: Entity<CardView>,
    _observers: Vec<Subscription>,
}

impl LoupeWindowView {
    pub fn new(
        model: Entity<AppModel>,
        shell: Entity<ShellState>,
        images: Entity<ImageStore>,
        modules: Entity<ModuleRegistry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let card = cx.new(|cx| CardView::new(&model, shell.clone(), images.clone(), cx));
        let loupe = cx.new(|cx| LoupeView::new(model, shell.clone(), images, modules.clone(), Follow::Window, cx));
        loupe.read(cx).focus_handle().clone().focus(window, cx);
        let _observers =
            vec![cx.observe(&shell, |_, _, cx| cx.notify()), cx.observe(&modules, |_, _, cx| cx.notify())];
        LoupeWindowView { shell, modules, loupe, card, _observers }
    }

    pub fn loupe(&self) -> &Entity<LoupeView> {
        &self.loupe
    }

    pub fn card(&self) -> &Entity<CardView> {
        &self.card
    }

    /// The window closed: see [`LoupeView::release`] and [`CardView::release`].
    fn release(&mut self, cx: &mut Context<Self>) {
        self.loupe.update(cx, |l, cx| l.release(cx));
        self.card.update(cx, |c, cx| c.release(cx));
    }
}

impl Render for LoupeWindowView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let root = div().id("loupe-window").size_full().flex().flex_col().bg(colors.canvas).text_color(colors.txt);
        if card::shown(&self.shell, &self.modules, cx) {
            return root.child(self.card.clone());
        }
        // With the card down, the loupe has the keys again.
        let focus = self.loupe.read(cx).focus_handle().clone();
        if window.focused(cx).is_none() {
            focus.focus(window, cx);
        }
        root.child(self.loupe.clone())
    }
}
