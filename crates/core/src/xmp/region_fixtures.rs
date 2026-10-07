//! Hand-written foreign sidecars carrying face regions — digiKam, Lightroom on a photo with
//! EXIF Orientation 6, Microsoft Photo — sidecars written by real tools (exiftool, exiv2;
//! #154), and an independent reader for what the face-region writer leaves in them (#135,
//! #136, #145, #147).
//!
//! The reader is `roxmltree`, not the xml-rs parser ChairPhoto reads and writes sidecars
//! with, so a bug the two would share cannot hide. Everything is matched by namespace URI:
//! a prefix or a local name alone never counts.

use super::{NS_CHAIRPHOTO, NS_MWG_RS, NS_RDF, NS_STAREA, NS_STDIM};

/// Microsoft Photo's region schema, which digiKam also writes beside MWG.
pub(crate) const NS_MP: &str = "http://ns.microsoft.com/photo/1.2/";
pub(crate) const NS_MPRI: &str = "http://ns.microsoft.com/photo/1.2/t/RegionInfo#";
pub(crate) const NS_MPREG: &str = "http://ns.microsoft.com/photo/1.2/t/Region#";
pub(crate) const NS_TIFF: &str = "http://ns.adobe.com/tiff/1.0/";
pub(crate) const NS_DIGIKAM: &str = "http://www.digikam.org/ns/1.0/";

/// digiKam's shape: MWG regions as nested `rdf:Description`s with attribute-form fields and
/// an attribute-form `AppliedToDimensions`, beside `MP:RegionInfo` and `digiKam:TagsList`.
/// Its photo is upright: 6000x4000, no Orientation.
pub(crate) const DIGIKAM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="XMP Core 4.4.0-Exiv2">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:digiKam="http://www.digikam.org/ns/1.0/"
    xmlns:MP="http://ns.microsoft.com/photo/1.2/"
    xmlns:MPRI="http://ns.microsoft.com/photo/1.2/t/RegionInfo#"
    xmlns:MPReg="http://ns.microsoft.com/photo/1.2/t/Region#"
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stDim="http://ns.adobe.com/xap/1.0/sType/Dimensions#"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#">
   <digiKam:TagsList>
    <rdf:Seq>
     <rdf:li>People/Bob</rdf:li>
    </rdf:Seq>
   </digiKam:TagsList>
   <MP:RegionInfo rdf:parseType="Resource">
    <MPRI:Regions>
     <rdf:Bag>
      <rdf:li MPReg:Rectangle="0.75, 0.6, 0.1, 0.2" MPReg:PersonDisplayName="Bob"/>
     </rdf:Bag>
    </MPRI:Regions>
   </MP:RegionInfo>
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:AppliedToDimensions stDim:w="6000" stDim:h="4000" stDim:unit="pixel"/>
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li>
       <rdf:Description mwg-rs:Name="Bob" mwg-rs:Type="Face">
        <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2" stArea:unit="normalized"/>
       </rdf:Description>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

/// Lightroom's shape on a portrait shot (a camera held upright): `tiff:Orientation="6"`, the
/// stored image 6000x4000 as MWG requires, Bob's region in the stored frame with a
/// `mwg-rs:Rotation` and an Adobe extension, the Area in attribute form.
pub(crate) const LIGHTROOM_ROTATED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 7.0-c000 1.000000, 0000/00/00-00:00:00">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:tiff="http://ns.adobe.com/tiff/1.0/"
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stDim="http://ns.adobe.com/xap/1.0/sType/Dimensions#"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
    xmlns:lr="http://ns.adobe.com/lightroom/1.0/"
    tiff:Orientation="6"
    xmp:Rating="4">
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:AppliedToDimensions stDim:w="6000" stDim:h="4000" stDim:unit="pixel"/>
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li>
       <rdf:Description mwg-rs:Name="Bob" mwg-rs:Type="Face" mwg-rs:Rotation="0.0">
        <mwg-rs:Area stArea:h="0.1" stArea:w="0.15" stArea:x="0.3" stArea:y="0.25" stArea:unit="normalized"/>
        <mwg-rs:Extensions rdf:parseType="Resource">
         <lr:AppliedFaceRecognition>true</lr:AppliedFaceRecognition>
        </mwg-rs:Extensions>
       </rdf:Description>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

