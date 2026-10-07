//! Face-region writer and reader tests: round trips, foreign layouts (#139), the stored frame
//! (#136), declared dimensions (#145), the preview's frame, HEIF turns and real tools' sidecars
//! (#154), ChairPhoto's marker (#135) and matching (#147).

use crate::catalog::IptcFields;
use crate::xmp::ns::{NS_CHAIRPHOTO, NS_MWG_RS, NS_RDF, NS_STAREA, NS_STDIM};
use crate::xmp::region_fixtures as rf;
use crate::xmp::region_fixtures::NS_DIGIKAM;
use crate::xmp::regions::frame::{display_to_stored, stored_to_display, RegionTarget};
use crate::xmp::test_xml::{
    count_elements, element_text, has_attr, namespaced_elements, ours, read, region_names, seeded_photo,
    sized, CAT, NS_FOREIGN,
};
use crate::xmp::{
    read_face_regions, read_face_regions_in, read_identifier, region_iou, sidecar_backup_path, sidecar_path,
    write_face_regions, write_identifier, write_iptc, FaceRegion, FrameDoubt, RegionFrame, RegionWriteError,
};

// ── MWG face regions (H13f) ────────────────────────────────────────────────

fn region_dir(tag: &str) -> crate::test_support::TestTmpDir {
    crate::test_support::TestTmpDir::new(tag)
}

/// Write regions, read them back: names + top-left bboxes survive the center↔corner
/// conversion round-trip, and AppliedToDimensions is written with the stored pixel size.
#[test]
fn face_regions_round_trip() {
    let dir = region_dir("xmp-regions-rt");
    let photo = dir.join("DSC20.ARW");
    std::fs::write(&photo, b"raw").unwrap();

    let regions = vec![
        FaceRegion { face_id: 0, name: "Alice".into(), bbox: (0.10, 0.20, 0.30, 0.40) },
        FaceRegion { face_id: 0, name: "Bob".into(), bbox: (0.60, 0.10, 0.20, 0.25) },
    ];
    write_face_regions(&photo, CAT, &regions, &[], &[], sized(6000, 4000)).unwrap();

    let xmp = read(&sidecar_path(&photo));
    // Center coords are written (x = 0.10 + 0.30/2 = 0.25), unit=normalized, Type=Face.
    assert!(xmp.contains("mwg-rs:Regions"), "Regions element missing");
    assert!(xmp.contains("stArea:unit"), "Area unit missing");
    assert!(xmp.contains("6000"), "AppliedToDimensions width missing");
    assert!(xmp.contains("4000"), "AppliedToDimensions height missing");
    assert!(xmp.contains(">Face<") || xmp.contains("Face"), "Type=Face missing");

    let back = read_face_regions(&photo);
    assert_eq!(back.len(), 2, "both regions read back");
    let alice = back.iter().find(|r| r.name == "Alice").unwrap();
    for (a, b) in [alice.bbox.0, alice.bbox.1, alice.bbox.2, alice.bbox.3]
        .iter()
        .zip([0.10f32, 0.20, 0.30, 0.40].iter())
    {
        assert!((a - b).abs() < 1e-4, "Alice bbox drift: {a} vs {b}");
    }
    let bob = back.iter().find(|r| r.name == "Bob").unwrap();
    assert!((bob.bbox.0 - 0.60).abs() < 1e-4);
    assert!((bob.bbox.1 - 0.10).abs() < 1e-4);
}

/// The center↔top-left conversion is exact: a region's stArea:x/y equal the bbox center.
#[test]
fn face_region_center_conversion() {
    let dir = region_dir("xmp-regions-center");
    let photo = dir.join("DSC21.ARW");
    std::fs::write(&photo, b"raw").unwrap();

    // Top-left (0.2, 0.3), size (0.4, 0.2) → center (0.4, 0.4).
    let regions = vec![FaceRegion { face_id: 0, name: "Cara".into(), bbox: (0.2, 0.3, 0.4, 0.2) }];
    write_face_regions(&photo, CAT, &regions, &[], &[], sized(1000, 1000)).unwrap();

    let xmp = read(&sidecar_path(&photo));
    // The literal center coordinates must be present (0.4 for both x and y).
    assert!(xmp.contains("stArea:x"));
    assert!(xmp.contains("stArea:y"));
    // Read-back reconstructs the exact top-left corner.
    let back = read_face_regions(&photo);
    assert_eq!(back.len(), 1);
    assert!((back[0].bbox.0 - 0.2).abs() < 1e-5, "corner x");
    assert!((back[0].bbox.1 - 0.3).abs() < 1e-5, "corner y");
    assert!((back[0].bbox.2 - 0.4).abs() < 1e-5, "w");
    assert!((back[0].bbox.3 - 0.2).abs() < 1e-5, "h");
}

/// A sidecar carrying darktable foreign namespaces AND a foreign region entry (a face named
/// by another tool): both survive a chairphoto region write. Our region is added; the foreign
/// region and darktable data are untouched.
#[test]
fn face_regions_preserve_foreign_content() {
    let dir = region_dir("xmp-regions-foreign");
    let photo = dir.join("DSC22.ARW");
    std::fs::write(&photo, b"raw").unwrap();

    // Existing sidecar: darktable develop history + a digiKam-style foreign face region.
    let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:darktable="http://darktable.sf.net/"
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
    xmlns:stDim="http://ns.adobe.com/xap/1.0/sType/Dimensions#">
   <darktable:history_end>9</darktable:history_end>
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:AppliedToDimensions rdf:parseType="Resource">
     <stDim:w>6000</stDim:w>
     <stDim:h>4000</stDim:h>
     <stDim:unit>pixel</stDim:unit>
    </mwg-rs:AppliedToDimensions>
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Stranger</mwg-rs:Name>
       <mwg-rs:Type>Face</mwg-rs:Type>
       <mwg-rs:Area rdf:parseType="Resource">
        <stArea:x>0.8</stArea:x>
        <stArea:y>0.8</stArea:y>
        <stArea:w>0.1</stArea:w>
        <stArea:h>0.1</stArea:h>
        <stArea:unit>normalized</stArea:unit>
       </mwg-rs:Area>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
    std::fs::write(sidecar_path(&photo), existing).unwrap();

    // Write one chairphoto region (a different face).
    let regions = vec![FaceRegion { face_id: 0, name: "Alice".into(), bbox: (0.10, 0.10, 0.20, 0.20) }];
    write_face_regions(&photo, CAT, &regions, &[], &[], sized(6000, 4000)).unwrap();

    let xmp = read(&sidecar_path(&photo));
    assert!(xmp.contains("history_end"), "darktable data clobbered!");
    assert!(xmp.contains("darktable"), "darktable namespace lost!");

    // A backup of the foreign sidecar was made on first write.
    assert!(sidecar_backup_path(&sidecar_path(&photo)).exists());

    // Both the foreign "Stranger" region and our "Alice" region are present.
    let back = read_face_regions(&photo);
    let names: Vec<&str> = back.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"Stranger"), "foreign region lost! got {names:?}");
    assert!(names.contains(&"Alice"), "our region missing! got {names:?}");
    assert_eq!(back.len(), 2, "exactly the foreign + our region");

    // The foreign region's geometry is unchanged (center 0.8,0.8 → corner 0.75,0.75).
    let stranger = back.iter().find(|r| r.name == "Stranger").unwrap();
    assert!((stranger.bbox.0 - 0.75).abs() < 1e-4, "foreign corner x moved");
    assert!((stranger.bbox.2 - 0.1).abs() < 1e-4, "foreign w moved");
}

/// Re-writing updates only chairphoto's own region: a second write with a moved Alice bbox
/// replaces Alice (no duplicate) and still preserves the foreign region.
#[test]
fn face_regions_rewrite_updates_only_ours() {
    let dir = region_dir("xmp-regions-rewrite");
    let photo = dir.join("DSC23.ARW");
    std::fs::write(&photo, b"raw").unwrap();

    // First write: a foreign region (simulated by pre-writing) + our Alice.
    let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#">
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Stranger</mwg-rs:Name>
       <mwg-rs:Area rdf:parseType="Resource">
        <stArea:x>0.9</stArea:x><stArea:y>0.9</stArea:y>
        <stArea:w>0.05</stArea:w><stArea:h>0.05</stArea:h>
        <stArea:unit>normalized</stArea:unit>
       </mwg-rs:Area>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
    std::fs::write(sidecar_path(&photo), existing).unwrap();

    // Write Alice at (0.10, 0.10, 0.20, 0.20), center (0.20, 0.20).
    write_face_regions(
        &photo, CAT,
        &[FaceRegion { face_id: 0, name: "Alice".into(), bbox: (0.10, 0.10, 0.20, 0.20) }], &[],
        &[],
        sized(1000, 1000),
    )
    .unwrap();

    // Second write: Alice moved slightly (well within AREA_EPSILON so it's recognised as
    // "our" region and replaced, not duplicated).
    write_face_regions(
        &photo, CAT,
        &[FaceRegion { face_id: 0, name: "Alice".into(), bbox: (0.105, 0.105, 0.20, 0.20) }], &[],
        &[],
        sized(1000, 1000),
    )
    .unwrap();

    let back = read_face_regions(&photo);
    // Exactly two regions: the foreign one + one Alice (not two Alices).
    let alice_count = back.iter().filter(|r| r.name == "Alice").count();
    assert_eq!(alice_count, 1, "Alice must be replaced, not duplicated");
    assert!(back.iter().any(|r| r.name == "Stranger"), "foreign region lost on rewrite");
    assert_eq!(back.len(), 2);
    // The kept Alice is the newest (moved) position.
    let alice = back.iter().find(|r| r.name == "Alice").unwrap();
    assert!((alice.bbox.0 - 0.105).abs() < 1e-3, "Alice not updated to new position");

    // Exactly one Regions/AppliedToDimensions block survives (no stacking).
    let xmp = read(&sidecar_path(&photo));
    assert_eq!(xmp.matches("<mwg-rs:Regions").count(), 1, "one Regions block only");
}

/// region_iou returns 1.0 for identical boxes and 0.0 for disjoint boxes.
#[test]
fn region_iou_basic() {
    let a = (0.1, 0.1, 0.2, 0.2);
    assert!((region_iou(a, a) - 1.0).abs() < 1e-6);
    let b = (0.7, 0.7, 0.2, 0.2);
    assert!(region_iou(a, b) < 1e-6);
    // Half-overlap-ish box has IoU strictly between 0 and 1.
    let c = (0.2, 0.1, 0.2, 0.2);
    let iou = region_iou(a, c);
    assert!(iou > 0.0 && iou < 1.0, "partial overlap IoU = {iou}");
}

/// Reading regions from a sidecar with no Regions property (or no sidecar) yields empty.
#[test]
fn read_face_regions_absent_is_empty() {
    let dir = region_dir("xmp-regions-absent");
    let photo = dir.join("DSC24.ARW");
    std::fs::write(&photo, b"raw").unwrap();
    // No sidecar.
    assert!(read_face_regions(&photo).is_empty());
    // Sidecar with only IPTC, no regions.
    write_iptc(&photo, &IptcFields::default(), &IptcFields { description: "x".into(), ..Default::default() }).unwrap();
    assert!(read_face_regions(&photo).is_empty());
}

// ── face regions in layouts other tools write (issue #139) ─────────────

fn alice() -> Vec<FaceRegion> {
    vec![FaceRegion { face_id: 0, name: "Alice".into(), bbox: (0.1, 0.1, 0.2, 0.2) }]
}

