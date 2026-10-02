//! Foreign sidecars carrying IPTC/dc values ChairPhoto never imported, and a
//! namespace-aware reader for them (issue #144). Shared by the `write_iptc` unit tests,
//! the IPTC save tests (`app::iptc`) and the geocoder's (`plugins::map::geocode`).
//!
//! Values are read with xml-rs, not ChairPhoto's own parser, and matched by namespace URI:
//! a prefix or a local name alone never counts.

use super::{MANAGED, NS_DC, NS_EXIF, NS_IPTC, NS_PHOTOSHOP, NS_RDF, NS_XMP};

/// Lightroom's shape: one `rdf:Description`, simple properties in compact attribute form,
/// arrays as elements.
pub(crate) const LIGHTROOM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 7.0-c000 1.000000, 0000/00/00-00:00:00">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/"
    xmlns:Iptc4xmpCore="http://iptc.org/std/Iptc4xmpCore/1.0/xmlns/"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    xmp:Rating="3"
    photoshop:Headline="Foreign headline"
    photoshop:City="Bergen"
    photoshop:Instructions="keep"
    Iptc4xmpCore:CountryCode="NO"
    crs:Exposure2012="+0.50">
   <dc:creator>
    <rdf:Seq>
     <rdf:li>Foreign Photographer</rdf:li>
     <rdf:li>Second Shooter</rdf:li>
    </rdf:Seq>
   </dc:creator>
   <dc:rights>
    <rdf:Alt>
     <rdf:li xml:lang="x-default">(c) Foreign</rdf:li>
    </rdf:Alt>
   </dc:rights>
   <dc:description>
    <rdf:Alt>
     <rdf:li xml:lang="x-default">Foreign caption</rdf:li>
    </rdf:Alt>
   </dc:description>
   <dc:title>
    <rdf:Alt>
     <rdf:li xml:lang="x-default">Foreign title</rdf:li>
    </rdf:Alt>
   </dc:title>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

/// exiftool's shape: single quotes, one `rdf:Description` per namespace, so the dc values
/// sit in Description #2 — not the first, where ChairPhoto appends its own (#142, probe P07).
pub(crate) const EXIFTOOL: &str = r#"<?xpacket begin='' id='W5M0MpCehiHzreSzNTczkc9d'?>
<x:xmpmeta xmlns:x='adobe:ns:meta/' x:xmptk='Image::ExifTool 12.76'>
<rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'>
 <rdf:Description rdf:about='' xmlns:exif='http://ns.adobe.com/exif/1.0/'>
  <exif:ExposureTime>1/250</exif:ExposureTime>
 </rdf:Description>
 <rdf:Description rdf:about='' xmlns:dc='http://purl.org/dc/elements/1.1/'>
  <dc:creator>
   <rdf:Seq>
    <rdf:li>Foreign Photographer</rdf:li>
    <rdf:li>Second Shooter</rdf:li>
   </rdf:Seq>
  </dc:creator>
  <dc:description>
   <rdf:Alt>
    <rdf:li xml:lang='x-default'>Foreign caption</rdf:li>
   </rdf:Alt>
  </dc:description>
  <dc:rights>
   <rdf:Alt>
    <rdf:li xml:lang='x-default'>(c) Foreign</rdf:li>
   </rdf:Alt>
  </dc:rights>
  <dc:title>
   <rdf:Alt>
    <rdf:li xml:lang='x-default'>Foreign title</rdf:li>
   </rdf:Alt>
  </dc:title>
 </rdf:Description>
 <rdf:Description rdf:about='' xmlns:photoshop='http://ns.adobe.com/photoshop/1.0/'>
  <photoshop:City>Bergen</photoshop:City>
  <photoshop:Headline>Foreign headline</photoshop:Headline>
  <photoshop:Instructions>keep</photoshop:Instructions>
 </rdf:Description>
 <rdf:Description rdf:about='' xmlns:Iptc4xmpCore='http://iptc.org/std/Iptc4xmpCore/1.0/xmlns/'
  Iptc4xmpCore:CountryCode='NO'/>
 <rdf:Description rdf:about='' xmlns:xmp='http://ns.adobe.com/xap/1.0/'>
  <xmp:Rating>3</xmp:Rating>
 </rdf:Description>
</rdf:RDF>
</x:xmpmeta>
<?xpacket end='w'?>"#;

/// Both foreign layouts, by name.
pub(crate) const FOREIGN: [(&str, &str); 2] = [("lightroom", LIGHTROOM), ("exiftool", EXIFTOOL)];

