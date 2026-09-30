//! The shell's look: the palette as `Hsla`, the colour-label vocabulary, and the few
//! recurring shapes (pills, dots, separators) the React shell styled in `App.css`.
//!
//! Colours come from the [`Palette`] global, not only from gpui-component's `Theme`: the
//! shell uses `mute`, `line`, `well`, `sel` and `rating`, which the Theme has no field for
//! (shell-apis.md § 1). Sizes are the React stylesheet's (`src/App.css` § Shell), in px.

use crate::theme::{standard, Palette, Tokens};
use gpui_kit::component::Colorize as _;
use gpui_kit::{point, px, App, BoxShadow, Hsla, Pixels};

/// `--r` in App.css: the default corner radius.
pub const RADIUS: Pixels = px(8.);

/// Every token of the active palette, parsed once per render.
#[derive(Debug, Clone, Copy)]
pub struct Colors {
    pub canvas: Hsla,
    pub panel: Hsla,
    pub elev: Hsla,
    pub well: Hsla,
    pub border: Hsla,
    pub line: Hsla,
    pub txt: Hsla,
    pub dim: Hsla,
    pub mute: Hsla,
    pub accent: Hsla,
    pub onaccent: Hsla,
    pub sel: Hsla,
    pub ok: Hsla,
    pub danger: Hsla,
    pub rating: Hsla,
    pub scrim: Hsla,
}

impl Colors {
    /// The active palette, or ChairPhoto Standard before one is applied (headless tests
    /// that never set the global).
    pub fn get(cx: &App) -> Colors {
        match cx.try_global::<Palette>() {
            Some(p) => Colors::from_tokens(&p.tokens),
            None => Colors::from_tokens(&standard()),
        }
    }

    pub fn from_tokens(t: &Tokens) -> Colors {
        // Every token is hex (`theme::css_hex`); a colour gpui cannot parse is a bug in the
        // palette, shown as magenta rather than hidden.
        let c = |v: &str| Hsla::parse_hex(v).unwrap_or(gpui_kit::hsla(0.83, 1., 0.5, 1.));
        Colors {
            canvas: c(&t.canvas),
            panel: c(&t.panel),
            elev: c(&t.elev),
            well: c(&t.well),
            border: c(&t.border),
            line: c(&t.line),
            txt: c(&t.txt),
            dim: c(&t.dim),
            mute: c(&t.mute),
            accent: c(&t.accent),
            onaccent: c(&t.onaccent),
            sel: c(&t.sel),
            ok: c(&t.ok),
            danger: c(&t.danger),
            rating: c(&t.rating),
            scrim: c(&t.scrim),
        }
    }

    /// `color-mix(in srgb, var(--accent) 45%, transparent)`: the chips' and attention
    /// badges' border.
    pub fn accent_border(&self) -> Hsla {
        self.accent.opacity(0.45)
    }
}

/// One colour label: its stored name and swatch (`src/modules/labels.ts`). Labels are
/// stored meaning, never theme colour, so the swatches are fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorLabel {
    pub name: &'static str,
    pub hex: &'static str,
}

/// The colour-label vocabulary, in display order. `""` is "no label".
pub const COLOR_LABELS: [ColorLabel; 5] = [
    ColorLabel { name: "Red", hex: "#DC2626" },
    ColorLabel { name: "Yellow", hex: "#FFD700" },
    ColorLabel { name: "Green", hex: "#10B981" },
    ColorLabel { name: "Blue", hex: "#3B82F6" },
    ColorLabel { name: "Purple", hex: "#8B5CF6" },
];

impl ColorLabel {
    pub fn color(&self) -> Hsla {
        Hsla::parse_hex(self.hex).expect("label swatches are valid hex")
    }
}

/// The active ring of a label dot (`.fd.on` / `.bench-dot.on`): a 2 px gap in the panel
/// colour, then a 1.5 px ring in the dot's own colour.
pub fn dot_ring(dot: Hsla, gap: Hsla) -> Vec<BoxShadow> {
    let ring = |color, spread| BoxShadow {
        color,
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: spread,
        inset: false,
    };
    vec![ring(dot, px(3.5)), ring(gap, px(2.))]
}

/// Thousands separators, as `toLocaleString()` printed counts in the React shell
/// (en-US grouping: `12,345`).
pub fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_counts_match_to_locale_string() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1234567), "1,234,567");
    }

    #[test]
    fn standard_palette_parses_into_colors() {
        let c = Colors::from_tokens(&standard());
        assert_eq!(c.accent, Hsla::parse_hex("#E0A458").unwrap());
        assert!(c.sel.a < 0.2, "sel carries its alpha: {:?}", c.sel);
    }
}
