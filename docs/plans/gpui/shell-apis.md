# GPUI shell APIs in gpui-kit 0.7

Research ticket #96 (map #92). This document answers how ChairPhoto's shell does theming, fonts,
keymaps, the second window, pickers and confirms, the clipboard, window options and headless tests
on `gpui-kit =0.7.0`. That crate pins `gpui-pre =0.3.7`, not upstream `gpui` 0.2.2. See the Phase 0
spike, `3e13e1b` on `feature/gpui-spike`.

**Citations** use the form `crate@version:path:line`, with the path relative to that crate's root in
`~/.cargo/registry/src/index.crates.io-*/`. Once a section has named a crate, later citations in it
are shortened to `path:line`. Paths without a crate prefix are in this repository.

**How it was checked.** A throwaway scratch crate outside the repo used `gpui-kit =0.7.0`, the
spike's `Cargo.lock` and `test-support` as a dev-dependency:

- **Compile-verified**
  - `src/bin/q1.rs`, `q4_7.rs`
- **Run** (all passed)
  - Theme update: `tests/q1_theme.rs`
  - Keymap and capture ordering, a minimal view test: `tests/q8.rs`, 4 tests
  - Shared model across two windows, clipboard round trip: `tests/q4_6.rs`, 2 tests
  - Font loading on the real Linux platform: `src/bin/q2.rs`
- **Read-only**
  - Real Wayland/Hyprland behaviour (portal flags, decorations, clipboard ownership), which was not driven here

Each section has a **Verified how** line.

Summary:

| # | Answer in one line |
|---|---|
| 1 | Build a `gpui_component::ThemeConfig` from the tokens and apply it with `Theme::update(cx, \|t\| t.apply_config(&cfg))`. This also syncs the Base theme and redraws every window. The Omarchy watcher's `EventSink` feeds a channel that a foreground task applies. |
| 2 | Serve TTF/OTF files through an `AssetSource`, then call `cx.text_system().add_fonts(..)`. The default UI font is `Theme::font_family`. WOFF2 is silently ignored. |
| 3 | Overlays get their own `key_context` and focus themselves; the deepest context wins. A root `capture_action` or `intercept_keystrokes` is the escape hatch. A raw `capture_key_down` runs only when no binding matched. |
| 4 | Open the second window with `gpui_kit::open_window`; its view holds a shared `Entity<LoupeModel>`. `notify` redraws every window that reads it. Focus with `activate_window` and close with `remove_window`. Use `QuitMode::Explicit`. |
| 5 | `prompt_for_paths` / `prompt_for_new_path` return a `oneshot` that you await in `cx.spawn`. They use the xdg portal and offer no default folder or filters. Confirms use `AlertDialog` through a small oneshot adapter. `window.prompt` is the fallback. |
| 6 | `cx.write_to_clipboard(ClipboardItem::new_string(..))` / `cx.read_from_clipboard()?.text()`. On Wayland a write is silently dropped unless one of the app's windows has focus. |
| 7 | Set `app_id`, `titlebar.title`, `window_min_size` and `window_decorations` in `WindowOptions`. `TitleBar` draws window controls only under client-side decorations. |
| 8 | `#[gpui_kit::test]` with `TestAppContext::add_window_view` gives a `VisualTestContext` (`dispatch_action`, `simulate_keystrokes`). `gpui_kit::test::TestWindowExt` adds `find`, `click`, `press` and `input`. |

## 1. Theme from ChairPhoto's tokens, refreshed live

**Which Theme the widgets read.** There are two globals. Components read
`gpui_component::Theme` through `cx.theme()` (gpui-component@0.7.0:src/theme/mod.rs:44-53, :128).
`gpui_base::Theme` (gpui-base@0.7.0:src/theme.rs:17) is a *projection* of it: `base_theme()` builds it and
`Theme::sync_base` installs it (gpui-component@0.7.0:src/theme/mod.rs:453). Base paints only the scrollbar, resize
handles and plot motion from it. ChairPhoto writes only the component Theme, and always through
`Theme::update`, which reconciles `colors`/`tokens`, rebuilds the Base projection and calls
`cx.refresh_windows()` (mod.rs:268, :313-314). `Theme::global_mut` skips all of that (mod.rs:231).

**base::Root and the component Root.** `gpui_kit::open_window` wraps content in `base::Root`
(gpui-kit@0.7.0:src/lib.rs:153). gpui-component has no Root of its own. It registers `WindowState` as a
`RootPlugin` of `base::Root` during `init` (gpui-component@0.7.0:src/root.rs:21). That plugin sets
`rem = theme.font_size` (root.rs:435-436) and paints the root surface's `font_family`, background
(`tokens.background`) and text color (root.rs:440-447). So the kit's `open_window` is the right entry point,
and the theme's font and colors reach every window with nothing more to do. `gpui_kit::init` must run before
the first window (lib.rs:170-172). Note that `theme::init` switches the theme to **Light** at startup
(theme/mod.rs:36-41), so ChairPhoto has to apply its own theme straight after `init`.

