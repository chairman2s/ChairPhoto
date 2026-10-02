//! [`ShellTimer`]: the host of the Develop → Library transition timing
//! ([`chairphoto_model::shell_timing`], a port of `src/modules/shellTiming.ts`) — dev evidence
//! behind the Darkroom's render-timing switch (`editor.renderTiming`).
//!
//! The model is sans-IO; this global gives it what the TypeScript took from the browser:
//!
//! | TS | here |
//! |---|---|
//! | `setShellTimingEnabled` from the Darkroom's settings read | [`ShellTimer::set_enabled`] from `Darkroom::read_settings` |
//! | `markShellLeave("develop")` on the Darkroom's Back | [`ShellTimer::leave`] from ← Library |
//! | `performance.now()` | the GPUI executor's clock (`BackgroundExecutor::now`), ms since this global was installed |
//! | `requestAnimationFrame` stall detector, the quiet `setTimeout` | one foreground ticker every [`FRAME`] while a transition is live: [`ShellTiming::note_frame`] then [`ShellTiming::poll`] — a tick that comes late is the UI thread blocked |
//! | `setTimeout(…, 0)` mark | the `timeout0` mark, on the first foreground turn after Back |
//! | the grid's React `Profiler` mount commit | the Library grid's first render after Back, timed around its `render` ([`ShellTimer::note_grid_commit`]) |
//! | `Thumbnail` mount / `onLoad` | the grid's tiles the first time each is built, and the first time each is built with its thumbnail ready ([`ShellTimer::note_tile`]) |
//! | `noteGridScroll` | the grid scrolling to the active photo |
//! | `timedInvoke` around api.ts calls | the shell's row read (`list_photos`) with its row count ([`ShellTimer::begin_invoke`] / [`ShellTimer::end_invoke`]) |
//! | `setSetting(SHELL_TIMING_KEY, …)` | written off the UI thread on the storage [`Runner`], to the catalog the Library rows came from (`with_catalog_as`) |
//!
//! The summary is logged as `[shell-timing] {json}` and stored under
//! `editor.renderTiming.lastShell`; the "started" marker is stored at Back, so a transition
//! that never paints a tile is still visible. Off, every hook is one global lookup and a
//! boolean check.

use crate::storage::Runner;
use chairphoto_core::app::{with_catalog, with_catalog_as, AppState, CatalogIdentity};
use chairphoto_model::shell_timing::{InvokeSpan, ShellTiming, SHELL_TIMING_KEY};
use gpui_kit::{App, Global, Task};
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// The stall detector's tick: one frame at 60 Hz.
pub const FRAME: Duration = Duration::from_millis(16);

/// The transition timing's host. A GPUI global, installed by `wire`.
pub struct ShellTimer {
    timing: ShellTiming,
    state: AppState,
    /// The clock's zero.
    origin: Instant,
    /// The catalog the transition's Library rows came from: where its record is written.
    from: Option<CatalogIdentity>,
    /// Tiles built, and tiles built with their thumbnail ready, since Back.
    mounted: HashSet<i64>,
    loaded: HashSet<i64>,
    /// The ticker of the live transition; dropping it (a newer Back) stops it.
    ticker: Option<Task<()>>,
    /// Every value written under [`SHELL_TIMING_KEY`], newest last (tests).
    #[cfg(test)]
    pub(crate) written: Vec<String>,
}

impl Global for ShellTimer {}

impl ShellTimer {
    pub fn install(state: AppState, cx: &mut App) {
        let origin = cx.background_executor().now();
        cx.set_global(ShellTimer {
            timing: ShellTiming::new(),
            state,
            origin,
            from: None,
            mounted: HashSet::new(),
            loaded: HashSet::new(),
            ticker: None,
            #[cfg(test)]
            written: Vec::new(),
        });
    }

    fn now(cx: &App) -> f64 {
        let origin = cx.try_global::<ShellTimer>().map_or_else(|| cx.background_executor().now(), |t| t.origin);
        cx.background_executor().now().saturating_duration_since(origin).as_secs_f64() * 1000.0
    }

    /// The Darkroom read `editor.renderTiming` (`"1"` = on).
    pub fn set_enabled(on: bool, cx: &mut App) {
        if cx.has_global::<ShellTimer>() {
            cx.global_mut::<ShellTimer>().timing.set_enabled(on);
        }
    }

    /// A transition is being recorded.
    pub fn live(cx: &App) -> bool {
        cx.try_global::<ShellTimer>().is_some_and(|t| t.timing.current().is_some_and(|c| !c.finished))
    }

