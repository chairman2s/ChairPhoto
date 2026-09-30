//! Follow Omarchy: map a backend-reported [`OmarchyPalette`] onto ChairPhoto's semantic
//! [`ThemeTokens`]. Port of `src/theme/omarchy.ts`. Pure — the controller that decides *when*
//! to call it is the view's.
//!
//! Mixing convention used throughout the role-mapping table: `mix(a, b, ratio)` blends sRGB
//! channels with `ratio` as the weight of `b` — `mix(x, y, 0.15)` is "85% x, 15% y".
//!
//! Semantic choices against the TypeScript:
//! - The palette is the backend's own [`OmarchyPalette`] (`chairphoto_core::appearance`),
//!   which the TS interface mirrored field for field; `?: string | null` is `Option<String>`.
//! - Channels are `f64` between parse and [`to_hex`], as JS numbers were; [`to_hex`] rounds
//!   with JavaScript's `Math.round` ([`crate::js_compat::math_round`]), so every token is
//!   byte-identical to the TS output.
//! - Hex digits parse like `parseInt(pair, 16)`: the leading hex digits of the pair count
//!   (`"1g"` → 1). Where JS got `NaN` (no leading digit) and would have printed
//!   `#NaNNaNNaN`, a channel here is 0. The backend's `parse_palette` rejects any such color
//!   before this module sees it, so the branch is unreachable in practice; this keeps the
//!   mapping total without inventing a `NaN` color.
//! - Lengths are counted in `char`s where JS counted UTF-16 units; they agree for every
//!   ASCII input, and a non-ASCII one has no hex digits to disagree about.

use super::standard;
use super::tokens::{ThemeMode, ThemeTokens};
use crate::js_compat::math_round;
pub use chairphoto_core::appearance::OmarchyPalette;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rgb {
    r: f64,
    g: f64,
    b: f64,
}

const BLACK: Rgb = Rgb { r: 0.0, g: 0.0, b: 0.0 };
const WHITE: Rgb = Rgb { r: 255.0, g: 255.0, b: 255.0 };

fn clamp(n: f64, min: f64, max: f64) -> f64 {
    // `Math.min(max, Math.max(min, n))`
    max.min(min.max(n))
}

/// `parseInt(digits, 16)` over a short string, with "no leading hex digit" as 0 (see the
/// module docs).
fn parse_hex_digits(digits: &[char]) -> f64 {
    let mut value = 0u32;
    for c in digits {
        match c.to_digit(16) {
            Some(d) => value = value * 16 + d,
            None => break,
        }
    }
    f64::from(value)
}

/// Parse `#rgb`, `#rrggbb`, or `#rrggbbaa` (case-insensitive; alpha is discarded — every
/// token this module produces is a solid color). Never panics: anything else is black.
fn parse_hex(input: &str) -> Rgb {
    let trimmed = input.trim();
    let s: Vec<char> = trimmed.strip_prefix('#').unwrap_or(trimmed).chars().collect();
    match s.len() {
        3 => Rgb {
            r: parse_hex_digits(&[s[0], s[0]]),
            g: parse_hex_digits(&[s[1], s[1]]),
            b: parse_hex_digits(&[s[2], s[2]]),
        },
        6 | 8 => Rgb {
            r: parse_hex_digits(&s[0..2]),
            g: parse_hex_digits(&s[2..4]),
            b: parse_hex_digits(&s[4..6]),
        },
        _ => BLACK,
    }
}

fn to_hex(c: Rgb) -> String {
    let h = |n: f64| math_round(clamp(n, 0.0, 255.0)) as u8;
    format!("#{:02x}{:02x}{:02x}", h(c.r), h(c.g), h(c.b))
}

/// Normalize any parseable hex form to lowercase `#rrggbb`.
fn normalize(hex: &str) -> String {
    to_hex(parse_hex(hex))
}

fn mix_rgb(a: Rgb, b: Rgb, ratio: f64) -> Rgb {
    let t = clamp(ratio, 0.0, 1.0);
    Rgb {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
    }
}

/// Hex-in, hex-out blend — the shape every formula in the role-mapping table is written against.
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

/// WCAG contrast ratio between two colors: 1 (no contrast) – 21 (max).
fn contrast_ratio(hex_a: &str, hex_b: &str) -> f64 {
    let la = relative_luminance(parse_hex(hex_a));
    let lb = relative_luminance(parse_hex(hex_b));
    let lighter = la.max(lb);
    let darker = la.min(lb);
    (lighter + 0.05) / (darker + 0.05)
}

