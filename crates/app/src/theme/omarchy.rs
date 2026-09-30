//! Follow Omarchy: map an Omarchy palette (the core's `appearance::OmarchyPalette`, already
//! validated there) onto ChairPhoto's tokens. A port of `mapPalette` in
//! `src/theme/omarchy.ts` — the same formulas, the same contrast guard, the same fallbacks —
//! so the GPUI app and the React app paint an Omarchy theme alike. Pure and total.
//!
//! Mixing convention: `mix(a, b, ratio)` blends sRGB channels with `ratio` as the weight of
//! `b` — `mix(x, y, 0.15)` is "85% x, 15% y".

use super::{standard, Mode, Tokens};
use chairphoto_core::appearance::{OmarchyMode, OmarchyPalette};
use gpui_kit::component::ThemeMode;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rgb {
    r: f64,
    g: f64,
    b: f64,
}

const BLACK: Rgb = Rgb { r: 0.0, g: 0.0, b: 0.0 };
const WHITE: Rgb = Rgb { r: 255.0, g: 255.0, b: 255.0 };

/// `#rgb`, `#rrggbb` or `#rrggbbaa`, alpha discarded. Never fails: the core validated every
/// colour before this sees it, and black keeps this total if that contract ever broke.
fn parse_hex(input: &str) -> Rgb {
    let s = input.trim().trim_start_matches('#');
    let ch = |h: &str| u8::from_str_radix(h, 16).map(f64::from).unwrap_or(0.0);
    match s.len() {
        3 => {
            let d = |i: usize| ch(&s[i..i + 1].repeat(2));
            Rgb { r: d(0), g: d(1), b: d(2) }
        }
        6 | 8 => Rgb { r: ch(&s[0..2]), g: ch(&s[2..4]), b: ch(&s[4..6]) },
        _ => BLACK,
    }
}