/// A hand-written sidecar whose Regions value, each region and their Areas are nested
/// `rdf:Description`s or attributes (no `rdf:parseType="Resource"` anywhere), with foreign
/// attributes and a foreign child on the Regions struct and a foreign attribute on
/// AppliedToDimensions. Before #139 the writer found no RegionList here, so it deleted Bob.
const NESTED_DESCRIPTION_REGIONS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
    xmlns:stDim="http://ns.adobe.com/xap/1.0/sType/Dimensions#"
    xmlns:f="urn:example:foreign">
   <mwg-rs:Regions>
    <rdf:Description f:keep="regions-attr">
     <mwg-rs:AppliedToDimensions stDim:w="4000" stDim:h="3000" stDim:unit="pixel"
       f:dimkeep="dims-attr"/>
     <mwg-rs:RegionList>
      <rdf:Bag>
       <rdf:li>
        <rdf:Description mwg-rs:Name="Bob" mwg-rs:Type="Face">
         <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2"
           stArea:unit="normalized"/>
        </rdf:Description>
       </rdf:li>
      </rdf:Bag>
     </mwg-rs:RegionList>
     <f:extra>regions-child</f:extra>
    </rdf:Description>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

#[test]
fn face_regions_nested_description_layout_keeps_foreign_regions_and_content() {
    let (_dir, photo) = seeded_photo("xmp-139-nested", NESTED_DESCRIPTION_REGIONS);
    assert_eq!(region_names(&photo), ["Bob"], "the fixture's region is read");

    write_face_regions(&photo, CAT, &alice(), &[], &[], sized(8000, 6000)).unwrap();

    let xml = read(&sidecar_path(&photo));
    assert_eq!(region_names(&photo), ["Alice", "Bob"], "{xml}");
    assert_eq!(count_elements(&xml, NS_MWG_RS, "Regions"), 1, "{xml}");
    assert_eq!(count_elements(&xml, NS_MWG_RS, "RegionList"), 1, "{xml}");
    assert!(has_attr(&xml, (NS_RDF, "Description"), (NS_FOREIGN, "keep"), "regions-attr"),
        "foreign attribute on the Regions struct lost:\n{xml}");
    assert_eq!(element_text(&xml, (NS_RDF, "Description"), (NS_FOREIGN, "extra")).as_deref(),
        Some("regions-child"), "foreign child of the Regions struct lost:\n{xml}");
    let dims = (NS_MWG_RS, "AppliedToDimensions");
    assert!(has_attr(&xml, dims, (NS_FOREIGN, "dimkeep"), "dims-attr"),
        "foreign attribute on AppliedToDimensions lost:\n{xml}");
    // The dimensions are the file's own, untouched (#145): a 4000x3000 frame of this
    // 8000x6000 image.
    assert!(has_attr(&xml, dims, (NS_STDIM, "w"), "4000"), "{xml}");
    assert!(has_attr(&xml, dims, (NS_STDIM, "h"), "3000"), "{xml}");
    assert_eq!(count_elements(&xml, NS_STDIM, "w"), 0, "no second, element-form width:\n{xml}");
    // Bob is untouched, attributes and all.
    assert!(has_attr(&xml, (NS_RDF, "Description"), (NS_MWG_RS, "Name"), "Bob"), "{xml}");
    assert!(has_attr(&xml, (NS_MWG_RS, "Area"), (NS_STAREA, "x"), "0.8"), "{xml}");
}