/// The #000000/#ffffff pole *not* already close to `fg_hex` — the one mixing toward moves a
/// background color away from the foreground's own brightness.
fn pole_away_from(fg_hex: &str) -> Rgb {
    if relative_luminance(parse_hex(fg_hex)) > 0.5 {
        BLACK
    } else {
        WHITE
    }
}

/// The pole a mode pushes low-contrast foreground roles toward: white in dark mode
/// (lightening text/accents against a dark surface), black in light mode.
fn contrast_pole_for_mode(mode: &ThemeMode) -> Rgb {
    match mode {
        ThemeMode::Dark => WHITE,
        ThemeMode::Light => BLACK,
    }
}

const MAX_CONTRAST_STEPS: usize = 20;
const CONTRAST_STEP: f64 = 0.05;

/// Contrast guard: push `hex` toward `pole` in 5% steps (max 20 — a full walk to the pole)
/// until its contrast against `against` reaches `min_ratio`, or the steps run out.
///
/// Deliberately not "substitute the Standard token": that would inject a dark-theme value
/// into a light palette the moment one role failed its check. Clamping stepwise keeps the
/// result recognizably the theme's own color family. Deterministic and bounded.
fn ensure_contrast(hex: String, against: &str, min_ratio: f64, pole: Rgb) -> String {
    let mut current = hex;
    for _ in 0..MAX_CONTRAST_STEPS {
        if contrast_ratio(&current, against) >= min_ratio {
            return current;
        }
        current = to_hex(mix_rgb(parse_hex(&current), pole, CONTRAST_STEP));
    }
    current
}

/// onaccent/onok: whichever of a near-black or near-white tint of `fill_hex` contrasts
/// better against that fill (ties go to near-black), so text drawn on an accent or ok chip
/// stays legible however light or dark the theme's accent/green is.
fn contrast_on(fill_hex: &str) -> String {
    let near_black = mix(fill_hex, "#000000", 0.85);
    let near_white = mix(fill_hex, "#ffffff", 0.85);
    if contrast_ratio(&near_black, fill_hex) >= contrast_ratio(&near_white, fill_hex) {
        near_black
    } else {
        near_white
    }
}

/// A palette mapped onto the token contract, with the palette's own mode.
#[derive(Debug, Clone, PartialEq)]
pub struct MappedPalette {
    pub tokens: ThemeTokens,
    pub mode: ThemeMode,
}

/// Map an Omarchy theme's palette onto ChairPhoto's [`ThemeTokens`]. Pure and total: any
/// palette produces a full, contrast-guarded token set.
pub fn map_palette(p: &OmarchyPalette) -> MappedPalette {
    let mode = p.mode.clone();
    let mode_pole = contrast_pole_for_mode(&mode);
    let away_pole = pole_away_from(&p.foreground);

    let panel = normalize(&p.background);
    let canvas = match &p.darker_background {
        Some(c) => normalize(c),
        None => mix(&p.background, &to_hex(away_pole), 0.15),
    };
    let elev = match &p.lighter_background {
        Some(c) => normalize(c),
        None => mix(&p.background, &p.foreground, 0.08),
    };
    let well = match mode {
        ThemeMode::Dark => mix(&canvas, "#000000", 0.2),
        ThemeMode::Light => mix(&canvas, "#000000", 0.08),
    };
    let border = mix(&p.background, &p.foreground, 0.14);
    let line = mix(&p.background, &p.foreground, 0.07);
    let dim = match &p.dark_foreground {
        Some(c) => normalize(c),
        None => mix(&p.foreground, &p.background, 0.28),
    };
    let mute = normalize(&p.muted);
    let sel = mix(&p.selection, &p.background, 0.7);
    // Fallbacks are Standard's values verbatim (not renormalized).
    let ok = p.green.as_deref().map_or_else(|| standard::OK.to_string(), normalize);
    let danger = p.red.as_deref().map_or_else(|| standard::DANGER.to_string(), normalize);
    let rating = p.yellow.as_deref().map_or_else(|| standard::RATING.to_string(), normalize);

    // Contrast guard: only the foreground-ish roles that sit on top of a background need it.
    // canvas/panel/elev/well/border/line are only ever the "against" side.
    let mut txt = normalize(&p.foreground);
    txt = ensure_contrast(txt, &canvas, 4.5, mode_pole);
    txt = ensure_contrast(txt, &panel, 4.5, mode_pole);
    let dim_guarded = ensure_contrast(dim, &panel, 4.5, mode_pole);
    let mute_guarded = ensure_contrast(mute, &panel, 3.0, mode_pole);
    let accent = ensure_contrast(normalize(&p.accent), &canvas, 3.0, mode_pole);

    // Derived from the *guarded* accent/ok, so the "on" color is legible against the fill
    // actually painted.
    let onaccent = contrast_on(&accent);
    let onok = contrast_on(&ok);

    let tokens = ThemeTokens {
        canvas,
        panel,
        elev,
        well,
        border,
        line,
        txt,
        dim: dim_guarded,
        mute: mute_guarded,
        accent,
        onaccent,
        sel,
        ok,
        onok,
        danger,
        rating,
        // Over-photo scrims and font stacks never theme — always Standard's, verbatim.
        scrim: standard::SCRIM.to_string(),
        font_sans: standard::FONT_SANS.to_string(),
        font_display: standard::FONT_DISPLAY.to_string(),
    };
    MappedPalette { tokens, mode }
}

