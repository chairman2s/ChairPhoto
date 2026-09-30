//! Embedded assets: ChairPhoto's fonts, and gpui-kit's default icons for everything else.
//!
//! GPUI does not load fonts from the `AssetSource` by itself (shell-apis.md § 2): the app lists
//! `fonts/` and hands the bytes to `add_fonts` ([`load_fonts`]). They must be TTF/OTF — WOFF2
//! is accepted and silently ignored.
//!
//! **The font files** (`crates/app/assets/fonts/`) are the faces `src/main.tsx` loads from
//! `@fontsource/instrument-sans` and `@fontsource/instrument-serif` 5.3.0 — Sans 400/500/600/
//! 700, Serif 400 and 400 italic — converted from those packages' `latin` and `latin-ext`
//! WOFF2 subsets: `woff2_decompress` each, then fontTools' `Merger` to join the two subsets
//! of a face into one TTF (321 code points for Sans, 319 for Serif). Both families are under
//! the SIL Open Font License 1.1 with no Reserved Font Name; the licence texts sit beside the
//! fonts (`OFL-InstrumentSans.txt`, `OFL-InstrumentSerif.txt`).

use gpui_kit::{App, AssetSource, Result, SharedString};
use std::borrow::Cow;

/// Every embedded font, by its asset path.
const FONTS: &[(&str, &[u8])] = &[
    ("fonts/InstrumentSans-Regular.ttf", include_bytes!("../assets/fonts/InstrumentSans-Regular.ttf")),
    ("fonts/InstrumentSans-Medium.ttf", include_bytes!("../assets/fonts/InstrumentSans-Medium.ttf")),
    ("fonts/InstrumentSans-SemiBold.ttf", include_bytes!("../assets/fonts/InstrumentSans-SemiBold.ttf")),
    ("fonts/InstrumentSans-Bold.ttf", include_bytes!("../assets/fonts/InstrumentSans-Bold.ttf")),
    ("fonts/InstrumentSerif-Regular.ttf", include_bytes!("../assets/fonts/InstrumentSerif-Regular.ttf")),
    ("fonts/InstrumentSerif-Italic.ttf", include_bytes!("../assets/fonts/InstrumentSerif-Italic.ttf")),
];

/// The app's `AssetSource`: [`FONTS`] under `fonts/`, gpui-kit's icon bundle for the rest
/// (the gpui-component widgets load their icons through it).
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = FONTS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        if path.trim_end_matches('/') == "fonts" {
            return Ok(FONTS.iter().map(|(p, _)| SharedString::from(*p)).collect());
        }
        gpui_kit::assets::Assets.list(path)
    }
}

/// Register the embedded fonts with the text system. Checks that both families actually
/// arrived, since `add_fonts` reports success for bytes it cannot use.
pub fn load_fonts(cx: &App) -> std::result::Result<(), String> {
    let fonts = FONTS.iter().map(|(_, bytes)| Cow::Borrowed(*bytes)).collect();
    cx.text_system().add_fonts(fonts).map_err(|e| e.to_string())?;
    let names = cx.text_system().all_font_names();
    for family in [crate::theme::FONT_SANS, crate::theme::FONT_DISPLAY] {
        if !names.iter().any(|n| n == family) {
            return Err(format!("font family {family:?} did not load"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fonts_are_listed_and_served_as_ttf() {
        let listed = Assets.list("fonts/").unwrap();
        assert_eq!(listed.len(), FONTS.len());
        for path in listed {
            let bytes = Assets.load(&path).unwrap().expect("listed font loads");
            // TrueType outlines: sfnt version 0x00010000 (not a WOFF2 'wOF2' wrapper).
            assert_eq!(&bytes[..4], &[0, 1, 0, 0], "{path} is not a TTF");
        }
    }
}
