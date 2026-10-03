//! Repairing the attribute prefixes ChairPhoto's own writer dropped before #138.
//!
//! Every release before #138 wrote sidecars through `xmltree` 0.11, which keys attributes by
//! local name and writes them back unprefixed. A sidecar such a release rewrote carries
//! `parseType="Resource"` for `rdf:parseType`, `about` for `rdf:about` (next to an empty
//! `rdf:about` the writer then inserted, see [`drop_shadowed_about`]), and the MWG struct
//! fields of an attribute-form `AppliedToDimensions` / `Area` (`w="6000"`, `x="0.5"`) in no
//! namespace. The face-region reader and writer then see no struct at all: every face write on
//! the photo is refused ("mwg-rs:Regions is not a struct") and import reads no regions
//! (agent-notes review of the face regions, R4; #143 item 3).
//!
//! [`repair_dropped_prefixes`] restores exactly these attributes, and nothing else:
//!
//! | Unprefixed attribute | On | Becomes |
//! |---|---|---|
//! | `about` | `rdf:Description` | `rdf:about` (replacing an empty or equal `rdf:about`) |
//! | `parseType` | a property element or `rdf:li` | `rdf:parseType` |
//! | `w`, `h`, `unit` | `mwg-rs:AppliedToDimensions` (or its one nested `rdf:Description`) | `stDim:w`, `stDim:h`, `stDim:unit` |
//! | `x`, `y`, `w`, `h`, `unit` | `mwg-rs:Area` (or its one nested `rdf:Description`) | `stArea:x` … `stArea:unit` |
//!
//! The first two are what RDF/XML itself reads an unqualified `about` / `parseType` as (RDF/XML
//! Syntax Specification, § 6.1.4, kept for backwards compatibility), so re-prefixing them
//! changes no meaning. The struct fields have one possible meaning on those elements: the MWG
//! schema defines no other `w`/`h`/`unit`/`x`/`y` there. Every other unprefixed attribute (a
//! dropped `digiKam:Confidence`, say) has no knowable namespace and is left as it is.
//!
//! The repair runs only on a sidecar that carries `chairphoto:LastWrite` — one a ChairPhoto
//! release has written, so the damage is ours — and only when it is unambiguous: if any
//! element already has the attribute it would restore (`parseType` next to `rdf:parseType`,
//! `x` next to `stArea:x` or a `stArea:x` element), nothing in the file is repaired and the
//! writers see it as before. `SidecarDocument::open` applies it in memory before any writer
//! runs and backs the sidecar up first (see `document.rs`).

use xmltree::{Element, Namespace, XMLNode};
use super::dom::{child, is_rdf, prefix_for_ns};
use super::ns::{NS_MWG_RS, NS_RDF, NS_STAREA, NS_STDIM};
use super::parse::{attr_is, ns_attr};

/// Restore the prefixes a pre-#138 ChairPhoto writer dropped under `rdf` (see the module docs),
/// in place. `Ok(true)` when anything was repaired, `Ok(false)` when nothing needed it, `Err`
/// (why) when the damage is ambiguous; then, as for `Ok(false)`, `rdf` is unchanged.
pub(super) fn repair_dropped_prefixes(rdf: &mut Element) -> Result<bool, String> {
    let Some(fixed) = repaired(rdf)? else { return Ok(false) };
    *rdf = fixed;
    Ok(true)
}

/// `rdf` with the dropped prefixes restored: `Ok(None)` when there were none, `Err` when one
/// could not be restored unambiguously.
fn repaired(rdf: &Element) -> Result<Option<Element>, String> {
    let mut fixed = rdf.clone();
    let mut count = 0;
    for node in &mut fixed.children {
        if let XMLNode::Element(e) = node {
            repair_node(e, &mut count)?;
        }
    }
    Ok((count > 0).then_some(fixed))
}

