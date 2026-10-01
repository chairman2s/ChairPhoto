//! ChairPhoto's look in GPUI: how a palette reaches the widgets. The token contract, the
//! ChairPhoto Standard palette and the Omarchy mapping are `chairphoto_model::theme` (the
//! reviewed ports of `src/theme/{tokens,standard,omarchy}.ts`); this module only turns their
//! CSS colour strings into the hex gpui parses.
//!
//! A palette becomes a `gpui_component::ThemeConfig` ([`theme_config`]) applied with
//! `Theme::update(.. apply_config ..)`, which derives the hover/active/button colours, rebuilds
//! gpui-base's projection and redraws every window (docs/plans/gpui/shell-apis.md § 1). The
//! tokens gpui has no field for — `mute`, `well`, `rating`, `line` as distinct from `border`,
//! `font-display` — live in the [`Palette`] global, set in the same `cx` update so both are
//! current in the same frame.
//!
//! **Appearance mode** (`src/theme/{prefs,controller}.ts`; Preferences → Appearance, #113).
//! "Follow Omarchy" — the product default — paints the system theme and tracks its changes,
//! falling back to Standard when no usable Omarchy theme exists; "ChairPhoto Standard" paints
//! Standard whatever the system does. The mode is a per-machine preference
//! ([`MachinePrefs`] key [`MODE_PREF`], React's `localStorage` key), not a catalog setting.
//! The [`Appearance`] global holds the mode and the latest system-theme answer, so a mode
//! change repaints at once and Preferences can say what is being followed. Every window
//! shares the one theme, so a pop-out loupe follows a mode change too (React's could not).

use crate::machine_prefs::MachinePrefs;
use chairphoto_core::appearance::{OmarchyMode, SystemThemeResult};
use chairphoto_model::theme::{omarchy, standard as model_standard, tokens::ThemeTokens};
use gpui_kit::component::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode};
use gpui_kit::{App, Global, SharedString};
use std::rc::Rc;
use std::sync::Arc;

/// The appearance mode's per-machine key (`STORAGE_KEY` in `src/theme/prefs.ts`). Not a
/// catalog setting, so not in a catalog settings namespace.
pub const MODE_PREF: &str = "appearance.mode";

/// Which palette source paints the app (`AppearanceMode` in `src/theme/tokens.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppearanceMode {
    #[default]
    FollowOmarchy,
    Standard,
}

impl AppearanceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AppearanceMode::FollowOmarchy => "follow-omarchy",
            AppearanceMode::Standard => "standard",
        }
    }

    /// A stored value; absent or unrecognised is the default, as `loadAppearanceMode`.
    pub fn parse(stored: Option<&str>) -> Self {
        match stored {
            Some("standard") => AppearanceMode::Standard,
            _ => AppearanceMode::FollowOmarchy,
        }
    }
}

/// Reads the current system theme: `appearance::read_current_theme` in the app; tests
/// install a fake so nothing reads the machine's Omarchy state. Blocking (two small files).
pub type ThemeReader = Arc<dyn Fn() -> SystemThemeResult + Send + Sync>;

/// The appearance mode and the latest system-theme answer (startup's read, then every
/// `appearance:theme_changed` and re-read). A GPUI global, installed by [`init_appearance`].
#[derive(Clone)]
pub struct Appearance {
    pub mode: AppearanceMode,
    pub system: SystemThemeResult,
    reader: ThemeReader,
}

impl Global for Appearance {}

/// Install the appearance (the mode from [`MachinePrefs`], the startup system theme) and
/// paint it.
pub fn init_appearance(system: &SystemThemeResult, reader: ThemeReader, cx: &mut App) {
    let mode = AppearanceMode::parse(MachinePrefs::read(cx, MODE_PREF).as_deref());
    cx.set_global(Appearance { mode, system: system.clone(), reader });
    paint(cx);
}

/// The current mode (the default before [`init_appearance`]).
pub fn mode(cx: &App) -> AppearanceMode {
    cx.try_global::<Appearance>().map(|a| a.mode).unwrap_or_default()
}

