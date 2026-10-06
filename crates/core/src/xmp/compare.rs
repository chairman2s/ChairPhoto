//! Whether two versions of one sidecar differ only in what ChairPhoto writes (#257).
//!
//! Storage carries ChairPhoto's own sidecar rewrite home without asking, and must not do so
//! for another program's edit. A timestamp cannot tell the two apart — a foreign tool keeps
//! `chairphoto:LastWrite` (it preserves unknown namespaces) and `exiftool -P`, `rsync -t` or
//! `touch -r` keep the mtime (review of release/storage, LOW-1, P2) — so the contents decide:
//! with every property a ChairPhoto in-library writer owns taken out, the two versions must be
//! the same document.

use super::dom::rdf_of;
use super::ns::{NS_CHAIRPHOTO, NS_EXIF, NS_MWG_RS, NS_RDF, NS_XMP};
use super::parse::attr_ns;
use super::repair::parse_for_read;
use std::path::Path;
use xmltree::{Element, XMLNode};

/// Every top-level property a ChairPhoto in-library sidecar writer replaces: the IPTC fields
/// (`iptc::MANAGED`), GPS, the identifier, the import batch, face regions (the whole
/// `mwg-rs:Regions` — its writer edits inside it) and the completion stamp.
fn owned(ns: &str, name: &str) -> bool {
    super::iptc::MANAGED.iter().any(|m| m.ns == ns && m.name == name)
        || matches!(
            (ns, name),
            (NS_EXIF, "GPSLatitude")
                | (NS_EXIF, "GPSLongitude")
                | (NS_XMP, "Identifier")
                | (NS_CHAIRPHOTO, "ImportBatch")
                | (NS_CHAIRPHOTO, "LastWrite")
                | (NS_MWG_RS, "Regions")
        )
}

/// Whether the sidecar files `a` and `b` hold the same document once every property
/// ChairPhoto writes is taken out ([`owned`]). Compared by namespace and local name, not by
/// prefix; attribute and property order across the top-level `rdf:Description`s, the
/// Descriptions' own `rdf:about`, and blank text do not count — ChairPhoto's writer may
/// regroup and re-indent what it does not own, never change it. `false` when either cannot
/// be read or parsed: an unknown difference is a difference.
pub fn differs_only_in_chairphoto_fields(a: &Path, b: &Path) -> bool {
    match (foreign_part(a), foreign_part(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The sorted canonical forms of every top-level property ChairPhoto does not own.
fn foreign_part(path: &Path) -> Option<Vec<String>> {
    let root = parse_for_read(std::fs::File::open(path).ok()?).ok()?;
    let rdf = rdf_of(&root)?;
    let mut out = Vec::new();
    for node in &rdf.children {
        match node {
            XMLNode::Element(desc) if desc.namespace.as_deref() == Some(NS_RDF) && desc.name == "Description" => {
                for (key, value) in &desc.attributes {
                    let ns = attr_ns(desc, key).unwrap_or("");
                    let local = key.split_once(':').map_or(key.as_str(), |(_, l)| l);
                    if (ns == NS_RDF && local == "about") || owned(ns, local) {
                        continue;
                    }
                    out.push(format!("@{{{ns}}}{local}={value}"));
                }
                for child in &desc.children {
                    match child {
                        XMLNode::Element(e) if owned(e.namespace.as_deref().unwrap_or(""), &e.name) => {}
                        XMLNode::Element(e) => out.push(canonical(e)),
                        XMLNode::Text(t) if !t.trim().is_empty() => out.push(format!("#{}", t.trim())),
                        _ => {}
                    }
                }
            }
            // Anything else under rdf:RDF is foreign and kept as it is.
            XMLNode::Element(e) => out.push(canonical(e)),
            XMLNode::Text(t) if !t.trim().is_empty() => out.push(format!("#{}", t.trim())),
            _ => {}
        }
    }
    out.sort();
    Some(out)
}

/// An element as `{ns}name[sorted attributes](children in order)`, prefixes resolved and
/// blank text dropped.
fn canonical(e: &Element) -> String {
    let mut attrs: Vec<String> = e
        .attributes
        .iter()
        .map(|(key, value)| {
            let ns = attr_ns(e, key).unwrap_or("");
            let local = key.split_once(':').map_or(key.as_str(), |(_, l)| l);
            format!("{{{ns}}}{local}={value}")
        })
        .collect();
    attrs.sort();
    let children: Vec<String> = e
        .children
        .iter()
        .filter_map(|n| match n {
            XMLNode::Element(c) => Some(canonical(c)),
            XMLNode::Text(t) | XMLNode::CData(t) if !t.trim().is_empty() => Some(format!("#{}", t.trim())),
            _ => None,
        })
        .collect();
    format!("{{{}}}{}[{}]({})", e.namespace.as_deref().unwrap_or(""), e.name, attrs.join(" "), children.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sidecar(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!(
                r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">{body}</rdf:RDF></x:xmpmeta>"#
            ),
        )
        .unwrap();
        path
    }

    /// ChairPhoto's writers (IPTC, GPS, identifier, stamp) change nothing that counts; a
    /// darktable history entry, another prefix for the same namespace, regrouped
    /// Descriptions and re-indentation are told apart correctly.
    #[test]
    fn only_what_chairphoto_owns_may_differ() {
        let dir = crate::test_support::TestTmpDir::new("xmp-compare");
        let raw = dir.join("DSC1.ARW");
        std::fs::write(&raw, b"raw").unwrap();
        std::fs::write(
            crate::xmp::sidecar_path(&raw),
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/" darktable:history_end="3"><darktable:history><rdf:Seq><rdf:li>exposure</rdf:li></rdf:Seq></darktable:history></rdf:Description></rdf:RDF></x:xmpmeta>"#,
        )
        .unwrap();
        let home = dir.join("home.xmp");
        std::fs::copy(crate::xmp::sidecar_path(&raw), &home).unwrap();
        let local = crate::xmp::sidecar_path(&raw);

        crate::xmp::write_gps(&raw, 59.9, 10.7).unwrap();
        crate::xmp::write_identifier(&raw, "0b6f8a4e-6f7c-4a59-9c38-111111111111").unwrap();
        assert!(differs_only_in_chairphoto_fields(&home, &local), "ChairPhoto's own writes");

        let xml = std::fs::read_to_string(&local).unwrap();
        std::fs::write(&local, xml.replace("exposure", "exposure, crop")).unwrap();
        assert!(!differs_only_in_chairphoto_fields(&home, &local), "a darktable edit");

        let a = sidecar(&dir, "a.xmp", r#"<rdf:Description xmlns:dt="http://darktable.sf.net/" dt:auto="1"/>"#);
        let b = sidecar(
            &dir,
            "b.xmp",
            "<rdf:Description xmlns:darktable=\"http://darktable.sf.net/\"\n   darktable:auto=\"1\"/>\n<rdf:Description/>",
        );
        assert!(differs_only_in_chairphoto_fields(&a, &b), "prefix, layout and an empty Description");
        let c = sidecar(&dir, "c.xmp", r#"<rdf:Description xmlns:dt="http://darktable.sf.net/" dt:auto="2"/>"#);
        assert!(!differs_only_in_chairphoto_fields(&a, &c));
        assert!(!differs_only_in_chairphoto_fields(&a, &dir.join("absent.xmp")), "unreadable is a difference");
    }
}