/// Repair `e` (a node element or property element below `rdf:RDF`) and everything under it.
fn repair_node(e: &mut Element, count: &mut usize) -> Result<(), String> {
    if is_rdf(e, "Description") {
        drop_shadowed_about(e, count)?;
        restore(e, "about", NS_RDF, "rdf", count)?;
    } else if !is_rdf(e, "Bag") && !is_rdf(e, "Seq") && !is_rdf(e, "Alt") {
        restore(e, "parseType", NS_RDF, "rdf", count)?;
    }
    if e.namespace.as_deref() == Some(NS_MWG_RS) {
        let fields: Option<(&[&str], &str, &str)> = match e.name.as_str() {
            "AppliedToDimensions" => Some((&["w", "h", "unit"], NS_STDIM, "stDim")),
            "Area" => Some((&["x", "y", "w", "h", "unit"], NS_STAREA, "stArea")),
            _ => None,
        };
        if let Some((fields, ns, prefix)) = fields {
            for f in fields {
                restore(e, f, ns, prefix, count)?;
            }
            // The nested-Description struct form carries the fields one level down.
            for node in &mut e.children {
                if let XMLNode::Element(d) = node {
                    if is_rdf(d, "Description") {
                        for f in fields {
                            restore(d, f, ns, prefix, count)?;
                        }
                    }
                }
            }
        }
    }
    for node in &mut e.children {
        if let XMLNode::Element(c) = node {
            repair_node(c, count)?;
        }
    }
    Ok(())
}

/// The released writers parsed with xmltree (turning `rdf:about` into `about`) and then ensured
/// `rdf:about` with `entry("rdf:about").or_insert("")`, so every Description they rewrote carries
/// both: `about` with the original value, `rdf:about=""` (#143 review, H1). When `rdf:about` is
/// empty or equal to `about`, the pair is that writer's: keep the original value as `rdf:about`
/// and drop `about`. Two different non-empty values are not its shape, and are ambiguous.
fn drop_shadowed_about(e: &mut Element, count: &mut usize) -> Result<(), String> {
    let Some(original) = e.attributes.get("about").cloned() else {
        return Ok(());
    };
    let Some(key) = e.attributes.keys().find(|k| attr_is(e, k, NS_RDF, "about")).cloned() else {
        return Ok(());
    };
    let inserted = &e.attributes[&key];
    if !inserted.is_empty() && *inserted != original {
        return Err(format!("rdf:Description has about={original:?} and rdf:about={inserted:?}"));
    }
    e.attributes.remove("about");
    e.attributes.insert(key, original);
    *count += 1;
    Ok(())
}

