//! The photo's identity on disk: its UUID (`xmp:Identifier`) and its import batch
//! (`chairphoto:ImportBatch`).

use std::path::{Path, PathBuf};
use xmltree::XMLNode;
use super::document::SidecarDocument;
use super::dom::{first_text, plain, rdf_of};
use super::ns::{NS_CHAIRPHOTO, NS_XMP};
use super::parse::{ns_attr, parse_xml};
use super::sidecar_path;

/// Read the photo UUID (`xmp:Identifier`) from its sidecar, if present. Used by the
/// scanner to recognise a moved/re-rooted file as the same photo (UUID is stable; path
/// is not). Returns `None` when there's no sidecar or no identifier. Tolerant of the
/// attribute form, a plain element, or an rdf array.
pub fn read_identifier(photo_path: &Path) -> Option<String> {
    let path = sidecar_path(photo_path);
    let file = std::fs::File::open(&path).ok()?;
    let root = parse_xml(file).ok()?;
    let rdf = rdf_of(&root)?;
    for node in &rdf.children {
        let XMLNode::Element(desc) = node else { continue };
        if desc.name != "Description" {
            continue;
        }
        // Compact (attribute) form: rdf:Description xmp:Identifier="…".
        if let Some(v) = ns_attr(desc, NS_XMP, "Identifier") {
            if !v.trim().is_empty() {
                return Some(v.trim().to_string());
            }
        }
        // Element form: <xmp:Identifier>… or an rdf:Bag/Alt of li.
        for child in &desc.children {
            if let XMLNode::Element(e) = child {
                if e.namespace.as_deref() == Some(NS_XMP) && e.name == "Identifier" {
                    if let Some(text) = first_text(e) {
                        return Some(text);
                    }
                }
            }
        }
    }
    None
}

/// Every `xmp:Identifier` value in the photo's sidecar, in document order: the attribute and
/// element forms of every `rdf:Description`, and every text of an element (each `rdf:li` of
/// a Bag/Seq/Alt). Empty when there is no sidecar, it does not parse, or it has none.
///
/// [`read_identifier`] answers with the first of these, which is what binding compares. A
/// caller about to let [`overwrite_identifier`] replace them all reads this instead (#150):
/// a second value — another photo's UUID appended by a DAM or `exiftool -XMP-xmp:Identifier+=`,
/// or one in a second Description — would otherwise be destroyed unseen.
pub fn read_identifiers(photo_path: &Path) -> Vec<String> {
    fn texts(e: &xmltree::Element, out: &mut Vec<String>) {
        for node in &e.children {
            match node {
                XMLNode::Text(t) if !t.trim().is_empty() => out.push(t.trim().to_string()),
                XMLNode::Element(child) => texts(child, out),
                _ => {}
            }
        }
    }
    let path = sidecar_path(photo_path);
    let Ok(file) = std::fs::File::open(&path) else { return Vec::new() };
    let Ok(root) = parse_xml(file) else { return Vec::new() };
    let Some(rdf) = rdf_of(&root) else { return Vec::new() };
    let mut values = Vec::new();
    for node in &rdf.children {
        let XMLNode::Element(desc) = node else { continue };
        if desc.name != "Description" {
            continue;
        }
        if let Some(v) = ns_attr(desc, NS_XMP, "Identifier").filter(|v| !v.trim().is_empty()) {
            values.push(v.trim().to_string());
        }
        for child in &desc.children {
            if let XMLNode::Element(e) = child {
                if e.namespace.as_deref() == Some(NS_XMP) && e.name == "Identifier" {
                    texts(e, &mut values);
                }
            }
        }
    }
    values
}

/// Write the photo's UUID into its sidecar as `xmp:Identifier`, merge-safe: only the
/// identifier (and `chairphoto:LastWrite`) are touched; all other content is preserved.
/// This is the binding invariant — the UUID must live on disk so moves/re-roots match.
/// Backs up a pre-existing foreign sidecar once before the first write.
pub fn write_identifier(photo_path: &Path, uuid: &str) -> Result<(), String> {
    let mut doc = SidecarDocument::open(photo_path)?;
    // Manage only xmp:Identifier (preserve everything else).
    doc.replace_owned(
        &[(NS_XMP, "Identifier")],
        vec![plain("xmp", NS_XMP, "Identifier", uuid)],
    );
    doc.commit()
}

