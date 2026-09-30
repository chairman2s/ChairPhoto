//! ChairPhoto's look in GPUI: the token contract (`src/theme/tokens.ts`), the ChairPhoto
//! Standard palette (`src/theme/standard.ts`), the Omarchy mapping (`src/theme/omarchy.ts`, in
//! [`omarchy`]) and how a palette reaches the widgets.
//!
//! A palette becomes a `gpui_component::ThemeConfig` ([`theme_config`]) applied with
//! `Theme::update(.. apply_config ..)`, which derives the hover/active/button colours, rebuilds
//! gpui-base's projection and redraws every window (docs/plans/gpui/shell-apis.md § 1). The
//! tokens gpui has no field for — `mute`, `well`, `rating`, `line` as distinct from `border`,
//! `font-display` — live in the [`Palette`] global, set in the same `cx` update so both are
//! current in the same frame.
//!
//! **Appearance mode.** The React app persists "Follow Omarchy" / "ChairPhoto Standard" in
//! localStorage (`src/theme/prefs.ts`); the GPUI app has no per-machine settings store yet
//! (Preferences, #113). Until then it always follows Omarchy — the product default — and
//! falls back to Standard when no usable Omarchy theme exists, exactly as follow mode does.

pub mod omarchy;

use chairphoto_core::appearance::SystemThemeResult;
use gpui_kit::component::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode};
use gpui_kit::{App, Global, SharedString};
use std::rc::Rc;

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

/// ChairPhoto Standard, the app-owned warm dark palette (`src/theme/standard.ts`). `sel` is
/// `rgba(224, 164, 88, 0.14)` and `scrim` `rgba(0, 0, 0, 0.66)` there; as hex alpha they are
/// `0x24` (0.14 × 255 = 35.7) and `0xA8` (0.66 × 255 = 168.3).
pub fn standard() -> Tokens {
    let s = |v: &str| v.to_string();
    Tokens {
        canvas: s("#14120F"),
        panel: s("#1B1815"),
        elev: s("#23201C"),
        well: s("#0D0C0A"),
        border: s("#2E2A25"),
        line: s("#241F1B"),
        txt: s("#EFE9E0"),
        dim: s("#A8A093"),
        mute: s("#6F675C"),
        accent: s("#E0A458"),
        onaccent: s("#241C10"),
        sel: s("#E0A45824"),
        ok: s("#10B981"),
        onok: s("#04150F"),
        danger: s("#F87171"),
        rating: s("#FFD700"),
        scrim: s("#000000A8"),
    }
}

/// ChairPhoto Standard is dark.
pub const STANDARD_MODE: Mode = ThemeMode::Dark;

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
            let (tokens, mode) = omarchy::map_palette(palette);
            (tokens, mode, result.theme_name.clone())
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

    /// No usable Omarchy theme is follow mode's fallback: Standard, not a half theme.
    #[test]
    fn an_unavailable_system_theme_falls_back_to_standard() {
        let (tokens, mode, name) = palette_for(&SystemThemeResult::unavailable());
        assert_eq!(tokens, standard());
        assert_eq!(mode, STANDARD_MODE);
        assert_eq!(name, None);
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
