//! The Obsidian module's notes and links. Port of the pure half of
//! `src/modules/plugins/obsidian.tsx` (docs/obsidian.md): what a companion note for a photo or
//! a tag is called, what it initially contains, the `obsidian://` URIs that create and open
//! it, and the record the module keeps of it.
//!
//! **ChairPhoto never writes into the vault.** Obsidian creates the note itself from the
//! `obsidian://new` URI ([`new_uri`]); nothing here touches a file. After creation the note is
//! the user's: "Forget" drops only the module's record.
//!
//! The `chairphoto://` links a note carries come from [`DeepLink::url`], so they are what the
//! app's own parser ([`crate::deep_link::parse`]) reads back.
//!
//! Semantic choices against the TypeScript, each reproducing it unless stated:
//! - Words split on JavaScript's `\s` ([`is_js_whitespace`]), so a no-break space separates
//!   words in a tag segment; the first character of each word is uppercased with Rust's
//!   `char::to_uppercase`, which agrees with `toUpperCase` (`ß` → `SS`).
//! - `slice(0, 10)` / `slice(0, 8)` count UTF-16 units; the capture time and uuid are ASCII,
//!   so taking characters is the same.
//! - Numbers interpolate as JavaScript does ([`number_to_string`]): `f/4`, not `f/4.0`.
//! - **Deliberate change: the settings are validated** ([`vault_name`], [`notes_folder`]). The
//!   React panel saved any text; a path where the vault's name belongs, or a folder that
//!   climbs out of the vault (`..`), now gets a reason instead of a note Obsidian cannot make.
//!   The folder's surrounding slashes are dropped, so `Notes/` no longer makes `Notes//…`.

use crate::deep_link::{DeepLink, DeepLinkView};
use crate::js_compat::{encode_uri_component, is_js_whitespace, js_trim, number_to_string};
use chairphoto_core::catalog::{Photo, Tag, TagTerm};
use serde::{Deserialize, Serialize};

/// The module's id: its settings namespace (`obsidian.<key>`).
pub const MODULE_ID: &str = "obsidian";
/// Setting key (`obsidian.vault`): the vault's name as Obsidian knows it.
pub const VAULT_KEY: &str = "vault";
/// Setting key (`obsidian.folder`): the notes folder inside the vault.
pub const FOLDER_KEY: &str = "folder";
/// The notes folder when none is set.
pub const DEFAULT_FOLDER: &str = "ChairPhoto";
/// Photo and tag aliases written into a note, at most (the note travels inside a URI).
pub const LIST_CAP: usize = 20;
/// What "Create note" says when no vault is set (React's toast).
pub const NO_VAULT: &str = "Set the Obsidian vault name in Preferences → Modules → Obsidian";

/// The settings key (without the module's prefix) of a photo's note record.
pub fn note_key(photo_uuid: &str) -> String {
    format!("note.{photo_uuid}")
}

/// The settings key (without the module's prefix) of a tag's note record.
pub fn tag_note_key(tag_uuid: &str) -> String {
    format!("tagnote.{tag_uuid}")
}

/// What the module remembers of a note it had Obsidian create (`NoteRecord`), stored as JSON
/// under [`note_key`] / [`tag_note_key`]. The JSON is the React app's, field for field, so a
/// record either app wrote reads in the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteRecord {
    pub vault: String,
    /// Vault-relative note path without `.md` (what `obsidian://open` wants).
    pub file: String,
    /// Milliseconds since the epoch.
    #[serde(rename = "createdAt")]
    pub created_at: i64,
}

impl NoteRecord {
    /// A stored record. Empty (what "Forget" writes; there is no delete) or unreadable is no
    /// note — React's `JSON.parse` failure left the panel without one as well.
    pub fn parse(raw: &str) -> Option<NoteRecord> {
        if raw.is_empty() {
            return None;
        }
        serde_json::from_str(raw).ok()
    }