    /// The user left `from_surface` (the Darkroom's ← Library). Starts a transition when the
    /// instrument is on: stores the "started" marker and starts the ticker. `catalog` is the
    /// catalog the Library rows came from.
    pub fn leave(from_surface: &str, catalog: Option<CatalogIdentity>, cx: &mut App) {
        let now = Self::now(cx);
        if !cx.has_global::<ShellTimer>() {
            return;
        }
        let t = cx.global_mut::<ShellTimer>();
        let Some(started) = t.timing.mark_shell_leave(from_surface, now) else { return };
        t.from = catalog;
        t.mounted.clear();
        t.loaded.clear();
        Self::persist(started, cx);
        let ticker = cx.spawn(async move |cx| {
            // `setTimeout(…, 0)`: when the UI thread first turns over after Back.
            cx.update(|cx| {
                let now = Self::now(cx);
                cx.global_mut::<ShellTimer>().timing.note_mark("timeout0", now);
            });
            loop {
                cx.background_executor().timer(FRAME).await;
                if !cx.update(Self::tick) {
                    return;
                }
            }
        });
        cx.global_mut::<ShellTimer>().ticker = Some(ticker);
    }

    /// One frame: the stall detector, then the quiet deadline. Returns whether the transition
    /// is still live.
    fn tick(cx: &mut App) -> bool {
        let now = Self::now(cx);
        let t = cx.global_mut::<ShellTimer>();
        t.timing.note_frame(now);
        match t.timing.poll(now) {
            Some(summary) => {
                let json = summary.to_json();
                eprintln!("[shell-timing] {json}");
                Self::persist(json, cx);
                false
            }
            None => t.timing.current().is_some_and(|c| !c.finished),
        }
    }

    /// Store `value` under [`SHELL_TIMING_KEY`], off the UI thread, in the transition's
    /// catalog (a write that finds another catalog open fails closed, logged).
    fn persist(value: String, cx: &mut App) {
        let t = cx.global_mut::<ShellTimer>();
        #[cfg(test)]
        t.written.push(value.clone());
        let (state, from) = (t.state.clone(), t.from);
        Runner::get(cx).spawn(move || {
            let write = |c: &chairphoto_core::catalog::Catalog| c.set_setting(SHELL_TIMING_KEY, &value);
            let result = match from {
                Some(from) => with_catalog_as(&state, from, write),
                None => with_catalog(&state, write),
            };
            if let Err(e) = result {
                eprintln!("shell timing: not stored: {e}");
            }
        });
    }

    /// The Library grid rendered, its `render` taking `render_ms`: the first one after Back is
    /// the transition's "commit".
    pub fn note_grid_commit(render_ms: f64, cx: &mut App) {
        if !Self::live(cx) {
            return;
        }
        let now = Self::now(cx);
        let t = cx.global_mut::<ShellTimer>();
        if t.timing.current().is_some_and(|c| c.commit_at.is_none()) {
            t.timing.note_grid_commit("mount", render_ms, now);
        }
    }

    /// The grid built tile `photo`, with its thumbnail `ready` or not.
    pub fn note_tile(photo: i64, ready: bool, cx: &mut App) {
        if !Self::live(cx) {
            return;
        }
        let now = Self::now(cx);
        let t = cx.global_mut::<ShellTimer>();
        if t.mounted.insert(photo) {
            t.timing.note_tile_mounted();
        }
        if ready && t.loaded.insert(photo) {
            t.timing.note_tile_loaded(now);
        }
    }

    /// The grid scrolled to the active photo.
    pub fn note_grid_scroll(cx: &mut App) {
        if !Self::live(cx) {
            return;
        }
        let now = Self::now(cx);
        cx.global_mut::<ShellTimer>().timing.note_grid_scroll(now);
    }

    /// A backend read starts; `None` outside a live transition.
    pub fn begin_invoke(cx: &mut App) -> Option<InvokeSpan> {
        if !Self::live(cx) {
            return None;
        }
        let now = Self::now(cx);
        cx.global_mut::<ShellTimer>().timing.begin_invoke(now)
    }

    /// A backend read `cmd` answered with `rows` rows: attributed when it took ≥ 50 ms.
    pub fn end_invoke(span: Option<InvokeSpan>, cmd: &str, rows: Option<usize>, cx: &mut App) {
        let Some(span) = span else { return };
        let now = Self::now(cx);
        if !cx.has_global::<ShellTimer>() {
            return;
        }
        let t = cx.global_mut::<ShellTimer>();
        t.timing.end_invoke(span, cmd, now);
        if let Some(rows) = rows {
            t.timing.note_invoke_rows(cmd, rows);
        }
    }

    /// The transition being (or last) recorded (tests).
    #[cfg(test)]
    pub(crate) fn current(cx: &App) -> Option<chairphoto_model::shell_timing::Transition> {
        cx.try_global::<ShellTimer>().and_then(|t| t.timing.current().cloned())
    }
}
