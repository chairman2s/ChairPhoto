//! Whether two versions of one sidecar differ only in what ChairPhoto writes (#257).
//!
//! Storage carries ChairPhoto's own sidecar rewrite home without asking, and must not do so
//! for another program's edit. A timestamp cannot tell the two apart — a foreign tool keeps
//! `chairphoto:LastWrite` (it preserves unknown namespaces) and `exiftool -P`, `rsync -t` or
//! `touch -r` keep the mtime (review of release/storage, LOW-1, P2) — so the contents decide:
//! with every property a ChairPhoto in-library writer owns taken out, the two versions must be
//! the same document.
//!
//! The documents are compared as trees, not as strings (review relA2, LOW-C): each file is
//! read by its own pass of the XML parser that keeps text exactly — leading and trailing
//! whitespace inside a value, and whitespace-only text against none — dropping only the
//! indentation between elements. Everything outside the first `rdf:RDF` — the `x:xmpmeta`
//! wrapper's attributes, its other children, a second `rdf:RDF` — must be identical too.
//! Comments and processing instructions (`<?xpacket …?>`) do not count.

use super::ns::{NS_CHAIRPHOTO, NS_EXIF, NS_MWG_RS, NS_RDF, NS_XMP};
use std::path::Path;

/// Every top-level property a ChairPhoto in-library sidecar writer replaces: the IPTC fields
/// (`iptc::MANAGED`), GPS, the identifier, the import batch, face regions (the whole
/// `mwg-rs:Regions` — its writer edits inside it, so foreign content inside an owned
/// property counts as ChairPhoto's; documented) and the completion stamp.
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

/// One node of a sidecar as compared: names resolved to their namespace, never a prefix.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Node {
    Element { ns: String, name: String, attrs: Vec<(String, String, String)>, children: Vec<Node> },
    /// A property written in attribute form on an `rdf:Description`.
    Attr(String, String, String),
    Text(String),
}