/// A system-theme answer arrived (`appearance:theme_changed`, a re-read): remember it, and
/// paint it when following. Standard ignores it.
pub fn on_system_theme(result: &SystemThemeResult, cx: &mut App) {
    match cx.try_global::<Appearance>().is_some() {
        true => {
            cx.global_mut::<Appearance>().system = result.clone();
            paint(cx);
        }
        // Not initialised (a bare test app): behave as follow mode.
        false => apply_system_theme(result, cx),
    }
}

/// Preferences → Appearance: switch the mode, persist it per machine and repaint — and, when
/// switching to Follow, re-read the system theme off the UI thread rather than trust the
/// last answer (`refreshAppearance`).
pub fn set_mode(next: AppearanceMode, cx: &mut App) {
    if !cx.has_global::<Appearance>() {
        return;
    }
    cx.global_mut::<Appearance>().mode = next;
    MachinePrefs::set(cx, MODE_PREF, next.as_str());
    paint(cx);
    if next == AppearanceMode::FollowOmarchy {
        reread_system_theme(cx);
    }
}

/// Re-read the system theme off the UI thread and hand it to [`on_system_theme`].
pub fn reread_system_theme(cx: &mut App) {
    let Some(reader) = cx.try_global::<Appearance>().map(|a| a.reader.clone()) else { return };
    let read = cx.background_executor().spawn(async move { reader() });
    cx.spawn(async move |cx| {
        let result = read.await;
        cx.update(|cx| on_system_theme(&result, cx));
    })
    .detach();
}

/// Paint what the mode says: the system theme when following, else Standard.
fn paint(cx: &mut App) {
    let Some(a) = cx.try_global::<Appearance>() else { return };
    match a.mode {
        AppearanceMode::FollowOmarchy => {
            let system = a.system.clone();
            apply_system_theme(&system, cx);
        }
        AppearanceMode::Standard => apply(standard(), STANDARD_MODE, None, cx),
    }
}

/// Preferences → Appearance's status line under Follow Omarchy (`appearanceStatusLine`).
pub fn status_line(result: &SystemThemeResult) -> String {
    if result.available {
        let mode = match result.palette.as_ref().map(|p| &p.mode) {
            Some(OmarchyMode::Dark) => "dark",
            Some(OmarchyMode::Light) => "light",
            None => "",
        };
        return format!("Following Omarchy · {} · {mode}", result.theme_name.as_deref().unwrap_or("unnamed theme"));
    }
    "Omarchy not detected — ChairPhoto Standard is in use. This is normal without Omarchy; nothing is missing.".into()
}

/// The UI font family (`font-sans`), embedded by [`crate::assets`].
pub const FONT_SANS: &str = "Instrument Sans";
/// The display family (`font-display`) for serif headings — per element, `.font_family(..)`.
pub const FONT_DISPLAY: &str = "Instrument Serif";

/// One colour per token of the contract (`TOKEN_NAMES` in `src/theme/tokens.ts`), as hex.
/// `#rrggbbaa` carries alpha: gpui-component's colour parser takes hex, not `rgba()`.
/// The two font tokens are the family constants above; they never theme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub canvas: String,
    pub panel: String,
    pub elev: String,
    pub well: String,
    pub border: String,
    pub line: String,
    pub txt: String,
    pub dim: String,
    pub mute: String,
    pub accent: String,
    pub onaccent: String,
    pub sel: String,
    pub ok: String,
    pub onok: String,
    pub danger: String,
    pub rating: String,
    pub scrim: String,
}

/// Dark or light — `ThemeMode` for gpui-component.
pub type Mode = ThemeMode;

impl Tokens {
    /// The model's tokens with every colour as hex gpui can parse (see [`css_hex`]).
    pub fn from_model(t: &ThemeTokens) -> Tokens {
        Tokens {
            canvas: css_hex(&t.canvas),
            panel: css_hex(&t.panel),
            elev: css_hex(&t.elev),
            well: css_hex(&t.well),
            border: css_hex(&t.border),
            line: css_hex(&t.line),
            txt: css_hex(&t.txt),
            dim: css_hex(&t.dim),
            mute: css_hex(&t.mute),
            accent: css_hex(&t.accent),
            onaccent: css_hex(&t.onaccent),
            sel: css_hex(&t.sel),
            ok: css_hex(&t.ok),
            onok: css_hex(&t.onok),
            danger: css_hex(&t.danger),
            rating: css_hex(&t.rating),
            scrim: css_hex(&t.scrim),
        }
    }
}