/// Lowercase `#rrggbb`; channels are rounded half up like JavaScript's `Math.round` (all
/// values are non-negative here, where that equals `f64::round`).
fn to_hex(c: Rgb) -> String {
    let h = |n: f64| n.clamp(0.0, 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", h(c.r), h(c.g), h(c.b))
}

fn normalize(hex: &str) -> String {
    to_hex(parse_hex(hex))
}

fn mix_rgb(a: Rgb, b: Rgb, ratio: f64) -> Rgb {
    let t = ratio.clamp(0.0, 1.0);
    Rgb { r: a.r + (b.r - a.r) * t, g: a.g + (b.g - a.g) * t, b: a.b + (b.b - a.b) * t }
}

fn mix(a: &str, b: &str, ratio: f64) -> String {
    to_hex(mix_rgb(parse_hex(a), parse_hex(b), ratio))
}

fn srgb_to_linear(channel: f64) -> f64 {
    let v = channel / 255.0;
    if v <= 0.03928 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance, 0 (black) – 1 (white).
fn relative_luminance(c: Rgb) -> f64 {
    0.2126 * srgb_to_linear(c.r) + 0.7152 * srgb_to_linear(c.g) + 0.0722 * srgb_to_linear(c.b)
}

/// WCAG contrast ratio, 1 – 21.
fn contrast_ratio(a: &str, b: &str) -> f64 {
    let (la, lb) = (relative_luminance(parse_hex(a)), relative_luminance(parse_hex(b)));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// The black/white pole *not* already close to `fg`.
fn pole_away_from(fg: &str) -> Rgb {
    if relative_luminance(parse_hex(fg)) > 0.5 {
        BLACK
    } else {
        WHITE
    }
}

/// White in dark mode, black in light mode: where low-contrast foregrounds are pushed.
fn contrast_pole_for_mode(mode: Mode) -> Rgb {
    if mode == ThemeMode::Dark {
        WHITE
    } else {
        BLACK
    }
}

const MAX_CONTRAST_STEPS: usize = 20;
const CONTRAST_STEP: f64 = 0.05;

/// Push `hex` toward `pole` in 5% steps (at most 20) until it reaches `min_ratio` against
/// `against`. Deterministic and total; see `ensureContrast` in omarchy.ts for why this clamps
/// rather than substituting the Standard token.
fn ensure_contrast(hex: &str, against: &str, min_ratio: f64, pole: Rgb) -> String {
    let mut current = hex.to_string();
    for _ in 0..MAX_CONTRAST_STEPS {
        if contrast_ratio(&current, against) >= min_ratio {
            return current;
        }
        current = to_hex(mix_rgb(parse_hex(&current), pole, CONTRAST_STEP));
    }
    current
}

/// The near-black or near-white tint of `fill` that contrasts better against it.
fn contrast_on(fill: &str) -> String {
    let near_black = mix(fill, "#000000", 0.85);
    let near_white = mix(fill, "#ffffff", 0.85);
    if contrast_ratio(&near_black, fill) >= contrast_ratio(&near_white, fill) {
        near_black
    } else {
        near_white
    }
}

/// Map an Omarchy palette onto the token contract, contrast-guarded. `mapPalette` in
/// `src/theme/omarchy.ts`, formula for formula.
pub fn map_palette(p: &OmarchyPalette) -> (Tokens, Mode) {
    let mode = match p.mode {
        OmarchyMode::Dark => ThemeMode::Dark,
        OmarchyMode::Light => ThemeMode::Light,
    };
    let std = standard();
    let mode_pole = contrast_pole_for_mode(mode);
    let away_pole = pole_away_from(&p.foreground);

    let panel = normalize(&p.background);
    let canvas = match &p.darker_background {
        Some(v) => normalize(v),
        None => mix(&p.background, &to_hex(away_pole), 0.15),
    };
    let elev = match &p.lighter_background {
        Some(v) => normalize(v),
        None => mix(&p.background, &p.foreground, 0.08),
    };
    let well = if mode == ThemeMode::Dark {
        mix(&canvas, "#000000", 0.2)
    } else {
        mix(&canvas, "#000000", 0.08)
    };
    let border = mix(&p.background, &p.foreground, 0.14);
    let line = mix(&p.background, &p.foreground, 0.07);
    let dim = match &p.dark_foreground {
        Some(v) => normalize(v),
        None => mix(&p.foreground, &p.background, 0.28),
    };
    let mute = normalize(&p.muted);
    let sel = mix(&p.selection, &p.background, 0.7);
    let ok = p.green.as_deref().map(normalize).unwrap_or(std.ok.clone());
    let danger = p.red.as_deref().map(normalize).unwrap_or(std.danger.clone());
    let rating = p.yellow.as_deref().map(normalize).unwrap_or(std.rating.clone());

    let mut txt = normalize(&p.foreground);
    txt = ensure_contrast(&txt, &canvas, 4.5, mode_pole);
    txt = ensure_contrast(&txt, &panel, 4.5, mode_pole);
    let dim = ensure_contrast(&dim, &panel, 4.5, mode_pole);
    let mute = ensure_contrast(&mute, &panel, 3.0, mode_pole);
    let accent = ensure_contrast(&normalize(&p.accent), &canvas, 3.0, mode_pole);

    // From the guarded accent/ok, so the "on" colour is legible on what is actually painted.
    let onaccent = contrast_on(&accent);
    let onok = contrast_on(&ok);

    let tokens = Tokens {
        canvas,
        panel,
        elev,
        well,
        border,
        line,
        txt,
        dim,
        mute,
        accent,
        onaccent,
        sel,
        ok,
        onok,
        danger,
        rating,
        // Over-photo scrims never theme: always Standard's.
        scrim: std.scrim,
    };
    (tokens, mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `DARK` fixture of `src/theme/__tests__/omarchy.test.ts`, trimmed to what it sets.
    fn dark() -> OmarchyPalette {
        chairphoto_core::appearance::parse_palette(
            r##"
mode = "dark"
accent = "#7AA2F7"
selection = "#283457"
muted = "#565f89"
background = "#1A1B26"
foreground = "#C0CAF5"
darker_background = "#16161E"
lighter_background = "#24283B"
dark_foreground = "#A9B1D6"
red = "#F7768E"
yellow = "#E0AF68"
green = "#9ECE6A"
"##,
        )
        .unwrap()
    }

    fn light() -> OmarchyPalette {
        chairphoto_core::appearance::parse_palette(
            r##"
mode = "light"
accent = "#2E7DE9"
selection = "#B7C1E3"
muted = "#848CB5"
background = "#E1E2E7"
foreground = "#3760BF"
"##,
        )
        .unwrap()
    }

    #[test]
    fn maps_identity_roles_and_overrides_like_the_ts_mapping() {
        let (t, mode) = map_palette(&dark());
        assert_eq!(mode, ThemeMode::Dark);
        assert_eq!(t.panel, "#1a1b26");
        assert_eq!(t.canvas, "#16161e");
        assert_eq!(t.elev, "#24283b");
        assert_eq!(t.dim, "#a9b1d6");
        assert_eq!(t.ok, "#9ece6a");
        assert_eq!(t.danger, "#f7768e");
        assert_eq!(t.rating, "#e0af68");
        assert_eq!(t.well, mix(&t.canvas, "#000000", 0.2));
        assert_eq!(t.border, mix("#1A1B26", "#C0CAF5", 0.14));
        assert_eq!(t.line, mix("#1A1B26", "#C0CAF5", 0.07));
        assert_eq!(t.sel, mix("#283457", "#1A1B26", 0.7));
        assert_eq!(t.scrim, standard().scrim);
    }

    #[test]
    fn light_palette_falls_back_and_derives_like_the_ts_mapping() {
        let (t, mode) = map_palette(&light());
        assert_eq!(mode, ThemeMode::Light);
        assert_eq!(t.ok, standard().ok);
        assert_eq!(t.danger, standard().danger);
        assert_eq!(t.rating, standard().rating);
        // Foreground is dark, so the away pole is white.
        assert_eq!(t.canvas, mix("#E1E2E7", "#ffffff", 0.15));
        assert_eq!(t.elev, mix("#E1E2E7", "#3760BF", 0.08));
        assert_eq!(t.well, mix(&t.canvas, "#000000", 0.08));
    }

    #[test]
    fn meets_the_contrast_guard_thresholds() {
        for p in [dark(), light()] {
            let (t, _) = map_palette(&p);
            assert!(contrast_ratio(&t.txt, &t.canvas) >= 4.5);
            assert!(contrast_ratio(&t.txt, &t.panel) >= 4.5);
            assert!(contrast_ratio(&t.dim, &t.panel) >= 4.5);
            assert!(contrast_ratio(&t.mute, &t.panel) >= 3.0);
            assert!(contrast_ratio(&t.accent, &t.canvas) >= 3.0);
        }
    }

    #[test]
    fn hex_helpers_match_javascript_rounding() {
        assert_eq!(normalize("#ABC"), "#aabbcc");
        assert_eq!(normalize("#AABBCCDD"), "#aabbcc");
        // 0x10 + (0x11 - 0x10) * 0.5 = 16.5 -> 17, as Math.round does.
        assert_eq!(mix("#101010", "#111111", 0.5), "#111111");
    }
}