/// Every value of the property `{ns}local` in the sidecar, in document order: an `rdf:Description`
/// attribute (compact form) is one value; an element is one value made of its descendant
/// texts joined by `|` (so a two-name `dc:creator` Seq reads `A|B`).
pub(crate) fn property_values(xml: &str, ns: &str, local: &str) -> Vec<String> {
    use xml::reader::{EventReader, XmlEvent};
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut open: Option<(usize, Vec<String>)> = None;
    for ev in EventReader::new(xml.as_bytes()) {
        match ev.unwrap_or_else(|e| panic!("sidecar is not well-formed XML: {e}\n{xml}")) {
            XmlEvent::StartElement { name, attributes, .. } => {
                depth += 1;
                if name.namespace.as_deref() == Some(NS_RDF) && name.local_name == "Description" {
                    out.extend(
                        attributes
                            .iter()
                            .filter(|a| a.name.namespace.as_deref() == Some(ns) && a.name.local_name == local)
                            .map(|a| a.value.clone()),
                    );
                }
                if open.is_none() && name.namespace.as_deref() == Some(ns) && name.local_name == local {
                    open = Some((depth, Vec::new()));
                }
            }
            XmlEvent::Characters(t) | XmlEvent::CData(t) => {
                if let Some((_, parts)) = open.as_mut() {
                    if !t.trim().is_empty() {
                        parts.push(t.trim().to_string());
                    }
                }
            }
            XmlEvent::EndElement { .. } => {
                if open.as_ref().is_some_and(|(d, _)| *d == depth) {
                    out.push(open.take().unwrap().1.join("|"));
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    out
}

fn label(ns: &str, local: &str) -> String {
    let prefix = match ns {
        NS_DC => "dc",
        NS_PHOTOSHOP => "photoshop",
        NS_IPTC => "Iptc4xmpCore",
        _ => unreachable!("not a managed namespace: {ns}"),
    };
    format!("{prefix}:{local}")
}

/// Every managed IPTC property in the sidecar with all its values, labelled `prefix:local`.
pub(crate) fn iptc(xml: &str) -> Vec<(String, Vec<String>)> {
    MANAGED.iter().map(|m| (label(m.ns, m.name), property_values(xml, m.ns, m.name))).collect()
}

/// What [`iptc`] reads from either foreign fixture before ChairPhoto writes anything.
pub(crate) fn foreign_iptc() -> Vec<(String, Vec<String>)> {
    let foreign = [
        ("dc:description", "Foreign caption"),
        ("dc:title", "Foreign title"),
        ("dc:rights", "(c) Foreign"),
        ("dc:creator", "Foreign Photographer|Second Shooter"),
        ("photoshop:Headline", "Foreign headline"),
        ("photoshop:City", "Bergen"),
        ("Iptc4xmpCore:CountryCode", "NO"),
    ];
    MANAGED
        .iter()
        .map(|m| {
            let l = label(m.ns, m.name);
            let values = foreign.iter().filter(|(f, _)| *f == l).map(|(_, v)| v.to_string()).collect();
            (l, values)
        })
        .collect()
}

/// `expected` with the property labelled `label` holding exactly `values`.
pub(crate) fn with(mut expected: Vec<(String, Vec<String>)>, label: &str, values: &[&str]) -> Vec<(String, Vec<String>)> {
    let slot = expected.iter_mut().find(|(l, _)| l == label).unwrap_or_else(|| panic!("no managed {label}"));
    slot.1 = values.iter().map(|v| v.to_string()).collect();
    expected
}

/// The non-IPTC foreign values both fixtures carry are all still there, once each.
pub(crate) fn assert_non_iptc_intact(xml: &str, layout: &str) {
    assert_eq!(property_values(xml, NS_XMP, "Rating"), ["3"], "{layout}: xmp:Rating\n{xml}");
    assert_eq!(property_values(xml, NS_PHOTOSHOP, "Instructions"), ["keep"],
        "{layout}: photoshop:Instructions\n{xml}");
    if layout == "exiftool" {
        assert_eq!(property_values(xml, NS_EXIF, "ExposureTime"), ["1/250"], "{layout}\n{xml}");
    } else {
        let crs = "http://ns.adobe.com/camera-raw-settings/1.0/";
        assert_eq!(property_values(xml, crs, "Exposure2012"), ["+0.50"], "{layout}\n{xml}");
    }
}

/// The fixtures read as they are meant to, before anything writes them.
#[test]
fn fixtures_carry_the_foreign_values() {
    for (layout, xml) in FOREIGN {
        assert_eq!(iptc(xml), foreign_iptc(), "{layout}");
        assert_non_iptc_intact(xml, layout);
    }
}