/// Whether the sidecar files `a` and `b` hold the same document once every property
/// ChairPhoto writes is taken out ([`owned`]). Within the first `rdf:RDF`, the properties of
/// all top-level `rdf:Description`s are compared as one set — ChairPhoto's writer may regroup
/// and re-indent what it does not own, never change it — and the Descriptions' own
/// `rdf:about` does not count. `false` when either cannot be read or parsed: an unknown
/// difference is a difference.
pub fn differs_only_in_chairphoto_fields(a: &Path, b: &Path) -> bool {
    match (foreign_part(a), foreign_part(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// The document at `path` with ChairPhoto's properties taken out of its first `rdf:RDF`.
fn foreign_part(path: &Path) -> Option<Node> {
    let root = parse(std::fs::File::open(path).ok()?)?;
    let mut seen_rdf = false;
    Some(strip_first_rdf(root, &mut seen_rdf))
}

fn strip_first_rdf(node: Node, seen: &mut bool) -> Node {
    match node {
        Node::Element { ns, name, attrs, children } if ns == NS_RDF && name == "RDF" && !*seen => {
            *seen = true;
            let mut props = Vec::new();
            for child in children {
                match child {
                    Node::Element { ns: dns, name: dname, attrs: dattrs, children: dchildren }
                        if dns == NS_RDF && dname == "Description" =>
                    {
                        for (ans, aname, value) in dattrs {
                            if !(ans == NS_RDF && aname == "about") && !owned(&ans, &aname) {
                                props.push(Node::Attr(ans, aname, value));
                            }
                        }
                        for prop in dchildren {
                            match &prop {
                                Node::Element { ns: pns, name: pname, .. } if owned(pns, pname) => {}
                                _ => props.push(prop),
                            }
                        }
                    }
                    other => props.push(other),
                }
            }
            props.sort();
            Node::Element { ns, name, attrs, children: props }
        }
        Node::Element { ns, name, attrs, children } => {
            let children = children.into_iter().map(|c| strip_first_rdf(c, seen)).collect();
            Node::Element { ns, name, attrs, children }
        }
        other => other,
    }
}

/// Parse a sidecar into [`Node`]s, keeping text exactly; whitespace-only text is dropped only
/// where it sits between elements (indentation). Attributes are sorted (their order means
/// nothing in XML); namespace declarations are not attributes here.
fn parse<R: std::io::Read>(r: R) -> Option<Node> {
    use xml::reader::{EventReader, ParserConfig, XmlEvent};
    let config = ParserConfig::new()
        .trim_whitespace(false)
        .whitespace_to_characters(true)
        .cdata_to_characters(true)
        .ignore_comments(true);
    let mut open: Vec<Node> = Vec::new();
    let mut root = None;
    for event in EventReader::new_with_config(r, config) {
        match event.ok()? {
            XmlEvent::StartElement { name, attributes, .. } => {
                let mut attrs: Vec<_> = attributes
                    .into_iter()
                    .map(|a| (a.name.namespace.unwrap_or_default(), a.name.local_name, a.value))
                    .collect();
                attrs.sort();
                open.push(Node::Element {
                    ns: name.namespace.unwrap_or_default(),
                    name: name.local_name,
                    attrs,
                    children: Vec::new(),
                });
            }
            XmlEvent::EndElement { .. } => {
                let mut done = open.pop()?;
                if let Node::Element { children, .. } = &mut done {
                    if children.iter().any(|c| matches!(c, Node::Element { .. })) {
                        children.retain(|c| !matches!(c, Node::Text(t) if t.trim().is_empty()));
                    }
                }
                match open.last_mut() {
                    Some(Node::Element { children, .. }) => children.push(done),
                    Some(_) => return None,
                    None => root = root.or(Some(done)),
                }
            }
            XmlEvent::Characters(text) | XmlEvent::Whitespace(text) | XmlEvent::CData(text) => {
                if let Some(Node::Element { children, .. }) = open.last_mut() {
                    match children.last_mut() {
                        Some(Node::Text(t)) => t.push_str(&text),
                        _ => children.push(Node::Text(text)),
                    }
                }
            }
            XmlEvent::EndDocument => break,
            _ => {}
        }
    }
    root
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

    // ── review relA2, LOW-C ──────────────────────────────────────────────────────────

    /// Every ChairPhoto writer, run over real tools' sidecars, still compares as "only
    /// ChairPhoto's" against the file before — the exact comparison must not flag its own
    /// writer's layout (the noise probe of the relA2 review).
    #[test]
    fn chairphotos_writers_over_foreign_sidecars_differ_only_in_what_they_own() {
        let dir = crate::test_support::TestTmpDir::new("xmp-compare-noise");
        for (tool, xml) in crate::xmp::test_fixtures::FOREIGN {
            let raw = dir.join(format!("{tool}.ARW"));
            std::fs::write(&raw, b"raw").unwrap();
            std::fs::write(crate::xmp::sidecar_path(&raw), xml).unwrap();
            let before = dir.join(format!("{tool}.before.xmp"));
            std::fs::copy(crate::xmp::sidecar_path(&raw), &before).unwrap();
            let fields = crate::catalog::IptcFields { title: "T".into(), city: "Oslo".into(), ..Default::default() };
            crate::xmp::write_iptc(&raw, &Default::default(), &fields).unwrap();
            crate::xmp::write_gps(&raw, 59.9, 10.7).unwrap();
            crate::xmp::write_identifier(&raw, "0b6f8a4e-6f7c-4a59-9c38-111111111111").unwrap();
            crate::xmp::write_import_batch(&raw, "1b6f8a4e-6f7c-4a59-9c38-111111111111").unwrap();
            assert!(
                differs_only_in_chairphoto_fields(&before, &crate::xmp::sidecar_path(&raw)),
                "{tool}: {}",
                std::fs::read_to_string(crate::xmp::sidecar_path(&raw)).unwrap()
            );
        }
    }

    /// The probes the string form got wrong: two structures whose canonical strings collided,
    /// whitespace at the ends of a value, whitespace-only against empty, and everything
    /// outside the first `rdf:RDF`.
    #[test]
    fn structure_whitespace_and_the_wrapper_all_count() {
        let dir = crate::test_support::TestTmpDir::new("xmp-compare-strict");
        let desc = |body: &str| format!(r#"<rdf:Description xmlns:f="http://f/">{body}</rdf:Description>"#);
        let differ = |a: &str, b: &str| {
            let (a, b) = (sidecar(&dir, "x.xmp", &desc(a)), sidecar(&dir, "y.xmp", &desc(b)));
            !differs_only_in_chairphoto_fields(&a, &b)
        };
        assert!(differ(r#"<f:s f:a="1 {http://f/}b=2"/>"#, r#"<f:s f:a="1" f:b="2"/>"#), "attribute collision");
        assert!(differ(r#"<f:a>x),{http://f/}b[](#y</f:a>"#, "<f:a>x</f:a><f:b>y</f:b>"), "children collision");
        assert!(differ("<f:a> x </f:a>", "<f:a>x</f:a>"), "whitespace at a value's ends");
        assert!(differ("<f:a>  </f:a>", "<f:a></f:a>"), "whitespace-only against empty");
        assert!(!differ("<f:a>x</f:a>\n  <f:b>y</f:b>", "<f:a>x</f:a><f:b>y</f:b>"), "indentation does not count");

        let whole = |wrapper_attr: &str, after: &str| {
            format!(
                r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" {wrapper_attr}><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"/>{after}</x:xmpmeta>"#
            )
        };
        let file = |name: &str, xml: String| {
            let p = dir.join(name);
            std::fs::write(&p, xml).unwrap();
            p
        };
        let base = file("base.xmp", whole("", ""));
        assert!(!differs_only_in_chairphoto_fields(&base, &file("tk.xmp", whole(r#"x:xmptk="other""#, ""))));
        assert!(!differs_only_in_chairphoto_fields(&base, &file("sib.xmp", whole("", r#"<f:x xmlns:f="http://f/">1</f:x>"#))));
        let second = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:f="http://f/" f:a="1"/></rdf:RDF>"#;
        let second_b = second.replace(r#"f:a="1""#, r#"f:a="2""#);
        assert!(!differs_only_in_chairphoto_fields(&file("r1.xmp", whole("", second)), &file("r2.xmp", whole("", &second_b))));
        assert!(differs_only_in_chairphoto_fields(&base, &file("same.xmp", whole("", ""))));
    }
}