/// A token's CSS colour as hex: `#rgb`/`#rrggbb`/`#rrggbbaa` pass through, and
/// `rgba(r, g, b, a)` becomes `#RRGGBBAA` with the alpha rounded to the nearest of 255 steps
/// (Standard's `sel` alpha 0.14 → `0x24`, its `scrim` alpha 0.66 → `0xA8`). Anything else is
/// returned unchanged, and gpui's parser rejects it visibly rather than guessing.
pub fn css_hex(v: &str) -> String {
    let v = v.trim();
    let Some(inner) = v.strip_prefix("rgba(").and_then(|r| r.strip_suffix(')')) else {
        return v.to_string();
    };
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    let [r, g, b, a] = parts.as_slice() else { return v.to_string() };
    let (Ok(r), Ok(g), Ok(b), Ok(a)) = (r.parse::<u8>(), g.parse::<u8>(), b.parse::<u8>(), a.parse::<f32>()) else {
        return v.to_string();
    };
    let a = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
}

/// ChairPhoto Standard, the app-owned warm dark palette (`chairphoto_model::theme::standard`).
pub fn standard() -> Tokens {
    Tokens::from_model(&model_standard::standard())
}

/// ChairPhoto Standard is dark.
pub const STANDARD_MODE: Mode = ThemeMode::Dark;

/// gpui-component's mode for a palette's mode.
fn gpui_mode(mode: &OmarchyMode) -> Mode {
    match mode {
        OmarchyMode::Dark => ThemeMode::Dark,
        OmarchyMode::Light => ThemeMode::Light,
    }
}

/// The active palette in full — including the tokens gpui's `Theme` has no field for — plus
/// where it came from. Read it with `cx.global::<Palette>()`.
#[derive(Debug, Clone)]
pub struct Palette {
    pub tokens: Tokens,
    pub mode: Mode,
    /// The Omarchy theme's name while following one; `None` on ChairPhoto Standard.
    pub omarchy_theme: Option<String>,
}

impl Global for Palette {}

/// The `ThemeConfig` for a palette: the token → field table in shell-apis.md § 1. Every field
/// left `None` falls back to a base colour inside `apply_config`.
pub fn theme_config(t: &Tokens, mode: Mode) -> ThemeConfig {
    let s = |v: &str| Some(SharedString::from(v.to_string()));
    let mut c = ThemeConfigColors::default();
    c.background = s(&t.canvas);
    c.foreground = s(&t.txt);
    c.border = s(&t.border);
    c.input = s(&t.border);
    c.muted = s(&t.well);
    c.muted_foreground = s(&t.dim);
    c.secondary = s(&t.elev);
    c.popover = s(&t.elev);
    c.popover_foreground = s(&t.txt);
    c.sidebar = s(&t.panel);
    c.title_bar = s(&t.panel);
    c.status_bar = s(&t.panel);
    c.tab_bar = s(&t.panel);
    c.sidebar_border = s(&t.line);
    c.title_bar_border = s(&t.line);
    c.table_row_border = s(&t.line);
    c.primary = s(&t.accent);
    c.primary_foreground = s(&t.onaccent);
    c.ring = s(&t.accent);
    c.selection = s(&t.sel);
    c.list_active = s(&t.sel);
    c.success = s(&t.ok);
    c.success_foreground = s(&t.onok);
    c.danger = s(&t.danger);
    // `danger_foreground` would otherwise fall back to `primary_foreground` (the accent's).
    c.danger_foreground = s(&t.txt);
    c.warning = s(&t.rating);
    c.overlay = s(&t.scrim);
    ThemeConfig {
        name: "ChairPhoto".into(),
        mode,
        font_family: Some(FONT_SANS.into()),
        colors: c,
        ..Default::default()
    }
}

/// Make `tokens` the app's look: the component `Theme` (and through it every window) and the
/// [`Palette`] global, in one update.
pub fn apply(tokens: Tokens, mode: Mode, omarchy_theme: Option<String>, cx: &mut App) {
    let cfg = Rc::new(theme_config(&tokens, mode));
    Theme::update(cx, |theme| theme.apply_config(&cfg));
    cx.set_global(Palette { tokens, mode, omarchy_theme });
}