// Port of `src/theme/__tests__/omarchy.test.ts` (18 cases, same names, grouped by the vitest
// `describe` blocks as submodules).
//
// The assertions deliberately avoid hand-computed hex/float literals for anything that
// passes through the contrast guard (txt/dim/mute/accent) — that guard's exact output depends
// on how many 5% steps it took, an implementation detail rather than the contract. Those roles
// are checked against docs/appearance.md's thresholds via a small independent reference
// implementation of the WCAG math below. Roles the guard never touches are asserted exactly.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::standard;

    // ---- independent reference color math (re-derived from the spec, not from the code
    // above) -------------------------------------------------------------------------------

    type RefRgb = [f64; 3];

    fn ref_parse_hex(hex: &str) -> RefRgb {
        let s = hex.strip_prefix('#').unwrap_or(hex);
        let full: String = if s.len() == 3 {
            s.chars().flat_map(|c| [c, c]).collect()
        } else {
            s[..6].to_string()
        };
        let ch = |i: usize| f64::from(u8::from_str_radix(&full[i..i + 2], 16).unwrap());
        [ch(0), ch(2), ch(4)]
    }

    fn ref_to_hex([r, g, b]: RefRgb) -> String {
        let h = |n: f64| {
            let v = n.clamp(0.0, 255.0);
            // Math.round: ties toward +inf
            let f = v.floor();
            (if v - f >= 0.5 { f + 1.0 } else { f }) as u8
        };
        format!("#{:02x}{:02x}{:02x}", h(r), h(g), h(b))
    }

    /// Normalize any parseable hex to lowercase #rrggbb.
    fn normalize_hex(hex: &str) -> String {
        ref_to_hex(ref_parse_hex(hex))
    }

    /// `ratio` is the weight of `b`.
    fn ref_mix(a: &str, b: &str, ratio: f64) -> String {
        let [ar, ag, ab] = ref_parse_hex(a);
        let [br, bg, bb] = ref_parse_hex(b);
        ref_to_hex([ar + (br - ar) * ratio, ag + (bg - ag) * ratio, ab + (bb - ab) * ratio])
    }

    fn ref_srgb_to_linear(channel: f64) -> f64 {
        let v = channel / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }

    fn rel_luminance(hex: &str) -> f64 {
        let [r, g, b] = ref_parse_hex(hex);
        0.2126 * ref_srgb_to_linear(r) + 0.7152 * ref_srgb_to_linear(g) + 0.0722 * ref_srgb_to_linear(b)
    }

    fn wcag_contrast(a: &str, b: &str) -> f64 {
        let la = rel_luminance(a);
        let lb = rel_luminance(b);
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    /// The #000000/#ffffff pole *not* already close to `fg_hex`.
    fn ref_pole_away_from(fg_hex: &str) -> &'static str {
        if rel_luminance(fg_hex) > 0.5 {
            "#000000"
        } else {
            "#ffffff"
        }
    }

    /// `/^#[0-9a-f]{6}$/`
    fn is_hex6(s: &str) -> bool {
        s.len() == 7
            && s.starts_with('#')
            && s[1..].chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    }

    fn assert_meets_thresholds(t: &ThemeTokens) {
        assert!(wcag_contrast(&t.txt, &t.canvas) >= 4.5);
        assert!(wcag_contrast(&t.txt, &t.panel) >= 4.5);
        assert!(wcag_contrast(&t.dim, &t.panel) >= 4.5);
        assert!(wcag_contrast(&t.mute, &t.panel) >= 3.0);
        assert!(wcag_contrast(&t.accent, &t.canvas) >= 3.0);
    }

    fn assert_hex6_except(t: &ThemeTokens, skip: &[&str]) {
        for (key, value) in t.entries() {
            if skip.contains(&key) {
                continue;
            }
            assert!(is_hex6(value), "{key} = {value}");
        }
    }

    // ---- fixtures ----------------------------------------------------------------------

    fn bare(mode: ThemeMode, background: &str, foreground: &str, accent: &str, selection: &str, muted: &str) -> OmarchyPalette {
        OmarchyPalette {
            mode,
            accent: accent.into(),
            selection: selection.into(),
            muted: muted.into(),
            background: background.into(),
            foreground: foreground.into(),
            dark_background: None,
            darker_background: None,
            lighter_background: None,
            dark_foreground: None,
            light_foreground: None,
            bright_foreground: None,
            red: None,
            yellow: None,
            green: None,
            cyan: None,
            blue: None,
            magenta: None,
            bright_red: None,
            bright_yellow: None,
            bright_green: None,
            bright_cyan: None,
            bright_blue: None,
            bright_magenta: None,
        }
    }

    /// Every optional field the mapping reads populated, each guarded role's naive value
    /// already clearing its threshold — the guard is a no-op. dark_foreground is pure white
    /// so its "prefers the override" check is guard-proof by construction.
    fn dark() -> OmarchyPalette {
        let mut p = bare(ThemeMode::Dark, "#141414", "#e8e8e8", "#4d8dff", "#ff6600", "#8a8a8a");
        p.darker_background = Some("#0a0a0a".into());
        p.lighter_background = Some("#1e1e1e".into());
        p.dark_foreground = Some("#ffffff".into());
        p.green = Some("#3ddc84".into());
        p.red = Some("#ff5555".into());
        p.yellow = Some("#ffcc33".into());
        p
    }

    /// Light mode, every optional field omitted — every fallback branch.
    fn light() -> OmarchyPalette {
        bare(ThemeMode::Light, "#f5f5f0", "#1a1a1a", "#3366cc", "#ffd54f", "#767676")
    }

    mod full_dark_palette {
        use super::*;

        #[test]
        fn returns_the_palette_s_own_mode() {
            assert_eq!(map_palette(&dark()).mode, ThemeMode::Dark);
        }

        #[test]
        fn maps_identity_roles_verbatim_normalized() {
            let d = dark();
            let t = map_palette(&d).tokens;
            assert_eq!(t.panel, normalize_hex(&d.background));
            assert_eq!(t.mute, normalize_hex(&d.muted));
            assert_eq!(t.ok, normalize_hex(d.green.as_deref().unwrap()));
            assert_eq!(t.danger, normalize_hex(d.red.as_deref().unwrap()));
            assert_eq!(t.rating, normalize_hex(d.yellow.as_deref().unwrap()));
        }

        #[test]
        fn prefers_the_optional_override_over_the_derived_formula_unguarded_roles() {
            let d = dark();
            let t = map_palette(&d).tokens;
            assert_eq!(t.canvas, normalize_hex(d.darker_background.as_deref().unwrap()));
            assert_eq!(t.elev, normalize_hex(d.lighter_background.as_deref().unwrap()));
        }

        #[test]
        fn prefers_the_optional_override_for_dim_a_guarded_role_white_always_clears_4_5() {
            let d = dark();
            let t = map_palette(&d).tokens;
            assert_eq!(t.dim, normalize_hex(d.dark_foreground.as_deref().unwrap()));
        }

        #[test]
        fn derives_well_border_line_sel_by_their_stated_blend() {
            let d = dark();
            let t = map_palette(&d).tokens;
            assert_eq!(t.well, ref_mix(&t.canvas, "#000000", 0.2)); // dark-mode ratio
            assert_eq!(t.border, ref_mix(&normalize_hex(&d.background), &normalize_hex(&d.foreground), 0.14));
            assert_eq!(t.line, ref_mix(&normalize_hex(&d.background), &normalize_hex(&d.foreground), 0.07));
            assert_eq!(t.sel, ref_mix(&normalize_hex(&d.selection), &normalize_hex(&d.background), 0.7));
        }

        #[test]
        fn scrim_and_font_tokens_are_chairphoto_standard_s_untouched() {
            let t = map_palette(&dark()).tokens;
            assert_eq!(t.scrim, standard::SCRIM);
            assert_eq!(t.font_sans, standard::FONT_SANS);
            assert_eq!(t.font_display, standard::FONT_DISPLAY);
        }

        #[test]
        fn meets_the_contrast_guard_s_thresholds() {
            assert_meets_thresholds(&map_palette(&dark()).tokens);
        }

        #[test]
        fn every_color_token_is_normalized_lowercase_rrggbb() {
            assert_hex6_except(&map_palette(&dark()).tokens, &["scrim", "font-sans", "font-display"]);
        }
    }

    mod light_palette_every_optional_omitted {
        use super::*;

        #[test]
        fn returns_the_palette_s_own_mode() {
            assert_eq!(map_palette(&light()).mode, ThemeMode::Light);
        }

        #[test]
        fn falls_back_to_chairphoto_standard_for_ok_danger_rating() {
            let t = map_palette(&light()).tokens;
            assert_eq!(t.ok, standard::OK);
            assert_eq!(t.danger, standard::DANGER);
            assert_eq!(t.rating, standard::RATING);
        }

        #[test]
        fn derives_canvas_elev_well_border_line_by_the_stated_formula_unguarded_roles() {
            let l = light();
            let t = map_palette(&l).tokens;
            let bg = normalize_hex(&l.background);
            let fg = normalize_hex(&l.foreground);
            assert_eq!(t.canvas, ref_mix(&bg, ref_pole_away_from(&l.foreground), 0.15));
            assert_eq!(t.elev, ref_mix(&bg, &fg, 0.08));
            assert_eq!(t.well, ref_mix(&t.canvas, "#000000", 0.08)); // light-mode ratio
            assert_eq!(t.border, ref_mix(&bg, &fg, 0.14));
            assert_eq!(t.line, ref_mix(&bg, &fg, 0.07));
        }

        #[test]
        fn derives_dim_from_the_formula_distinct_from_plain_foreground_guarded_role() {
            let l = light();
            assert_ne!(map_palette(&l).tokens.dim, normalize_hex(&l.foreground));
        }

        #[test]
        fn meets_the_contrast_guard_s_thresholds() {
            assert_meets_thresholds(&map_palette(&light()).tokens);
        }

        /// ok/danger/rating excepted: LIGHT omits green/red/yellow, so those fall back to
        /// Standard's own values verbatim, unnormalized by design.
        #[test]
        fn every_color_token_is_normalized_lowercase_rrggbb_ok_danger_rating_excepted() {
            assert_hex6_except(
                &map_palette(&light()).tokens,
                &["scrim", "font-sans", "font-display", "ok", "danger", "rating"],
            );
        }
    }

    mod contrast_clamp {
        use super::*;

        #[test]
        fn pushes_low_contrast_txt_dim_mute_accent_until_each_pair_clears_its_threshold() {
            // background/foreground/accent/muted are all close, dark grays — comfortably
            // below every threshold before the guard runs.
            let low = bare(ThemeMode::Dark, "#202020", "#282828", "#252525", "#ff0000", "#242424");
            let t = map_palette(&low).tokens;

            // The guard had work to do: the naive values fail against the computed surfaces.
            assert!(wcag_contrast(&normalize_hex(&low.foreground), &t.canvas) < 4.5);
            assert!(wcag_contrast(&normalize_hex(&low.muted), &t.panel) < 3.0);
            assert!(wcag_contrast(&normalize_hex(&low.accent), &t.canvas) < 3.0);

            // And the guarded output passes.
            assert_meets_thresholds(&t);
        }

        #[test]
        fn is_deterministic_and_total_never_throws_even_when_20_steps_can_t_fully_separate() {
            let extreme = bare(ThemeMode::Light, "#808080", "#7f7f7f", "#818181", "#808080", "#7e7e7e");
            let first = map_palette(&extreme);
            assert_eq!(map_palette(&extreme), first);
            // ok/danger/rating excepted: `extreme` omits green/red/yellow too.
            assert_hex6_except(&first.tokens, &["scrim", "font-sans", "font-display", "ok", "danger", "rating"]);
        }
    }

    mod onaccent_onok_contrast_pick {
        use super::*;

        /// A mid-gray canvas contrasts against both a near-black and a near-white accent
        /// (>3.0 either way), so the accent guard never fires and the pick is unambiguous.
        fn with_accent(accent: &str) -> OmarchyPalette {
            let mut p = bare(ThemeMode::Dark, "#202020", "#e0e0e0", accent, "#ff0000", "#888888");
            p.darker_background = Some("#808080".into());
            p
        }

        #[test]
        fn picks_a_near_white_tint_against_a_near_black_accent() {
            let t = map_palette(&with_accent("#050505")).tokens;
            assert_eq!(t.accent, "#050505"); // the guard left it untouched
            assert!(rel_luminance(&t.onaccent) > rel_luminance(&t.accent));
            assert!(wcag_contrast(&t.onaccent, &t.accent) > 3.0);
        }

        #[test]
        fn picks_a_near_black_tint_against_a_near_white_accent() {
            let t = map_palette(&with_accent("#fafafa")).tokens;
            assert_eq!(t.accent, "#fafafa");
            assert!(rel_luminance(&t.onaccent) < rel_luminance(&t.accent));
            assert!(wcag_contrast(&t.onaccent, &t.accent) > 3.0);
        }

        #[test]
        fn onok_is_picked_the_same_way_against_ok_unguarded_so_no_shielding_fixture_needed() {
            let mut near_black = dark();
            near_black.green = Some("#050505".into());
            let t = map_palette(&near_black).tokens;
            assert!(rel_luminance(&t.onok) > rel_luminance(&t.ok));

            let mut near_white = dark();
            near_white.green = Some("#fafafa".into());
            let t = map_palette(&near_white).tokens;
            assert!(rel_luminance(&t.onok) < rel_luminance(&t.ok));
        }
    }

    // New (not in the vitest file): golden output of the TypeScript `mapPalette` for five
    // palettes (the fixtures above plus a short-hex/alpha one), captured by running
    // `src/theme/omarchy.ts` under node 25 when this port was written. It pins the guarded
    // roles to the TS's exact step count, which the contract tests deliberately do not.
    #[test]
    fn matches_the_typescript_output_byte_for_byte() {
        let low = bare(ThemeMode::Dark, "#202020", "#282828", "#252525", "#ff0000", "#242424");
        let extreme = bare(ThemeMode::Light, "#808080", "#7f7f7f", "#818181", "#808080", "#7e7e7e");
        let short = bare(ThemeMode::Light, "#FFF", "#123", "#abcdef80", "#0f0", "#999");
        let cases = [
            (dark(), "#0a0a0a #141414 #1e1e1e #080808 #323232 #232323 #e8e8e8 #ffffff #8a8a8a #4d8dff #0c1526 #5b2d0e #3ddc84 #092114 #ff5555 #ffcc33"),
            (light(), "#f7f7f2 #f5f5f0 #e3e3df #e3e3df #d6d6d2 #e6e6e1 #1a1a1a #575756 #767676 #3366cc #e0e8f7 #f8ebc0 #10B981 #021c13 #F87171 #FFD700"),
            (low, "#414141 #202020 #212121 #343434 #212121 #212121 #aeaeae #8a8a8a #6e6e6e #8f8f8f #151515 #631616 #10B981 #021c13 #F87171 #FFD700"),
            (extreme, "#939393 #808080 #808080 #878787 #808080 #808080 #171717 #2e2e2e #353535 #464646 #e3e3e3 #808080 #10B981 #021c13 #F87171 #FFD700"),
            (short, "#ffffff #ffffff #ecedef #ebebeb #dee0e2 #eef0f1 #112233 #54606c #919191 #7d97b0 #13171a #b3ffb3 #10B981 #021c13 #F87171 #FFD700"),
        ];
        for (palette, expected) in cases {
            let t = map_palette(&palette).tokens;
            let got: Vec<&str> = t.entries()[..16].iter().map(|&(_, v)| v).collect();
            assert_eq!(got.join(" "), expected);
        }
    }

    mod sel_pre_blend {
        use super::*;

        #[test]
        fn is_a_solid_normalized_hex_color_between_selection_and_background_not_rgba() {
            let d = dark();
            let t = map_palette(&d).tokens;
            assert!(is_hex6(&t.sel));
            assert_eq!(t.sel, ref_mix(&normalize_hex(&d.selection), &normalize_hex(&d.background), 0.7));
            assert_ne!(t.sel, normalize_hex(&d.selection));
            assert_ne!(t.sel, normalize_hex(&d.background));
        }
    }
}
