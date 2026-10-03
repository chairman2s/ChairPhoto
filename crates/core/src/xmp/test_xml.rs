//! Helpers shared by the xmp unit tests: the test catalog's identity, seeded sidecars, and an
//! independent namespace-aware reader (xml-rs's event reader, not ChairPhoto's parser).

use std::path::{Path, PathBuf};
use crate::xmp::{read_face_regions, sidecar_path, RegionFrame};

/// The writing catalog's identity in these tests (`catalog::CATALOG_UUID_KEY`).
pub(super) const CAT: &str = "0b7c6e2a-1d3f-4e5a-9b8c-7d6e5f4a3b2c";

/// This catalog's marker for face `id`, as the file holds it.
pub(super) fn ours(id: i64) -> String {
    format!("{CAT}/{id}")
}

pub(super) fn read(path: &Path) -> String {
    String::from_utf8(std::fs::read(path).unwrap()).unwrap()
}

/// An upright photo (EXIF Orientation 1) of a known stored size.
pub(super) fn sized(w: u32, h: u32) -> RegionFrame {
    RegionFrame { orientation: Some(1), stored_size: Some((w, h)) }
}

/// Every attribute in `xml`, namespace-resolved by an independent namespace-aware reader
/// (xml-rs's event reader, not ChairPhoto's parser): `(element ns, element local,
/// attr ns, attr local, value)`, `""` meaning "no namespace". Panics if `xml` is not
/// well-formed, which includes using a prefix that is not declared in scope.
pub(super) fn namespaced_attributes(xml: &str) -> Vec<(String, String, String, String, String)> {
    use xml::reader::{EventReader, XmlEvent};
    let mut out = Vec::new();
    for ev in EventReader::new(xml.as_bytes()) {
        if let XmlEvent::StartElement { name, attributes, .. } =
            ev.unwrap_or_else(|e| panic!("sidecar is not well-formed XML: {e}\n{xml}"))
        {
            for a in attributes {
                out.push((
                    name.namespace.clone().unwrap_or_default(),
                    name.local_name.clone(),
                    a.name.namespace.unwrap_or_default(),
                    a.name.local_name,
                    a.value,
                ));
            }
        }
    }
    out
}

pub(super) const NS_FOREIGN: &str = "urn:example:foreign";

/// One element as an independent namespace-aware reader (xml-rs, not ChairPhoto's parser)
/// sees it: its parent's `{ns}local`, its own, and its direct non-blank text.
#[derive(Debug)]
pub(super) struct SeenElement {
    pub(super) parent: (String, String),
    pub(super) name: (String, String),
    pub(super) text: String,
}

pub(super) fn namespaced_elements(xml: &str) -> Vec<SeenElement> {
    use xml::reader::{EventReader, XmlEvent};
    let mut out: Vec<SeenElement> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    for ev in EventReader::new(xml.as_bytes()) {
        match ev.unwrap_or_else(|e| panic!("sidecar is not well-formed XML: {e}\n{xml}")) {
            XmlEvent::StartElement { name, .. } => {
                let parent = open.last().map(|&i| out[i].name.clone()).unwrap_or_default();
                let name = (name.namespace.unwrap_or_default(), name.local_name);
                open.push(out.len());
                out.push(SeenElement { parent, name, text: String::new() });
            }
            XmlEvent::Characters(t) => {
                if let Some(&i) = open.last() {
                    out[i].text.push_str(t.trim());
                }
            }
            XmlEvent::EndElement { .. } => {
                open.pop();
            }
            _ => {}
        }
    }
    out
}

pub(super) fn count_elements(xml: &str, ns: &str, local: &str) -> usize {
    namespaced_elements(xml)
        .iter()
        .filter(|e| e.name.0 == ns && e.name.1 == local)
        .count()
}

/// The text of the one `{ns}local` element whose parent is `{parent_ns}parent`.
pub(super) fn element_text(xml: &str, parent: (&str, &str), name: (&str, &str)) -> Option<String> {
    let seen = namespaced_elements(xml);
    let mut hits = seen.iter().filter(|e| {
        e.parent.0 == parent.0 && e.parent.1 == parent.1 && e.name.0 == name.0
            && e.name.1 == name.1
    });
    let hit = hits.next()?;
    assert!(hits.next().is_none(), "more than one {name:?} under {parent:?}\n{xml}");
    Some(hit.text.clone())
}

pub(super) fn has_attr(xml: &str, el: (&str, &str), attr: (&str, &str), value: &str) -> bool {
    namespaced_attributes(xml).iter().any(|(e_ns, e_l, a_ns, a_l, v)| {
        e_ns == el.0 && e_l == el.1 && a_ns == attr.0 && a_l == attr.1 && v == value
    })
}

pub(super) fn region_names(photo: &Path) -> Vec<String> {
    let mut names: Vec<String> = read_face_regions(photo).into_iter().map(|r| r.name).collect();
    names.sort();
    names
}

pub(super) fn seeded_photo(tag: &str, sidecar: &str) -> (crate::test_support::TestTmpDir, PathBuf) {
    let dir = crate::test_support::TestTmpDir::new(tag);
    let photo = dir.join("DSC.ARW");
    std::fs::write(&photo, b"raw").unwrap();
    std::fs::write(sidecar_path(&photo), sidecar).unwrap();
    (dir, photo)
}