/// Replace an `xmp:Identifier` that belongs to somebody else with `uuid` — the Overwrite
/// half of resolving an identity conflict (issue #33), and the ONLY writer allowed to
/// destroy an identifier it did not write. Everything else in the sidecar is preserved,
/// exactly as in [`write_identifier`].
///
/// Returns the path the previous sidecar was copied to, or `None` if nothing was backed up
/// (no sidecar existed yet, or a backup from an earlier write is already there and was
/// deliberately not replaced). Unlike [`write_identifier`], the backup does not depend on
/// `chairphoto:LastWrite`: a sidecar chairphoto has written can still carry a foreign
/// identifier, and that is precisely the case this function exists for. See
/// `document::BackupPolicy`.
pub fn overwrite_identifier(photo_path: &Path, uuid: &str) -> Result<Option<PathBuf>, String> {
    let mut doc = SidecarDocument::open_forcing_backup(photo_path)?;
    let backup = doc.backup().map(|p| p.to_path_buf());
    doc.replace_owned(
        &[(NS_XMP, "Identifier")],
        vec![plain("xmp", NS_XMP, "Identifier", uuid)],
    );
    doc.commit()?;
    Ok(backup)
}

/// Write the import-batch UUID into the photo's XMP sidecar as
/// `chairphoto:ImportBatch`, merge-safe: only that field (and
/// `chairphoto:LastWrite`) are touched; all other content is preserved.
/// The batch UUID lets the batch survive catalog loss / catalog merge across
/// machines.
/// Backs up a pre-existing foreign sidecar once before the first write,
/// mirroring the invariant in [`write_identifier`].
pub fn write_import_batch(photo_path: &Path, batch_uuid: &str) -> Result<(), String> {
    let mut doc = SidecarDocument::open(photo_path)?;
    // Manage only chairphoto:ImportBatch (preserve everything else).
    doc.replace_owned(
        &[(NS_CHAIRPHOTO, "ImportBatch")],
        vec![plain("chairphoto", NS_CHAIRPHOTO, "ImportBatch", batch_uuid)],
    );
    doc.commit()
}

