//! The Darkroom's Lens section text — `lensHint` from `src/components/darkroom/LensRail.tsx`
//! (docs/plans/lens-corrections). The switch itself is view code and ports with the rail.

use chairphoto_core::develop_source::LensInfo;

/// What the switch corrects, in words: the tables this file carries.
pub fn lens_hint(info: &LensInfo) -> String {
    let fixes: Vec<&str> = [
        (info.vignetting, "brightens the corners the lens darkened"),
        (info.distortion, "straightens lines the lens bent"),
        (info.chromatic, "removes colour fringing at the edges"),
    ]
    .into_iter()
    .filter_map(|(on, text)| on.then_some(text))
    .collect();
    let list = match fixes.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
        Some((only, _)) => only.to_string(),
        None => String::new(),
    };
    format!("{} tables: {list}.", info.source)
}

#[cfg(test)]
mod tests {
    // --- src/components/darkroom/__tests__/lensRail.test.ts (1 case) ---
    use super::*;

    fn info(vignetting: bool, distortion: bool, chromatic: bool) -> LensInfo {
        LensInfo { source: "Sony built-in".into(), vignetting, distortion, chromatic }
    }

    #[test]
    fn names_every_correction_the_files_tables_allow() {
        assert_eq!(
            lens_hint(&info(true, true, true)),
            "Sony built-in tables: brightens the corners the lens darkened, straightens lines the lens bent and removes colour fringing at the edges."
        );
        assert_eq!(lens_hint(&info(false, true, false)), "Sony built-in tables: straightens lines the lens bent.");
    }
}
