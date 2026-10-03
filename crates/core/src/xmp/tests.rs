//! Tests across every sidecar writer: foreign attributes keep their namespace (#138), and an
//! owned property is replaced in compact form and in every `rdf:Description` (#142, #143).

use crate::catalog::IptcFields;
use crate::xmp::ns::{NS_CHAIRPHOTO, NS_DC, NS_EXIF, NS_MWG_RS, NS_PHOTOSHOP, NS_RDF, NS_STAREA, NS_X, NS_XMP};
use crate::xmp::test_xml::{
    element_text, has_attr, namespaced_attributes, namespaced_elements, read, sized, CAT, NS_FOREIGN,
};
use crate::xmp::{
    overwrite_identifier, read_face_regions, read_gps, read_identifier, read_import_batch, sidecar_path,
    write_face_regions, write_gps, write_identifier, write_import_batch, write_iptc, write_keywords,
    FaceRegion, ReadRegion,
};

// ── prefixed attributes survive every writer (issue #138) ───────────────

const NS_XML: &str = "http://www.w3.org/XML/1998/namespace";
const NS_DARKTABLE: &str = "http://darktable.sf.net/";
const NS_CRS: &str = "http://ns.adobe.com/camera-raw-settings/1.0/";
const NS_DIGIKAM: &str = "http://www.digikam.org/ns/1.0/";

/// A foreign sidecar written by hand — not by ChairPhoto — whose foreign data is carried
/// in prefixed *attributes*: Exiv2's `x:xmptk`, compact `xmp:`/`darktable:`/`crs:`
/// properties on `rdf:Description`, an MWG region whose Area is in the attribute
/// (shorthand) form with a `digiKam:` extra, and a foreign Lang Alt's `xml:lang`.
const FOREIGN_PREFIXED_ATTRS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="XMP Core 4.4.0-Exiv2">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:darktable="http://darktable.sf.net/"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    xmlns:xmpRights="http://ns.adobe.com/xap/1.0/rights/"
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
    xmlns:digiKam="http://www.digikam.org/ns/1.0/"
    xmp:Rating="1"
    darktable:xmp_version="5"
    darktable:auto_presets_applied="1"
    crs:Exposure2012="+0.50">
   <xmpRights:UsageTerms>
    <rdf:Alt>
     <rdf:li xml:lang="x-default">All rights reserved</rdf:li>
    </rdf:Alt>
   </xmpRights:UsageTerms>
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Bob</mwg-rs:Name>
       <mwg-rs:Type>Face</mwg-rs:Type>
       <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2"
         stArea:unit="normalized" digiKam:Confidence="87"/>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

fn assert_foreign_attributes_intact(xml: &str, after: &str) {
    let attrs = namespaced_attributes(xml);
    let expected = [
        (NS_X, "xmpmeta", NS_X, "xmptk", "XMP Core 4.4.0-Exiv2"),
        (NS_RDF, "Description", NS_RDF, "about", ""),
        (NS_RDF, "Description", NS_XMP, "Rating", "1"),
        (NS_RDF, "Description", NS_DARKTABLE, "xmp_version", "5"),
        (NS_RDF, "Description", NS_DARKTABLE, "auto_presets_applied", "1"),
        (NS_RDF, "Description", NS_CRS, "Exposure2012", "+0.50"),
        (NS_RDF, "li", NS_XML, "lang", "x-default"),
        (NS_MWG_RS, "Area", NS_STAREA, "x", "0.8"),
        (NS_MWG_RS, "Area", NS_STAREA, "y", "0.7"),
        (NS_MWG_RS, "Area", NS_STAREA, "w", "0.1"),
        (NS_MWG_RS, "Area", NS_STAREA, "h", "0.2"),
        (NS_MWG_RS, "Area", NS_STAREA, "unit", "normalized"),
        (NS_MWG_RS, "Area", NS_DIGIKAM, "Confidence", "87"),
    ];
    for (ens, el_name, ans, an, v) in expected {
        let found = attrs.iter().any(|(e_ns, e_l, a_ns, a_l, val)| {
            e_ns == ens && e_l == el_name && a_ns == ans && a_l == an && val == v
        });
        assert!(found, "after {after}: {{{ans}}}{an}=\"{v}\" on {{{ens}}}{el_name} was lost \
            or moved to another namespace\nattributes: {attrs:#?}\n{xml}");
    }
    // No attribute was demoted to no-namespace (the #138 symptom: `x`, `Rating`, …).
    // Every attribute in the fixture and in ChairPhoto's own output is prefixed.
    let stripped: Vec<_> = attrs.iter().filter(|(_, _, a_ns, _, _)| a_ns.is_empty()).collect();
    assert!(stripped.is_empty(), "after {after}: attributes lost their namespace: {stripped:?}");
}

