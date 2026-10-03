//! Export keywords: `dc:subject` and `lr:hierarchicalSubject`.

use std::path::Path;
use super::document::SidecarDocument;
use super::dom::bag;
use super::ns::{NS_DC, NS_LR};

/// Write export keywords into the photo's XMP sidecar, merging with existing content.
/// Manages ONLY `dc:subject` (flat) and `lr:hierarchicalSubject` — every other element
/// (IPTC fields, develop settings, foreign namespaces) is preserved. Mirrors the
/// merge-safety invariant of [`write_iptc`](super::write_iptc). An empty list clears that property.
///
/// This is the **export** path: it writes the destination copy of a sidecar, so unlike
/// [`write_iptc`](super::write_iptc) it does NOT back up a pre-existing foreign sidecar (the destination is
/// a throwaway copy, and keywords are always re-derivable from the catalog). If this is
/// ever used on an in-library sidecar, add the foreign-backup-once logic back.
pub fn write_keywords(
    photo_path: &Path,
    flat: &[String],
    hierarchical: &[String],
) -> Result<(), String> {
    // Export destination copy: not subject to the in-library backup-once rule (AGENTS.md).
    let mut doc = SidecarDocument::open_no_backup(photo_path)?;

    let owned = [(NS_DC, "subject"), (NS_LR, "hierarchicalSubject")];
    let mut replacements = Vec::new();
    if !flat.is_empty() {
        replacements.push(bag("dc", NS_DC, "subject", flat));
    }
    if !hierarchical.is_empty() {
        replacements.push(bag("lr", NS_LR, "hierarchicalSubject", hierarchical));
    }

    doc.replace_owned(&owned, replacements);
    doc.commit()
}

#[cfg(test)]
mod tests {
    use super::*;
    use xmltree::XMLNode;
    use crate::xmp::dom::{child, first_text};
    use crate::xmp::ns::NS_RDF;
    use crate::xmp::parse::parse_xml;
    use crate::xmp::test_xml::read;
    use crate::xmp::{sidecar_backup_path, sidecar_path};

    // ── write_keywords (issue #62) ──────────────────────────────────────────

    fn keywords_dir(tag: &str) -> crate::test_support::TestTmpDir {
        crate::test_support::TestTmpDir::new(tag)
    }

    /// Parse the sidecar and return the `rdf:li` text values of the `rdf:Bag` under the
    /// (namespace, name) property on `rdf:Description` — used to check that flat keywords
    /// land under `dc:subject` and hierarchical ones under `lr:hierarchicalSubject`, not
    /// swapped or merged together.
    fn bag_items(xmp: &str, ns: &str, name: &str) -> Vec<String> {
        let root = parse_xml(xmp.as_bytes()).unwrap();
        let rdf = root.get_child(("RDF", NS_RDF)).unwrap();
        for node in &rdf.children {
            let XMLNode::Element(desc) = node else { continue };
            if desc.name != "Description" {
                continue;
            }
            if let Some(prop) = child(desc, ns, name) {
                if let Some(bag_el) = prop.get_child(("Bag", NS_RDF)) {
                    return bag_el
                        .children
                        .iter()
                        .filter_map(|n| match n {
                            XMLNode::Element(li) => first_text(li),
                            _ => None,
                        })
                        .collect();
                }
            }
        }
        Vec::new()
    }

    /// Flat keywords land in `dc:subject` and hierarchical keywords land in
    /// `lr:hierarchicalSubject` — each in its own element, not swapped or merged.
    #[test]
    fn write_keywords_flat_and_hierarchical_land_in_right_elements() {
        let dir = keywords_dir("xmp-kw-elements");
        let photo = dir.join("DSC40.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        let flat = vec!["Sunset".to_string(), "Beach".to_string()];
        let hierarchical = vec![
            "Nature|Landscape".to_string(),
            "Nature|Landscape|Sunset".to_string(),
        ];
        write_keywords(&photo, &flat, &hierarchical).unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("dc:subject"), "dc:subject missing");
        assert!(xmp.contains("lr:hierarchicalSubject"), "lr:hierarchicalSubject missing");

        assert_eq!(bag_items(&xmp, NS_DC, "subject"), flat, "dc:subject content mismatch");
        assert_eq!(
            bag_items(&xmp, NS_LR, "hierarchicalSubject"),
            hierarchical,
            "lr:hierarchicalSubject content mismatch"
        );
    }

    /// A pre-existing foreign sidecar's elements and namespace survive a keyword write —
    /// same merge-safety invariant as the other managed-property writers.
    #[test]
    fn write_keywords_foreign_elements_survive() {
        let dir = keywords_dir("xmp-kw-foreign");
        let photo = dir.join("DSC41.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/">
   <darktable:history_end>4</darktable:history_end>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();

        write_keywords(
            &photo,
            &["Ferry".to_string()],
            &["Places|Norway".to_string()],
        )
        .unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("Ferry"), "flat keyword not written");
        assert!(xmp.contains("Places|Norway"), "hierarchical keyword not written");
        assert!(xmp.contains("history_end"), "darktable data clobbered by keyword write!");
        assert!(xmp.contains("darktable"), "darktable namespace lost by keyword write!");
    }

    /// The documented exemption (AGENTS.md "Export-only destination copies are not subject
    /// to this rule", `document.rs:24`): `write_keywords` uses `open_no_backup`, so even a
    /// foreign, never-before-seen sidecar must NOT be backed up. This is the one property
    /// that was previously only a comment, not a checked behavior.
    #[test]
    fn write_keywords_does_not_back_up_export_destination() {
        let dir = keywords_dir("xmp-kw-no-backup");
        let photo = dir.join("DSC42.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/">
   <darktable:history_end>2</darktable:history_end>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();
        let backup = sidecar_backup_path(&sidecar_path(&photo));
        assert!(!backup.exists());

        write_keywords(&photo, &["Ferry".to_string()], &[]).unwrap();

        assert!(
            !backup.exists(),
            "export-destination write must never create a .chairphoto-backup file"
        );
        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("Ferry"), "keyword still must be written");
    }

    /// Re-writing keywords replaces, not duplicates, both properties.
    #[test]
    fn write_keywords_rewrite_replaces_not_duplicates() {
        let dir = keywords_dir("xmp-kw-rewrite");
        let photo = dir.join("DSC43.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        write_keywords(
            &photo,
            &["First".to_string()],
            &["Old|Path".to_string()],
        )
        .unwrap();
        write_keywords(
            &photo,
            &["Second".to_string()],
            &["New|Path".to_string()],
        )
        .unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("Second"));
        assert!(!xmp.contains("First"), "old flat keyword should be replaced, not duplicated");
        assert!(xmp.contains("New|Path"));
        assert!(!xmp.contains("Old|Path"), "old hierarchical keyword should be replaced, not duplicated");
        assert_eq!(xmp.matches("dc:subject").count(), 2, "one open + one close tag only");
        assert_eq!(xmp.matches("lr:hierarchicalSubject").count(), 2, "one open + one close tag only");
    }
}
