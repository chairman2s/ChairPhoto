//! Embedded assets: ChairPhoto's fonts, the Lucide icons the app draws that gpui-kit's
//! default icon bundle lacks ([`ExtraIcons`]), and that bundle for everything else.
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

// **The extra icons** are Lucide's (ISC; the Feather-derived ones MIT), embedded from the
// `gpui-kit-assets` crate's copy, which carries the upstream licence (`LICENSE-LUCIDE`) — the
// same source and terms as the default bundle (and as every icon gpui-component itself draws,
// which the app's own icons sit alongside on screen). A copy of that licence text travels in
// this repo too, at `crates/app/assets/icons/LICENSE-LUCIDE`, for `packaging/PKGBUILD` to
// install (#177; MODULE_LICENSING.md). gpui-kit 0.7.0's default bundle
// (`default-icons.txt`) has only the icons its own widgets use; an `IconName` outside it and
// this list draws nothing (#173). `every_icon_the_app_names_is_served` checks the sources.
gpui_kit::assets::icon_assets!(
    pub ExtraIcons,
    [
        // The rail: Library and Develop (`shell::sidebar::RailIcon`).
        LayoutGrid,
        SlidersHorizontal,
        // Module main views on the rail: Statistics, People.
        ChartNoAxesColumn,
        UserGroup,
        // The loupe's rotate-left chip (its rotate-right, `RotateCw`, is in the default bundle).
        RotateCcw,
    ]
);

/// Every embedded font, by its asset path.
const FONTS: &[(&str, &[u8])] = &[
    ("fonts/InstrumentSans-Regular.ttf", include_bytes!("../assets/fonts/InstrumentSans-Regular.ttf")),
    ("fonts/InstrumentSans-Medium.ttf", include_bytes!("../assets/fonts/InstrumentSans-Medium.ttf")),
    ("fonts/InstrumentSans-SemiBold.ttf", include_bytes!("../assets/fonts/InstrumentSans-SemiBold.ttf")),
    ("fonts/InstrumentSans-Bold.ttf", include_bytes!("../assets/fonts/InstrumentSans-Bold.ttf")),
    ("fonts/InstrumentSerif-Regular.ttf", include_bytes!("../assets/fonts/InstrumentSerif-Regular.ttf")),
    ("fonts/InstrumentSerif-Italic.ttf", include_bytes!("../assets/fonts/InstrumentSerif-Italic.ttf")),
];

/// The app's `AssetSource`: [`FONTS`] under `fonts/`, the extra icons ([`ExtraIcons`]), and
/// gpui-kit's default icon bundle for the rest
/// (the gpui-component widgets load their icons through it).
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = FONTS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        if path.trim_end_matches('/') == "fonts" {
            return Ok(FONTS.iter().map(|(p, _)| SharedString::from(*p)).collect());
        }
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
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

    /// `IconName::ChartNoAxesColumn` → `icons/chart-no-axes-column.svg` (`IconName::path`'s
    /// kebab case: a hyphen before each capital and each digit run).
    fn icon_path(name: &str) -> String {
        let mut out = String::from("icons/");
        let mut prev: Option<char> = None;
        for c in name.chars() {
            let starts_word = c.is_ascii_uppercase() || (c.is_ascii_digit() && !prev.is_some_and(|p| p.is_ascii_digit()));
            if starts_word && prev.is_some() {
                out.push('-');
            }
            out.push(c.to_ascii_lowercase());
            prev = Some(c);
        }
        out + ".svg"
    }

    /// #173: every `IconName::…` the app's sources name is served — the rail's Statistics
    /// button drew nothing because its icon was outside the default bundle. Scans
    /// `crates/app/src` for the names, so a new icon outside the bundle fails here until it
    /// joins [`ExtraIcons`].
    #[test]
    fn every_icon_the_app_names_is_served() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        let mut names = std::collections::BTreeSet::new();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            for (at, _) in text.match_indices("IconName::") {
                let name: String =
                    text[at + "IconName::".len()..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
                if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                    names.insert(name);
                }
            }
        }
        for must in ["ChartNoAxesColumn", "Map", "UserGroup", "Network"] {
            assert!(names.contains(must), "the scan missed {must}: {names:?}");
        }
        assert_eq!(icon_path("ChartNoAxesColumn"), "icons/chart-no-axes-column.svg");
        assert_eq!(icon_path("Building2"), "icons/building-2.svg");
        for name in &names {
            let path = icon_path(name);
            assert!(matches!(Assets.load(&path), Ok(Some(_))), "IconName::{name} ({path}) is not served");
        }
    }
}