/// Microsoft Photo's shape (Windows Live Photo Gallery): `MP:RegionInfo` only, no MWG.
pub(crate) const MS_PHOTO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="uuid:faf5bdd5-ba3d-11da-ad31-d33d75182f1b"
    xmlns:MP="http://ns.microsoft.com/photo/1.2/">
   <MP:RegionInfo>
    <rdf:Description xmlns:MPRI="http://ns.microsoft.com/photo/1.2/t/RegionInfo#">
     <MPRI:Regions>
      <rdf:Bag xmlns:MPReg="http://ns.microsoft.com/photo/1.2/t/Region#">
       <rdf:li MPReg:Rectangle="0.4, 0.3, 0.2, 0.25" MPReg:PersonDisplayName="Carol"
         MPReg:PersonEmailDigest="0F1E2D3C"/>
      </rdf:Bag>
     </MPRI:Regions>
    </rdf:Description>
   </MP:RegionInfo>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

/// The three foreign layouts, by name.
pub(crate) const FOREIGN_REGIONS: [(&str, &str); 3] =
    [("digikam", DIGIKAM), ("lightroom-o6", LIGHTROOM_ROTATED), ("ms-photo", MS_PHOTO)];

// Sidecars written by real tools (#154), byte for byte as the tools wrote them; provenance in
// `crates/core/tests/fixtures/faces/README.md`. Each holds Bob at stored-frame center
// (0.325, 0.25), 0.15 x 0.1, `AppliedToDimensions` 600x400, `tiff:Orientation` 6: the values
// were given by hand, the serialisation is the tool's.

/// exiftool 13.55 creating a sidecar from a JPEG with EXIF Orientation 6: an `xpacket` with a
/// BOM, single quotes, one `rdf:Description` per namespace (exif, tiff, dc, mwg-rs), structs
/// as `rdf:parseType='Resource'` with element-form fields in alphabetical order.
pub(crate) const EXIFTOOL_O6: &str = include_str!("../../tests/fixtures/faces/regions/exiftool-o6.xmp");

/// exiv2 0.28.9 (`XMP Core 4.4.0-Exiv2`, the serialiser digiKam writes through) adding the
/// region to a one-keyword sidecar: one `rdf:Description`, simple fields as attributes, the
/// region a nested `rdf:Description` with attribute-form fields.
pub(crate) const EXIV2_O6: &str = include_str!("../../tests/fixtures/faces/regions/exiv2-o6.xmp");

/// The real tools' sidecars, by tool.
pub(crate) const REAL_TOOL_REGIONS: [(&str, &str); 2] = [("exiftool 13.55", EXIFTOOL_O6), ("exiv2 0.28.9", EXIV2_O6)];

/// One MWG region as the independent reader sees it: MWG's center form, unconverted.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MwgRegion {
    pub name: String,
    /// `stArea:x/y/w/h`: the center and the size.
    pub area: (f32, f32, f32, f32),
    /// The ChairPhoto marker (`chairphoto:FaceId`), if the region carries one.
    pub face_id: Option<String>,
}

/// Everything the independent reader takes from a sidecar's MWG regions.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct MwgRead {
    /// Every `mwg-rs:Regions` property's `AppliedToDimensions` as `(w, h)` text.
    pub dims: Vec<Option<(String, String)>>,
    /// Every region of every RegionList, in document order.
    pub regions: Vec<MwgRegion>,
    /// How many `rdf:RDF` elements the document holds (one, for a valid sidecar).
    pub rdf_elements: usize,
}

fn parse(xml: &str) -> roxmltree::Document<'_> {
    roxmltree::Document::parse(xml).unwrap_or_else(|e| panic!("not well-formed XML: {e}\n{xml}"))
}