    /// The JSON React's `JSON.stringify` wrote: `{"vault":…,"file":…,"createdAt":…}`.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a NoteRecord serializes")
    }

    /// The note's name: the file's last path segment, what the panel shows.
    pub fn name(&self) -> &str {
        last_segment(&self.file)
    }

    /// `obsidian://open` for this note.
    pub fn open_uri(&self) -> String {
        open_uri(&self.vault, &self.file)
    }
}

/// `p.split("/").pop()`.
fn last_segment(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// An Obsidian tag for a ChairPhoto tag path (`obsidianTag`): Obsidian tags cannot contain
/// spaces, so each word of a segment is capitalised and joined, keeping the `/` hierarchy —
/// `Street Photography/Old Town` → `StreetPhotography/OldTown`.
pub fn obsidian_tag(full_path: &str) -> String {
    full_path
        .split('/')
        .map(|segment| {
            segment
                .split(is_js_whitespace)
                .filter(|w| !w.is_empty())
                .map(|w| {
                    let mut chars = w.chars();
                    let first = chars.next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default();
                    first + chars.as_str()
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A photo note's name (`noteName`): capture date + file stem + uuid prefix, readable and
/// unique across cards that reuse `DSC` numbers — `2026-07-04 _81A8352 3f9c1a7e`.
pub fn note_name(photo: &Photo) -> String {
    let date: String = photo.capture_time.as_deref().unwrap_or("").chars().take(10).collect();
    let date = if date.is_empty() { "undated".to_string() } else { date };
    // `.replace(/\.[^.]+$/, "")`: drop a final `.ext` with at least one character.
    let file = last_segment(&photo.path);
    let stem = match file.rfind('.') {
        Some(dot) if dot + 1 < file.len() => &file[..dot],
        _ => file,
    };
    let prefix: String = photo.uuid.chars().take(8).collect();
    format!("{date} {stem} {prefix}")
}

/// A photo note's initial text (`noteContent`): frontmatter with what the catalog knows
/// (fields the photo lacks are left out, ≤ [`LIST_CAP`] tags) and the two links back.
pub fn note_content(photo: &Photo, tags: &[Tag]) -> String {
    let exposure = [
        photo.aperture.map(|a| format!("f/{}", number_to_string(a))),
        photo.shutter_speed.as_deref().filter(|s| !s.is_empty()).map(|s| format!("{s}s")),
        photo.iso.map(|i| format!("ISO {i}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let truthy = |v: &Option<String>| v.as_deref().filter(|s| !s.is_empty()).map(str::to_string);
    let mut lines: Vec<String> = vec![
        "---".into(),
        "type: ChairPhoto".into(),
        format!("chairphoto: {}", photo.uuid),
        format!("photo: {}", last_segment(&photo.path)),
    ];
    lines.extend(truthy(&photo.capture_time).map(|v| format!("captured: {v}")));
    lines.extend(truthy(&photo.camera_model).map(|v| format!("camera: {v}")));
    lines.extend(truthy(&photo.lens).map(|v| format!("lens: {v}")));
    if !exposure.is_empty() {
        lines.push(format!("exposure: {exposure}"));
    }
    if photo.rating > 0 {
        lines.push(format!("rating: {}", photo.rating));
    }
    if !tags.is_empty() {
        lines.push("tags:".into());
        lines.extend(tags.iter().take(LIST_CAP).map(|t| format!("  - {}", obsidian_tag(&t.full_path))));
    }
    let grid = DeepLink::Photo { uuid: photo.uuid.clone(), view: DeepLinkView::Grid }.url();
    let loupe = DeepLink::Photo { uuid: photo.uuid.clone(), view: DeepLinkView::Loupe }.url();
    lines.extend(["---".into(), String::new(), format!("[Open in ChairPhoto]({grid}) · [Open in loupe]({loupe})"), String::new()]);
    lines.join("\n")
}

/// A tag note's name (`tagNoteName`): the leaf name made filesystem-safe plus the uuid
/// prefix, so two `Bridge` tags under different parents never collide.
pub fn tag_note_name(tag: &Tag) -> String {
    let name: String =
        tag.name.chars().map(|c| if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '-' } else { c }).collect();
    let prefix: String = tag.uuid.chars().take(8).collect();
    format!("{name} {prefix}")
}

/// A tag's terms as note aliases: trimmed, blanks dropped, first occurrence kept
/// (`[...new Set(terms.map(t => t.text.trim()).filter(Boolean))]`).
pub fn aliases(terms: &[TagTerm]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in terms {
        let text = js_trim(&t.text);
        if !text.is_empty() && !out.iter().any(|a| a == text) {
            out.push(text.to_string());
        }
    }
    out
}

/// A tag note's initial text (`tagNoteContent`): frontmatter with the tag's uuid, path,
/// ≤ [`LIST_CAP`] aliases and its Obsidian tag, the link back, then the description.
pub fn tag_note_content(tag: &Tag, aliases: &[String]) -> String {
    let mut lines: Vec<String> = vec![
        "---".into(),
        "type: ChairPhotoTag".into(),
        format!("chairphoto-tag: {}", tag.uuid),
        format!("tag: {}", tag.full_path),
    ];
    if !aliases.is_empty() {
        lines.push("aliases:".into());
        lines.extend(aliases.iter().take(LIST_CAP).map(|a| format!("  - {a}")));
    }
    lines.push("tags:".into());
    lines.push(format!("  - {}", obsidian_tag(&tag.full_path)));
    let link = DeepLink::Tag { uuid: tag.uuid.clone() }.url();
    lines.extend(["---".into(), String::new(), format!("[Show photos in ChairPhoto]({link})"), String::new()]);
    let description = js_trim(&tag.description);
    if !description.is_empty() {
        lines.push(description.to_string());
        lines.push(String::new());
    }
    lines.join("\n")
}

/// A photo note's vault-relative file: `<folder>/<name>`.
pub fn note_file(folder: &str, photo: &Photo) -> String {
    format!("{folder}/{}", note_name(photo))
}

/// A tag note's vault-relative file: `<folder>/Tags/<name>`.
pub fn tag_note_file(folder: &str, tag: &Tag) -> String {
    format!("{folder}/Tags/{}", tag_note_name(tag))
}

/// `obsidian://new`: Obsidian creates `file` in `vault` with `content`.
pub fn new_uri(vault: &str, file: &str, content: &str) -> String {
    format!(
        "obsidian://new?vault={}&file={}&content={}",
        encode_uri_component(vault),
        encode_uri_component(file),
        encode_uri_component(content)
    )
}

/// `obsidian://open`: Obsidian opens `file` in `vault`.
pub fn open_uri(vault: &str, file: &str) -> String {
    format!("obsidian://open?vault={}&file={}", encode_uri_component(vault), encode_uri_component(file))
}

/// The vault setting as saved: trimmed; empty means not set. A path is refused — Obsidian
/// takes the vault's *name* (docs/obsidian.md), and a name holds no `/` or `\`.
pub fn vault_name(input: &str) -> Result<String, String> {
    let name = js_trim(input);
    if name.contains(['/', '\\']) {
        return Err("Enter the vault's name as Obsidian shows it, not its path.".into());
    }
    Ok(name.to_string())
}

/// The notes-folder setting as saved: trimmed, surrounding `/` dropped; empty means the
/// default ([`DEFAULT_FOLDER`]). A folder that is not plainly inside the vault (`..`, `.`,
/// an empty segment, a `\`) is refused.
pub fn notes_folder(input: &str) -> Result<String, String> {
    let folder = js_trim(input).trim_matches('/');
    let bad = folder.contains('\\') || (!folder.is_empty() && folder.split('/').any(|s| matches!(js_trim(s), "" | "." | "..")));
    if bad {
        return Err("The notes folder must be a folder inside the vault, like ChairPhoto or Notes/Photos.".into());
    }
    Ok(folder.to_string())
}

/// The folder a new note goes in, from the stored setting: [`notes_folder`], else
/// [`DEFAULT_FOLDER`].
pub fn folder_or_default(stored: Option<&str>) -> Result<String, String> {
    let folder = notes_folder(stored.unwrap_or(""))?;
    Ok(if folder.is_empty() { DEFAULT_FOLDER.to_string() } else { folder })
}

#[cfg(test)]
mod tests {
    //! Expected strings are the React functions' output: `obsidian.tsx`'s `obsidianTag`,
    //! `noteName`, `noteContent`, `tagNoteName`, `tagNoteContent` and the URI templates,
    //! copied verbatim into a Node script and run on these same inputs (Node v25.2.1).
    use super::*;
    use crate::deep_link::parse;
    use chairphoto_core::catalog::PickState;

    const U: &str = "3f9c1a7e-0b1c-4d2e-8f3a-112233445566";

    fn photo(path: &str) -> Photo {
        Photo {
            id: 1,
            uuid: U.into(),
            path: path.into(),
            rating: 0,
            label: String::new(),
            pick_state: PickState::None,
            capture_time: None,
            width: None,
            height: None,
            camera_model: None,
            lens: None,
            aperture: None,
            shutter_speed: None,
            iso: None,
            external_editors: String::new(),
            thumbnail_path: None,
            stack_count: 0,
            stack_parent_id: None,
            metadata_ready: 1,
            sharpness: None,
            sharpness_method: None,
            burst_flag: None,
            version_count: 0,
            cover_token: None,
            cover_pin: Default::default(),
        }
    }

    fn full() -> Photo {
        Photo {
            capture_time: Some("2026-07-04T09:30:00".into()),
            camera_model: Some("ILCE-7RM6".into()),
            lens: Some("FE 24-70mm F2.8 GM".into()),
            aperture: Some(2.8),
            shutter_speed: Some("1/500".into()),
            iso: Some(400),
            rating: 4,
            ..photo("2026/07/_81A8352.ARW")
        }
    }

    fn tag(full_path: &str) -> Tag {
        Tag {
            id: 1,
            uuid: U.into(),
            name: full_path.rsplit('/').next().unwrap().into(),
            full_path: full_path.into(),
            parent_id: None,
            description: String::new(),
            auto_rule: None,
            private: false,
        }
    }

    fn term(text: &str) -> TagTerm {
        TagTerm { id: 1, tag_id: 1, text: text.into(), language: None, is_primary: false, export: true }
    }

    fn tags_a() -> Vec<Tag> {
        vec![tag("Street Photography/Old Town"), tag("Places/Vestfold/Tønsberg"), tag("ärlig  ßtraße/x\u{a0}y")]
    }

    const CONTENT_A: &str = "---\ntype: ChairPhoto\nchairphoto: 3f9c1a7e-0b1c-4d2e-8f3a-112233445566\nphoto: _81A8352.ARW\ncaptured: 2026-07-04T09:30:00\ncamera: ILCE-7RM6\nlens: FE 24-70mm F2.8 GM\nexposure: f/2.8 · 1/500s · ISO 400\nrating: 4\ntags:\n  - StreetPhotography/OldTown\n  - Places/Vestfold/Tønsberg\n  - ÄrligSStraße/XY\n---\n\n[Open in ChairPhoto](chairphoto://3f9c1a7e-0b1c-4d2e-8f3a-112233445566) · [Open in loupe](chairphoto://3f9c1a7e-0b1c-4d2e-8f3a-112233445566/loupe)\n";

    #[test]
    fn note_names_match_react() {
        assert_eq!(note_name(&full()), "2026-07-04 _81A8352 3f9c1a7e");
        assert_eq!(note_name(&photo("IMG.tar.gz")), "undated IMG.tar 3f9c1a7e");
        let blank = Photo { capture_time: Some(String::new()), ..photo("dir/.hidden") };
        assert_eq!(note_name(&blank), "undated  3f9c1a7e");
        assert_eq!(note_name(&photo("dir/trailing.")), "undated trailing. 3f9c1a7e", "`\\.[^.]+$` needs a character");
        assert_eq!(note_file("ChairPhoto", &full()), "ChairPhoto/2026-07-04 _81A8352 3f9c1a7e");
    }

    #[test]
    fn photo_note_content_matches_react() {
        assert_eq!(note_content(&full(), &tags_a()), CONTENT_A);
        let b = Photo { aperture: Some(4.0), ..photo("IMG.tar.gz") };
        assert_eq!(
            note_content(&b, &[]),
            "---\ntype: ChairPhoto\nchairphoto: 3f9c1a7e-0b1c-4d2e-8f3a-112233445566\nphoto: IMG.tar.gz\nexposure: f/4\n---\n\n[Open in ChairPhoto](chairphoto://3f9c1a7e-0b1c-4d2e-8f3a-112233445566) · [Open in loupe](chairphoto://3f9c1a7e-0b1c-4d2e-8f3a-112233445566/loupe)\n"
        );
        // Empty strings are falsy, ISO 0 is not; at most 20 tags.
        let c = Photo {
            capture_time: Some(String::new()),
            camera_model: Some(String::new()),
            lens: Some(String::new()),
            shutter_speed: Some(String::new()),
            iso: Some(0),
            ..photo("dir/.hidden")
        };
        let many: Vec<Tag> = (0..25).map(|i| tag(&format!("T{i}"))).collect();
        let expected_tags: String = (0..20).map(|i| format!("  - T{i}\n")).collect();
        assert_eq!(
            note_content(&c, &many),
            format!("---\ntype: ChairPhoto\nchairphoto: {U}\nphoto: .hidden\nexposure: ISO 0\ntags:\n{expected_tags}---\n\n[Open in ChairPhoto](chairphoto://{U}) · [Open in loupe](chairphoto://{U}/loupe)\n")
        );
    }

    #[test]
    fn tag_notes_match_react() {
        let mut odd = tag("x");
        odd.name = r#"a\b/c:d*e?f"g<h>i|j"#.into();
        assert_eq!(tag_note_name(&odd), "a-b-c-d-e-f-g-h-i-j 3f9c1a7e");
        let terms = [term("  Tønsberg "), term("Tunsberg"), term("Tønsberg"), term(""), term("   ")];
        let al = aliases(&terms);
        assert_eq!(al, vec!["Tønsberg", "Tunsberg"]);
        let mut t = tag("Places/Vestfold/Tønsberg");
        t.description = "  Old town.\nSecond line.  ".into();
        assert_eq!(
            tag_note_content(&t, &al),
            "---\ntype: ChairPhotoTag\nchairphoto-tag: 3f9c1a7e-0b1c-4d2e-8f3a-112233445566\ntag: Places/Vestfold/Tønsberg\naliases:\n  - Tønsberg\n  - Tunsberg\ntags:\n  - Places/Vestfold/Tønsberg\n---\n\n[Show photos in ChairPhoto](chairphoto://tag/3f9c1a7e-0b1c-4d2e-8f3a-112233445566)\n\nOld town.\nSecond line.\n"
        );
        let mut bare = tag("Bridge");
        bare.description = "   ".into();
        assert_eq!(
            tag_note_content(&bare, &[]),
            "---\ntype: ChairPhotoTag\nchairphoto-tag: 3f9c1a7e-0b1c-4d2e-8f3a-112233445566\ntag: Bridge\ntags:\n  - Bridge\n---\n\n[Show photos in ChairPhoto](chairphoto://tag/3f9c1a7e-0b1c-4d2e-8f3a-112233445566)\n"
        );
        let many: Vec<String> = (0..25).map(|i| format!("a{i}")).collect();
        assert_eq!(tag_note_content(&bare, &many).matches("\n  - a").count(), LIST_CAP);
        assert_eq!(tag_note_file("ChairPhoto", &bare), "ChairPhoto/Tags/Bridge 3f9c1a7e");
    }

    #[test]
    fn uris_match_react() {
        assert_eq!(
            new_uri("My Vault", &note_file("ChairPhoto", &full()), CONTENT_A),
            "obsidian://new?vault=My%20Vault&file=ChairPhoto%2F2026-07-04%20_81A8352%203f9c1a7e&content=---%0Atype%3A%20ChairPhoto%0Achairphoto%3A%203f9c1a7e-0b1c-4d2e-8f3a-112233445566%0Aphoto%3A%20_81A8352.ARW%0Acaptured%3A%202026-07-04T09%3A30%3A00%0Acamera%3A%20ILCE-7RM6%0Alens%3A%20FE%2024-70mm%20F2.8%20GM%0Aexposure%3A%20f%2F2.8%20%C2%B7%201%2F500s%20%C2%B7%20ISO%20400%0Arating%3A%204%0Atags%3A%0A%20%20-%20StreetPhotography%2FOldTown%0A%20%20-%20Places%2FVestfold%2FT%C3%B8nsberg%0A%20%20-%20%C3%84rligSStra%C3%9Fe%2FXY%0A---%0A%0A%5BOpen%20in%20ChairPhoto%5D(chairphoto%3A%2F%2F3f9c1a7e-0b1c-4d2e-8f3a-112233445566)%20%C2%B7%20%5BOpen%20in%20loupe%5D(chairphoto%3A%2F%2F3f9c1a7e-0b1c-4d2e-8f3a-112233445566%2Floupe)%0A"
        );
        assert_eq!(
            open_uri("My Vault", "ChairPhoto/2026-07-04 _81A8352 3f9c1a7e"),
            "obsidian://open?vault=My%20Vault&file=ChairPhoto%2F2026-07-04%20_81A8352%203f9c1a7e"
        );
    }

    /// The record's JSON is `JSON.stringify`'s; empty (Forget) and garbage are no note.
    #[test]
    fn records_round_trip_with_react() {
        let raw = r#"{"vault":"My Vault","file":"ChairPhoto/x","createdAt":1767225600000}"#;
        let rec = NoteRecord::parse(raw).unwrap();
        assert_eq!(rec, NoteRecord { vault: "My Vault".into(), file: "ChairPhoto/x".into(), created_at: 1_767_225_600_000 });
        assert_eq!(rec.to_json(), raw);
        assert_eq!(rec.name(), "x");
        assert_eq!(rec.open_uri(), "obsidian://open?vault=My%20Vault&file=ChairPhoto%2Fx");
        assert_eq!(NoteRecord::parse(""), None);
        assert_eq!(NoteRecord::parse("{not json"), None);
        assert_eq!(note_key(U), format!("note.{U}"));
        assert_eq!(tag_note_key(U), format!("tagnote.{U}"));
    }

    /// Every link a note carries is one the app's parser opens: the photo, its loupe, the tag.
    #[test]
    fn note_links_parse_back_to_their_subject() {
        let links = |text: &str| -> Vec<DeepLink> {
            text.split(['(', ')']).filter(|s| s.starts_with("chairphoto://")).map(|s| parse(s).expect(s)).collect()
        };
        assert_eq!(
            links(&note_content(&full(), &[])),
            vec![
                DeepLink::Photo { uuid: U.into(), view: DeepLinkView::Grid },
                DeepLink::Photo { uuid: U.into(), view: DeepLinkView::Loupe }
            ]
        );
        assert_eq!(links(&tag_note_content(&tag("Bridge"), &[])), vec![DeepLink::Tag { uuid: U.into() }]);
    }

    #[test]
    fn settings_are_validated() {
        assert_eq!(vault_name("  My Vault \u{feff}").as_deref(), Ok("My Vault"));
        assert_eq!(vault_name("   ").as_deref(), Ok(""));
        assert!(vault_name("/home/me/Vault").is_err());
        assert!(vault_name(r"C:\Vault").is_err());
        assert_eq!(notes_folder("  /Notes/Photos/ ").as_deref(), Ok("Notes/Photos"));
        assert_eq!(notes_folder("").as_deref(), Ok(""));
        for bad in ["../outside", "a/../b", "a//b", "./a", r"a\b", "a/ /b"] {
            assert!(notes_folder(bad).is_err(), "{bad}");
        }
        assert_eq!(folder_or_default(None).as_deref(), Ok(DEFAULT_FOLDER));
        assert_eq!(folder_or_default(Some("  ")).as_deref(), Ok(DEFAULT_FOLDER));
        assert_eq!(folder_or_default(Some("Notes/")).as_deref(), Ok("Notes"));
        assert!(folder_or_default(Some("..")).is_err());
    }
}
