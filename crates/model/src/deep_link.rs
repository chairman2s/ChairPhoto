//! `chairphoto://` deep links. Port of the two URL matchers in `src/modules/api.ts`
//! (`onDeepLinkPhoto`, `onDeepLinkTag`):
//!
//! | URL | Link |
//! |---|---|
//! | `chairphoto://<uuid>` | [`DeepLink::Photo`], view [`DeepLinkView::Grid`] |
//! | `chairphoto://<uuid>/loupe` | [`DeepLink::Photo`], view [`DeepLinkView::Loupe`] |
//! | `chairphoto://<uuid>/develop` | [`DeepLink::Photo`], view [`DeepLinkView::Develop`] |
//! | `chairphoto://tag/<uuid>` | [`DeepLink::Tag`] |
//!
//! The TypeScript regexes, verbatim, both with the `i` flag and applied to `url.trim()`:
//!
//! ```text
//! ^chairphoto:\/{2,3}([0-9a-fA-F-]{36})(?:\/(loupe|develop))?\/?$
//! ^chairphoto:\/{2,3}tag\/([0-9a-fA-F-]{36})\/?$
//! ```
//!
//! Semantic choices against the TypeScript, each reproducing it rather than improving on it:
//! - Two or three slashes (`chairphoto:///<uuid>` is what some launchers produce), and one
//!   optional trailing slash.
//! - The scheme and the words `tag`, `loupe`, `develop` match ASCII-case-insensitively, which
//!   is what a non-`u` JavaScript `/i` does for ASCII letters. The uuid is returned lowercase
//!   (the catalog stores it lowercase).
//! - The uuid is **any 36 characters from `[0-9a-fA-F-]`**, not a well-formed uuid: the
//!   regex did not check the dash positions, and a malformed one simply finds no photo.
//! - `trim` is JavaScript's ([`js_trim`]), so a leading BOM is ignored.
//! - The two matchers never overlap: `tag` is not 36 hex characters.
//!
//! The React app had no tests for these regexes; the tests below pin the regexes' behaviour
//! case by case, so the port is checked against the regex, not against itself.

use crate::js_compat::js_trim;

/// Which surface a photo link asks for: the Library grid (default), the inline loupe, or the
/// Develop editor (`DeepLinkView` in api.ts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepLinkView {
    Grid,
    Loupe,
    Develop,
}

impl DeepLinkView {
    /// The name api.ts used (`"grid" | "loupe" | "develop"`).
    pub fn as_str(self) -> &'static str {
        match self {
            DeepLinkView::Grid => "grid",
            DeepLinkView::Loupe => "loupe",
            DeepLinkView::Develop => "develop",
        }
    }
}

/// A parsed `chairphoto://` link. Uuids are lowercase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepLink {
    /// Select the photo with this uuid, then open `view`.
    Photo { uuid: String, view: DeepLinkView },
    /// Filter the Library to the tag with this uuid.
    Tag { uuid: String },
}

const UUID_LEN: usize = 36;

/// Parse one URL, as both api.ts matchers together did. `None` for anything else.
pub fn parse(url: &str) -> Option<DeepLink> {
    let rest = strip_prefix_ascii_ci(js_trim(url), "chairphoto:")?;
    let slashes = rest.bytes().take_while(|&b| b == b'/').count();
    if !(2..=3).contains(&slashes) {
        return None;
    }
    let rest = &rest[slashes..];
    if let Some((uuid, tail)) = take_uuid(rest) {
        // `(?:\/(loupe|develop))?\/?$`: drop the one optional trailing slash, then what is
        // left is nothing or `/<view>`.
        let view = match tail.strip_suffix('/').unwrap_or(tail) {
            "" => DeepLinkView::Grid,
            word => match word.strip_prefix('/') {
                Some(w) if w.eq_ignore_ascii_case("loupe") => DeepLinkView::Loupe,
                Some(w) if w.eq_ignore_ascii_case("develop") => DeepLinkView::Develop,
                _ => return None,
            },
        };
        return Some(DeepLink::Photo { uuid, view });
    }
    let rest = strip_prefix_ascii_ci(rest, "tag/")?;
    let (uuid, tail) = take_uuid(rest)?;
    matches!(tail, "" | "/").then_some(DeepLink::Tag { uuid })
}

/// `[0-9a-fA-F-]{36}` at the start of `s`: the uuid (lowercased) and what follows.
fn take_uuid(s: &str) -> Option<(String, &str)> {
    let head = s.as_bytes().get(..UUID_LEN)?;
    if !head.iter().all(|b| b.is_ascii_hexdigit() || *b == b'-') {
        return None;
    }
    // All ASCII, so byte 36 is a char boundary.
    Some((s[..UUID_LEN].to_ascii_lowercase(), &s[UUID_LEN..]))
}

