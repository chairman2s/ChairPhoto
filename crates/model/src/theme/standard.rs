//! ChairPhoto Standard: the app-owned warm dark palette, and the default look. Port of
//! `src/theme/standard.ts`, values verbatim (including their casing — `#10B981` stays
//! upper-case, which the Omarchy mapping's fallbacks rely on being copied as-is).

use super::tokens::{ThemeMode, ThemeTokens};

pub const CANVAS: &str = "#14120F";
pub const PANEL: &str = "#1B1815";
pub const ELEV: &str = "#23201C";
pub const WELL: &str = "#0D0C0A";
pub const BORDER: &str = "#2E2A25";
pub const LINE: &str = "#241F1B";
pub const TXT: &str = "#EFE9E0";
pub const DIM: &str = "#A8A093";
pub const MUTE: &str = "#6F675C";
pub const ACCENT: &str = "#E0A458";
pub const ONACCENT: &str = "#241C10";
pub const SEL: &str = "rgba(224, 164, 88, 0.14)";
pub const OK: &str = "#10B981";
pub const ONOK: &str = "#04150F";
pub const DANGER: &str = "#F87171";
pub const RATING: &str = "#FFD700";
pub const SCRIM: &str = "rgba(0, 0, 0, 0.66)";
pub const FONT_SANS: &str = r#""Instrument Sans", system-ui, -apple-system, "Segoe UI", sans-serif"#;
pub const FONT_DISPLAY: &str = r#""Instrument Serif", Georgia, serif"#;

/// Standard's mode.
pub const STANDARD_MODE: ThemeMode = ThemeMode::Dark;

/// ChairPhoto Standard as a token set.
pub fn standard() -> ThemeTokens {
    ThemeTokens {
        canvas: CANVAS.into(),
        panel: PANEL.into(),
        elev: ELEV.into(),
        well: WELL.into(),
        border: BORDER.into(),
        line: LINE.into(),
        txt: TXT.into(),
        dim: DIM.into(),
        mute: MUTE.into(),
        accent: ACCENT.into(),
        onaccent: ONACCENT.into(),
        sel: SEL.into(),
        ok: OK.into(),
        onok: ONOK.into(),
        danger: DANGER.into(),
        rating: RATING.into(),
        scrim: SCRIM.into(),
        font_sans: FONT_SANS.into(),
        font_display: FONT_DISPLAY.into(),
    }
}