**Build path.** Fill a `ThemeConfig` with hex strings and let `Theme::apply_config` derive the rest. That
covers about 130 `ThemeColor` fields (theme_color.rs:59-342), including hover and active states, button
variants, list, table, tab and slider colors. Each field left `None` falls back to a base color. For example,
`button_primary`/`slider_bar`/`selection` fall back to `primary`, and `ring` falls back to `blue`
(gpui-component@0.7.0:src/theme/schema.rs:682-1050, e.g. :981, :996, :977). `apply_config`
(schema.rs:1064-1110) also takes `font.family`/`font.size`/`radius`. Called inside `Theme::update`, it
installs the config and switches to its mode without a second reload (mod.rs:295-306).
`try_parse_color` accepts `#rgb/#rrggbb/#rrggbbaa` hex or named scale colors, **not `rgba()`**
(color.rs:693-697). `sel` and `scrim` therefore become `#E0A45824` and `#000000A8`.

```rust
use gpui_kit::component::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode};
fn config(t: &Tokens, mode: ThemeMode) -> ThemeConfig {
    let s = |v: &str| Some(SharedString::from(v.to_string()));
    let mut c = ThemeConfigColors::default();
    c.background = s(t.canvas);   c.foreground = s(t.txt);
    c.border = s(t.border);       c.input = s(t.border);
    c.muted = s(t.well);          c.muted_foreground = s(t.dim);
    c.secondary = s(t.elev);      c.popover = s(t.elev);   c.popover_foreground = s(t.txt);
    c.sidebar = s(t.panel);       c.title_bar = s(t.panel); c.status_bar = s(t.panel); c.tab_bar = s(t.panel);
    c.sidebar_border = s(t.line); c.title_bar_border = s(t.line); c.table_row_border = s(t.line);
    c.primary = s(t.accent);      c.primary_foreground = s(t.onaccent); c.ring = s(t.accent);
    c.selection = s(t.sel);       c.list_active = s(t.sel);
    c.success = s(t.ok);          c.success_foreground = s(t.onok);
    c.danger = s(t.danger);       c.warning = s(t.rating);  c.overlay = s(t.scrim);
    ThemeConfig { name: "ChairPhoto".into(), mode, font_family: Some("Instrument Sans".into()),
                  colors: c, ..Default::default() }
}
fn apply(t: &Tokens, mode: ThemeMode, cx: &mut App) {
    let cfg = Rc::new(config(t, mode));
    Theme::update(cx, |theme| theme.apply_config(&cfg)); // sync_base + refresh_windows
}
```