/// Read the import-batch UUID (`chairphoto:ImportBatch`) from the photo's
/// sidecar, if present. Returns `None` when there's no sidecar or no field.
pub fn read_import_batch(photo_path: &Path) -> Option<String> {
    let path = sidecar_path(photo_path);
    let file = std::fs::File::open(&path).ok()?;
    let root = parse_xml(file).ok()?;
    let rdf = rdf_of(&root)?;
    for node in &rdf.children {
        let XMLNode::Element(desc) = node else { continue };
        if desc.name != "Description" {
            continue;
        }
        for child in &desc.children {
            if let XMLNode::Element(e) = child {
                if e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "ImportBatch" {
                    if let Some(text) = first_text(e) {
                        return Some(text);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::IptcFields;
    use crate::xmp::test_xml::read;
    use crate::xmp::{sidecar_backup_path, write_iptc};

    #[test]
    fn identifier_round_trips_and_preserves_foreign() {
        let dir = crate::test_support::TestTmpDir::new("xmp-uuid");
        let photo = dir.join("DSC9.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        // No sidecar yet → no identifier.
        assert_eq!(read_identifier(&photo), None);

        write_identifier(&photo, "uuid-abc-123").unwrap();
        assert_eq!(read_identifier(&photo).as_deref(), Some("uuid-abc-123"));

        // A later IPTC write must not drop the identifier, and re-writing the identifier
        // keeps a single instance.
        write_iptc(&photo, &IptcFields::default(), &IptcFields { description: "x".into(), ..Default::default() }).unwrap();
        assert_eq!(read_identifier(&photo).as_deref(), Some("uuid-abc-123"));
        write_identifier(&photo, "uuid-abc-123").unwrap();
        let xmp = read(&sidecar_path(&photo));
        assert_eq!(xmp.matches("xmp:Identifier").count(), 2, "one open + one close tag only");
        assert!(xmp.contains("x"), "IPTC field survived the identifier rewrite");
    }

    /// Overwrite (issue #33) destroys an identifier chairphoto did not write, so the
    /// pre-existing sidecar must be preserved verbatim first — and everything else in that
    /// sidecar (here darktable's element) must survive the rewrite, exactly as
    /// `write_identifier` guarantees.
    #[test]
    fn overwrite_identifier_backs_up_the_foreign_sidecar_and_preserves_the_rest() {
        let dir = crate::test_support::TestTmpDir::new("xmp-overwrite-foreign");
        let photo = dir.join("DSC20.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/"
    xmlns:xmp="http://ns.adobe.com/xap/1.0/">
   <darktable:history_end>7</darktable:history_end>
   <xmp:Identifier>somebody-elses-uuid</xmp:Identifier>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();

        let backup = overwrite_identifier(&photo, "our-uuid").unwrap();
        assert_eq!(backup.as_deref(), Some(sidecar_backup_path(&sidecar_path(&photo)).as_path()));
        assert_eq!(std::fs::read_to_string(backup.unwrap()).unwrap(), existing,
            "the destroyed identifier must survive verbatim in the backup");

        assert_eq!(read_identifier(&photo).as_deref(), Some("our-uuid"));
        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("history_end"), "foreign element clobbered by the overwrite");
        assert!(!xmp.contains("somebody-elses-uuid"), "the conflicting identifier must be gone");
        assert_eq!(xmp.matches("xmp:Identifier").count(), 2, "one open + one close tag only");
    }

    /// The case plain `write_identifier` would NOT back up: a sidecar chairphoto has already
    /// written (it carries `chairphoto:LastWrite`) that nevertheless holds a foreign
    /// identifier — the ordinary result of duplicating a file after import. Overwrite must
    /// still preserve it, because it is about to destroy that identifier.
    #[test]
    fn overwrite_identifier_backs_up_even_a_chairphoto_written_sidecar() {
        let dir = crate::test_support::TestTmpDir::new("xmp-overwrite-ours");
        let photo = dir.join("DSC21.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        // chairphoto writes it once (stamping LastWrite), then the file is duplicated and
        // ends up under a row whose catalog uuid is different.
        write_identifier(&photo, "uuid-of-the-original").unwrap();
        let backup_path = sidecar_backup_path(&sidecar_path(&photo));
        assert!(!backup_path.exists(), "no backup yet: nothing existed before the first write");
        let before = read(&sidecar_path(&photo));

        let backup = overwrite_identifier(&photo, "uuid-of-the-duplicate").unwrap();
        assert_eq!(backup.as_deref(), Some(backup_path.as_path()),
            "a chairphoto-written sidecar must still be backed up before Overwrite destroys \
             an identifier — `chairphoto:LastWrite` does not make the identifier ours");
        assert_eq!(std::fs::read_to_string(&backup_path).unwrap(), before);
        assert_eq!(read_identifier(&photo).as_deref(), Some("uuid-of-the-duplicate"));
    }

    /// A backup that already exists is the earliest state we ever saw. A later Overwrite
    /// must not trade it away for a newer, chairphoto-written one.
    #[test]
    fn overwrite_identifier_never_replaces_an_existing_backup() {
        let dir = crate::test_support::TestTmpDir::new("xmp-overwrite-keeps-backup");
        let photo = dir.join("DSC22.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        let original = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/">
   <xmp:Identifier>first-foreign-uuid</xmp:Identifier>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), original).unwrap();

        overwrite_identifier(&photo, "second-uuid").unwrap();
        let backup_path = sidecar_backup_path(&sidecar_path(&photo));
        assert_eq!(std::fs::read_to_string(&backup_path).unwrap(), original);

        // A second Overwrite reports no new backup and leaves the original snapshot intact.
        let backup = overwrite_identifier(&photo, "third-uuid").unwrap();
        assert_eq!(backup, None, "an existing backup is left alone, and reported as such");
        assert_eq!(std::fs::read_to_string(&backup_path).unwrap(), original,
            "the earliest snapshot must survive later overwrites");
        assert_eq!(read_identifier(&photo).as_deref(), Some("third-uuid"));
    }

    #[test]
    fn import_batch_round_trips_and_preserves_foreign() {
        let dir = crate::test_support::TestTmpDir::new("xmp-batch");
        let photo = dir.join("DSC10.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        // No sidecar yet → no batch.
        assert_eq!(read_import_batch(&photo), None);

        write_import_batch(&photo, "batch-uuid-001").unwrap();
        assert_eq!(read_import_batch(&photo).as_deref(), Some("batch-uuid-001"));

        // Writing the batch UUID a second time replaces, not duplicates.
        write_import_batch(&photo, "batch-uuid-001").unwrap();
        let xmp = read(&sidecar_path(&photo));
        assert_eq!(
            xmp.matches("chairphoto:ImportBatch").count(),
            2, // open + close tag, once
            "batch field must not be duplicated"
        );

        // A later IPTC write must not drop the batch field.
        write_iptc(&photo, &IptcFields::default(), &IptcFields { description: "boat".into(), ..Default::default() }).unwrap();
        assert_eq!(read_import_batch(&photo).as_deref(), Some("batch-uuid-001"),
            "batch uuid must survive an IPTC write");

        // And a new write_identifier must not drop the batch field either.
        write_identifier(&photo, "uuid-photo-001").unwrap();
        assert_eq!(read_import_batch(&photo).as_deref(), Some("batch-uuid-001"),
            "batch uuid must survive an identifier write");
    }

    #[test]
    fn import_batch_preserves_foreign_elements() {
        let dir = crate::test_support::TestTmpDir::new("xmp-batch-foreign");
        let photo = dir.join("DSC11.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        // Simulate an existing darktable sidecar.
        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/">
   <darktable:history_end>3</darktable:history_end>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();

        write_import_batch(&photo, "batch-uuid-002").unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("batch-uuid-002"), "batch UUID not written");
        assert!(xmp.contains("history_end"), "darktable data clobbered by batch write!");
        assert!(xmp.contains("darktable"), "darktable namespace lost by batch write!");
        // A backup must have been made on first-ever write to a foreign sidecar.
        assert!(sidecar_backup_path(&sidecar_path(&photo)).exists());
    }
}
