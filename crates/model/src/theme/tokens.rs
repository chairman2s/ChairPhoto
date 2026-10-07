//! The semantic design-token contract for ChairPhoto's UI. Port of the types in
//! `src/theme/tokens.ts`.
//!
//! Every themeable surface reads one of these tokens rather than a hardcoded color. A
//! palette (ChairPhoto Standard, or one derived from the user's Omarchy theme) is one
//! [`ThemeTokens`] value.
//!
//! Semantic choices against the TypeScript:
//! - `ThemeTokens` was `Record<TokenName, string>`; here it is a struct with one field per
//!   token (`font-sans` → `font_sans`), so a missing token is a compile error. The CSS names
//!   and their declaration order survive as [`TOKEN_NAMES`] and [`ThemeTokens::entries`].
//! - Values stay strings: most are `#rrggbb`, but `sel`/`scrim` in Standard are `rgba(…)` and
//!   the font tokens are CSS font stacks. Turning them into GPUI colors and fonts is the
//!   view's job.
//! - `ThemeMode` is the backend's own [`OmarchyMode`] (`"light"` / `"dark"`), re-exported,
//!   since the TS type was that same two-value string.

pub use chairphoto_core::appearance::OmarchyMode as ThemeMode;

/// Every token name, in the order the app's `:root` block declared them.
pub const TOKEN_NAMES: [&str; 19] = [
    "canvas",
    "panel",
    "elev",
    "well",
    "border",
    "line",
    "txt",
    "dim",
    "mute",
    "accent",
    "onaccent",
    "sel",
    "ok",
    "onok",
    "danger",
    "rating",
    "scrim",
    "font-sans",
    "font-display",
];

/// One value per token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeTokens {
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
    pub font_sans: String,
    pub font_display: String,
}

impl ThemeTokens {
    /// `(token name, value)` for every token, in [`TOKEN_NAMES`] order — what
    /// `Object.entries(tokens)` iterated in the TypeScript.
    pub fn entries(&self) -> [(&'static str, &str); 19] {
        [
            (TOKEN_NAMES[0], &self.canvas),
            (TOKEN_NAMES[1], &self.panel),
            (TOKEN_NAMES[2], &self.elev),
            (TOKEN_NAMES[3], &self.well),
            (TOKEN_NAMES[4], &self.border),
            (TOKEN_NAMES[5], &self.line),
            (TOKEN_NAMES[6], &self.txt),
            (TOKEN_NAMES[7], &self.dim),
            (TOKEN_NAMES[8], &self.mute),
            (TOKEN_NAMES[9], &self.accent),
            (TOKEN_NAMES[10], &self.onaccent),
            (TOKEN_NAMES[11], &self.sel),
            (TOKEN_NAMES[12], &self.ok),
            (TOKEN_NAMES[13], &self.onok),
            (TOKEN_NAMES[14], &self.danger),
            (TOKEN_NAMES[15], &self.rating),
            (TOKEN_NAMES[16], &self.scrim),
            (TOKEN_NAMES[17], &self.font_sans),
            (TOKEN_NAMES[18], &self.font_display),
        ]
    }
}
