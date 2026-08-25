//! Detecting *external* (non-chairphoto) editors from sidecar files next to a
//! photo. Read-only: we only read sidecars, never write to the photo folder.
//!
//! This populates `photos.external_editors`, which drives the "edited" filter. The
//! key rule: chairphoto's own `.xmp` (it writes IPTC/keywords there) must NOT count
//! as an external edit, or every photo we tag would look "edited".

use crate::companions::EditSignal;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Detect external editors from sidecars next to `photo_path`. Returns sorted,
/// de-duplicated editor names; empty when there are no sidecars or only
/// chairphoto's own.
pub fn detect_external_editors(photo_path: &Path) -> Vec<String> {
    let mut found: BTreeSet<String> = BTreeSet::new();

    // Which files can sit beside a photo is declared once, in `crate::companions`, so this
    // and the backup carry-set cannot drift apart. Each kind says how its presence should
    // be read:
    //   ByExtension — RawTherapee's `.pp3`, ART's `.arp`: presence proves an edit.
    //   ByContent   — `.xmp` comes from darktable, Lightroom *or* chairphoto itself, so
    //                 only the contents can say (chairphoto's own never counts).
    //   Never       — RapidRAW's `.rrdata` is written when it *opens* a photo, so its
    //                 presence proves nothing and must not mark the RAW edited.
    for companion in crate::companions::found_beside(photo_path) {
        match crate::companions::lookup(companion.ext).map(|c| c.edit_signal) {
            Some(EditSignal::ByExtension(editor)) => {
                found.insert(editor.into());
            }
            Some(EditSignal::ByContent) => {
                if let Ok(text) = std::fs::read_to_string(&companion.path) {
                    if let Some(editor) = attribute_xmp(&text) {
                        found.insert(editor);
                    }
                }
            }
            Some(EditSignal::Never) | None => {}
        }
    }

    found.into_iter().collect()
}

/// Comma+space joined form for storing in `photos.external_editors`.
pub fn detect_external_editors_joined(photo_path: &Path) -> String {
    detect_external_editors(photo_path).join(", ")
}

/// Candidate develop-sidecar paths for a given editor next to `photo_path` — used by the
/// external-edit round-trip to detect that a develop session wrote a sidecar and to pass it
/// to the editor's CLI. Returns the paths whether or not they exist yet.
///   darktable → `<photo>.xmp` (appended) and `<stem>.xmp` (basename)
///   rawtherapee → `<photo>.pp3`; art → `<photo>.arp`
pub(crate) fn develop_sidecars(photo_path: &Path, editor_key: &str) -> Vec<PathBuf> {
    let appended_path = |ext: &str| {
        let mut s = photo_path.as_os_str().to_os_string();
        s.push(".");
        s.push(ext);
        PathBuf::from(s)
    };
    match editor_key {
        "darktable" => vec![appended_path("xmp"), photo_path.with_extension("xmp")],
        "rawtherapee" => vec![appended_path("pp3")],
        "art" => vec![appended_path("arp")],
        _ => vec![],
    }
}

/// The editor a `.xmp`'s content implies, or `None` if it carries only chairphoto's
/// own data (or nothing recognisable). A file edited by several apps reports the
/// develop editor; chairphoto-only never counts.
fn attribute_xmp(text: &str) -> Option<String> {
    // darktable stamps its own namespace/prefix; this is unambiguous.
    if text.contains("darktable:") || text.contains("xmlns:darktable") {
        return Some("darktable".into());
    }
    // Camera Raw Settings (`crs:`) is written by Lightroom / Adobe Camera Raw.
    if text.contains("crs:") || text.contains("xmlns:crs") {
        return Some("Lightroom".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path, contents: &str) {
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn detects_rawtherapee_and_art_by_extension() {
        let dir = crate::test_support::TestTmpDir::new("sidecars-ext");
        let photo = dir.join("DSC1.ARW");
        touch(&photo, "raw");
        touch(&dir.join("DSC1.ARW.pp3"), "rt");
        touch(&dir.join("DSC1.ARW.arp"), "art");

        assert_eq!(
            detect_external_editors(&photo),
            vec!["ART".to_string(), "RawTherapee".to_string()]
        );
    }

    #[test]
    fn attributes_xmp_to_darktable_and_lightroom() {
        let dir = crate::test_support::TestTmpDir::new("sidecars-xmp");

        let dt = dir.join("DT.ARW");
        touch(&dt, "raw");
        touch(
            &dir.join("DT.ARW.xmp"),
            r#"<x:xmpmeta xmlns:darktable="http://darktable.sf.net/"><darktable:history/></x:xmpmeta>"#,
        );
        assert_eq!(detect_external_editors(&dt), vec!["darktable".to_string()]);

        // Lightroom basename form: `LR.xmp` next to `LR.ARW` (not the appended form).
        let lr = dir.join("LR.ARW");
        touch(&lr, "raw");
        touch(
            &dir.join("LR.xmp"),
            r#"<x:xmpmeta xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"><crs:Exposure2012>0.5</crs:Exposure2012></x:xmpmeta>"#,
        );
        assert_eq!(detect_external_editors(&lr), vec!["Lightroom".to_string()]);
    }

    #[test]
    fn chairphoto_own_xmp_is_not_an_external_edit() {
        let dir = crate::test_support::TestTmpDir::new("sidecars-own");
        let photo = dir.join("MINE.ARW");
        touch(&photo, "raw");
        // Resembles chairphoto's own sidecar: dc:subject + chairphoto namespace, no
        // develop editor.
        touch(
            &dir.join("MINE.ARW.xmp"),
            r#"<x:xmpmeta xmlns:dc="http://purl.org/dc/elements/1.1/"
               xmlns:chairphoto="https://chairphoto.local/ns/1.0/">
               <dc:subject><rdf:Bag><rdf:li>cat</rdf:li></rdf:Bag></dc:subject>
               <chairphoto:LastWrite>123</chairphoto:LastWrite></x:xmpmeta>"#,
        );
        assert!(detect_external_editors(&photo).is_empty());
    }

    #[test]
    fn rrdata_sidecar_is_ignored_by_editor_detection() {
        // RapidRAW writes a `<filename>.rrdata` JSON sidecar next to the source. It must not
        // be mistaken for an external develop sidecar (that would falsely mark the RAW
        // "edited" and would break the "edited" filter).
        let dir = crate::test_support::TestTmpDir::new("sidecars-rrdata");
        let photo = dir.join("DSC1.ARW");
        touch(&photo, "raw");
        touch(&dir.join("DSC1.ARW.rrdata"), r#"{"edits":{}}"#);
        assert!(detect_external_editors(&photo).is_empty());
    }

    #[test]
    fn no_sidecars_means_no_editors() {
        let dir = crate::test_support::TestTmpDir::new("sidecars-none");
        let photo = dir.join("PLAIN.ARW");
        touch(&photo, "raw");
        assert!(detect_external_editors(&photo).is_empty());
    }
}