/// Issue #138: every ChairPhoto sidecar writer read-modify-writes the sidecar, and must
/// keep each foreign attribute's namespace. A hand-written foreign sidecar carrying
/// prefixed attributes from several namespaces goes through every writer in turn; after
/// each one an independent namespace-aware reader must still find every foreign
/// attribute under its original namespace URI with its original value, and ChairPhoto's
/// own reader must still read the attribute-form (`stArea:x="…"`) region.
#[test]
fn writers_preserve_prefixed_foreign_attributes() {
    let dir = crate::test_support::TestTmpDir::new("xmp-prefixed-attrs");
    let photo = dir.join("DSC138.ARW");
    std::fs::write(&photo, b"raw").unwrap();
    std::fs::write(sidecar_path(&photo), FOREIGN_PREFIXED_ATTRS).unwrap();
    assert_foreign_attributes_intact(FOREIGN_PREFIXED_ATTRS, "nothing (fixture self-check)");

    let bob = |regions: &[ReadRegion], after: &str| {
        let r = regions.iter().find(|r| r.name == "Bob").unwrap_or_else(|| {
            panic!("after {after}: Bob's attribute-form region unread: {regions:?}")
        });
        // Center (0.8, 0.7), size 0.1×0.2 → top-left (0.75, 0.6).
        let (x, y, w, h) = r.bbox;
        assert!((x - 0.75).abs() < 1e-4 && (y - 0.6).abs() < 1e-4, "after {after}: {r:?}");
        assert!((w - 0.1).abs() < 1e-4 && (h - 0.2).abs() < 1e-4, "after {after}: {r:?}");
    };
    bob(&read_face_regions(&photo), "nothing");

    let uuid = "6f1c1f0e-8f5e-4a51-9a51-3c1b2a0d1388";
    let steps: Vec<(&str, Box<dyn Fn()>)> = vec![
        ("write_iptc", Box::new(|| {
            let fields = IptcFields {
                title: "Ferry".into(),
                city: "Trondheim".into(),
                ..Default::default()
            };
            write_iptc(&photo, &IptcFields::default(), &fields).unwrap();
        })),
        ("write_keywords", Box::new(|| {
            write_keywords(&photo, &["boat".into()], &["Places|Norway".into()]).unwrap();
        })),
        ("write_identifier", Box::new(|| write_identifier(&photo, uuid).unwrap())),
        ("write_import_batch", Box::new(|| write_import_batch(&photo, uuid).unwrap())),
        ("write_gps", Box::new(|| write_gps(&photo, 63.43, 10.39).unwrap())),
        ("write_face_regions", Box::new(|| {
            let ours = [FaceRegion { face_id: 0, name: "Alice".into(), bbox: (0.1, 0.1, 0.2, 0.2) }];
            write_face_regions(&photo, CAT, &ours, &[], &[], sized(6000, 4000)).unwrap();
        })),
        ("overwrite_identifier", Box::new(|| {
            overwrite_identifier(&photo, uuid).unwrap();
        })),
    ];
    for (name, step) in steps {
        step();
        assert_foreign_attributes_intact(&read(&sidecar_path(&photo)), name);
        bob(&read_face_regions(&photo), name);
    }

    // ChairPhoto's own data from those writers reads back too.
    assert_eq!(read_identifier(&photo).as_deref(), Some(uuid));
    assert_eq!(read_import_batch(&photo).as_deref(), Some(uuid));
    assert!(read_gps(&photo).is_some());
    let names: Vec<String> = read_face_regions(&photo).into_iter().map(|r| r.name).collect();
    assert!(names.contains(&"Alice".to_string()), "{names:?}");
}