/// The palette for a system-theme answer: the mapped Omarchy palette when one is available,
/// else ChairPhoto Standard (follow mode's fallback, `src/theme/controller.ts`).
pub fn palette_for(result: &SystemThemeResult) -> (Tokens, Mode, Option<String>) {
    match (&result.palette, result.available) {
        (Some(palette), true) => {
            let mapped = omarchy::map_palette(palette);
            (Tokens::from_model(&mapped.tokens), gpui_mode(&mapped.mode), result.theme_name.clone())
        }
        _ => (standard(), STANDARD_MODE, None),
    }
}

/// Apply a system-theme answer — startup's read, or an `appearance:theme_changed` event.
pub fn apply_system_theme(result: &SystemThemeResult, cx: &mut App) {
    let (tokens, mode, name) = palette_for(result);
    apply(tokens, mode, name, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::component::{ActiveTheme as _, Colorize as _};
    use gpui_kit::{Hsla, TestAppContext};

    fn hsla(hex: &str) -> Hsla {
        Hsla::parse_hex(hex).unwrap()
    }

    /// Standard's tokens reach the component theme through `apply_config`, including the
    /// fields it derives (button_primary, slider_bar from primary) and the alpha-carrying
    /// selection, and the tokens gpui has no field for are in the Palette global.
    #[gpui_kit::test]
    fn standard_builds_the_component_theme_from_the_tokens(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            apply(standard(), STANDARD_MODE, None, cx);
        });
        cx.update(|cx| {
            let theme = cx.theme();
            assert_eq!(theme.mode, ThemeMode::Dark);
            assert_eq!(theme.font_family.as_ref(), FONT_SANS);
            assert_eq!(theme.background, hsla("#14120F"));
            assert_eq!(theme.foreground, hsla("#EFE9E0"));
            assert_eq!(theme.primary, hsla("#E0A458"));
            assert_eq!(theme.button_primary, hsla("#E0A458"));
            assert_eq!(theme.slider_bar, hsla("#E0A458"));
            assert_eq!(theme.sidebar, hsla("#1B1815"));
            assert!((theme.selection.a - 0x24 as f32 / 255.0).abs() < 0.01, "{:?}", theme.selection);
            let palette = cx.global::<Palette>();
            assert_eq!(palette.tokens.mute, "#6F675C");
            assert_eq!(palette.tokens.rating, "#FFD700");
            assert_eq!(palette.omarchy_theme, None);
        });
    }

    fn omarchy(name: &str, background: &str) -> SystemThemeResult {
        let toml = format!(
            "mode = \"dark\"\naccent = \"#7aa2f7\"\nselection = \"#33467c\"\nmuted = \"#565f89\"\n\
             background = \"{background}\"\nforeground = \"#c0caf5\"\n"
        );
        let palette = chairphoto_core::appearance::parse_palette(&toml).unwrap();
        SystemThemeResult { available: true, theme_name: Some(name.into()), palette: Some(palette) }
    }

    fn install(stored_mode: Option<&str>, system: &SystemThemeResult, reread: SystemThemeResult, cx: &mut App) {
        gpui_kit::init(cx);
        cx.set_global(crate::storage::Runner::manual());
        let mut prefs = MachinePrefs::in_memory();
        if let Some(m) = stored_mode {
            cx.set_global(prefs.clone());
            MachinePrefs::set(cx, MODE_PREF, m);
            prefs = cx.global::<MachinePrefs>().clone();
        }
        cx.set_global(prefs);
        init_appearance(system, Arc::new(move || reread.clone()), cx);
    }

    /// Standard holds whatever the system does: startup with an Omarchy theme available, and
    /// a later `theme_changed`, both leave Standard painted — but the answer is remembered,
    /// so switching to Follow paints the newest theme at once, persists the mode per machine,
    /// and the re-read it starts lands too.
    #[gpui_kit::test]
    fn standard_ignores_the_system_theme_and_follow_paints_it(cx: &mut TestAppContext) {
        let first = omarchy("tokyo-night", "#1a1b26");
        let reread = omarchy("re-read", "#101010");
        cx.update(|cx| install(Some("standard"), &first, reread, cx));
        cx.update(|cx| {
            assert_eq!(mode(cx), AppearanceMode::Standard);
            assert_eq!(cx.global::<Palette>().tokens, standard(), "Standard at startup");
            on_system_theme(&omarchy("nord", "#2e3440"), cx);
            assert_eq!(cx.global::<Palette>().tokens, standard(), "a theme change does not repaint Standard");
            assert_eq!(cx.global::<Appearance>().system.theme_name.as_deref(), Some("nord"));

            set_mode(AppearanceMode::FollowOmarchy, cx);
            assert_eq!(cx.global::<Palette>().omarchy_theme.as_deref(), Some("nord"), "the remembered answer, at once");
            assert_eq!(MachinePrefs::read(cx, MODE_PREF).as_deref(), Some("follow-omarchy"));
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(cx.global::<Palette>().omarchy_theme.as_deref(), Some("re-read"), "the re-read landed");
            on_system_theme(&omarchy("nord", "#2e3440"), cx);
            assert_eq!(cx.global::<Palette>().omarchy_theme.as_deref(), Some("nord"), "following tracks changes");
            set_mode(AppearanceMode::Standard, cx);
            assert_eq!(cx.global::<Palette>().tokens, standard());
            assert_eq!(MachinePrefs::read(cx, MODE_PREF).as_deref(), Some("standard"));
        });
    }

    /// No stored mode, or one this build does not know, is Follow (`loadAppearanceMode`).
    #[gpui_kit::test]
    fn an_absent_or_unknown_mode_follows_omarchy(cx: &mut TestAppContext) {
        assert_eq!(AppearanceMode::parse(Some("sepia")), AppearanceMode::FollowOmarchy);
        assert_eq!(AppearanceMode::parse(Some("standard")), AppearanceMode::Standard);
        cx.update(|cx| {
            install(None, &omarchy("tokyo-night", "#1a1b26"), SystemThemeResult::unavailable(), cx);
            assert_eq!(mode(cx), AppearanceMode::FollowOmarchy);
            assert_eq!(cx.global::<Palette>().omarchy_theme.as_deref(), Some("tokyo-night"));
        });
    }

    /// Preferences → Appearance's status line, both of React's wordings.
    #[test]
    fn the_status_line_says_what_is_followed() {
        assert_eq!(status_line(&omarchy("tokyo-night", "#1a1b26")), "Following Omarchy · tokyo-night · dark");
        assert_eq!(
            status_line(&SystemThemeResult::unavailable()),
            "Omarchy not detected — ChairPhoto Standard is in use. This is normal without Omarchy; nothing is missing."
        );
    }

    /// No usable Omarchy theme is follow mode's fallback: Standard, not a half theme.
    #[test]
    fn an_unavailable_system_theme_falls_back_to_standard() {
        let (tokens, mode, name) = palette_for(&SystemThemeResult::unavailable());
        assert_eq!(tokens, standard());
        assert_eq!(mode, STANDARD_MODE);
        assert_eq!(name, None);
    }

    /// Standard's two `rgba()` tokens become the hex alpha gpui parses, and hex passes through.
    #[test]
    fn css_hex_converts_rgba_and_passes_hex_through() {
        assert_eq!(css_hex("rgba(224, 164, 88, 0.14)"), "#E0A45824");
        assert_eq!(css_hex("rgba(0, 0, 0, 0.66)"), "#000000A8");
        assert_eq!(css_hex("#14120F"), "#14120F");
        assert_eq!(standard().sel, "#E0A45824");
        assert_eq!(standard().scrim, "#000000A8");
    }

    /// Every token parses as the colour gpui-component will read (hex only, no `rgba()`).
    #[test]
    fn every_standard_token_is_hex_gpui_can_parse() {
        let t = standard();
        for v in [
            &t.canvas, &t.panel, &t.elev, &t.well, &t.border, &t.line, &t.txt, &t.dim, &t.mute,
            &t.accent, &t.onaccent, &t.sel, &t.ok, &t.onok, &t.danger, &t.rating, &t.scrim,
        ] {
            assert!(Hsla::parse_hex(v).is_ok(), "{v}");
        }
    }
}