/// The element that carries a struct value's fields: the element itself (parseType Resource
/// or attribute form), or its one nested `rdf:Description`.
fn struct_body<'a, 'i>(e: roxmltree::Node<'a, 'i>) -> roxmltree::Node<'a, 'i> {
    if e.attribute((NS_RDF, "parseType")) == Some("Resource") {
        return e;
    }
    e.children().find(|c| c.has_tag_name((NS_RDF, "Description"))).unwrap_or(e)
}

/// A struct field's text, as an attribute or a child element.
fn field(body: roxmltree::Node, ns: &str, local: &str) -> Option<String> {
    if let Some(v) = body.attribute((ns, local)) {
        return Some(v.trim().to_string());
    }
    body.children()
        .find(|c| c.has_tag_name((ns, local)))
        .map(|c| c.text().unwrap_or("").trim().to_string())
}

/// Read every MWG region and AppliedToDimensions in `xml`.
pub(crate) fn mwg(xml: &str) -> MwgRead {
    let doc = parse(xml);
    let mut out = MwgRead {
        rdf_elements: doc.descendants().filter(|n| n.has_tag_name((NS_RDF, "RDF"))).count(),
        ..Default::default()
    };
    for regions in doc.descendants().filter(|n| n.has_tag_name((NS_MWG_RS, "Regions"))) {
        let body = struct_body(regions);
        let dims = body.children().find(|c| c.has_tag_name((NS_MWG_RS, "AppliedToDimensions")));
        out.dims.push(dims.map(|d| {
            let d = struct_body(d);
            (field(d, NS_STDIM, "w").unwrap_or_default(), field(d, NS_STDIM, "h").unwrap_or_default())
        }));
        let lis = body
            .children()
            .filter(|c| c.has_tag_name((NS_MWG_RS, "RegionList")))
            .flat_map(|l| l.children().filter(|c| c.is_element()))
            .flat_map(|container| container.children().filter(|c| c.has_tag_name((NS_RDF, "li"))));
        for li in lis {
            let li = struct_body(li);
            let area = li
                .children()
                .find(|c| c.has_tag_name((NS_MWG_RS, "Area")))
                .map(struct_body)
                .expect("a region without an Area");
            let coord = |f| field(area, NS_STAREA, f).and_then(|v| v.parse::<f32>().ok()).expect(f);
            out.regions.push(MwgRegion {
                name: field(li, NS_MWG_RS, "Name").unwrap_or_default(),
                area: (coord("x"), coord("y"), coord("w"), coord("h")),
                face_id: field(li, NS_CHAIRPHOTO, "FaceId"),
            });
        }
    }
    out
}

/// The regions named `name`.
pub(crate) fn named<'a>(read: &'a MwgRead, name: &str) -> Vec<&'a MwgRegion> {
    read.regions.iter().filter(|r| r.name == name).collect()
}

/// A canonical text of every element with expanded name `{ns}local` and its whole subtree —
/// expanded names, namespaced attributes and text, in document order — so a test can say a
/// foreign structure came through a write unchanged without comparing prefixes or layout.
pub(crate) fn subtree(xml: &str, ns: &str, local: &str) -> Vec<String> {
    let doc = parse(xml);
    doc.descendants().filter(|n| n.has_tag_name((ns, local))).map(canonical).collect()
}

/// [`subtree`]'s canonical text of one node.
fn canonical(n: roxmltree::Node) -> String {
    fn walk(n: roxmltree::Node, out: &mut String) {
        if n.is_element() {
            let name = n.tag_name();
            out.push_str(&format!("<{{{}}}{}", name.namespace().unwrap_or(""), name.name()));
            let mut attrs: Vec<String> = n
                .attributes()
                .map(|a| format!(" {{{}}}{}={:?}", a.namespace().unwrap_or(""), a.name(), a.value()))
                .collect();
            attrs.sort();
            out.push_str(&attrs.concat());
            out.push('>');
            for c in n.children() {
                walk(c, out);
            }
            out.push_str("</>");
        } else if let Some(t) = n.text() {
            out.push_str(t.trim());
        }
    }
    let mut s = String::new();
    walk(n, &mut s);
    s
}