/// The other half of #138: once prefixed attributes survive parsing, a property a writer
/// owns can arrive in compact attribute form (`xmp:Identifier="…"`, `photoshop:City="…"`
/// on the Description, as exiftool and darktable write them). The writer owns that form
/// too: it must be replaced, not left beside the new element as a second value — and for
/// the identifier, `read_identifier` (which checks the attribute first) must then see the
/// new UUID, or an Overwrite (issue #33) silently would not take.
#[test]
fn writers_replace_owned_properties_in_compact_attribute_form() {
    let dir = crate::test_support::TestTmpDir::new("xmp-compact-owned");
    let photo = dir.join("DSC139.ARW");
    std::fs::write(&photo, b"raw").unwrap();
    std::fs::write(
        sidecar_path(&photo),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/"
    xmp:Identifier="foreign-id" photoshop:City="Oslo" xmp:Rating="3"/>
 </rdf:RDF>
</x:xmpmeta>"#,
    )
    .unwrap();
    assert_eq!(read_identifier(&photo).as_deref(), Some("foreign-id"), "compact form is read");

    let uuid = "0b7d5f7e-4d0c-4c55-8a3e-1f5e6f0a1390";
    overwrite_identifier(&photo, uuid).unwrap();
    write_iptc(&photo, &IptcFields::default(), &IptcFields { city: "Trondheim".into(), ..Default::default() })
        .unwrap();

    let xml = read(&sidecar_path(&photo));
    assert_eq!(read_identifier(&photo).as_deref(), Some(uuid), "{xml}");
    let attrs = namespaced_attributes(&xml);
    let has = |ns: &str, local: &str| attrs.iter().any(|a| a.2 == ns && a.3 == local);
    assert!(!has(NS_XMP, "Identifier"), "stale compact identifier kept:\n{xml}");
    assert!(!has(NS_PHOTOSHOP, "City"), "stale compact City kept:\n{xml}");
    assert!(has(NS_XMP, "Rating"), "a property no writer owns must survive:\n{xml}");
    assert_eq!(xml.matches("Oslo").count(), 0, "{xml}");
    assert_eq!(xml.matches("<photoshop:City>Trondheim</photoshop:City>").count(), 1, "{xml}");
}

