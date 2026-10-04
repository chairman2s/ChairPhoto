//! The photo's identity on disk: its UUID (`xmp:Identifier`) and its import batch
//! (`chairphoto:ImportBatch`).

use std::path::{Path, PathBuf};
use xmltree::XMLNode;
use super::document::SidecarDocument;
use super::dom::{first_text, plain, rdf_of};
use super::ns::{NS_CHAIRPHOTO, NS_XMP, NS_XMPIDQ};
use super::parse::ns_attr;
use super::repair::parse_for_read;
use super::sidecar_path;

/// Read the photo UUID (`xmp:Identifier`) from its sidecar, if present. Used by the
/// scanner to recognise a moved/re-rooted file as the same photo (UUID is stable; path
/// is not). Returns `None` when there's no sidecar or no identifier. Tolerant of the
/// attribute form, a plain element, or an rdf array.
pub fn read_identifier(photo_path: &Path) -> Option<String> {
    let path = sidecar_path(photo_path);
    let file = std::fs::File::open(&path).ok()?;
    let root = parse_for_read(file).ok()?;
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
                    if let Some(text) = first_identifier_text(e) {
                        return Some(text);
                    }
                }
            }
        }
    }
    None
}

/// [`first_text`], but skipping an `xmpidq:*` qualifier the way [`identifiers_in_rdf`]'s own
/// walk does (#222 L2) — the qualifier is not a value of its own, it names a scheme for the
/// `rdf:value` beside it (XMP Basic's qualified form). Before this, a scheme-first `rdf:li`
/// (`xmpidq:Scheme` before `rdf:value`, which is valid XML — element order inside a struct is
/// not significant) made the plain [`first_text`] return the scheme name (e.g. `"DAM"`) as
/// the identifier, because it is simply the first text node depth-first. `read_identifiers`
/// already skipped it; this is what makes the singular reader agree with it rather than
/// minting `legacy_photo_identity` from a scheme name shared by every sidecar in that layout.
fn first_identifier_text(e: &xmltree::Element) -> Option<String> {
    for node in &e.children {
        match node {
            XMLNode::Text(t) if !t.trim().is_empty() => return Some(t.trim().to_string()),
            XMLNode::Element(child) if child.namespace.as_deref() == Some(NS_XMPIDQ) => {}
            XMLNode::Element(child) => {
                if let Some(t) = first_identifier_text(child) {
                    return Some(t);
                }
            }
            _ => {}
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
///
/// A qualifier on XMP Basic's qualified form — `rdf:value` next to an `xmpidq:*` property
/// such as `xmpidq:Scheme`, inside an `rdf:li` — is not a value of its own (#222 N1): before
/// this, `[dam:asset/2, DAM]` read as two values and a bulk Overwrite skipped the sidecar as
/// carrying more than one, reporting the scheme name as if it were a second identifier.
pub fn read_identifiers(photo_path: &Path) -> Vec<String> {
    let path = sidecar_path(photo_path);
    let Ok(file) = std::fs::File::open(&path) else { return Vec::new() };
    let Ok(root) = parse_for_read(file) else { return Vec::new() };
    let Some(rdf) = rdf_of(&root) else { return Vec::new() };
    identifiers_in_rdf(rdf)
}

/// [`read_identifiers`]'s walk, taking the already-parsed `rdf:RDF` element directly — shared
/// with [`overwrite_identifier_checked`], which reads from a [`SidecarDocument`] already open
/// under the sidecar's file lock rather than a fresh, unlocked parse (#222 N3).
fn identifiers_in_rdf(rdf: &xmltree::Element) -> Vec<String> {
    fn texts(e: &xmltree::Element, out: &mut Vec<String>) {
        for node in &e.children {
            match node {
                XMLNode::Text(t) if !t.trim().is_empty() => out.push(t.trim().to_string()),
                // The qualifier, not a value: skip it and its text rather than recursing in.
                XMLNode::Element(child) if child.namespace.as_deref() == Some(NS_XMPIDQ) => {}
                XMLNode::Element(child) => texts(child, out),
                _ => {}
            }
        }
    }
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

/// Why [`overwrite_identifier_checked`] did not write: its `guard` refused (a validation the
/// caller maps to its own "this isn't allowed" error, distinct from a failure), or opening,
/// reading or committing the sidecar failed for a filesystem reason (what every other sidecar
/// writer reports as a plain `String`).
#[derive(Debug)]
pub enum CheckedOverwriteError {
    Refused(String),
    Io(String),
}

/// [`overwrite_identifier`], but first lets `guard` look at the identifier values the
/// sidecar carries right now — under the same file lock the write itself takes, opened
/// before `guard` runs and held through the write — and refuse by returning `Err` (#222 N3).
///
/// A bulk Overwrite's "this sidecar carries exactly one non-UUID value" precondition used to
/// be checked by a separate, unlocked [`read_identifiers`] call before this function's own
/// open took the lock. A value another tool appended to the sidecar in that gap was then
/// destroyed unseen: the precondition passed against a snapshot the write never acted on.
/// Reading from the already-open, already-locked document narrows that gap against
/// ChairPhoto's own writers to nothing — there is no read the write's own open did not also
/// make — though not against an external tool: [`super::lock::FILE_TURNS`] is in-process
/// only, so `exiftool` or a DAM writing between this open's read and the commit is still
/// possible (#222 N4; this call is no slower at closing that particular door than any other
/// sidecar writer).
///
/// The backup this Overwrite always forces is deferred until `guard` passes (#222 L1):
/// before this, a refused Overwrite still left a `.chairphoto-backup` beside the sidecar,
/// because the open that read what `guard` was about to judge had already copied it. `guard`
/// now runs against an open that has made no file-system change of its own; only once it
/// returns `Ok` does the backup — still *before* the destructive write itself — actually
/// happen.
pub fn overwrite_identifier_checked(
    photo_path: &Path,
    uuid: &str,
    guard: impl FnOnce(&[String]) -> Result<(), String>,
) -> Result<Option<PathBuf>, CheckedOverwriteError> {
    let mut doc = SidecarDocument::open_checking_before_backup(photo_path).map_err(CheckedOverwriteError::Io)?;
    guard(&identifiers_in_rdf(doc.rdf_mut())).map_err(CheckedOverwriteError::Refused)?;
    doc.force_pending_backup().map_err(CheckedOverwriteError::Io)?;
    let backup = doc.backup().map(|p| p.to_path_buf());
    doc.replace_owned(
        &[(NS_XMP, "Identifier")],
        vec![plain("xmp", NS_XMP, "Identifier", uuid)],
    );
    doc.commit().map_err(CheckedOverwriteError::Io)?;
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
    let root = parse_for_read(file).ok()?;
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

    // --- read_identifiers and XMP Basic's qualified form (#222 N1) --------------------

    /// A Bag item qualified the way XMP Basic allows — `rdf:value` beside `xmpidq:Scheme`,
    /// in an `rdf:li` marked `rdf:parseType="Resource"` — reads as one value, not two: the
    /// scheme name is a qualifier, never counted alongside it. Before this fix
    /// `read_identifiers` returned `["dam:asset/2", "DAM"]`, and a bulk Overwrite skipped the
    /// sidecar as carrying more than one identifier value (#150's guard).
    #[test]
    fn read_identifiers_reads_a_qualified_bag_item_as_one_value() {
        let dir = crate::test_support::TestTmpDir::new("xmp-identifiers-qualified");
        let photo = dir.join("DSC30.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:xmpidq="http://ns.adobe.com/xmp/Identifier/qual/1.0/">
   <xmp:Identifier>
    <rdf:Bag>
     <rdf:li rdf:parseType="Resource">
      <rdf:value>dam:asset/2</rdf:value>
      <xmpidq:Scheme>DAM</xmpidq:Scheme>
     </rdf:li>
    </rdf:Bag>
   </xmp:Identifier>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();

        assert_eq!(read_identifiers(&photo), vec!["dam:asset/2".to_string()]);
        // The singular reader must agree, since it answers with the first of these.
        assert_eq!(read_identifier(&photo).as_deref(), Some("dam:asset/2"));
    }

    /// **r5 review, #222 L2.** Same qualified Bag item, but `xmpidq:Scheme` written
    /// *before* `rdf:value` — valid XML, since element order inside an `rdf:parseType`
    /// struct carries no meaning. The test above happens to put `rdf:value` first, which
    /// hid this: `first_text`'s plain depth-first search returns whichever text node comes
    /// first in the file, so a scheme-first struct made the singular reader answer "DAM" —
    /// every sidecar of this shape then shared one `legacy_photo_identity`. Independently
    /// checked against Python's `xml.etree.ElementTree` (`.find('rdf:value')` /
    /// `.find('xmpidq:Scheme')` on this exact document): the value is `dam:asset/2`, the
    /// scheme is `DAM`.
    #[test]
    fn read_identifier_agrees_with_read_identifiers_when_the_scheme_comes_first() {
        let dir = crate::test_support::TestTmpDir::new("xmp-identifier-scheme-first");
        let photo = dir.join("DSC32.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:xmpidq="http://ns.adobe.com/xmp/Identifier/qual/1.0/">
   <xmp:Identifier>
    <rdf:Bag>
     <rdf:li rdf:parseType="Resource">
      <xmpidq:Scheme>DAM</xmpidq:Scheme>
      <rdf:value>dam:asset/2</rdf:value>
     </rdf:li>
    </rdf:Bag>
   </xmp:Identifier>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();

        assert_eq!(read_identifiers(&photo), vec!["dam:asset/2".to_string()]);
        assert_eq!(read_identifier(&photo).as_deref(), Some("dam:asset/2"), "not the scheme name \"DAM\"");
    }

    /// A second, genuinely distinct Bag item beside a qualified one is still seen: the
    /// qualifier skip must not swallow a real second value.
    #[test]
    fn read_identifiers_still_sees_a_second_item_beside_a_qualified_one() {
        let dir = crate::test_support::TestTmpDir::new("xmp-identifiers-qualified-plus-one");
        let photo = dir.join("DSC31.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:xmpidq="http://ns.adobe.com/xmp/Identifier/qual/1.0/">
   <xmp:Identifier>
    <rdf:Bag>
     <rdf:li rdf:parseType="Resource">
      <rdf:value>dam:asset/2</rdf:value>
      <xmpidq:Scheme>DAM</xmpidq:Scheme>
     </rdf:li>
     <rdf:li>another-photos-uuid</rdf:li>
    </rdf:Bag>
   </xmp:Identifier>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();

        assert_eq!(read_identifiers(&photo), vec!["dam:asset/2".to_string(), "another-photos-uuid".to_string()]);
    }

    // --- overwrite_identifier_checked reads under the write's own lock (#222 N3) -------

    /// The guard sees whatever the sidecar carries once the write's own open gets the file
    /// turn — not a value read before this call ever waited for it. A second "writer" (held
    /// here by a manually reserved turn, standing in for another process) appends a value
    /// while the checked overwrite is blocked waiting for that same turn; once released, the
    /// guard must see both values, proving the read happened after the wait, not before it.
    #[test]
    fn overwrite_identifier_checked_sees_a_value_appended_while_it_waits_for_the_lock() {
        let dir = crate::test_support::TestTmpDir::new("xmp-identifiers-checked-race");
        let photo = dir.join("DSC32.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        let sidecar = sidecar_path(&photo);
        let solo = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/">
   <xmp:Identifier>dam:solo</xmp:Identifier>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(&sidecar, solo).unwrap();
        assert_eq!(read_identifiers(&photo), vec!["dam:solo".to_string()], "precondition");

        let appended = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/">
   <xmp:Identifier>
    <rdf:Bag>
     <rdf:li>dam:solo</rdf:li>
     <rdf:li>second-writer-uuid</rdf:li>
    </rdf:Bag>
   </xmp:Identifier>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

        // Stand in for a concurrent writer already in possession of the sidecar's turn.
        let held = crate::xmp::lock::FILE_TURNS.reserve(crate::xmp::lock::key(&sidecar));

        let seen: std::sync::Arc<std::sync::Mutex<Option<Vec<String>>>> = Default::default();
        let seen2 = seen.clone();
        let photo2 = photo.clone();
        let handle = std::thread::spawn(move || {
            overwrite_identifier_checked(&photo2, "new-uuid", move |all| {
                *seen2.lock().unwrap() = Some(all.to_vec());
                Ok(())
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!handle.is_finished(), "the checked overwrite must wait for the held turn");

        // "Another tool" writes its own second value while it still holds the turn.
        std::fs::write(&sidecar, appended).unwrap();
        drop(held); // the turn is free; the checked overwrite may now open and read

        handle.join().unwrap().unwrap();
        assert_eq!(
            seen.lock().unwrap().clone(),
            Some(vec!["dam:solo".to_string(), "second-writer-uuid".to_string()]),
            "the guard must see what the write's own open reads under the lock, not a value \
             read before this call ever waited for its turn"
        );
    }

    // --- a refused checked Overwrite leaves no backup (#222 L1) -------------------------

    /// Before this fix, the backup this Overwrite always forces was copied at open — before
    /// `guard` ever ran — so a refusal (the caller's own "this isn't allowed" validation)
    /// still left `.chairphoto-backup` beside the sidecar, even though nothing about the
    /// sidecar itself was going to change. The guard here refuses unconditionally, whatever
    /// it is shown.
    #[test]
    fn a_refused_checked_overwrite_leaves_no_backup() {
        let dir = crate::test_support::TestTmpDir::new("xmp-identifiers-checked-refuse");
        let photo = dir.join("DSC33.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        let sidecar = sidecar_path(&photo);
        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/">
   <xmp:Identifier>dam:refused</xmp:Identifier>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(&sidecar, existing).unwrap();
        let before: Vec<_> = std::fs::read_dir(&*dir).unwrap().map(|e| e.unwrap().file_name()).collect();

        let r = overwrite_identifier_checked(&photo, "0b5f8a6e-1111-4222-8333-444455556666", |_| Err("no".into()));

        assert!(matches!(r, Err(CheckedOverwriteError::Refused(_))));
        assert!(!sidecar_backup_path(&sidecar).exists(), "a refused overwrite must create no backup");
        let after: Vec<_> = std::fs::read_dir(&*dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(before.len(), after.len(), "a refused overwrite created a file");
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), existing, "the sidecar itself is untouched");
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
