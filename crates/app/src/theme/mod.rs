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
//! **Appearance mode.** The React app persists "Follow Omarchy" / "ChairPhoto Standard" in
//! localStorage (`src/theme/prefs.ts`); the GPUI app has no per-machine settings store yet
//! (Preferences, #113). Until then it always follows Omarchy — the product default — and
//! falls back to Standard when no usable Omarchy theme exists, exactly as follow mode does.

use chairphoto_core::appearance::{OmarchyMode, SystemThemeResult};
use chairphoto_model::theme::{omarchy, standard as model_standard, tokens::ThemeTokens};
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