| ChairPhoto token | Theme field(s) it sets |
|---|---|
| `canvas` | `background` (root surface, list/table/tab/popover fallbacks) |
| `panel` | `sidebar`, `title_bar`, `status_bar`, `tab_bar` |
| `elev` | `secondary` (and through it `accent`, the hover color), `popover` |
| `well` | `muted` |
| `border` | `border`, `input` (the input's border) |
| `line` | `sidebar_border`, `title_bar_border`, `table_row_border` |
| `txt` | `foreground`, `popover_foreground` |
| `dim` | `muted_foreground` |
| `accent` / `onaccent` | `primary` / `primary_foreground`, and `ring`. `button_primary`, `slider_bar`, `link`, `caret` and `drag_border` derive from them. |
| `sel` | `selection`, `list_active` (alpha clamped to 0.2, schema.rs:1023-1050) |
| `ok` / `onok` | `success` / `success_foreground` |
| `danger` | `danger` (`danger_foreground` falls back to `primary_foreground`, so set it explicitly if needed) |
| `rating` | nearest is `warning`, but only by approximation |
| `scrim` | `overlay` (the modal backdrop) |
| `font-sans` | `Theme::font_family` / `ThemeConfig.font_family` |

**No equivalent** (these belong in a ChairPhoto-owned `Global` beside the Theme): `mute` (the third text
tier; gpui has two), `well` as a distinct inset surface (`muted` is only an approximation), `rating` (the star
color), `font-display` (Instrument Serif, used as a per-element `.font_family(..)`), the split between `line`
and `border`, which gpui treats as one `border` with a few per-component borders, and `onok` beyond success
buttons. `mode` maps to `ThemeMode::{Light,Dark}`. `color-scheme` and `data-appearance` have no equivalent
because there are no native web controls.

**Live Omarchy refresh.** `appearance::start_watcher(events: impl EventSink)` stays unchanged
(crates/core/src/appearance/mod.rs:347). The GPUI `EventSink` sends the settled `SystemThemeResult` into an async
channel. A task on the main thread applies it: port `mapPalette` (src/theme/omarchy.ts) to Rust, then
`apply(..)`, or `apply(STANDARD)` when `available: false`. `Theme::update` re-renders every window, so the
pop-out loupe follows too. Store the app-owned tokens in the same `cx.update` call so both are current in the
same frame.

```rust
let (tx, mut rx) = futures::channel::mpsc::unbounded::<Option<Tokens>>(); // tx -> EventSink impl
cx.spawn(async move |cx: &mut AsyncApp| {
    while let Some(next) = rx.next().await {
        let _ = cx.update(|cx| apply(next.as_ref().unwrap_or(&STANDARD), ThemeMode::Dark, cx));
    }
}).detach();
```

*Verified how:* **compile-verified** (`scratchpad/shellcheck/src/bin/q1.rs`, `cargo check --bin q1`; this
uses `futures` 0.3, which is already in gpui's dependency graph), and **run-verified headless**
(`tests/q1_theme.rs`, `#[gpui_kit::test]`). The test checks that after `Theme::update(apply_config)`,
`primary`, `button_primary` and `slider_bar` all equal `#E0A458`, the Base projection's
`tokens.colors.primary` matches, the appearance is Dark, and the `#E0A45824` selection alpha is 0.141. The
token-to-field *visual* fit is read-only: nothing was rendered to pixels.

## 2. Embedded fonts and the default UI font

Fonts are **not** loaded from the `AssetSource` automatically. `Application::with_assets`
(gpui-pre@0.3.7:src/app.rs:202) feeds only the SVG renderer and `cx.asset_source()` (app.rs:2077). The
application reads the bytes itself and calls `cx.text_system().add_fonts(Vec<Cow<'static,[u8]>>)`
(gpui-pre@0.3.7:src/text_system.rs:295), which also clears the font caches (text_system.rs:295-301). gpui's own
examples do the same (gpui-pre@0.3.7:examples/text.rs:380-387; examples/example_support/fonts.rs:7-34). On
Linux the backend is cosmic-text/fontdb (gpui-pre-linux@0.3.7:src/linux/platform.rs:171 →
gpui-pre-wgpu@0.3.7:src/cosmic_text_system.rs:329-335, `fontdb::Source::Binary`).

```rust
struct Assets; // rust-embed or include_bytes!, serving "fonts/*.ttf" (+ icons)
impl AssetSource for Assets { fn load(..) -> ..; fn list(..) -> .. }

fn load_fonts(cx: &App) -> Result<()> {
    let src = cx.asset_source();
    let mut fonts = Vec::new();
    for path in src.list("fonts/")? {
        if let Some(bytes) = src.load(&path)? { fonts.push(bytes); }
    }
    cx.text_system().add_fonts(fonts)
}

application().with_assets(Assets).run(|cx| {
    load_fonts(cx).expect("fonts");
    gpui_kit::init(cx);
    Theme::update(cx, |t| t.font_family = "Instrument Sans".into()); // default UI font
    // Display type: div().font_family("Instrument Serif")…
});
```

**Default UI font** = `Theme::font_family` (gpui-component@0.7.0:src/theme/mod.rs:147; default `.SystemUIFont`,
mod.rs:750). The Root plugin applies it to each window's root surface (root.rs:444). Components inherit it
from there, and `ThemeConfig.font_family` (`"font.family"`, schema.rs:50-51) sets it through a config. Changing
it inside `Theme::update` re-runs font resolution (mod.rs:307-311). The `.SystemUIFont` substitution in
`system_font.rs:32` applies only while the family is still `.SystemUIFont`, so an explicit family is used
as-is.

**Inter:** it is listed in `package.json` (`@fontsource/inter`), but no file under `src/` imports it (grep of
`src/` for `@fontsource/inter`/`"Inter"` found nothing). It does not need embedding unless a later ticket
finds a use.

*Verified how:* **run-verified on this machine (Linux/Wayland, real platform, no window)** with
`src/bin/q2.rs`. The test decompressed the TTFs from the `@fontsource` woff2 files with `woff2_decompress`,
served them through a custom `AssetSource` and loaded them with `add_fonts`.
`all_font_names()` then listed both "Instrument Sans" and "Instrument Serif", `resolve_font(font("Instrument
Sans"))` resolved to that family, and `Theme.font_family` read back "Instrument Sans". **Negative result:**
passing the raw `.woff2` to `add_fonts` returned `Ok(())`, but the family did **not** appear in
`all_font_names()`. The failure is silent. This matches the README's "use raw TTF/OTF … rather than … WOFF2"
(gpui-pre@0.3.7:examples/README.md:62-63). Weight matching (the 500/600/700 faces) was not verified:
`get_font_for_id` echoes the requested weight.

## 3. Actions, keymaps, and the capture-phase overlays

**What the React code does today.** `ProofSheet` (src/components/darkroom/ProofSheet.tsx:23-32) and
`DuelView` (src/components/darkroom/DuelView.tsx:52-63) add a `window` keydown listener with
`capture = true`, so the overlay sees Escape (and, in Duel, ←/→/↓) *before* the Darkroom's own
handlers, then `stopPropagation()` so the Darkroom does not also close / navigate.

**How GPUI dispatches a keystroke** (gpui-pre@0.3.7):

1. Keystroke interceptors (`App::intercept_keystrokes`, src/app.rs:2321-2325) run first; a
   `cx.stop_propagation()` there prevents action dispatch.
2. The keymap is matched against the focused element's context stack
   (`Window::dispatch_key_event`, src/window.rs:5802, match at :5878). Matching bindings are
   sorted **deepest context first, then later-registered first** (src/keymap.rs:188-190;
   depth from `KeyBindingContextPredicate::depth_of`, src/keymap/context.rs:260). A binding with
   **no context** gets depth = full stack length (src/keymap.rs:246-251), i.e. it ties with the
   deepest context and wins on load order.
3. Each matched binding's action is dispatched on the focused node (src/window.rs:5943-5957):
   global capture → element **capture** root→focus (:6316) → element **bubble** focus→root (:6339)
   → global bubble. Bubble action handlers **stop propagation by default** (:6349); call
   `cx.propagate()` to fall through to the *next matched binding*. Capture handlers do not stop
   unless they call `cx.stop_propagation()` (src/app.rs:2387).
4. Only if no binding consumed the key: raw `capture_key_down` root→focus, then `on_key_down`
   focus→root (`dispatch_key_down_up_event`, src/window.rs:6049-6078).

So GPUI's analogue of "overlay wins" is **context depth + focus**, not DOM capture. A raw
`capture_key_down` is *not* "before everything" — any matching action binding pre-empts it.
Element API: `key_context`, `track_focus`, `on_action`, `capture_action`, `on_key_down`,
`capture_key_down` (src/elements/div.rs:830, :788, :1100, :1086, :1127, :1139).
Predicates support `A > B` (descendant), `!`, `&&`, `||`, `key == value` / `key != value`
(src/keymap/context.rs:19-45), with `KeyContext::add` / `set` (:117, :126).

**Recommended port.** Each overlay is a focusable view with its own context; opening it
focuses it; its bindings live in that context, so they outrank the Darkroom's:

```rust
actions!(duel, [PickLeft, PickRight, Skip, CloseDuel]);
cx.bind_keys([
    KeyBinding::new("left",   PickLeft,  Some("Duel")),
    KeyBinding::new("right",  PickRight, Some("Duel")),
    KeyBinding::new("down",   Skip,      Some("Duel")),
    KeyBinding::new("escape", CloseDuel, Some("Duel")),
    KeyBinding::new("escape", CloseDarkroom, Some("Darkroom")), // shadowed while Duel is focused
]);
// DuelView::render
div().key_context("Duel").track_focus(&self.focus)
    .on_action(cx.listener(|this, _: &PickLeft, w, cx| this.pick(0, w, cx)))
    .on_action(cx.listener(|this, _: &CloseDuel, _, cx| cx.emit(DuelEvent::Close)))
// on open: self.focus.focus(window, cx); on close: restore the Darkroom's focus handle.
```

If an overlay must swallow keys regardless of where focus is (the true `window`-capture
semantics), put `capture_action` on the *shell root* and veto with `cx.stop_propagation()`
while the overlay is open, or use `cx.intercept_keystrokes` (returns a `Subscription` — hold it
for the overlay's lifetime; that is the listener-ownership rule in AGENTS.md).

**Text inputs.** gpui-base's Input binds `escape`, `up/down/left/right`, etc. in context
`"Input"` (gpui-base@0.7.0:src/input/base/state.rs:130, :176-180), deeper than any overlay, so a
focused Input gets arrows first. Its Escape handler consumes only when it has something to
dismiss and otherwise `cx.propagate()`s (state.rs:2064-2101), so Escape falls through to the
overlay's `CloseDuel`. `Dialog` binds `escape → Cancel` in `"Dialog"` (gpui-base@0.7.0:src/dialog.rs:18,91);
`Root` binds tab/shift-tab/copy in `"Root"` (gpui-base@0.7.0:src/root.rs:12-21).

**Verified how:** compiled and run — `tests/q8.rs` in the scratch crate, 4/4 pass
(`cargo test --test q8`). The Q3 tests assert the observed order:
- `deeper_context_binding_wins_and_capture_action_runs_first`: Darkroom(escape→CloseDarkroom,
  `capture_key_down`, `capture_action(CloseDuel)`) › Duel(escape→CloseDuel, focused). Log =
  `["darkroom:capture_action(CloseDuel)", "duel:CloseDuel"]` — CloseDarkroom never fires and
  **neither `capture_key_down` nor `on_key_down` runs**.
- `unbound_key_reaches_capture_key_down_before_bubble`: with no bindings, log =
  `["darkroom:capture_key_down", "duel:keydown(bubble)"]`.
- `focused_input_escape_propagates_to_overlay_binding`: gpui-component `Input` focused inside
  Duel; Escape still reaches `CloseDuel`.

## 4. A second window sharing entities (the pop-out loupe)

Entities live in the `App`, not in a window, so the loupe window can hold `Entity` handles the
main window created. Open it with `gpui_kit::open_window` (gpui-kit@0.7.0:src/lib.rs:144), which
wraps the content in the Base `Root` — every window needs that Root for gpui-component dialogs,
sheets, notifications and tooltips (gpui-component@0.7.0:src/root.rs:20-21 registers
`WindowState` as a Root plugin). It returns `(AnyWindowHandle, Entity<V>)`; the handle's root type
is `Root`, not `V`.

**Share a model, not a view.** An entity read during render is tracked per window, and
`cx.notify()` invalidates every window that currently tracks it (gpui-pre@0.3.7:src/app.rs:2794-2830,
`window_invalidators_by_entity` :812). But GPUI keeps only one "current window" per entity
(`current_window_by_entity`, src/app.rs:815, set at :1233, used by `_in` APIs at :1976), and
the `move_entity_between_windows` example (examples/move_entity_between_windows.rs:55-85) re-hosts
a view by *closing* the old window. So: one `LoupeModel` entity (photo id, edit JSON, module card),
one view entity per window that reads it. This replaces today's `loupe:photo` / `loupe:card` /
`loupe:ready` event handshake (src/modules/loupe.ts:1-10): the new window reads current state on
its first render, so there is no ready race.

```rust
struct LoupeView { model: Entity<LoupeModel>, focus: FocusHandle, _sub: Subscription }
impl LoupeView {
    fn new(model: Entity<LoupeModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _sub = cx.observe(&model, |_, _, cx| cx.notify()); // optional: render already tracks reads
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self { model, focus, _sub }
    }
}

// In the main shell (Context<Shell>): open, or focus if already open.
fn open_loupe(&mut self, cx: &mut Context<Self>) {
    if let Some(h) = self.loupe {
        if h.update(cx, |_, window, _| window.activate_window()).is_ok() { return; }
        self.loupe = None; // user closed it: update() on a closed window returns Err
    }
    let (model, this) = (self.model.clone(), cx.weak_entity());
    let options = WindowOptions {
        app_id: Some("chairphoto".into()),
        titlebar: Some(TitlebarOptions { title: Some("ChairPhoto — Loupe".into()), ..Default::default() }),
        ..Default::default()
    };
    cx.defer(move |cx| {
        if let Ok((handle, _)) = gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| LoupeView::new(model, window, cx))
        }) {
            this.update(cx, |this, _| this.loupe = Some(handle)).ok();
        }
    });
}
// Close: handle.update(cx, |_, window, _| window.remove_window()).ok();
```

- **Focus:** `Window::activate_window` (src/window.rs:6413) raises it; `FocusHandle::focus` sets
  keyboard focus within it. `App::active_window` (src/app.rs:1343) reports the focused window.
- **Closing:** `Window::remove_window` (src/window.rs:2280); `App::on_window_closed`
  (src/app.rs:2493) fires after a window is gone; `Window::on_window_should_close`
  (src/window.rs:6658) can veto a close (unsaved edits).
- **Quit policy:** the default on Linux is `QuitMode::LastWindowClosed` (src/app.rs:389-397), so
  closing the main window while the loupe is open would keep the app alive. Set
  `cx.set_quit_mode(QuitMode::Explicit)` (src/app.rs:1754) and `cx.quit()` from
  `on_window_closed` when the main window's id closes.
- `cx.defer` keeps `open_window` out of the caller's window update. `open_window` draws the new
  window synchronously (src/app.rs:1350-1383).

**Verified how:** compile-verified (`src/bin/q4_7.rs` in the scratch crate, `cargo check`, no
warnings). Also run: `tests/q4_6.rs::shared_model_invalidates_both_windows` opens two windows
through `gpui_kit::open_window` on one model. An update without `notify` redraws neither window.
`notify` redraws both. After `remove_window`, the closed handle's `update` returns `Err` and the
other window still works. `cargo test --test q4_6`: 2 passed. Real-compositor focus/raise on
Hyprland was not exercised.

## 5. File/folder pickers and confirm dialogs

**Pickers** return a `futures::channel::oneshot::Receiver`. Await it in `cx.spawn`:

- `App::prompt_for_paths(PathPromptOptions { files, directories, multiple, prompt })
  -> Receiver<Result<Option<Vec<PathBuf>>>>` (gpui-pre@0.3.7:src/app.rs:1690; options at
  src/platform.rs:2485-2494).
- `App::prompt_for_new_path(directory: &Path, suggested_name: Option<&str>)
  -> Receiver<Result<Option<PathBuf>>>` (src/app.rs:1703).
- On Linux both go through the xdg-desktop-portal via `ashpd` 0.13, modal to the window
  (gpui-pre-linux@0.3.7:src/linux/platform.rs:456-513 `OpenFileRequest`, :516-560
  `SaveFileRequest`). A missing portal is `Ok(Err(..))`. A cancel is `Ok(Ok(None))`.

```rust
let rx = cx.prompt_for_paths(PathPromptOptions {
    files: false, directories: true, multiple: false, prompt: Some("Choose library".into()),
});
cx.spawn(async move |this, cx| {
    let picked = match rx.await {
        Ok(Ok(paths)) => paths,           // None = user cancelled
        Ok(Err(e)) => { log::warn!("{e}"); None } // portal missing/failed
        Err(_) => None,                    // sender dropped
    };
    this.update(cx, |this, cx| { /* … */ cx.notify(); }).ok();
}).detach();
```

**Gaps vs today's `pickFolder` / `pickFile` / `pickBundleFile`** (src/modules/api.ts:328-364):
`PathPromptOptions` has **no `defaultPath` / current folder and no file-type filters**. Only
`prompt_for_new_path` takes a starting directory. Options: validate the `.chairphoto` extension after
the pick, or call `ashpd::desktop::file_chooser::OpenFileRequest` directly (`current_folder`,
`filters`), pinned to the same `ashpd` version as `gpui-pre-linux` (0.13.13 in the lockfile).

**Confirm, GPUI prompt:** `Window::prompt(level, message, detail, answers, cx)
-> oneshot::Receiver<usize>` gives the index of the clicked answer (src/window.rs:6455-6491;
example examples/window.rs:274-288). On Wayland there is no native dialog
(gpui-pre-linux@0.3.7:src/linux/wayland/window.rs:1779-1787 returns `None`), so GPUI renders its own
fallback prompt (`fallback_prompt_renderer`, gpui-pre@0.3.7:src/window/prompts.rs:73) unless
`App::set_prompt_builder` (src/app.rs:2730) installs a custom renderer. Prompts are not re-entrant
(src/window.rs:6467-6469 `unreachable!`).

```rust
let answer = window.prompt(PromptLevel::Warning, "Delete album?", Some("Photos are not deleted."),
    &[PromptButton::ok("Delete"), PromptButton::cancel("Cancel")], cx);
cx.spawn(async move |this, cx| { let delete = answer.await == Ok(0); /* … */ }).detach();
```

**Confirm, gpui-component `AlertDialog`:** `window.open_alert_dialog(cx, |alert, window, cx| …)`
(gpui-component@0.7.0:src/window_ext.rs:52, :134). The builder is `Fn`, not `FnOnce`, and results
arrive by callback, not as a future: `.confirm()` adds Cancel (src/dialog/alert_dialog.rs:95);
`.on_ok` / `.on_cancel` return `bool` = "close the dialog" (:276, :287); plus
`.ok_text`/`.ok_variant(ButtonVariant::Danger)`/`.cancel_text` (:209-227). There is no
`.warning()`. A oneshot adapter gives the same async shape as `window.prompt`:

```rust
fn confirm(window: &mut Window, cx: &mut App, title: &'static str, body: &'static str)
    -> oneshot::Receiver<bool> {
    let (tx, rx) = oneshot::channel();
    let tx = Rc::new(RefCell::new(Some(tx)));
    window.open_alert_dialog(cx, move |alert, _, _| {
        let (ok, cancel) = (tx.clone(), tx.clone());
        alert.title(title).description(body).confirm()
            .ok_text("Delete").ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, _, _| { ok.borrow_mut().take().map(|t| t.send(true)); true })
            .on_cancel(move |_, _, _| { cancel.borrow_mut().take().map(|t| t.send(false)); true })
    });
    rx // Err(Canceled) if the dialog is dismissed another way (Escape / close_dialog): treat as false
}
```

Recommendation: use the component `AlertDialog` for all in-app confirms (themed, testable with
`TestWindowExt`, one look). Keep `window.prompt` only where no Root is present.

**Verified how:** compile-verified (`src/bin/q4_7.rs`: `prompt_for_paths`, `prompt_for_new_path`,
`window.prompt`, and the `AlertDialog` → oneshot adapter). The portal folder picker and
`open_alert_dialog` were exercised in the Phase 0 spike (3e13e1b). `prompt_for_new_path`,
the Wayland fallback prompt and the adapter's dismiss path were not run.

## 6. Clipboard

`cx.write_to_clipboard(ClipboardItem::new_string(s))` and
`cx.read_from_clipboard().and_then(|i| i.text())` (gpui-pre@0.3.7:src/app.rs:1549, :1521;
`ClipboardItem::new_string` / `text` src/platform.rs:2704, :2738). `read_from_clipboard_async`
(src/app.rs:1532) exists for platforms with async clipboards. On Linux there is also the primary
selection (`read_from_primary` / `write_to_primary`, src/app.rs:1556, :1563). The input example
uses the same calls (examples/input.rs:155-165).

```rust
cx.write_to_clipboard(ClipboardItem::new_string(bundle.join(" "))); // ExportPanel "Copy"
```

Wayland caveat: a write takes ownership only while one of the app's windows has pointer or
keyboard focus **and** a press serial exists. Otherwise it logs a warning and silently does nothing
(gpui-pre-linux@0.3.7:src/linux/wayland/client.rs:1325-1349). The call returns `()`, so today's
"Couldn't copy" error branch (src/components/ExportPanel.tsx:95-100) has no failure signal to
report. Only call it from a click/key handler.

**Verified how:** compile-verified (`q4_7.rs`). Round trip run on the test platform:
`tests/q4_6.rs::clipboard_roundtrip_on_test_platform` passes. A real Wayland write/read was not
exercised.

## 7. Window app_id, title, min size, decorations and TitleBar

All of these go on `WindowOptions` (gpui-pre@0.3.7:src/platform.rs:2174-2243):
`app_id` (:2229), `titlebar: Some(TitlebarOptions { title, .. })` (:2181, :2375-2385),
`window_min_size` (:2232), `window_decorations: Option<WindowDecorations>` (:2236),
`inactive_frame_interval` (:2213, default 33.3 ms at :2359). At runtime:
`Window::set_window_title` / `set_app_id` (src/window.rs:2876, :2888),
`request_decorations` (:2379), `window_decorations()` (:2856). On Wayland, title/app_id map to
`xdg_toplevel.set_title/set_app_id` and min size to `set_min_size`
(gpui-pre-linux@0.3.7:src/linux/wayland/window.rs:278-279, :585-591).

```rust
let options = WindowOptions {
    app_id: Some("chairphoto".into()),          // must match the .desktop file / Hyprland rules
    titlebar: Some(TitlebarOptions { title: Some("ChairPhoto".into()), ..Default::default() }),
    window_min_size: Some(size(px(960.), px(600.))),
    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1400.), px(900.)), cx))),
    window_background: WindowBackgroundAppearance::Opaque,
    ..Default::default()                        // or ..TitleBar::window_options() for a custom bar
};
```

**Decorations.** `None` means GPUI requests **server-side** decorations
(gpui-pre@0.3.7:src/window.rs:1597). On Wayland without `xdg-decoration`, GPUI falls back to client
decorations (gpui-pre-linux@0.3.7:src/linux/wayland/window.rs:2094-2110).
gpui-component's `TitleBar` (gpui-component@0.7.0:src/title_bar.rs:42-57, height 34 px :15) is a
draggable bar you put children in. `TitleBar::window_options()` (:81-91) sets a transparent
titlebar and `app_owns_titlebar_drag` (macOS-only effect). On Linux it draws min/max/close only
when the window is actually client-decorated, and only the controls the compositor supports
(:258-283). `on_close_window` overrides its close button (Linux only, :95-103). For client
decorations, wrap content in `window_border()` (src/window_border.rs:25) so there are resize
edges and a shadow inset.

Recommendation for ChairPhoto on Hyprland: keep server-side decorations (today's Tauri window
uses the default decorations, src-tauri/tauri.conf.json:13-22). Render `TitleBar` as the in-app
bar that holds the command pill and menus. Under SSD it shows no window controls, which matches
tiling.

**Verified how:** compile-verified (`q4_7.rs`: `app_id`, `window_min_size`,
`WindowDecorations::Client`, `..TitleBar::window_options()`, `TitleBar::new().on_close_window`,
`set_window_title`). `app_id` was also exercised in the spike. Read-only: what Hyprland answers to
the decoration request, and how TitleBar looks under client decorations.

## 8. Headless view tests

Two layers, both usable through gpui-kit with `features = ["test-support"]` on the
dev-dependency:

- **GPUI's own**: `#[gpui_kit::test]` gives a `&mut TestAppContext` (headless `TestPlatform`,
  deterministic executor). `cx.add_window_view(|window, cx| V)` returns
  `(Entity<V>, &mut VisualTestContext)` (gpui-pre@0.3.7:src/app/test_context.rs:344-370);
  `VisualTestContext::dispatch_action` / `simulate_keystrokes("cmd-k left")` (:859, :883) run
  the real keymap and then `run_until_parked`.
- **gpui-kit `test` module** (gpui-kit@0.7.0:src/test.rs): `TestWindowExt` on `Window` —
  `find(id) -> ElementSnapshot`, `click`, `press("escape")`, `input("text")`, `scroll`,
  `drag_to`, `render_frame` (:25-50); `TestAppContextExt::wait_for` for async UI (:412-455).
  Elements are found by `ElementId`; custom elements opt in with `.test_support()` before
  `.track_focus()` (TESTING.md:50-54, :67-79). Its fixtures open windows through the production
  `gpui_kit::open_window` (tests/common/mod.rs:8-31), so the Base `Root` is present — needed for
  gpui-component widgets and dialogs. Worked example: tests/ui.rs:57-97.

Gotcha: in test modules import items explicitly, not `use gpui_kit::*` — with test-support
the glob brings in GPUI's `test` attribute and shadows `#[test]`
(gpui-kit@0.7.0:tests/test_macro.rs:1-6, src/lib.rs:90-92).

Minimal example (passes):

```rust
use gpui_kit::prelude::*;
use gpui_kit::{actions, div, AppContext, Context, FocusHandle, KeyBinding, TestAppContext, Window};

actions!(q8, [Next]);
struct Counter { n: i32, focus: FocusHandle }
impl Render for Counter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().key_context("Counter").track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Next, _, cx| { this.n += 1; cx.notify(); }))
            .child(format!("{}", self.n))
    }
}

#[gpui_kit::test]
fn renders_and_dispatches_an_action(cx: &mut TestAppContext) {
    cx.update(|cx| cx.bind_keys([KeyBinding::new("right", Next, Some("Counter"))]));
    let (view, cx) = cx.add_window_view(|_, cx| Counter { n: 0, focus: cx.focus_handle() });
    cx.update(|window, cx| view.read(cx).focus.clone().focus(window, cx));
    cx.run_until_parked();
    cx.dispatch_action(Next);        // straight to the focused node
    cx.simulate_keystrokes("right"); // through the keymap
    assert_eq!(view.read_with(cx, |v, _| v.n), 2);
}
```

For views with gpui-component widgets, call `cx.update(gpui_kit::init)` first and open via
`gpui_kit::open_window` inside `cx.update(...)`, then drive with
`cx.update_window(handle, |_, window, cx| { window.render_frame(cx); window.click("field", cx); window.press("escape", cx); })`
(as in the Q3 tests above).

**Verified how:** compiled and run — scratch crate `tests/q8.rs`, `cargo test --test q8`:
`test result: ok. 4 passed; 0 failed` (0.03 s). Ran on this machine (Wayland session present);
the test platform is headless by construction, but a display-less CI run was not tried.

## Open risks

1. **Fonts: WOFF2 fails silently.** The `@fontsource` packages ship only woff/woff2. Vendor the OFL
   TTF/OTF files as static 400/500/600/700 instances plus Serif, and assert at startup that
   `all_font_names()` contains each family. Picking the right face for each weight, and
   variable-font axes, are untested.
2. **Theme fidelity.** `apply_config` derives the hover, active and button colors. They may not
   match the React look, so a screenshot pass is needed. `mute`, `well`, `rating`, `font-display`
   and the `line`/`border` split need an app-owned `Global` updated in the same `cx.update` as
   `Theme::update`. `theme::init` forces Light at startup, so apply ChairPhoto's theme before
   opening the first window.
3. **Context-less key bindings tie with the deepest context** and win on load order. Bind every
   app-wide key under a root context such as `"ChairPhoto"`, never `None`.
4. **Overlays win only while focused.** Each overlay must focus itself on open and restore the
   previous focus on close. A focused `Input` owns the arrow keys. `intercept_keystrokes` is
   app-global and covers the loupe window too.
5. **Pickers have no default folder and no filters.** Today's `pickFolder(defaultPath)` and the
   `.chairphoto` filter need either post-pick validation or a direct `ashpd` call pinned to
   gpui-pre-linux's version (0.13.13). The portal is opened with `modal(true)` and a window
   identifier, which was not checked on Hyprland beyond the spike's folder pick.
6. **Clipboard writes can be dropped silently** on Wayland when no app window has focus or there
   is no press serial. The call returns `()`, so no copy error can be shown.
7. **Quit policy.** On Linux the default `QuitMode::LastWindowClosed` keeps the app running when
   the main window closes while the loupe is open. Use `QuitMode::Explicit` together with
   `on_window_closed`.
8. **Decorations on Hyprland are unverified.** `None` requests server-side decorations. What
   Hyprland grants, and how `TitleBar` and `window_border()` look under client-side decorations,
   needs a real run with the `chairphoto-app` skill.
9. **`window.prompt` is not re-entrant** (`unreachable!`), and on Wayland it always uses GPUI's
   in-window fallback. Prefer `AlertDialog` everywhere.
10. **Test coverage limits.** `ElementSnapshot` cannot inspect pixels, and no test was run on a
    machine without a display. In the windowless font check, `cx.quit()` did not exit the process
    (the cause is not investigated).