/// Issue #142: exiftool writes one `rdf:Description` per namespace, so an owned property
/// can sit in any of them, as an element or in compact attribute form. The identifier,
/// IPTC and GPS writers must remove every stale instance, not only the first
/// Description's, and write exactly one value — while every property they do not own,
/// in every Description, survives.
#[test]
fn writers_replace_owned_properties_in_every_description() {
    let dir = crate::test_support::TestTmpDir::new("xmp-142-multi-desc");
    let photo = dir.join("DSC142.ARW");
    std::fs::write(&photo, b"raw").unwrap();
    // Hand-written in exiftool's shape: single quotes, one Description per namespace.
    std::fs::write(sidecar_path(&photo), r#"<?xpacket begin='' id='W5M0MpCehiHzreSzNTczkc9d'?>
<x:xmpmeta xmlns:x='adobe:ns:meta/' x:xmptk='Image::ExifTool 12.76'>
<rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'>
 <rdf:Description rdf:about='' xmlns:dc='http://purl.org/dc/elements/1.1/'>
  <dc:format>image/x-sony-arw</dc:format>
 </rdf:Description>
 <rdf:Description rdf:about='' xmlns:exif='http://ns.adobe.com/exif/1.0/'>
  <exif:ExposureTime>1/250</exif:ExposureTime>
  <exif:GPSLatitude>59,54.834000N</exif:GPSLatitude>
  <exif:GPSLongitude>10,45.132000E</exif:GPSLongitude>
 </rdf:Description>
 <rdf:Description rdf:about='' xmlns:photoshop='http://ns.adobe.com/photoshop/1.0/'
  photoshop:City='Oslo' photoshop:Instructions='keep'/>
 <rdf:Description rdf:about='' xmlns:xmp='http://ns.adobe.com/xap/1.0/'>
  <xmp:Identifier>
   <rdf:Bag><rdf:li>dam:asset/4711</rdf:li></rdf:Bag>
  </xmp:Identifier>
  <xmp:Rating>3</xmp:Rating>
 </rdf:Description>
</rdf:RDF>
</x:xmpmeta>
<?xpacket end='w'?>"#).unwrap();

    let uuid = "6f1c1f0e-8f5e-4a51-9a51-3c1b2a0d1420";
    overwrite_identifier(&photo, uuid).unwrap();
    write_iptc(&photo, &IptcFields::default(), &IptcFields { city: "Trondheim".into(), ..Default::default() })
        .unwrap();
    write_gps(&photo, 63.4305, 10.3951).unwrap();

    let xml = read(&sidecar_path(&photo));
    let attrs = namespaced_attributes(&xml);
    let compact = |ns: &str, local: &str| attrs.iter().filter(|a| a.2 == ns && a.3 == local).count();
    let elements = namespaced_elements(&xml);
    let texts = |ns: &str, local: &str| -> Vec<String> {
        elements
            .iter()
            .filter(|e| e.name.0 == ns && e.name.1 == local)
            .map(|e| e.text.clone())
            .collect()
    };
    // Exactly one value of each owned property, ChairPhoto's, in element form.
    assert_eq!(texts(NS_XMP, "Identifier"), [uuid], "{xml}");
    assert_eq!(compact(NS_XMP, "Identifier"), 0, "{xml}");
    assert!(!xml.contains("dam:asset/4711"), "the overwritten identifier lingers:\n{xml}");
    assert_eq!(texts(NS_PHOTOSHOP, "City"), ["Trondheim"], "{xml}");
    assert_eq!(compact(NS_PHOTOSHOP, "City"), 0, "stale compact City kept:\n{xml}");
    assert_eq!(texts(NS_EXIF, "GPSLatitude"), ["63,25.830000N"], "{xml}");
    assert_eq!(texts(NS_EXIF, "GPSLongitude"), ["10,23.706000E"], "{xml}");
    assert_eq!(read_identifier(&photo).as_deref(), Some(uuid));
    let (lat, lng) = read_gps(&photo).unwrap();
    assert!((lat - 63.4305).abs() < 1e-6 && (lng - 10.3951).abs() < 1e-6, "{lat},{lng}");
    // What no writer owns survives in its own Description.
    assert_eq!(texts(NS_DC, "format"), ["image/x-sony-arw"], "{xml}");
    assert_eq!(texts(NS_EXIF, "ExposureTime"), ["1/250"], "{xml}");
    assert_eq!(texts(NS_XMP, "Rating"), ["3"], "{xml}");
    assert_eq!(compact(NS_PHOTOSHOP, "Instructions"), 1, "{xml}");
    assert_eq!(texts(NS_CHAIRPHOTO, "LastWrite").len(), 1, "{xml}");
}

/// Issue #143 item 4: owned compact properties are recognised by namespace URI, never by
/// local name or prefix. A foreign namespace may use an owned local name (`foo:Identifier`,
/// `foo:City`): those attributes are not ours and must survive. An owned namespace may be
/// bound to a non-canonical prefix (`xap:` for xmp, `ps:` for photoshop): those attributes
/// are ours and must go. Before this test, `attr_is` ignoring the namespace passed every
/// test in this module.
#[test]
fn compact_owned_properties_are_matched_by_namespace_not_local_name() {
    let dir = crate::test_support::TestTmpDir::new("xmp-143-attr-ns");
    let photo = dir.join("DSC143.ARW");
    std::fs::write(&photo, b"raw").unwrap();
    std::fs::write(sidecar_path(&photo), r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:foo="urn:example:foreign"
    xmlns:xap="http://ns.adobe.com/xap/1.0/"
    xmlns:ps="http://ns.adobe.com/photoshop/1.0/"
    foo:Identifier="keep-me" foo:City="keep-city"
    xap:Identifier="drop-me" ps:City="Oslo"/>
 </rdf:RDF>
</x:xmpmeta>"#).unwrap();
    assert_eq!(read_identifier(&photo).as_deref(), Some("drop-me"),
        "the identifier is read by namespace, whatever its prefix");

    let uuid = "6f1c1f0e-8f5e-4a51-9a51-3c1b2a0d1430";
    overwrite_identifier(&photo, uuid).unwrap();
    write_iptc(&photo, &IptcFields::default(), &IptcFields { city: "Trondheim".into(), ..Default::default() })
        .unwrap();

    let xml = read(&sidecar_path(&photo));
    let desc = (NS_RDF, "Description");
    assert!(has_attr(&xml, desc, (NS_FOREIGN, "Identifier"), "keep-me"),
        "a foreign attribute with an owned local name was removed:\n{xml}");
    assert!(has_attr(&xml, desc, (NS_FOREIGN, "City"), "keep-city"),
        "a foreign attribute with an owned local name was removed:\n{xml}");
    let attrs = namespaced_attributes(&xml);
    let compact = |ns: &str, local: &str| attrs.iter().any(|a| a.2 == ns && a.3 == local);
    assert!(!compact(NS_XMP, "Identifier"), "xap:Identifier is ours and was kept:\n{xml}");
    assert!(!compact(NS_PHOTOSHOP, "City"), "ps:City is ours and was kept:\n{xml}");
    assert_eq!(read_identifier(&photo).as_deref(), Some(uuid), "{xml}");
    assert_eq!(element_text(&xml, desc, (NS_PHOTOSHOP, "City")).as_deref(),
        Some("Trondheim"), "{xml}");
}