/// Rename `e`'s unprefixed attribute `local` to `{ns}local`, under a prefix `e`'s namespaces
/// bind to `ns` (binding `preferred`, or a fresh one, if none does). `Err` when `e` already has
/// `{ns}local`, as an attribute or a child element.
fn restore(e: &mut Element, local: &str, ns: &str, preferred: &str, count: &mut usize) -> Result<(), String> {
    let Some(value) = e.attributes.get(local).cloned() else {
        return Ok(());
    };
    if ns_attr(e, ns, local).is_some() || child(e, ns, local).is_some() {
        return Err(format!(
            "{{{}}}{} has both an unprefixed {local} and {{{ns}}}{local}",
            e.namespace.as_deref().unwrap_or(""),
            e.name
        ));
    }
    let prefix = prefix_for_ns(e.namespaces.get_or_insert_with(Namespace::empty), ns, preferred);
    e.attributes.remove(local);
    e.attributes.insert(format!("{prefix}:{local}"), value);
    *count += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::xmp::ns::{NS_MWG_RS, NS_RDF, NS_STAREA, NS_STDIM};
    use crate::xmp::region_fixtures::{mwg, named};
    use crate::xmp::test_xml::{
        element_text, has_attr, namespaced_attributes, read, region_names, seeded_photo, sized, CAT,
    };
    use crate::xmp::{
        read_face_regions, sidecar_backup_path, sidecar_path, write_face_regions, FaceRegion, RegionWriteError,
    };

    // ── sidecars a pre-#138 release rewrote (#143 item 3) ───────────────────

    const NS_DARKTABLE: &str = "http://darktable.sf.net/";

    /// The damage a released pre-#138 writer did, written by hand (the other fixtures are made
    /// by running that writer's own sequence, [`released_rewrite`]): every attribute prefix
    /// dropped, and `rdf:about` both turned into `about` and inserted again, empty. Bob is
    /// digiKam's region (attribute-form Area, with a `digiKam:Confidence` that lost its prefix
    /// too), Ann the pre-marker ChairPhoto writer's, darktable's history is foreign, and
    /// `chairphoto:LastWrite` (an element) survived.
    const DAMAGED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description about="" rdf:about=""
    xmlns:darktable="http://darktable.sf.net/"
    xmlns:chairphoto="https://chairphoto.local/ns/1.0/"
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
    xmlns:stDim="http://ns.adobe.com/xap/1.0/sType/Dimensions#"
    xmlns:digiKam="http://www.digikam.org/ns/1.0/">
   <darktable:history_end>5</darktable:history_end>
   <mwg-rs:Regions parseType="Resource">
    <mwg-rs:AppliedToDimensions w="6000" h="4000" unit="pixel"/>
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li parseType="Resource">
       <mwg-rs:Name>Bob</mwg-rs:Name>
       <mwg-rs:Type>Face</mwg-rs:Type>
       <mwg-rs:Area x="0.8" y="0.7" w="0.1" h="0.2" unit="normalized" Confidence="87"/>
      </rdf:li>
      <rdf:li parseType="Resource">
       <mwg-rs:Name>Ann</mwg-rs:Name>
       <mwg-rs:Type>Face</mwg-rs:Type>
       <mwg-rs:Area parseType="Resource">
        <stArea:x>0.25</stArea:x>
        <stArea:y>0.35</stArea:y>
        <stArea:w>0.1</stArea:w>
        <stArea:h>0.1</stArea:h>
        <stArea:unit>normalized</stArea:unit>
       </mwg-rs:Area>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
   <chairphoto:LastWrite>1727000000</chairphoto:LastWrite>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

    /// [`DAMAGED`] as it was before a released build rewrote it.
    fn intact() -> String {
        DAMAGED
            .replace(r#"about="" rdf:about="""#, r#"rdf:about="""#)
            .replace(r#"parseType="Resource""#, r#"rdf:parseType="Resource""#)
            .replace(r#"w="6000" h="4000" unit="pixel""#, r#"stDim:w="6000" stDim:h="4000" stDim:unit="pixel""#)
            .replace(
                r#"x="0.8" y="0.7" w="0.1" h="0.2" unit="normalized" Confidence="87""#,
                r#"stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2" stArea:unit="normalized" digiKam:Confidence="87""#,
            )
    }

    /// What one write by a released pre-#138 build did to `xml` (v2026.8.0
    /// `src-tauri/src/xmp/mod.rs:66-75`): `xmltree` 0.11's parse (attribute keys by local name),
    /// `rdf:about` ensured on the first Description with `entry(..).or_insert_with(String::new)`,
    /// and `xmltree`'s write.
    fn released_rewrite(xml: &str) -> String {
        let mut root = xmltree::Element::parse(xml.as_bytes()).unwrap();
        let rdf = root.get_mut_child("RDF").unwrap();
        let desc = rdf.get_mut_child("Description").unwrap();
        desc.attributes.entry("rdf:about".to_string()).or_insert_with(String::new);
        let mut out = Vec::new();
        root.write(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn carl() -> FaceRegion {
        FaceRegion { face_id: 7, name: "Carl".into(), bbox: (0.5, 0.1, 0.1, 0.15) }
    }

    /// The review's R4: on such a sidecar every face write was refused ("mwg-rs:Regions is
    /// not a struct"). Now the prefixes are restored, the write lands, the sidecar is backed up
    /// as it was, and every foreign region and element survives.
    #[test]
    fn a_face_write_repairs_a_sidecar_whose_prefixes_a_pre_138_release_dropped() {
        let (_dir, photo) = seeded_photo("xmp-143-damaged", DAMAGED);
        assert!(read_face_regions(&photo).is_empty(), "reading does not repair");

        write_face_regions(&photo, CAT, &[carl()], &[], &[], sized(6000, 4000)).unwrap();

        let xml = read(&sidecar_path(&photo));
        // Restored, per an independent namespace-aware reader.
        assert!(has_attr(&xml, (NS_RDF, "Description"), (NS_RDF, "about"), ""), "{xml}");
        assert!(has_attr(&xml, (NS_MWG_RS, "Regions"), (NS_RDF, "parseType"), "Resource"), "{xml}");
        let dims = (NS_MWG_RS, "AppliedToDimensions");
        assert!(has_attr(&xml, dims, (NS_STDIM, "w"), "6000"), "{xml}");
        assert!(has_attr(&xml, dims, (NS_STDIM, "h"), "4000"), "{xml}");
        assert!(has_attr(&xml, dims, (NS_STDIM, "unit"), "pixel"), "{xml}");
        assert!(has_attr(&xml, (NS_MWG_RS, "Area"), (NS_STAREA, "x"), "0.8"), "{xml}");
        let restored = ["about", "parseType", "x", "y", "w", "h", "unit"];
        let unprefixed: Vec<_> = namespaced_attributes(&xml)
            .into_iter()
            .filter(|a| a.2.is_empty() && restored.contains(&a.3.as_str()))
            .collect();
        assert!(unprefixed.is_empty(), "{unprefixed:?}\n{xml}");
        // What has no knowable namespace is left as it was.
        assert!(has_attr(&xml, (NS_MWG_RS, "Area"), ("", "Confidence"), "87"), "{xml}");
        // Every region: the two foreign ones as they were, and ours.
        let read_back = mwg(&xml);
        assert_eq!(read_back.dims, [Some(("6000".to_string(), "4000".to_string()))]);
        assert_eq!(named(&read_back, "Bob")[0].area, (0.8, 0.7, 0.1, 0.2), "{xml}");
        assert_eq!(named(&read_back, "Ann")[0].area, (0.25, 0.35, 0.1, 0.1), "{xml}");
        assert_eq!(named(&read_back, "Ann")[0].face_id, None, "{xml}");
        assert_eq!(named(&read_back, "Carl").len(), 1, "{xml}");
        assert_eq!(region_names(&photo), ["Ann", "Bob", "Carl"]);
        let history = element_text(&xml, (NS_RDF, "Description"), (NS_DARKTABLE, "history_end"));
        assert_eq!(history.as_deref(), Some("5"), "{xml}");
        // The damaged file, backed up byte for byte before the repair.
        let backup = std::fs::read_to_string(sidecar_backup_path(&sidecar_path(&photo))).unwrap();
        assert_eq!(backup, DAMAGED);
    }

    /// The same, with the damage made by the released writer's own sequence ([`released_rewrite`])
    /// once and twice, the way the review's probe r1/r1b made it: `about=""` next to an empty
    /// `rdf:about` (#143 review, H1).
    #[test]
    fn a_face_write_repairs_what_a_released_build_wrote() {
        let once = released_rewrite(&intact());
        let twice = released_rewrite(&once);
        for (case, damaged) in [("once", once), ("twice", twice)] {
            assert!(!damaged.contains("rdf:parseType") && damaged.contains(r#" parseType="Resource""#), "{damaged}");
            assert!(damaged.contains(r#" about="""#) && damaged.contains(r#"rdf:about="""#), "{damaged}");

            let (_dir, photo) = seeded_photo(&format!("xmp-143-released-{case}"), &damaged);
            write_face_regions(&photo, CAT, &[carl()], &[], &[], sized(6000, 4000)).unwrap();

            let xml = read(&sidecar_path(&photo));
            let read_back = mwg(&xml);
            assert_eq!(read_back.dims, [Some(("6000".to_string(), "4000".to_string()))], "{case}: {xml}");
            assert_eq!(named(&read_back, "Bob")[0].area, (0.8, 0.7, 0.1, 0.2), "{case}: {xml}");
            assert_eq!(named(&read_back, "Ann")[0].area, (0.25, 0.35, 0.1, 0.1), "{case}: {xml}");
            assert_eq!(region_names(&photo), ["Ann", "Bob", "Carl"], "{case}");
            let abouts: Vec<_> = namespaced_attributes(&xml).into_iter().filter(|a| a.3 == "about").collect();
            assert_eq!(abouts.len(), 1, "{case}: one about, in rdf: {abouts:?}\n{xml}");
            assert!(has_attr(&xml, (NS_RDF, "Description"), (NS_RDF, "about"), ""), "{case}: {xml}");
        }
    }

    /// A Description whose original `rdf:about` was not empty (old Photoshop wrote
    /// `rdf:about="uuid:…"`): the released writer left `about="uuid:…" rdf:about=""`. The
    /// original value is the unprefixed one, and it is what the repair keeps.
    #[test]
    fn a_non_empty_about_the_released_writer_shadowed_is_kept() {
        let damaged = released_rewrite(&intact().replace(r#"rdf:about="""#, r#"rdf:about="uuid:faf5bdd5-ba3d""#));
        assert!(damaged.contains(r#"about="uuid:faf5bdd5-ba3d""#) && damaged.contains(r#"rdf:about="""#), "{damaged}");
        let (_dir, photo) = seeded_photo("xmp-143-about-uuid", &damaged);
        write_face_regions(&photo, CAT, &[carl()], &[], &[], sized(6000, 4000)).unwrap();

        let xml = read(&sidecar_path(&photo));
        let abouts: Vec<_> = namespaced_attributes(&xml).into_iter().filter(|a| a.3 == "about").collect();
        assert_eq!(abouts.len(), 1, "{abouts:?}\n{xml}");
        assert!(has_attr(&xml, (NS_RDF, "Description"), (NS_RDF, "about"), "uuid:faf5bdd5-ba3d"), "{xml}");
        assert_eq!(region_names(&photo), ["Ann", "Bob", "Carl"]);
    }

    /// When the damage is ambiguous (an element with both `parseType` and `rdf:parseType`, an
    /// Area field twice, two different non-empty abouts), or the file was never written by ChairPhoto (no
    /// `chairphoto:LastWrite`, so the damage is not ours), nothing is repaired: the face write
    /// is refused as before and the sidecar is left byte for byte.
    #[test]
    fn an_ambiguous_or_foreign_damaged_sidecar_is_left_untouched() {
        let cases = [
            ("both-parse-types", DAMAGED.replace(
                r#"<mwg-rs:Regions parseType="Resource">"#,
                r#"<mwg-rs:Regions parseType="Resource" rdf:parseType="Resource">"#,
            )),
            ("area-field-twice", DAMAGED.replace(r#"x="0.8""#, r#"x="0.8" stArea:x="0.8""#)),
            ("never-ours", DAMAGED.replace("<chairphoto:LastWrite>1727000000</chairphoto:LastWrite>", "")),
            ("two-different-abouts", DAMAGED.replace(r#"about="" rdf:about="""#, r#"about="uuid:a" rdf:about="uuid:b""#)),
        ];
        for (case, sidecar) in cases {
            assert_ne!(sidecar, DAMAGED, "{case}: the fixture changed");
            let (_dir, photo) = seeded_photo(&format!("xmp-143-damaged-{case}"), &sidecar);
            let err = write_face_regions(&photo, CAT, &[carl()], &[], &[], sized(6000, 4000)).unwrap_err();
            assert!(matches!(err, RegionWriteError::Refused(_)), "{case}: {err:?}");
            assert_eq!(read(&sidecar_path(&photo)), sidecar, "{case}");
            // A sidecar without `chairphoto:LastWrite` is backed up at open whatever the
            // writer then does (the first-write rule); the backup is the file as it was.
            let backup = std::fs::read_to_string(sidecar_backup_path(&sidecar_path(&photo))).ok();
            let expected = (case == "never-ours").then_some(sidecar.as_str());
            assert_eq!(backup.as_deref(), expected, "{case}");
        }
    }
}