/// Every property of every top-level `rdf:Description` but `mwg-rs:Regions` — attribute or
/// element, in [`subtree`]'s canonical text, sorted: all a region write must leave as it was
/// outside the regions (#154's real-tool sidecars, whose properties sit in either form).
pub(crate) fn non_region_properties(xml: &str) -> Vec<String> {
    let doc = parse(xml);
    let mut out = Vec::new();
    for desc in doc.descendants().filter(|n| {
        n.has_tag_name((NS_RDF, "Description")) && n.parent().is_some_and(|p| p.has_tag_name((NS_RDF, "RDF")))
    }) {
        for a in desc.attributes().filter(|a| a.namespace() != Some(NS_RDF)) {
            out.push(format!("{{{}}}{}={:?}", a.namespace().unwrap_or(""), a.name(), a.value()));
        }
        for c in desc.children().filter(|c| c.is_element() && !c.has_tag_name((NS_MWG_RS, "Regions"))) {
            out.push(canonical(c));
        }
    }
    out.sort();
    out
}

/// The value of attribute `{ans}alocal` on every element `{ens}elocal`.
pub(crate) fn attribute_values(xml: &str, el: (&str, &str), attr: (&str, &str)) -> Vec<String> {
    let doc = parse(xml);
    doc.descendants()
        .filter(|n| n.has_tag_name(el))
        .filter_map(|n| n.attribute(attr).map(str::to_string))
        .collect()
}

/// Every foreign structure the fixtures carry besides MWG regions, as [`subtree`] reads them.
pub(crate) fn foreign_structures(xml: &str) -> Vec<Vec<String>> {
    vec![
        subtree(xml, NS_MP, "RegionInfo"),
        subtree(xml, NS_DIGIKAM, "TagsList"),
        attribute_values(xml, (NS_RDF, "Description"), (NS_TIFF, "Orientation")),
        attribute_values(xml, (NS_RDF, "Description"), (super::NS_XMP, "Rating")),
    ]
}

/// The fixtures read as they are meant to, before anything writes them.
#[test]
fn region_fixtures_read_as_written() {
    let digikam = mwg(DIGIKAM);
    assert_eq!(digikam.dims, [Some(("6000".into(), "4000".into()))]);
    assert_eq!(digikam.regions, [MwgRegion { name: "Bob".into(), area: (0.8, 0.7, 0.1, 0.2), face_id: None }]);
    assert_eq!(subtree(DIGIKAM, NS_MPREG, "Rectangle"), Vec::<String>::new(), "an attribute, not an element");
    assert_eq!(attribute_values(DIGIKAM, (NS_RDF, "li"), (NS_MPREG, "PersonDisplayName")), ["Bob"]);
    let lightroom = mwg(LIGHTROOM_ROTATED);
    assert_eq!(lightroom.dims, [Some(("6000".into(), "4000".into()))]);
    assert_eq!(lightroom.regions.len(), 1);
    assert_eq!(attribute_values(LIGHTROOM_ROTATED, (NS_RDF, "Description"), (NS_TIFF, "Orientation")), ["6"]);
    let ms = mwg(MS_PHOTO);
    assert!(ms.regions.is_empty() && ms.dims.is_empty());
    assert_eq!(attribute_values(MS_PHOTO, (NS_RDF, "li"), (NS_MPREG, "PersonDisplayName")), ["Carol"]);
    assert_eq!(subtree(MS_PHOTO, NS_MPRI, "Regions").len(), 1);
    for (layout, xml) in FOREIGN_REGIONS {
        assert_eq!(mwg(xml).rdf_elements, 1, "{layout}");
    }
    for (tool, xml) in REAL_TOOL_REGIONS {
        let read = mwg(xml);
        assert_eq!(read.dims, [Some(("600".into(), "400".into()))], "{tool}");
        assert_eq!(read.regions, [MwgRegion { name: "Bob".into(), area: (0.325, 0.25, 0.15, 0.1), face_id: None }], "{tool}");
        assert_eq!(read.rdf_elements, 1, "{tool}");
    }
}