fn strip_prefix_ascii_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
    const U_UPPER: &str = "0A1B2C3D-4E5F-4A6B-8C7D-9E0F1A2B3C4D";

    fn photo(view: DeepLinkView) -> Option<DeepLink> {
        Some(DeepLink::Photo { uuid: U.into(), view })
    }

    fn tag() -> Option<DeepLink> {
        Some(DeepLink::Tag { uuid: U.into() })
    }

    #[test]
    fn a_bare_uuid_opens_the_grid() {
        assert_eq!(parse(&format!("chairphoto://{U}")), photo(DeepLinkView::Grid));
    }

    #[test]
    fn loupe_and_develop_suffixes_pick_the_view() {
        assert_eq!(parse(&format!("chairphoto://{U}/loupe")), photo(DeepLinkView::Loupe));
        assert_eq!(parse(&format!("chairphoto://{U}/develop")), photo(DeepLinkView::Develop));
    }

    #[test]
    fn two_or_three_slashes_but_not_one_or_four() {
        assert_eq!(parse(&format!("chairphoto:///{U}")), photo(DeepLinkView::Grid));
        assert_eq!(parse(&format!("chairphoto:///{U}/loupe")), photo(DeepLinkView::Loupe));
        assert_eq!(parse(&format!("chairphoto:///tag/{U}")), tag());
        assert_eq!(parse(&format!("chairphoto:/{U}")), None);
        assert_eq!(parse(&format!("chairphoto:{U}")), None);
        assert_eq!(parse(&format!("chairphoto:////{U}")), None);
        assert_eq!(parse(&format!("chairphoto:////tag/{U}")), None);
    }

    #[test]
    fn one_trailing_slash_is_allowed() {
        assert_eq!(parse(&format!("chairphoto://{U}/")), photo(DeepLinkView::Grid));
        assert_eq!(parse(&format!("chairphoto://{U}/develop/")), photo(DeepLinkView::Develop));
        assert_eq!(parse(&format!("chairphoto://tag/{U}/")), tag());
        assert_eq!(parse(&format!("chairphoto://{U}//")), None);
        assert_eq!(parse(&format!("chairphoto://{U}/loupe//")), None);
        assert_eq!(parse(&format!("chairphoto://tag/{U}//")), None);
    }

    #[test]
    fn any_casing_matches_and_the_uuid_comes_back_lowercase() {
        assert_eq!(parse(&format!("ChairPhoto://{U_UPPER}/LOUPE")), photo(DeepLinkView::Loupe));
        assert_eq!(parse(&format!("CHAIRPHOTO://{U}/Develop")), photo(DeepLinkView::Develop));
        assert_eq!(parse(&format!("chairphoto://TAG/{U_UPPER}")), tag());
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_the_javascript_way() {
        assert_eq!(parse(&format!("  chairphoto://{U}\n")), photo(DeepLinkView::Grid));
        assert_eq!(parse(&format!("\u{FEFF}chairphoto://tag/{U}\t")), tag());
        // U+0085 is not JavaScript whitespace, so it is not trimmed and the match fails.
        assert_eq!(parse(&format!("chairphoto://{U}\u{0085}")), None);
        // Inner whitespace is never trimmed.
        assert_eq!(parse(&format!("chairphoto:// {U}")), None);
    }

    #[test]
    fn the_uuid_is_exactly_36_characters_from_the_hex_and_dash_class() {
        assert_eq!(parse(&format!("chairphoto://{}", &U[..35])), None);
        assert_eq!(parse(&format!("chairphoto://{U}a")), None);
        assert_eq!(parse(&format!("chairphoto://{}g", &U[..35])), None);
        assert_eq!(parse(&format!("chairphoto://tag/{}", &U[..35])), None);
        assert_eq!(parse(&format!("chairphoto://tag/{U}0")), None);
        // The regex never checked dash positions: 36 dashes is a "uuid" (that finds no photo).
        let dashes = "-".repeat(36);
        assert_eq!(
            parse(&format!("chairphoto://{dashes}")),
            Some(DeepLink::Photo { uuid: dashes.clone(), view: DeepLinkView::Grid })
        );
        let undashed = "0123456789abcdef0123456789abcdef0123";
        assert_eq!(parse(&format!("chairphoto://tag/{undashed}")), Some(DeepLink::Tag { uuid: undashed.into() }));
    }

    #[test]
    fn unknown_views_queries_and_fragments_do_not_match() {
        assert_eq!(parse(&format!("chairphoto://{U}/compare")), None);
        assert_eq!(parse(&format!("chairphoto://{U}/loupe/develop")), None);
        assert_eq!(parse(&format!("chairphoto://{U}?view=loupe")), None);
        assert_eq!(parse(&format!("chairphoto://{U}#loupe")), None);
        assert_eq!(parse(&format!("chairphoto://tag/{U}/loupe")), None);
        assert_eq!(parse(&format!("chairphoto://tags/{U}")), None);
        assert_eq!(parse(&format!("chairphoto://album/{U}")), None);
    }

    #[test]
    fn other_schemes_and_non_links_do_not_match() {
        assert_eq!(parse(&format!("https://{U}")), None);
        assert_eq!(parse(&format!("xchairphoto://{U}")), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("chairphoto://"), None);
        assert_eq!(parse("chairphoto://tag/"), None);
        assert_eq!(parse("/home/me/Pictures/a.nef"), None);
        // Non-ASCII where the uuid should be must not panic on a char boundary.
        assert_eq!(parse("chairphoto://ééééééééééééééééééééééééééééééééééé"), None);
        assert_eq!(parse("chairphoto://tag/éééééééééééééééééééééééééééééééééé"), None);
        assert_eq!(parse("chairphotö://x"), None);
    }

    #[test]
    fn the_view_names_are_the_typescript_ones() {
        assert_eq!(DeepLinkView::Grid.as_str(), "grid");
        assert_eq!(DeepLinkView::Loupe.as_str(), "loupe");
        assert_eq!(DeepLinkView::Develop.as_str(), "develop");
    }
}