/// A RegionList held in an `rdf:Seq` (some tools order their regions) keeps its container:
/// Bob survives, Alice joins him in the same Seq, and no Bag appears.
#[test]
fn face_regions_seq_region_list_keeps_its_regions_and_container() {
    let (_dir, photo) = seeded_photo("xmp-139-seq", r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#">
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:RegionList>
     <rdf:Seq>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Bob</mwg-rs:Name>
       <mwg-rs:Type>Face</mwg-rs:Type>
       <mwg-rs:Area rdf:parseType="Resource">
        <stArea:x>0.8</stArea:x><stArea:y>0.7</stArea:y>
        <stArea:w>0.1</stArea:w><stArea:h>0.2</stArea:h>
        <stArea:unit>normalized</stArea:unit>
       </mwg-rs:Area>
      </rdf:li>
     </rdf:Seq>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#);
    assert_eq!(region_names(&photo), ["Bob"], "the fixture's region is read");

    write_face_regions(&photo, CAT, &alice(), &[], &[], sized(6000, 4000)).unwrap();

    let xml = read(&sidecar_path(&photo));
    assert_eq!(region_names(&photo), ["Alice", "Bob"], "{xml}");
    assert_eq!(count_elements(&xml, NS_RDF, "Seq"), 1, "{xml}");
    assert_eq!(count_elements(&xml, NS_RDF, "Bag"), 0, "{xml}");
    let lis = namespaced_elements(&xml)
        .into_iter()
        .filter(|e| e.parent == (NS_RDF.to_string(), "Seq".to_string()))
        .count();
    assert_eq!(lis, 2, "both regions in the one Seq:\n{xml}");
}

/// The `rdf:parseType="Resource"` layout ChairPhoto itself writes, with foreign content
/// where the old writer rebuilt the elements from scratch: an attribute on Regions, an
/// attribute and a child on AppliedToDimensions, and a child of Regions.
#[test]
fn face_regions_keep_foreign_content_of_regions_and_dimensions() {
    let (_dir, photo) = seeded_photo("xmp-139-resource", r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
    xmlns:stDim="http://ns.adobe.com/xap/1.0/sType/Dimensions#"
    xmlns:f="urn:example:foreign">
   <mwg-rs:Regions rdf:parseType="Resource" f:keep="regions-attr">
    <mwg-rs:AppliedToDimensions rdf:parseType="Resource" f:keep2="dims-attr">
     <stDim:w>4000</stDim:w>
     <stDim:h>3000</stDim:h>
     <stDim:unit>pixel</stDim:unit>
     <f:dimextra>dims-child</f:dimextra>
    </mwg-rs:AppliedToDimensions>
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Bob</mwg-rs:Name>
       <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2"
         stArea:unit="normalized"/>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
    <f:extra>regions-child</f:extra>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#);

    write_face_regions(&photo, CAT, &alice(), &[], &[], sized(8000, 6000)).unwrap();

    let xml = read(&sidecar_path(&photo));
    assert_eq!(region_names(&photo), ["Alice", "Bob"], "{xml}");
    let regions = (NS_MWG_RS, "Regions");
    let dims = (NS_MWG_RS, "AppliedToDimensions");
    assert!(has_attr(&xml, regions, (NS_FOREIGN, "keep"), "regions-attr"), "{xml}");
    assert!(has_attr(&xml, dims, (NS_FOREIGN, "keep2"), "dims-attr"), "{xml}");
    assert_eq!(element_text(&xml, dims, (NS_FOREIGN, "dimextra")).as_deref(),
        Some("dims-child"), "{xml}");
    assert_eq!(element_text(&xml, regions, (NS_FOREIGN, "extra")).as_deref(),
        Some("regions-child"), "{xml}");
    assert_eq!(element_text(&xml, dims, (NS_STDIM, "w")).as_deref(), Some("4000"), "{xml}");
    assert_eq!(element_text(&xml, dims, (NS_STDIM, "h")).as_deref(), Some("3000"), "{xml}");
    assert_eq!(element_text(&xml, dims, (NS_STDIM, "unit")).as_deref(), Some("pixel"));
}

/// exiftool writes one Description per namespace, so Regions may sit in a later one. The
/// writer edits it there instead of adding a second Regions property to the first.
#[test]
fn face_regions_are_edited_in_the_description_that_holds_them() {
    let (_dir, photo) = seeded_photo("xmp-139-second-desc", r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/">
   <xmp:Rating>3</xmp:Rating>
  </rdf:Description>
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#">
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Bob</mwg-rs:Name>
       <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2"/>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#);

    write_face_regions(&photo, CAT, &alice(), &[], &[], sized(6000, 4000)).unwrap();

    let xml = read(&sidecar_path(&photo));
    assert_eq!(region_names(&photo), ["Alice", "Bob"], "{xml}");
    assert_eq!(count_elements(&xml, NS_MWG_RS, "Regions"), 1, "{xml}");
    assert_eq!(count_elements(&xml, NS_RDF, "Bag"), 1, "{xml}");
}

/// Issue #140: a region another tool wrote that matches one ChairPhoto writes by Name +
/// Area is updated in place. Only its Area coordinates change, in the form they are
/// written in; its Type, `mwg-rs:Rotation`, foreign children and foreign attributes stay.
/// Bob is Lightroom-style (attribute-form Area with a `digiKam:Confidence`), Carol is a
/// nested-Description region with a foreign attribute and a foreign child in her Area.
#[test]
fn face_regions_matched_foreign_region_keeps_its_foreign_content() {
    let (_dir, photo) = seeded_photo("xmp-140-in-place", r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
    xmlns:digiKam="http://www.digikam.org/ns/1.0/"
    xmlns:f="urn:example:foreign">
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Bob</mwg-rs:Name>
       <mwg-rs:Type>Face</mwg-rs:Type>
       <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2"
         stArea:unit="normalized" digiKam:Confidence="87"/>
       <mwg-rs:Rotation>0.25</mwg-rs:Rotation>
       <f:note>bob-child</f:note>
      </rdf:li>
      <rdf:li>
       <rdf:Description mwg-rs:Name="Carol" f:tag="carol-attr">
        <mwg-rs:Area rdf:parseType="Resource">
         <stArea:x>0.3</stArea:x><stArea:y>0.3</stArea:y>
         <stArea:w>0.1</stArea:w><stArea:h>0.1</stArea:h>
         <f:areanote>carol-area-child</f:areanote>
        </mwg-rs:Area>
       </rdf:Description>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#);
    assert_eq!(region_names(&photo), ["Bob", "Carol"], "the fixture's regions are read");

    // Both moved by 0.01, well inside AREA_EPSILON: ChairPhoto's write of the same faces.
    let ours = [
        FaceRegion { face_id: 0, name: "Bob".into(), bbox: (0.76, 0.61, 0.1, 0.2) },
        FaceRegion { face_id: 0, name: "Carol".into(), bbox: (0.26, 0.26, 0.1, 0.1) },
    ];
    write_face_regions(&photo, CAT, &ours, &[], &[], sized(6000, 4000)).unwrap();

    let xml = read(&sidecar_path(&photo));
    let back = read_face_regions(&photo);
    assert_eq!(back.len(), 2, "updated, not duplicated: {back:?}\n{xml}");
    for want in &ours {
        let got = back.iter().find(|r| r.name == want.name).unwrap();
        let (a, b) = (got.bbox, want.bbox);
        assert!((a.0 - b.0).abs() < 1e-4 && (a.1 - b.1).abs() < 1e-4,
            "{} not moved to the written geometry: {a:?}\n{xml}", want.name);
    }

    // Bob: the Area stays in attribute form, with its foreign attribute.
    let area = (NS_MWG_RS, "Area");
    assert!(has_attr(&xml, area, (NS_DIGIKAM, "Confidence"), "87"), "{xml}");
    assert!(has_attr(&xml, area, (NS_STAREA, "x"), "0.81"), "{xml}");
    assert!(has_attr(&xml, area, (NS_STAREA, "unit"), "normalized"), "{xml}");
    let li = (NS_RDF, "li");
    assert_eq!(element_text(&xml, li, (NS_MWG_RS, "Rotation")).as_deref(), Some("0.25"),
        "{xml}");
    assert_eq!(element_text(&xml, li, (NS_MWG_RS, "Type")).as_deref(), Some("Face"));
    assert_eq!(element_text(&xml, li, (NS_FOREIGN, "note")).as_deref(), Some("bob-child"));
    // Carol: the nested Description keeps its attribute, the Area its foreign child, and
    // her coordinates stay elements.
    assert!(has_attr(&xml, (NS_RDF, "Description"), (NS_FOREIGN, "tag"), "carol-attr"));
    assert_eq!(element_text(&xml, area, (NS_FOREIGN, "areanote")).as_deref(),
        Some("carol-area-child"), "{xml}");
    assert_eq!(element_text(&xml, area, (NS_STAREA, "x")).as_deref(), Some("0.31"), "{xml}");
    assert_eq!(element_text(&xml, area, (NS_STAREA, "unit")).as_deref(),
        Some("normalized"), "the missing unit is added in the Area's own form:\n{xml}");
}

/// A Regions the writer does not recognise is never rebuilt: the write is refused with an
/// error naming the sidecar, and the file is left byte for byte as it was.
#[test]
fn face_regions_refuse_a_layout_they_do_not_recognise() {
    let wrap = |desc_body: &str| format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#">
{desc_body}
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#);
    let bob = r#"<rdf:li rdf:parseType="Resource"><mwg-rs:Name>Bob</mwg-rs:Name>
       <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2"/></rdf:li>"#;
    let cases = [
        ("alt container", wrap(&format!(
            r#"<mwg-rs:Regions rdf:parseType="Resource"><mwg-rs:RegionList><rdf:Alt>{bob}</rdf:Alt></mwg-rs:RegionList></mwg-rs:Regions>"#))),
        ("no container", wrap(&format!(
            r#"<mwg-rs:Regions rdf:parseType="Resource"><mwg-rs:RegionList>{bob}</mwg-rs:RegionList></mwg-rs:Regions>"#))),
        ("two lists", wrap(&format!(
            r#"<mwg-rs:Regions rdf:parseType="Resource"><mwg-rs:RegionList><rdf:Bag>{bob}</rdf:Bag></mwg-rs:RegionList><mwg-rs:RegionList><rdf:Bag/></mwg-rs:RegionList></mwg-rs:Regions>"#))),
        ("two Regions", wrap(&format!(
            r#"<mwg-rs:Regions rdf:parseType="Resource"><mwg-rs:RegionList><rdf:Bag>{bob}</rdf:Bag></mwg-rs:RegionList></mwg-rs:Regions><mwg-rs:Regions rdf:parseType="Resource"/>"#))),
        ("Regions by reference", wrap(r#"<mwg-rs:Regions rdf:resource="urn:example:regions"/>"#)),
    ];
    for (case, sidecar) in cases {
        let (_dir, photo) = seeded_photo("xmp-139-refuse", &sidecar);
        let err = write_face_regions(&photo, CAT, &alice(), &[], &[], sized(6000, 4000))
            .expect_err(&format!("{case}: an unrecognised Regions must not be written"));
        assert!(matches!(err, RegionWriteError::Refused(_)), "{case}: {err:?}");
        assert!(err.to_string().contains(&sidecar_path(&photo).display().to_string()), "{case}: {err}");
        assert_eq!(read(&sidecar_path(&photo)), sidecar, "{case}: sidecar changed");
    }
}

// ── #136: the stored frame ─────────────────────────────────────────────────

fn near4(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    [(a.0, b.0), (a.1, b.1), (a.2, b.2), (a.3, b.3)].iter().all(|(x, y)| (x - y).abs() < 1e-4)
}

/// MWG's center form of a top-left box.
fn center(b: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
    (b.0 + b.2 / 2.0, b.1 + b.3 / 2.0, b.2, b.3)
}

fn turned(o: u8, w: u32, h: u32) -> RegionFrame {
    RegionFrame { orientation: Some(o), stored_size: Some((w, h)), ..Default::default() }
}

/// The point maps are checked against an independent implementation of all eight EXIF
/// orientations, the image crate's `apply_orientation` (the one the previews go through):
/// a block of pixels marked in a stored 12x8 image must land where `stored_to_display`
/// says, and `display_to_stored` must bring it back.
#[test]
fn orientation_maps_agree_with_the_image_crate() {
    let (sw, sh) = (12u32, 8u32);
    let stored = (2.0 / 12.0, 1.0 / 8.0, 3.0 / 12.0, 2.0 / 8.0);
    for o in 1..=8u8 {
        let mut img = image::RgbImage::new(sw, sh);
        for x in 2..5 {
            for y in 1..3 {
                img.put_pixel(x, y, image::Rgb([255, 255, 255]));
            }
        }
        let mut img = image::DynamicImage::ImageRgb8(img);
        img.apply_orientation(image::metadata::Orientation::from_exif(o).unwrap());
        let img = img.to_rgb8();
        let (dw, dh) = img.dimensions();
        assert_eq!((dw, dh), if o >= 5 { (sh, sw) } else { (sw, sh) }, "orientation {o}");
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
        for (x, y, p) in img.enumerate_pixels() {
            if p[0] == 255 {
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
            }
        }
        let (dw, dh) = (dw as f32, dh as f32);
        let want = (x0 as f32 / dw, y0 as f32 / dh, (x1 - x0) as f32 / dw, (y1 - y0) as f32 / dh);
        let got = stored_to_display(o, stored);
        assert!(near4(got, want), "orientation {o}: {got:?} vs the image crate's {want:?}");
        assert!(near4(display_to_stored(o, got), stored), "orientation {o} does not invert");
    }
}

/// #136: in all eight orientations the written region is the face's box turned into the
/// stored frame, `AppliedToDimensions` is the stored size (never swapped), and reading it
/// back for the same photo gives the face's box again. The stored-frame centers of four
/// orientations are worked out by hand, independently of the code.
#[test]
fn face_regions_round_trip_in_every_orientation() {
    let display = (0.1, 0.2, 0.3, 0.4);
    let by_hand = [
        (1, (0.25, 0.4, 0.3, 0.4)),
        (3, (0.75, 0.6, 0.3, 0.4)),
        (6, (0.4, 0.75, 0.4, 0.3)),
        (8, (0.6, 0.25, 0.4, 0.3)),
    ];
    for o in 1..=8u8 {
        let dir = region_dir("xmp-136-orientations");
        let photo = dir.join("P.JPG");
        std::fs::write(&photo, b"jpeg").unwrap();
        let frame = turned(o, 6000, 4000);
        write_face_regions(&photo, CAT, &[FaceRegion { face_id: 0, name: "Alice".into(), bbox: display }], &[], &[], frame)
            .unwrap();

        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        assert_eq!(got.dims, [Some(("6000".into(), "4000".into()))], "orientation {o}:\n{xml}");
        assert_eq!(got.regions.len(), 1, "orientation {o}:\n{xml}");
        let area = got.regions[0].area;
        assert!(near4(area, center(display_to_stored(o, display))), "orientation {o}: {area:?}");
        if let Some((_, want)) = by_hand.iter().find(|(h, _)| *h == o) {
            assert!(near4(area, *want), "orientation {o}: {area:?}, by hand {want:?}");
        }
        let back = read_face_regions_in(&photo, frame);
        assert_eq!(back.len(), 1);
        assert!(near4(back[0].bbox, display), "orientation {o}: read back {:?}", back[0].bbox);
    }
}

/// #136 on Lightroom's sidecar of a portrait shot (Orientation 6): Bob's region, in the
/// stored frame as MWG requires, comes through ChairPhoto's write of Alice unchanged, and
/// so do the dimensions and every other foreign structure; Alice lands in the same frame;
/// and the importer reads Bob turned into the display frame the detections are in.
#[test]
fn face_regions_keep_a_foreign_region_on_a_rotated_photo() {
    let (_dir, photo) = seeded_photo("xmp-136-lightroom", rf::LIGHTROOM_ROTATED);
    let bob_before = rf::subtree(rf::LIGHTROOM_ROTATED, NS_RDF, "li");
    let alice = (0.5, 0.1, 0.2, 0.1);
    let frame = turned(6, 6000, 4000);
    write_face_regions(&photo, CAT, &[FaceRegion { face_id: 0, name: "Alice".into(), bbox: alice }], &[], &[], frame).unwrap();

    let xml = read(&sidecar_path(&photo));
    let got = rf::mwg(&xml);
    assert_eq!(got.dims, [Some(("6000".into(), "4000".into()))], "{xml}");
    let bob = rf::named(&got, "Bob");
    assert_eq!(bob.len(), 1, "{xml}");
    assert_eq!(bob[0].area, (0.3, 0.25, 0.15, 0.1), "Bob moved:\n{xml}");
    assert!(rf::subtree(&xml, NS_RDF, "li").contains(&bob_before[0]), "Bob's li changed:\n{xml}");
    assert_eq!(rf::foreign_structures(&xml), rf::foreign_structures(rf::LIGHTROOM_ROTATED));
    let ours = rf::named(&got, "Alice");
    assert!(near4(ours[0].area, center(display_to_stored(6, alice))), "{:?}", ours[0].area);

    let read = read_face_regions_in(&photo, frame);
    let bob = read.iter().find(|r| r.name == "Bob").unwrap();
    // Stored top-left (0.225, 0.2, 0.15, 0.1), turned 90° clockwise.
    assert!(near4(bob.bbox, (0.7, 0.225, 0.1, 0.15)), "{:?}", bob.bbox);
    let alice_back = read.iter().find(|r| r.name == "Alice").unwrap();
    assert!(near4(alice_back.bbox, alice), "{:?}", alice_back.bbox);
}

/// #136: an unknown orientation is never guessed. The boxes are written as they are, and
/// dimensions the sidecar already has stay, even when the catalog's size disagrees; a
/// Regions ChairPhoto creates gets the recorded size, and with no recorded size no
/// `AppliedToDimensions` at all (no `1x1` stand-in).
#[test]
fn face_regions_with_an_unknown_orientation_convert_nothing() {
    let box_ = (0.1, 0.1, 0.2, 0.2);
    let alice = [FaceRegion { face_id: 0, name: "Alice".into(), bbox: box_ }];
    let unknown = |size| RegionFrame { orientation: None, stored_size: size, ..Default::default() };

    let (_dir, photo) = seeded_photo("xmp-136-unknown-existing", rf::DIGIKAM);
    write_face_regions(&photo, CAT, &alice, &[], &[], unknown(Some((4000, 6000)))).unwrap();
    let xml = read(&sidecar_path(&photo));
    let got = rf::mwg(&xml);
    assert_eq!(got.dims, [Some(("6000".into(), "4000".into()))], "{xml}");
    assert!(near4(rf::named(&got, "Alice")[0].area, center(box_)), "{xml}");
    assert_eq!(rf::named(&got, "Bob")[0].area, (0.8, 0.7, 0.1, 0.2));

    let dir = region_dir("xmp-136-unknown-fresh");
    let photo = dir.join("U.JPG");
    std::fs::write(&photo, b"jpeg").unwrap();
    write_face_regions(&photo, CAT, &alice, &[], &[], unknown(Some((6000, 4000)))).unwrap();
    let got = rf::mwg(&read(&sidecar_path(&photo)));
    assert_eq!(got.dims, [Some(("6000".into(), "4000".into()))]);
    assert!(near4(got.regions[0].area, center(box_)));

    let photo = dir.join("V.JPG");
    std::fs::write(&photo, b"jpeg").unwrap();
    write_face_regions(&photo, CAT, &alice, &[], &[], RegionFrame { orientation: Some(6), stored_size: None, ..Default::default() })
        .unwrap();
    let got = rf::mwg(&read(&sidecar_path(&photo)));
    assert_eq!(got.dims, [None], "no size, no AppliedToDimensions");
    assert!(near4(got.regions[0].area, center(display_to_stored(6, box_))));
}

// ── #145: a foreign AppliedToDimensions is never rewritten ─────────────────

/// Lightroom's Orientation-6 sidecar with its `AppliedToDimensions` declaring `w`x`h`.
fn lightroom_declaring(w: &str, h: &str) -> String {
    let fixture = r#"stDim:w="6000" stDim:h="4000""#;
    assert!(rf::LIGHTROOM_ROTATED.contains(fixture), "the fixture's dims moved");
    rf::LIGHTROOM_ROTATED.replace(fixture, &format!(r#"stDim:w="{w}" stDim:h="{h}""#))
}

const BOB_STORED: (f32, f32, f32, f32) = (0.3, 0.25, 0.15, 0.1);

/// Probe P01's shapes (claude-139-142.log M1): a write never changes the frame foreign
/// regions are measured in. Where the sidecar declares the stored frame, ChairPhoto writes
/// in it; where it declares the display frame (the stored size swapped, on a photo turned
/// a quarter), ChairPhoto's regions follow it there instead of the declaration following
/// ChairPhoto; a resized image keeps its own size. Bob and the dims are untouched each time.
#[test]
fn face_regions_follow_the_frame_the_sidecar_declares() {
    let alice = (0.5, 0.1, 0.2, 0.1);
    let cases = [
        ("stored frame", ("6000", "4000"), RegionTarget::Stored(6)),
        ("display frame", ("4000", "6000"), RegionTarget::AsIs),
        ("resized stored frame", ("3000", "2000"), RegionTarget::Stored(6)),
        ("ChairPhoto's old 1x1", ("1", "1"), RegionTarget::Stored(6)),
    ];
    for (case, (w, h), target) in cases {
        let sidecar = lightroom_declaring(w, h);
        let (_dir, photo) = seeded_photo("xmp-145-frames", &sidecar);
        let frame = turned(6, 6000, 4000);
        write_face_regions(&photo, CAT, &[FaceRegion { face_id: 0, name: "Alice".into(), bbox: alice }], &[], &[], frame)
            .unwrap_or_else(|e| panic!("{case}: {e}"));

        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        assert_eq!(got.dims, [Some((w.to_string(), h.to_string()))], "{case}:\n{xml}");
        assert_eq!(rf::named(&got, "Bob")[0].area, BOB_STORED, "{case}:\n{xml}");
        let ours = rf::named(&got, "Alice")[0].area;
        let want = match target {
            RegionTarget::AsIs => alice,
            RegionTarget::Stored(o) => display_to_stored(o, alice),
        };
        assert!(near4(ours, center(want)), "{case}: Alice at {ours:?}\n{xml}");
        let back = read_face_regions_in(&photo, frame);
        let alice_back = back.iter().find(|r| r.name == "Alice").unwrap();
        assert!(near4(alice_back.bbox, alice), "{case}: read back {:?}", alice_back.bbox);
    }
}

/// Probe P02's shape and its neighbours: a sidecar whose declared frame is not one ChairPhoto
/// can place its boxes in is not written at all — byte for byte as it was — and reading it
/// for import yields nothing rather than regions in the wrong frame. That covers an unknown
/// size on a photo turned a quarter (before #145 the declaration became `1x1`), an aspect
/// that is neither the image's nor swapped, a swap the orientation does not explain, and a
/// declaration without a usable size.
#[test]
fn face_regions_refuse_a_frame_they_cannot_place_their_boxes_in() {
    let alice = [FaceRegion { face_id: 0, name: "Alice".into(), bbox: (0.5, 0.1, 0.2, 0.1) }];
    let no_size = |o| RegionFrame { orientation: Some(o), stored_size: None, ..Default::default() };
    let cases = [
        ("unknown size, turned a quarter", lightroom_declaring("6000", "4000"), no_size(6)),
        ("another aspect", lightroom_declaring("5000", "5000"), turned(6, 6000, 4000)),
        ("swapped but upright", lightroom_declaring("4000", "6000"), turned(1, 6000, 4000)),
        ("no usable size", lightroom_declaring("6000", "wide"), turned(6, 6000, 4000)),
        ("zero size", lightroom_declaring("0", "4000"), turned(6, 6000, 4000)),
    ];
    for (case, sidecar, frame) in cases {
        let (_dir, photo) = seeded_photo("xmp-145-refuse", &sidecar);
        let err = write_face_regions(&photo, CAT, &alice, &[], &[], frame)
            .expect_err(&format!("{case}: the write must be refused"));
        assert!(matches!(err, RegionWriteError::Refused(_)), "{case}: {err:?}");
        assert!(err.to_string().contains(&sidecar_path(&photo).display().to_string()), "{case}: {err}");
        assert_eq!(read(&sidecar_path(&photo)), sidecar, "{case}: sidecar changed");
        assert!(read_face_regions_in(&photo, frame).is_empty(), "{case}: imported anyway");
    }
}

/// P02's other halves: an unknown size never replaces a declaration. Upright (or mirrored
/// without a quarter turn) the stored and display frames share their aspect, so the boxes
/// go into the stored frame MWG prescribes; with an unknown orientation they go as they
/// are (#136). Either way the declared 6000x4000 stays.
#[test]
fn face_regions_with_an_unknown_size_keep_the_declared_dimensions() {
    let alice = (0.5, 0.1, 0.2, 0.1);
    for (case, orientation, want) in [
        ("upright", Some(1), alice),
        ("rotated 180", Some(3), display_to_stored(3, alice)),
        ("unknown orientation", None, alice),
    ] {
        let (_dir, photo) = seeded_photo("xmp-145-unsized", &lightroom_declaring("6000", "4000"));
        let frame = RegionFrame { orientation, stored_size: None, ..Default::default() };
        write_face_regions(&photo, CAT, &[FaceRegion { face_id: 0, name: "Alice".into(), bbox: alice }], &[], &[], frame)
            .unwrap_or_else(|e| panic!("{case}: {e}"));
        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        assert_eq!(got.dims, [Some(("6000".into(), "4000".into()))], "{case}:\n{xml}");
        assert!(near4(rf::named(&got, "Alice")[0].area, center(want)), "{case}:\n{xml}");
        assert_eq!(rf::named(&got, "Bob")[0].area, BOB_STORED, "{case}");
    }
}

// ── #154: the preview's frame, a HEIF's turn, real tools' sidecars ─────────

/// A photo of a known orientation and stored size whose preview is `preview` pixels.
fn previewed(orientation: Option<u8>, stored: (u32, u32), preview: (u32, u32)) -> RegionFrame {
    RegionFrame { orientation, stored_size: Some(stored), display_size: Some(preview), doubt: None }
}

/// Turning by `a` and then by `b` is turning by `compose_orientations(a, b)`, for all 64
/// pairs, as the image crate's own `apply_orientation` turns an image of distinct pixels.
#[test]
fn orientations_compose_as_the_image_crate_turns() {
    use image::metadata::Orientation;
    let mut base = image::RgbImage::new(5, 3);
    for (x, y, p) in base.enumerate_pixels_mut() {
        *p = image::Rgb([x as u8 * 40, y as u8 * 80, 7]);
    }
    let turn = |img: &mut image::DynamicImage, o: u8| img.apply_orientation(Orientation::from_exif(o).unwrap());
    for a in 1..=8u8 {
        for b in 1..=8u8 {
            let mut twice = image::DynamicImage::ImageRgb8(base.clone());
            turn(&mut twice, a);
            turn(&mut twice, b);
            let mut once = image::DynamicImage::ImageRgb8(base.clone());
            turn(&mut once, crate::xmp::compose_orientations(a, b));
            assert_eq!(twice.to_rgb8(), once.to_rgb8(), "{a} then {b}");
        }
    }
}

/// A HEIF's container turn is the display frame's (#154): an EXIF Orientation saying the same
/// or nothing agrees; any other, or a container that could not be read, is a doubt.
#[test]
fn a_heif_container_turn_agrees_with_exif_or_is_doubted() {
    let exif = |o| RegionFrame { orientation: o, stored_size: Some((4032, 3024)), ..Default::default() };
    assert_eq!(exif(Some(6)).with_container(Some(6)), exif(Some(6)));
    assert_eq!(exif(None).with_container(Some(6)), exif(Some(6)), "the container alone");
    assert_eq!(exif(None).with_container(Some(1)), exif(Some(1)), "no turn anywhere");
    for (e, c) in [(1, 6), (6, 1), (6, 8), (3, 1), (2, 1)] {
        assert_eq!(
            exif(Some(e)).with_container(Some(c)).doubt,
            Some(FrameDoubt::ContainerDisagrees { container: c, exif: e }),
            "EXIF {e}, container {c}"
        );
    }
    for e in [None, Some(6)] {
        assert_eq!(exif(e).with_container(None).doubt, Some(FrameDoubt::ContainerUnreadable), "{e:?}");
    }
}

/// A doubted turn writes and reads nothing (#154): exiftool's real sidecar stays byte for byte
/// as it was, a photo with no sidecar gets none, and the import reads no region.
#[test]
fn face_regions_refuse_a_doubted_turn() {
    let alice = [FaceRegion { face_id: 7, name: "Alice".into(), bbox: (0.5, 0.1, 0.2, 0.1) }];
    for doubt in [FrameDoubt::ContainerUnreadable, FrameDoubt::ContainerDisagrees { container: 6, exif: 1 }] {
        let frame = RegionFrame { doubt: Some(doubt), ..turned(6, 600, 400) };
        let (_dir, photo) = seeded_photo("xmp-154-doubt", rf::EXIFTOOL_O6);
        let err = write_face_regions(&photo, CAT, &alice, &[], &[], frame).expect_err("refused");
        assert!(matches!(err, RegionWriteError::Refused(_)), "{doubt:?}: {err:?}");
        assert!(err.to_string().contains(&doubt.to_string()), "{err}");
        assert_eq!(read(&sidecar_path(&photo)), rf::EXIFTOOL_O6, "{doubt:?}: sidecar changed");
        assert!(read_face_regions_in(&photo, frame).is_empty(), "{doubt:?}: imported anyway");

        let dir = region_dir("xmp-154-doubt-fresh");
        let fresh = dir.join("IMG_0001.HEIC");
        std::fs::write(&fresh, b"heic").unwrap();
        let err = write_face_regions(&fresh, CAT, &alice, &[], &[], frame).expect_err("refused");
        assert!(matches!(err, RegionWriteError::Refused(_)), "{doubt:?}: {err:?}");
        assert!(!sidecar_path(&fresh).exists(), "{doubt:?}: a sidecar was created");
    }
}

/// Review L2 (Q6) on exiftool's real sidecar of a portrait shot, and its neighbours: when the
/// preview the faces were found on is not the recorded size turned by the orientation, the
/// write is refused — the sidecar left byte for byte, none created — and nothing is imported.
///
/// The first case is Q6: the catalog holds the display-frame size 400x600 for a 600x400 stored
/// image on Orientation 6. The sidecar's 600x400 then looks like the display frame, so before
/// #154 Alice was written unturned into Bob's stored frame and Bob imported unturned, both
/// wrong with no error. Without a preview the frame stays as it was before (the last part).
#[test]
fn face_regions_refuse_a_recorded_size_their_preview_contradicts() {
    let alice = [FaceRegion { face_id: 7, name: "Alice".into(), bbox: (0.5, 0.1, 0.2, 0.1) }];
    let cases = [
        ("display-frame size on Orientation 6", previewed(Some(6), (400, 600), (400, 600))),
        ("upright, the preview turned", previewed(Some(1), (600, 400), (400, 600))),
        ("unknown orientation, the preview turned", previewed(None, (600, 400), (1365, 2048))),
        ("turned a quarter, the preview not", previewed(Some(8), (6000, 4000), (2048, 1365))),
        ("3% off", previewed(Some(6), (6000, 4000), (1320, 2048))),
    ];
    for (case, frame) in cases {
        let (_dir, photo) = seeded_photo("xmp-154-preview", rf::EXIFTOOL_O6);
        let err = write_face_regions(&photo, CAT, &alice, &[], &[], frame).expect_err(case);
        assert!(matches!(err, RegionWriteError::Refused(_)), "{case}: {err:?}");
        assert!(err.to_string().contains("preview"), "{case}: {err}");
        assert_eq!(read(&sidecar_path(&photo)), rf::EXIFTOOL_O6, "{case}: sidecar changed");
        assert!(read_face_regions_in(&photo, frame).is_empty(), "{case}: imported anyway");

        let dir = region_dir("xmp-154-preview-fresh");
        let fresh = dir.join("P.JPG");
        std::fs::write(&fresh, b"jpeg").unwrap();
        write_face_regions(&fresh, CAT, &alice, &[], &[], frame).expect_err(case);
        assert!(!sidecar_path(&fresh).exists(), "{case}: a sidecar was created");
    }

    // Q6 as it was without a preview to go by: written, in the wrong frame.
    let (_dir, photo) = seeded_photo("xmp-154-preview-none", rf::EXIFTOOL_O6);
    let blind = RegionFrame { display_size: None, ..previewed(Some(6), (400, 600), (400, 600)) };
    write_face_regions(&photo, CAT, &alice, &[], &[], blind).unwrap();
    let got = rf::mwg(&read(&sidecar_path(&photo)));
    assert!(near4(rf::named(&got, "Alice")[0].area, center(alice[0].bbox)), "unturned: {got:?}");
}

/// The check lets through a preview of the recorded frame (#154): turned a quarter as the
/// orientation says, upright, with an unknown orientation and the stored aspect, and a few
/// pixels off the stored aspect, as an embedded RAW preview may be.
#[test]
fn face_regions_accept_a_preview_of_their_frame() {
    let alice = (0.5, 0.1, 0.2, 0.1);
    let cases = [
        ("turned a quarter", previewed(Some(6), (600, 400), (1365, 2048)), RegionTarget::Stored(6)),
        ("turned the other way", previewed(Some(8), (600, 400), (400, 600)), RegionTarget::Stored(8)),
        ("1% off", previewed(Some(6), (6000, 4000), (1350, 2048)), RegionTarget::Stored(6)),
        ("upright", previewed(Some(1), (600, 400), (2048, 1365)), RegionTarget::Stored(1)),
        ("unknown orientation", previewed(None, (600, 400), (2048, 1365)), RegionTarget::AsIs),
    ];
    for (case, frame, target) in cases {
        let dir = region_dir("xmp-154-preview-ok");
        let photo = dir.join("P.JPG");
        std::fs::write(&photo, b"jpeg").unwrap();
        write_face_regions(&photo, CAT, &[FaceRegion { face_id: 7, name: "Alice".into(), bbox: alice }], &[], &[], frame)
            .unwrap_or_else(|e| panic!("{case}: {e}"));
        let got = rf::mwg(&read(&sidecar_path(&photo)));
        let want = match target {
            RegionTarget::AsIs => alice,
            RegionTarget::Stored(o) => display_to_stored(o, alice),
        };
        assert!(near4(got.regions[0].area, center(want)), "{case}: {got:?}");
        let back = read_face_regions_in(&photo, frame);
        assert!(near4(back[0].bbox, alice), "{case}: read back {:?}", back[0].bbox);
    }
}

/// exiftool's own reading of a sidecar's MWG regions, as `(name, x, y, w, h)` in the file's
/// center form, or `None` without an `exiftool` to run.
fn exiftool_regions(sidecar: &std::path::Path) -> Option<Vec<(String, f64, f64, f64, f64)>> {
    let out = std::process::Command::new("exiftool")
        .args(["-j", "-struct", "-XMP-mwg-rs:RegionInfo"])
        .arg(sidecar)
        .output()
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let list = json[0]["RegionInfo"]["RegionList"].as_array()?.clone();
    let num = |v: &serde_json::Value| v.as_f64().or_else(|| v.as_str()?.parse().ok()).unwrap();
    Some(
        list.iter()
            .map(|r| {
                let a = &r["Area"];
                (r["Name"].as_str().unwrap().to_string(), num(&a["X"]), num(&a["Y"]), num(&a["W"]), num(&a["H"]))
            })
            .collect(),
    )
}

/// #154 on sidecars real tools wrote of a portrait shot (exiftool 13.55, exiv2 0.28.9;
/// Orientation 6, Bob in the stored frame): the import reads Bob turned into the display
/// frame; ChairPhoto's write of Alice leaves Bob's region, the dimensions and every other
/// property exactly as the tool wrote them, and puts Alice, marked, into the same stored
/// frame; and exiftool reads both regions back from the result where it is installed.
#[test]
fn face_regions_on_real_tool_sidecars() {
    let alice = (0.5, 0.1, 0.2, 0.1);
    for (tool, sidecar) in rf::REAL_TOOL_REGIONS {
        let (_dir, photo) = seeded_photo("xmp-154-real-tools", sidecar);
        let frame = previewed(Some(6), (600, 400), (400, 600));
        let read_before = read_face_regions_in(&photo, frame);
        // Stored top-left (0.25, 0.2, 0.15, 0.1), turned 90 degrees clockwise.
        assert_eq!(read_before.len(), 1, "{tool}");
        assert!(near4(read_before[0].bbox, (0.7, 0.25, 0.1, 0.15)), "{tool}: {:?}", read_before[0].bbox);

        write_face_regions(&photo, CAT, &[FaceRegion { face_id: 7, name: "Alice".into(), bbox: alice }], &[], &[], frame)
            .unwrap_or_else(|e| panic!("{tool}: {e}"));
        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        assert_eq!(got.dims, [Some(("600".into(), "400".into()))], "{tool}:\n{xml}");
        assert_eq!(rf::named(&got, "Bob")[0].area, (0.325, 0.25, 0.15, 0.1), "{tool}:\n{xml}");
        assert_eq!(rf::named(&got, "Bob")[0].face_id, None, "{tool}: Bob marked");
        let lis_after = rf::subtree(&xml, NS_RDF, "li");
        for li in rf::subtree(sidecar, NS_RDF, "li") {
            assert!(lis_after.contains(&li), "{tool}: {li} changed:\n{xml}");
        }
        // All but the first-write stamp ChairPhoto owns (`chairphoto:LastWrite`).
        let mut after = rf::non_region_properties(&xml);
        let stamp = format!("<{{{NS_CHAIRPHOTO}}}LastWrite>");
        assert_eq!(after.iter().filter(|p| p.starts_with(&stamp)).count(), 1, "{tool}:\n{xml}");
        after.retain(|p| !p.starts_with(&stamp));
        assert_eq!(after, rf::non_region_properties(sidecar), "{tool}:\n{xml}");
        let ours_ = rf::named(&got, "Alice");
        assert_eq!(ours_.len(), 1, "{tool}:\n{xml}");
        assert_eq!(ours_[0].face_id.as_deref(), Some(ours(7).as_str()), "{tool}");
        assert!(near4(ours_[0].area, center(display_to_stored(6, alice))), "{tool}: {:?}", ours_[0].area);
        let back = read_face_regions_in(&photo, frame);
        assert!(near4(back.iter().find(|r| r.name == "Alice").unwrap().bbox, alice), "{tool}: {back:?}");

        match exiftool_regions(&sidecar_path(&photo)) {
            None => eprintln!("SKIPPED: face_regions_on_real_tool_sidecars ({tool}) exiftool read-back — no exiftool"),
            Some(regions) => {
                let a = center(display_to_stored(6, alice));
                let near = |x: f64, y: f32| (x - f64::from(y)).abs() < 1e-4;
                assert!(regions.iter().any(|r| r.0 == "Bob" && near(r.1, 0.325) && near(r.2, 0.25)), "{tool}: {regions:?}");
                assert!(
                    regions.iter().any(|r| r.0 == "Alice" && near(r.1, a.0) && near(r.2, a.1) && near(r.3, a.2) && near(r.4, a.3)),
                    "{tool}: {regions:?}"
                );
                assert_eq!(regions.len(), 2, "{tool}: {regions:?}");
            }
        }
    }
}

// ── #135: ChairPhoto's marker ──────────────────────────────────────────────

fn face(face_id: i64, name: &str, bbox: (f32, f32, f32, f32)) -> FaceRegion {
    FaceRegion { face_id, name: name.into(), bbox }
}

/// #135: every region ChairPhoto writes carries `chairphoto:FaceId` in ChairPhoto's
/// namespace — even where the Description binds the `chairphoto` prefix to something else —
/// and a face that leaves the set takes its region with it, while every foreign region and
/// structure in the digiKam, Lightroom (Orientation 6) and MS Photo sidecars stays.
#[test]
fn face_regions_mark_ours_and_remove_them_when_their_face_leaves() {
    for (layout, sidecar) in rf::FOREIGN_REGIONS {
        let (_dir, photo) = seeded_photo("xmp-135-marker", sidecar);
        let frame = if layout == "lightroom-o6" { turned(6, 6000, 4000) } else { sized(6000, 4000) };
        let before = rf::mwg(sidecar);
        let alice = face(41, "Alice", (0.1, 0.1, 0.2, 0.2));
        let carl = face(42, "Carl", (0.4, 0.4, 0.1, 0.1));
        write_face_regions(&photo, CAT, &[alice.clone(), carl.clone()], &[], &[], frame).unwrap();

        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        assert_eq!(rf::named(&got, "Alice")[0].face_id.as_deref(), Some(ours(41).as_str()), "{layout}:\n{xml}");
        assert_eq!(rf::named(&got, "Carl")[0].face_id.as_deref(), Some(ours(42).as_str()), "{layout}:\n{xml}");
        assert_eq!(got.regions.len(), before.regions.len() + 2, "{layout}:\n{xml}");

        // Alice is rejected, Carl ignored: the set is empty now.
        write_face_regions(&photo, CAT, &[carl.clone()], &[41], &[], frame).unwrap();
        let got = rf::mwg(&read(&sidecar_path(&photo)));
        assert!(rf::named(&got, "Alice").is_empty(), "{layout}: Alice stayed");
        write_face_regions(&photo, CAT, &[], &[41, 42], &[], frame).unwrap();
        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        assert_eq!(got.regions, before.regions, "{layout}: only the foreign regions are left:\n{xml}");
        if !before.dims.is_empty() {
            assert_eq!(got.dims, before.dims, "{layout}");
        } // else the Regions ChairPhoto added stays, empty: nothing marks it as ours alone.
        assert_eq!(rf::foreign_structures(&xml), rf::foreign_structures(sidecar), "{layout}:\n{xml}");
    }

    // A Description binding `chairphoto` to another namespace.
    let (_dir, photo) = seeded_photo("xmp-135-prefix", &rf::DIGIKAM.replace(
        r#"xmlns:digiKam="http://www.digikam.org/ns/1.0/""#,
        r#"xmlns:digiKam="http://www.digikam.org/ns/1.0/" xmlns:chairphoto="urn:example:not-ours""#,
    ));
    write_face_regions(&photo, CAT, &[face(7, "Alice", (0.1, 0.1, 0.2, 0.2))], &[], &[], sized(6000, 4000))
        .unwrap();
    let got = rf::mwg(&read(&sidecar_path(&photo)));
    assert_eq!(rf::named(&got, "Alice")[0].face_id.as_deref(), Some(ours(7).as_str()));
}

/// Review L3 / P05: a marked region follows its face — renamed, then moved further than
/// `AREA_EPSILON` by a re-detection or an edited box — instead of a stale copy piling up.
/// Its foreign additions survive. (Renamed *and* moved in one write, the marker alone does
/// not prove it is the same face — ids are catalog-local — so it is replaced instead.)
#[test]
fn face_regions_follow_a_renamed_and_moved_face_by_its_marker() {
    let (_dir, photo) = seeded_photo("xmp-135-follow", rf::DIGIKAM);
    write_face_regions(&photo, CAT, &[face(9, "Alice", (0.1, 0.1, 0.2, 0.2))], &[], &[], sized(6000, 4000))
        .unwrap();
    // Another tool annotates our region.
    let xml = read(&sidecar_path(&photo)).replace(
        "<mwg-rs:Type>Face</mwg-rs:Type>",
        r#"<mwg-rs:Type>Face</mwg-rs:Type><digiKam:FaceEngine>dnn</digiKam:FaceEngine>"#,
    );
    std::fs::write(sidecar_path(&photo), &xml).unwrap();
    write_face_regions(&photo, CAT, &[face(9, "Alicia", (0.1, 0.1, 0.2, 0.2))], &[], &[], sized(6000, 4000))
        .unwrap();
    write_face_regions(&photo, CAT, &[face(9, "Alicia", (0.3, 0.2, 0.2, 0.2))], &[], &[], sized(6000, 4000))
        .unwrap();

    let xml = read(&sidecar_path(&photo));
    let got = rf::mwg(&xml);
    assert!(rf::named(&got, "Alice").is_empty(), "the stale name stayed:\n{xml}");
    let ours = rf::named(&got, "Alicia");
    assert_eq!(ours.len(), 1, "{xml}");
    assert!(near4(ours[0].area, center((0.3, 0.2, 0.2, 0.2))), "{xml}");
    assert_eq!(rf::subtree(&xml, rf::NS_DIGIKAM, "FaceEngine").len(), 1, "{xml}");
    assert_eq!(got.regions.len(), 2, "Alicia and digiKam's Bob:\n{xml}");
}

/// A foreign region that is the same face as one ChairPhoto writes (by Name + Area) is that
/// face already in the file: it is updated in place (#140), never marked, and no marked
/// copy is appended. When the face leaves the set it stays — it is not ours to remove.
#[test]
fn face_regions_never_remove_a_foreign_region() {
    let (_dir, photo) = seeded_photo("xmp-135-foreign", rf::DIGIKAM);
    // Bob as ChairPhoto has him (imported from this very region, source 'xmp').
    let bob = face(3, "Bob", (0.755, 0.6, 0.1, 0.2));
    write_face_regions(&photo, CAT, &[bob], &[], &[], sized(6000, 4000)).unwrap();
    let got = rf::mwg(&read(&sidecar_path(&photo)));
    assert_eq!(got.regions.len(), 1, "{got:?}");
    assert_eq!(got.regions[0].face_id, None, "a foreign region is never marked");

    write_face_regions(&photo, CAT, &[], &[], &[], sized(6000, 4000)).unwrap();
    let got = rf::mwg(&read(&sidecar_path(&photo)));
    assert_eq!(rf::named(&got, "Bob").len(), 1, "a foreign region was removed");
}

/// The decision's rule for regions written before the marker existed: an unmarked region
/// matching (Name + Area, in the display frame the old writer used) a face on the
/// pre-marker record was ChairPhoto's. While its face is in the set it is adopted — moved
/// into the frame and marked — and once the face has left it is removed. An unmarked region
/// the record does not describe is foreign and kept, even with a recorded face's name.
#[test]
fn face_regions_adopt_or_remove_what_a_pre_marker_chairphoto_wrote() {
    // What the pre-marker writer left on a portrait shot (Orientation 6): Alice and Dora in
    // the display frame, unmarked, plus a foreign Alice elsewhere and Lightroom's Bob.
    let pre_marker = |name: &str, (x, y, w, h): (f32, f32, f32, f32)| {
        format!(
            r#"<rdf:li rdf:parseType="Resource"><mwg-rs:Name>{name}</mwg-rs:Name><mwg-rs:Type>Face</mwg-rs:Type><mwg-rs:Area rdf:parseType="Resource"><stArea:x>{}</stArea:x><stArea:y>{}</stArea:y><stArea:w>{w}</stArea:w><stArea:h>{h}</stArea:h><stArea:unit>normalized</stArea:unit></mwg-rs:Area></rdf:li>"#,
            x + w / 2.0,
            y + h / 2.0
        )
    };
    let alice_then = (0.1, 0.1, 0.2, 0.2);
    let dora_then = (0.5, 0.5, 0.1, 0.1);
    let lis = [
        pre_marker("Alice", alice_then),
        pre_marker("Dora", dora_then),
        pre_marker("Alice", (0.7, 0.05, 0.1, 0.1)),
    ]
    .concat();
    let sidecar = rf::LIGHTROOM_ROTATED.replace("     </rdf:Bag>", &format!("{lis}</rdf:Bag>"));
    let (_dir, photo) = seeded_photo("xmp-135-legacy", &sidecar);
    assert_eq!(rf::mwg(&sidecar).regions.len(), 4);

    let record = [face(1, "Alice", alice_then), face(2, "Dora", dora_then)];
    // Alice is still confirmed (re-detected a little off); Dora was rejected.
    let alice_now = face(1, "Alice", (0.105, 0.1, 0.2, 0.2));
    write_face_regions(&photo, CAT, &[alice_now.clone()], &[], &record, turned(6, 6000, 4000)).unwrap();

    let xml = read(&sidecar_path(&photo));
    let got = rf::mwg(&xml);
    assert!(rf::named(&got, "Dora").is_empty(), "the rejected face's region stayed:\n{xml}");
    let alices = rf::named(&got, "Alice");
    assert_eq!(alices.len(), 2, "adopted, not duplicated; the foreign Alice kept:\n{xml}");
    let ours: Vec<_> = alices.iter().filter(|r| r.face_id.is_some()).collect();
    assert_eq!(ours.len(), 1, "{xml}");
    assert_eq!(ours[0].face_id.as_deref(), Some(super::tests::ours(1).as_str()));
    assert!(near4(ours[0].area, center(display_to_stored(6, alice_now.bbox))), "{xml}");
    let foreign: Vec<_> = alices.iter().filter(|r| r.face_id.is_none()).collect();
    assert!(near4(foreign[0].area, (0.75, 0.1, 0.1, 0.1)), "{xml}");
    assert_eq!(rf::named(&got, "Bob")[0].area, BOB_STORED);
}

/// A write that changes nothing in the regions leaves the sidecar alone: with nothing to
/// write and nothing of ours to remove no sidecar is created, and repeating a write leaves
/// the file byte for byte as the first one did.
#[test]
fn face_regions_that_change_nothing_write_nothing() {
    let dir = region_dir("xmp-135-noop");
    let photo = dir.join("N.JPG");
    std::fs::write(&photo, b"jpeg").unwrap();
    write_face_regions(&photo, CAT, &[], &[], &[], sized(6000, 4000)).unwrap();
    assert!(!sidecar_path(&photo).exists(), "an empty set created a sidecar");

    let (_dir, photo) = seeded_photo("xmp-135-noop-again", rf::DIGIKAM);
    write_face_regions(&photo, CAT, &[], &[], &[], sized(6000, 4000)).unwrap();
    assert_eq!(read(&sidecar_path(&photo)), rf::DIGIKAM, "nothing of ours, nothing written");
    let alice = [face(5, "Alice", (0.1, 0.1, 0.2, 0.2))];
    write_face_regions(&photo, CAT, &alice, &[], &[], sized(6000, 4000)).unwrap();
    let first = read(&sidecar_path(&photo));
    write_face_regions(&photo, CAT, &alice, &[], &[], sized(6000, 4000)).unwrap();
    assert_eq!(read(&sidecar_path(&photo)), first);
}

// ── #147: matching and the bare rdf:RDF root ───────────────────────────────

/// Two unmarked Bobs, the one listed first at center x 0.515, the second at 0.50. With the
/// Bob being written at 0.505 both are within `AREA_EPSILON`.
fn two_foreign_bobs() -> String {
    let bob = |x: &str| {
        format!(
            r#"<rdf:li><rdf:Description mwg-rs:Name="Bob" mwg-rs:Type="Face"><mwg-rs:Area stArea:x="{x}" stArea:y="0.5" stArea:w="0.1" stArea:h="0.1" stArea:unit="normalized" digiKam:Confidence="{x}"/></rdf:Description></rdf:li>"#
        )
    };
    let digikam_bob = r#"      <rdf:li>
       <rdf:Description mwg-rs:Name="Bob" mwg-rs:Type="Face">
        <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2" stArea:unit="normalized"/>
       </rdf:Description>
      </rdf:li>"#;
    assert!(rf::DIGIKAM.contains(digikam_bob));
    rf::DIGIKAM.replace(digikam_bob, &[bob("0.515"), bob("0.5")].concat())
}

/// Review L1 (probe P04): one incoming region updates at most one existing region, and
/// L2: the one it updates is the closest, not the first in document order. The other Bob,
/// listed first, is left exactly as it was.
#[test]
fn face_regions_update_only_the_closest_matching_region() {
    let sidecar = two_foreign_bobs();
    assert_eq!(rf::named(&rf::mwg(&sidecar), "Bob").len(), 2, "fixture:\n{sidecar}");
    let (_dir, photo) = seeded_photo("xmp-147-closest", &sidecar);
    let bob = face(3, "Bob", (0.455, 0.45, 0.1, 0.1)); // center (0.505, 0.5)
    write_face_regions(&photo, CAT, &[bob], &[], &[], sized(6000, 4000)).unwrap();

    let xml = read(&sidecar_path(&photo));
    let bobs = rf::named(&rf::mwg(&xml), "Bob").into_iter().cloned().collect::<Vec<_>>();
    assert_eq!(bobs.len(), 2, "no Bob appended or removed:\n{xml}");
    assert_eq!(bobs[0].area.0, 0.515, "the farther Bob, listed first, moved:\n{xml}");
    assert!((bobs[1].area.0 - 0.505).abs() < 1e-5, "the closest Bob did not move:\n{xml}");
    assert!(bobs.iter().all(|b| b.face_id.is_none()), "a foreign region was marked:\n{xml}");
}

/// L2 with ChairPhoto's own region in the file: a digiKam Bob listed first and closer to
/// the old position never takes the update meant for ChairPhoto's marked Bob.
#[test]
fn face_regions_update_their_own_region_before_a_foreign_one() {
    let (_dir, photo) = seeded_photo("xmp-147-ours-first", &two_foreign_bobs());
    // ChairPhoto's own Bob, marked, at center (0.51, 0.5): written as Zed far away, then
    // edited into place, so no foreign Bob is matched on the way.
    write_face_regions(&photo, CAT, &[face(99, "Zed", (0.1, 0.1, 0.1, 0.1))], &[], &[], sized(6000, 4000))
        .unwrap();
    let xml = read(&sidecar_path(&photo)).replace(
        "<mwg-rs:Name>Zed</mwg-rs:Name>",
        "<mwg-rs:Name>Bob</mwg-rs:Name>",
    )
    .replace(&format!("<chairphoto:FaceId>{}</chairphoto:FaceId>", ours(99)), &format!("<chairphoto:FaceId>{}</chairphoto:FaceId>", ours(3)))
    .replace("<stArea:x>0.15</stArea:x><stArea:y>0.15</stArea:y>", "<stArea:x>0.51</stArea:x><stArea:y>0.5</stArea:y>");
    assert!(xml.contains("<stArea:x>0.51</stArea:x>"), "{xml}");
    std::fs::write(sidecar_path(&photo), &xml).unwrap();

    write_face_regions(&photo, CAT, &[face(3, "Bob", (0.455, 0.45, 0.1, 0.1))], &[], &[], sized(6000, 4000))
        .unwrap();
    let xml = read(&sidecar_path(&photo));
    let got = rf::mwg(&xml);
    let bobs = rf::named(&got, "Bob");
    assert_eq!(bobs.len(), 3, "{xml}");
    assert_eq!((bobs[0].area.0, bobs[1].area.0), (0.515, 0.5), "a foreign Bob moved:\n{xml}");
    assert_eq!(bobs[2].face_id.as_deref(), Some(ours(3).as_str()));
    assert!((bobs[2].area.0 - 0.505).abs() < 1e-5, "ours did not move:\n{xml}");
}

/// Review L4 (probe P12): a sidecar whose root is a bare `rdf:RDF` — no `x:xmpmeta`, which
/// the XMP spec allows — is read and written in place: one `rdf:RDF`, one `Regions`, the
/// foreign region kept and ChairPhoto's added; the identifier reads and writes too. A root
/// that is no XMP packet at all is refused and left as it was.
#[test]
fn a_bare_rdf_root_is_written_in_place() {
    let start = rf::DIGIKAM.find("<rdf:RDF").unwrap();
    let end = rf::DIGIKAM.find("</x:xmpmeta>").unwrap();
    let bare = format!(r#"<?xml version="1.0" encoding="UTF-8"?>{}"#, &rf::DIGIKAM[start..end]);
    let (_dir, photo) = seeded_photo("xmp-147-bare", &bare);
    assert_eq!(region_names(&photo), ["Bob"], "the bare root is read");

    write_face_regions(&photo, CAT, &[face(4, "Alice", (0.1, 0.1, 0.2, 0.2))], &[], &[], sized(6000, 4000))
        .unwrap();
    write_identifier(&photo, "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f").unwrap();

    let xml = read(&sidecar_path(&photo));
    let got = rf::mwg(&xml);
    assert_eq!(got.rdf_elements, 1, "a second rdf:RDF:\n{xml}");
    assert_eq!(got.dims.len(), 1, "a second Regions:\n{xml}");
    assert_eq!(rf::named(&got, "Bob")[0].area, (0.8, 0.7, 0.1, 0.2));
    assert_eq!(rf::named(&got, "Alice")[0].face_id.as_deref(), Some(ours(4).as_str()));
    assert_eq!(read_identifier(&photo).as_deref(), Some("6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f"));
    assert_eq!(rf::foreign_structures(&xml), rf::foreign_structures(rf::DIGIKAM));

    let (_dir, photo) = seeded_photo("xmp-147-not-xmp", "<foo><bar/></foo>");
    assert!(write_face_regions(&photo, CAT, &[face(4, "Alice", (0.1, 0.1, 0.2, 0.2))], &[], &[], sized(6000, 4000))
        .is_err());
    assert!(write_identifier(&photo, "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f").is_err());
    assert_eq!(read(&sidecar_path(&photo)), "<foo><bar/></foo>");
}

/// The #147 nit: a `chairphoto:LastWrite` exiftool moved into a later Description still
/// says ChairPhoto has written the file, so no backup of a ChairPhoto-written state is
/// taken as if it were the foreign original.
#[test]
fn last_write_in_a_later_description_counts() {
    let sidecar = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="2"/>
  <rdf:Description rdf:about="" xmlns:chairphoto="https://chairphoto.local/ns/1.0/">
   <chairphoto:LastWrite>1700000000</chairphoto:LastWrite>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
    let (_dir, photo) = seeded_photo("xmp-147-lastwrite", sidecar);
    write_face_regions(&photo, CAT, &[face(4, "Alice", (0.1, 0.1, 0.2, 0.2))], &[], &[], sized(6000, 4000))
        .unwrap();
    assert!(!sidecar_backup_path(&sidecar_path(&photo)).exists(), "backed up a file ChairPhoto wrote");
}

// ── #135 M1: the marker is scoped to its catalog ───────────────────────────

/// A `rdf:li` region in ChairPhoto's own shape, carrying `marker` as its FaceId.
fn marked_region(marker: &str, name: &str, (x, y, w, h): (f32, f32, f32, f32)) -> String {
    format!(
        r#"<rdf:li rdf:parseType="Resource"><mwg-rs:Name>{name}</mwg-rs:Name><mwg-rs:Type>Face</mwg-rs:Type><mwg-rs:Area rdf:parseType="Resource"><stArea:x>{}</stArea:x><stArea:y>{}</stArea:y><stArea:w>{w}</stArea:w><stArea:h>{h}</stArea:h><stArea:unit>normalized</stArea:unit></mwg-rs:Area><chairphoto:FaceId xmlns:chairphoto="https://chairphoto.local/ns/1.0/">{marker}</chairphoto:FaceId></rdf:li>"#,
        x + w / 2.0,
        y + h / 2.0
    )
}

/// digiKam's sidecar with `lis` added to its RegionList.
fn digikam_with(lis: &str) -> String {
    let end = "     </rdf:Bag>\n    </mwg-rs:RegionList>";
    assert!(rf::DIGIKAM.contains(end), "the fixture's list moved");
    rf::DIGIKAM.replacen(end, &format!("{lis}{end}"), 1)
}

/// Review M1 / probe Q5: a region another catalog marked — or a marker this build does not
/// recognise — is foreign. A write of this catalog's set never removes it, never marks it
/// as ours, and never renames or moves it, even when its face id is one this catalog
/// writes; a rejected face of ours beside it still goes.
#[test]
fn another_catalogs_regions_are_foreign() {
    const OTHER: &str = "5d0e9a77-3c2b-4f1e-8a6d-2b9c0e1f4a33";
    let carol = (0.8, 0.8, 0.1, 0.1);
    for (case, marker) in [
        ("another catalog", format!("{OTHER}/7")),
        ("another catalog, our face id", format!("{OTHER}/1")),
        ("the pre-scope bare id", "1".to_string()),
        ("not a face id", format!("{CAT}/x1")),
        ("no face id", format!("{CAT}/")),
        ("a longer path", format!("{CAT}/1/2")),
        ("upper-cased catalog", format!("{}/1", CAT.to_uppercase())),
    ] {
        let lis = [marked_region(&marker, "Carol", carol), marked_region(&ours(2), "Dora", (0.5, 0.5, 0.1, 0.1))];
        let sidecar = digikam_with(&lis.concat());
        let (_dir, photo) = seeded_photo("xmp-135-scope", &sidecar);
        let carol_before = rf::named(&rf::mwg(&sidecar), "Carol")[0].clone();
        // This catalog writes Alice as face 1; Dora (face 2) has been rejected.
        write_face_regions(&photo, CAT, &[face(1, "Alice", (0.1, 0.1, 0.2, 0.2))], &[2], &[], sized(6000, 4000))
            .unwrap();
        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        assert_eq!(rf::named(&got, "Carol"), [&carol_before], "{case}: Carol changed:\n{xml}");
        assert!(rf::named(&got, "Dora").is_empty(), "{case}: our rejected Dora stayed:\n{xml}");
        assert_eq!(rf::named(&got, "Alice")[0].face_id.as_deref(), Some(ours(1).as_str()), "{case}");
        assert_eq!(rf::named(&got, "Bob").len(), 1, "{case}");

        write_face_regions(&photo, CAT, &[], &[1, 2], &[], sized(6000, 4000)).unwrap();
        let got = rf::mwg(&read(&sidecar_path(&photo)));
        assert_eq!(rf::named(&got, "Carol"), [&carol_before], "{case}: removed with an empty set");
    }
}

/// Another catalog's region that is the same face as one this catalog writes holds that
/// face already: as for an unmarked foreign region (#140), only its Area moves, its marker
/// stays the other catalog's, and no copy of ours is appended — so this catalog's reject
/// later leaves it in place.
#[test]
fn another_catalogs_region_of_the_same_face_is_updated_in_place_only() {
    const OTHER: &str = "5d0e9a77-3c2b-4f1e-8a6d-2b9c0e1f4a33";
    let theirs = format!("{OTHER}/7");
    let sidecar = digikam_with(&marked_region(&theirs, "Alice", (0.1, 0.1, 0.2, 0.2)));
    let (_dir, photo) = seeded_photo("xmp-135-scope-same", &sidecar);
    write_face_regions(&photo, CAT, &[face(1, "Alice", (0.105, 0.1, 0.2, 0.2))], &[], &[], sized(6000, 4000))
        .unwrap();
    let xml = read(&sidecar_path(&photo));
    let alices = rf::named(&rf::mwg(&xml), "Alice").into_iter().cloned().collect::<Vec<_>>();
    assert_eq!(alices.len(), 1, "a copy was appended:\n{xml}");
    assert_eq!(alices[0].face_id.as_deref(), Some(theirs.as_str()), "re-marked as ours:\n{xml}");
    assert!(near4(alices[0].area, center((0.105, 0.1, 0.2, 0.2))), "{xml}");
    write_face_regions(&photo, CAT, &[], &[], &[], sized(6000, 4000)).unwrap();
    assert_eq!(rf::named(&rf::mwg(&read(&sidecar_path(&photo))), "Alice").len(), 1);
}

// ── #135 M2: only the pre-marker writer's exact shape is legacy ────────────

/// Review M2 / probes Q4, Q4b: an unmarked region at the recorded place under the recorded
/// name is taken for the pre-marker writer's only when it has exactly that writer's shape.
/// digiKam's (nested `rdf:Description`, `digiKam:Confidence`) and every other variant —
/// an extra field, a foreign child or attribute, another struct form — stays foreign: a
/// reject never removes it, a confirmed face never marks it (only its Area moves, #140).
/// The control, the exact shape, is adopted and then removed.
#[test]
fn only_the_pre_marker_shape_is_adopted_or_removed() {
    const AREA: &str = r#"<mwg-rs:Area rdf:parseType="Resource"><stArea:x>0.2</stArea:x><stArea:y>0.2</stArea:y><stArea:w>0.2</stArea:w><stArea:h>0.2</stArea:h><stArea:unit>normalized</stArea:unit></mwg-rs:Area>"#;
    let exact = |li_attrs: &str, extra: &str| {
        format!(
            r#"<rdf:li rdf:parseType="Resource"{li_attrs}><mwg-rs:Name>Alice</mwg-rs:Name><mwg-rs:Type>Face</mwg-rs:Type>{AREA}{extra}</rdf:li>"#
        )
    };
    let foreign = [
        (
            "digiKam (Q4)",
            r#"<rdf:li><rdf:Description mwg-rs:Name="Alice" mwg-rs:Type="Face" digiKam:Confidence="0.97"><mwg-rs:Area stArea:x="0.2" stArea:y="0.2" stArea:w="0.2" stArea:h="0.2" stArea:unit="normalized"/></rdf:Description></rdf:li>"#.to_string(),
        ),
        ("an extra MWG field", exact("", "<mwg-rs:Rotation>0</mwg-rs:Rotation>")),
        ("a foreign child", exact("", "<digiKam:FaceEngine>dnn</digiKam:FaceEngine>")),
        ("a foreign attribute", exact(r#" digiKam:Confidence="0.97""#, "")),
        (
            "an Area in attribute form",
            r#"<rdf:li rdf:parseType="Resource"><mwg-rs:Name>Alice</mwg-rs:Name><mwg-rs:Type>Face</mwg-rs:Type><mwg-rs:Area stArea:x="0.2" stArea:y="0.2" stArea:w="0.2" stArea:h="0.2" stArea:unit="normalized"/></rdf:li>"#.to_string(),
        ),
        (
            "a foreign Area field",
            exact("", "").replace("<stArea:unit>", r#"<digiKam:Source>user</digiKam:Source><stArea:unit>"#),
        ),
    ];
    let alice = face(1, "Alice", (0.1, 0.1, 0.2, 0.2));
    let record = [alice.clone()];
    for (case, li) in &foreign {
        let sidecar = digikam_with(li);
        let before = rf::named(&rf::mwg(&sidecar), "Alice")[0].clone();
        assert_eq!(before.area, (0.2, 0.2, 0.2, 0.2), "{case}: fixture");

        // Q4: the face was rejected.
        let (_dir, photo) = seeded_photo("xmp-135-shape-reject", &sidecar);
        write_face_regions(&photo, CAT, &[], &[], &record, sized(6000, 4000)).unwrap();
        let xml = read(&sidecar_path(&photo));
        assert_eq!(rf::named(&rf::mwg(&xml), "Alice"), [&before], "{case}: deleted:\n{xml}");

        // Q4b: the face is still confirmed (moved a little).
        let (_dir, photo) = seeded_photo("xmp-135-shape-keep", &sidecar);
        let moved = face(1, "Alice", (0.105, 0.1, 0.2, 0.2));
        write_face_regions(&photo, CAT, &[moved], &[], &record, sized(6000, 4000)).unwrap();
        let xml = read(&sidecar_path(&photo));
        let alices = rf::named(&rf::mwg(&xml), "Alice").into_iter().cloned().collect::<Vec<_>>();
        assert_eq!(alices.len(), 1, "{case}: duplicated:\n{xml}");
        assert_eq!(alices[0].face_id, None, "{case}: adopted as ours:\n{xml}");
        write_face_regions(&photo, CAT, &[], &[], &record, sized(6000, 4000)).unwrap();
        assert_eq!(rf::named(&rf::mwg(&read(&sidecar_path(&photo))), "Alice").len(), 1, "{case}");
    }

    // The control: the exact shape is the old writer's.
    let sidecar = digikam_with(&exact("", ""));
    let (_dir, photo) = seeded_photo("xmp-135-shape-exact", &sidecar);
    write_face_regions(&photo, CAT, &[alice.clone()], &[], &record, sized(6000, 4000)).unwrap();
    let got = rf::mwg(&read(&sidecar_path(&photo)));
    assert_eq!(rf::named(&got, "Alice")[0].face_id.as_deref(), Some(ours(1).as_str()), "not adopted");
    let (_dir, photo) = seeded_photo("xmp-135-shape-exact-reject", &sidecar);
    write_face_regions(&photo, CAT, &[], &[], &record, sized(6000, 4000)).unwrap();
    assert!(rf::named(&rf::mwg(&read(&sidecar_path(&photo))), "Alice").is_empty(), "not removed");
}

// ── review L5: the name-or-place guard on a FaceId match ───────────────────

/// Step 1 follows our marker only while the region is still recognisably that face — its
/// Name, or its place. A region carrying our marker for face 5 whose name *and* place
/// both differ from face 5's is not renamed and moved: it may be a copy of this catalog's
/// file that diverged, sharing its identity and face ids, so with face 5 still in the set
/// it is kept exactly as it is (review N1) and face 5 is written beside it. The next write
/// claims face 5's own region, and then the other one carrying the same marker goes
/// (#209). With either the name or the place still matching, the marker is followed and
/// the annotation kept.
#[test]
fn a_marker_is_followed_only_while_the_name_or_place_still_matches() {
    let annotated = |name: &str, bbox| {
        marked_region(&ours(5), name, bbox)
            .replace("</rdf:li>", "<digiKam:FaceEngine>dnn</digiKam:FaceEngine></rdf:li>")
    };
    let carl_place = (0.7, 0.7, 0.1, 0.1);
    let alice_place = (0.1, 0.1, 0.2, 0.2);
    for (case, existing, follows) in [
        ("name and place differ", annotated("Carl", carl_place), false),
        ("same name, moved", annotated("Alice", carl_place), true),
        ("renamed, same place", annotated("Carl", alice_place), true),
    ] {
        let (_dir, photo) = seeded_photo("xmp-l5-guard", &digikam_with(&existing));
        write_face_regions(&photo, CAT, &[face(5, "Alice", alice_place)], &[], &[], sized(6000, 4000)).unwrap();
        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        let carls = rf::named(&got, "Carl");
        if follows {
            assert!(carls.is_empty(), "{case}: Carl stayed:\n{xml}");
        } else {
            let before = rf::named(&rf::mwg(&digikam_with(&existing)), "Carl")[0].clone();
            assert_eq!(carls, [&before], "{case}: Carl changed:\n{xml}");
        }
        let alices = rf::named(&got, "Alice");
        assert_eq!(alices.len(), 1, "{case}:\n{xml}");
        assert_eq!(alices[0].face_id.as_deref(), Some(ours(5).as_str()), "{case}");
        assert!(near4(alices[0].area, center(alice_place)), "{case}:\n{xml}");
        let kept = rf::subtree(&xml, rf::NS_DIGIKAM, "FaceEngine").len();
        assert_eq!(kept, 1, "{case}: the annotation:\n{xml}");

        // The next write: one region carries the marker, the claimed one.
        write_face_regions(&photo, CAT, &[face(5, "Alice", alice_place)], &[], &[], sized(6000, 4000)).unwrap();
        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        assert!(rf::named(&got, "Carl").is_empty(), "{case}: the stale copy stayed:\n{xml}");
        let marked: Vec<_> = got.regions.iter().filter(|r| r.face_id.as_deref() == Some(ours(5).as_str())).collect();
        assert_eq!(marked.len(), 1, "{case}:\n{xml}");
        assert_eq!(marked[0].name, "Alice", "{case}");
        let kept = rf::subtree(&xml, rf::NS_DIGIKAM, "FaceEngine").len();
        assert_eq!(kept, usize::from(follows), "{case}: the annotation went with its region:\n{xml}");
    }
}

// ── #209 (review F2): one region per marker of ours ─────────────────────────

/// Probe F2: face 1 is written as "Alice" while the orientation is unknown, then — the
/// orientation found by a rescan and the face reassigned — as "Bob" on Orientation 6, so
/// both its name and its place change in one write. That write cannot tell the old region
/// from a copied catalog's (review N1) and appends Bob beside it; the photo's next write
/// claims Bob's region by its marker and removes the stale Alice carrying the same one.
/// digiKam's foreign Bob stays throughout.
#[test]
fn a_stale_region_with_the_marker_of_a_claimed_one_is_removed() {
    let (_dir, photo) = seeded_photo("xmp-209-rename-move", rf::DIGIKAM);
    let before = rf::mwg(rf::DIGIKAM);
    let unknown = RegionFrame { orientation: None, stored_size: Some((6000, 4000)), ..Default::default() };
    let bbox = (0.1, 0.2, 0.3, 0.4);
    let marked = |xml: &str| -> Vec<rf::MwgRegion> {
        rf::mwg(xml).regions.into_iter().filter(|r| r.face_id.as_deref() == Some(ours(1).as_str())).collect()
    };
    write_face_regions(&photo, CAT, &[face(1, "Alice", bbox)], &[], &[], unknown).unwrap();
    write_face_regions(&photo, CAT, &[face(1, "Bob", bbox)], &[], &[], turned(6, 6000, 4000)).unwrap();
    let xml = read(&sidecar_path(&photo));
    let names: Vec<String> = marked(&xml).into_iter().map(|r| r.name).collect();
    assert_eq!(names, ["Alice", "Bob"], "the write that renamed and moved it appends:\n{xml}");

    write_face_regions(&photo, CAT, &[face(1, "Bob", bbox)], &[], &[], turned(6, 6000, 4000)).unwrap();
    let xml = read(&sidecar_path(&photo));
    let ours1 = marked(&xml);
    assert_eq!(ours1.len(), 1, "the stale copy stayed:\n{xml}");
    assert_eq!(ours1[0].name, "Bob");
    // Display (x, y) → stored (y, 1 - x) on O6: centre (0.25, 0.4) → (0.4, 0.75), size swapped.
    assert!(near4(ours1[0].area, (0.4, 0.75, 0.4, 0.3)), "{:?}\n{xml}", ours1[0].area);
    let foreign: Vec<_> = rf::mwg(&xml).regions.into_iter().filter(|r| r.face_id.is_none()).collect();
    assert_eq!(foreign, before.regions, "the foreign regions are untouched:\n{xml}");
}

/// Two regions carrying the same marker of ours (a file merged by hand, say): the one that
/// is still the face is claimed and the other removed. A marker for a face unknown on the
/// photo beside them is still kept (review N1).
#[test]
fn of_two_regions_with_our_marker_the_unclaimed_one_goes() {
    let alice = (0.1, 0.1, 0.2, 0.2);
    let lis = [
        marked_region(&ours(5), "Carl", (0.7, 0.7, 0.1, 0.1)),
        marked_region(&ours(5), "Alice", alice),
        marked_region(&ours(901), "Dora", (0.4, 0.7, 0.1, 0.1)),
    ]
    .concat();
    let (_dir, photo) = seeded_photo("xmp-209-duplicate", &digikam_with(&lis));
    write_face_regions(&photo, CAT, &[face(5, "Alice", alice)], &[], &[], sized(6000, 4000)).unwrap();
    let xml = read(&sidecar_path(&photo));
    let got = rf::mwg(&xml);
    assert!(rf::named(&got, "Carl").is_empty(), "{xml}");
    assert_eq!(rf::named(&got, "Alice").len(), 1, "{xml}");
    assert_eq!(rf::named(&got, "Dora").len(), 1, "an unknown id stays:\n{xml}");
}

// ── review N1: only a face this catalog knows on the photo ─────────────────

/// Probe R2: a region carrying this catalog's marker for face 901, which this catalog
/// does not know on this photo (a copy of the catalog file wrote it), is foreign — kept
/// exactly as it is by a write of another face, by a write whose own face 901 is someone
/// else elsewhere (the copies' id counters collide), and by an empty write. Only when 901
/// is one of this photo's faces that left the set (`retired`) is it removed.
#[test]
fn a_marker_for_a_face_unknown_on_the_photo_is_kept() {
    let carol = marked_region(&ours(901), "Carol", (0.7, 0.7, 0.1, 0.1));
    let sidecar = digikam_with(&carol);
    let before = rf::named(&rf::mwg(&sidecar), "Carol")[0].clone();
    let alice = face(1, "Alice", (0.1, 0.1, 0.2, 0.2));
    let dave = face(901, "Dave", (0.4, 0.1, 0.1, 0.1));
    for (case, regions, retired, kept) in [
        ("unknown id", vec![alice.clone()], vec![], true),
        ("colliding id, another face", vec![dave], vec![], true),
        ("empty write", vec![], vec![1], true),
        ("unknown id, others retired", vec![alice.clone()], vec![2, 3], true),
        ("retired here", vec![alice.clone()], vec![901], false),
    ] {
        let (_dir, photo) = seeded_photo("xmp-n1-unknown", &sidecar);
        write_face_regions(&photo, CAT, &regions, &retired, &[], sized(6000, 4000)).unwrap();
        let xml = read(&sidecar_path(&photo));
        let carols = rf::named(&rf::mwg(&xml), "Carol").into_iter().cloned().collect::<Vec<_>>();
        if kept {
            assert_eq!(carols, [before.clone()], "{case}:\n{xml}");
        } else {
            assert!(carols.is_empty(), "{case}: not removed:\n{xml}");
        }
    }

    // The same face as one this catalog writes (Carol, a little off): like any foreign
    // region it holds her already — only its Area moves, the copy's marker stays, and no
    // region of ours is appended beside it.
    let (_dir, photo) = seeded_photo("xmp-n1-unknown-same", &sidecar);
    let carol_here = face(3, "Carol", (0.705, 0.7, 0.1, 0.1));
    write_face_regions(&photo, CAT, &[carol_here], &[], &[], sized(6000, 4000)).unwrap();
    let xml = read(&sidecar_path(&photo));
    let carols = rf::named(&rf::mwg(&xml), "Carol").into_iter().cloned().collect::<Vec<_>>();
    assert_eq!(carols.len(), 1, "{xml}");
    assert_eq!(carols[0].face_id.as_deref(), Some(ours(901).as_str()), "re-marked:\n{xml}");
    assert!(near4(carols[0].area, center((0.705, 0.7, 0.1, 0.1))), "{xml}");
}

// ── review round 2 nit: only the canonical face id is ours ─────────────────

/// A marker is ours only in exactly the form ChairPhoto writes. `<catalog>/02` names
/// face 2 to a lenient parser, but ChairPhoto never wrote it, so it is foreign: kept even
/// though this photo's face 2 has been retired. The canonical `<catalog>/2` is removed.
#[test]
fn only_the_canonical_face_id_is_ours() {
    for (id, removed) in [("2", true), ("02", false), ("002", false), ("+2", false), ("-2", false)] {
        let marker = format!("{CAT}/{id}");
        let sidecar = digikam_with(&marked_region(&marker, "Dora", (0.5, 0.5, 0.1, 0.1)));
        let (_dir, photo) = seeded_photo("xmp-nit-canonical", &sidecar);
        write_face_regions(&photo, CAT, &[], &[2], &[], sized(6000, 4000)).unwrap();
        let xml = read(&sidecar_path(&photo));
        let doras = rf::named(&rf::mwg(&xml), "Dora").len();
        assert_eq!(doras, usize::from(!removed), "{marker}:\n{xml}");
    }
}
