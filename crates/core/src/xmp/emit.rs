//! Serialising a sidecar DOM: [`serialize`], and the pass before it that makes every element
//! come out in the namespace the DOM gives it (#143).
//!
//! `xmltree` writes an element as `prefix:local` and hands its `namespaces` map to xml-rs's
//! emitter, which declares a mapping only when no enclosing element declared the same one
//! (`NamespaceStack::put_checked`) and never checks the prefix it writes against the element's
//! namespace. An element ChairPhoto builds (`xmp:Identifier`, with no map of its own) therefore
//! lands in whatever namespace the file binds `xmp` to where it is written: a foreign file that
//! binds `xmp` or `exif` to another URI would receive ChairPhoto's properties in *its*
//! namespace. [`fit_prefixes`] replays the emitter's own namespace stack over the tree and, for
//! an element whose prefix would not resolve to its namespace, writes it under a prefix that
//! does — one already in scope, or a fresh one declared on the element.

use xml::namespace::NamespaceStack;
use xmltree::{Element, Namespace, XMLNode};

/// Serialise `root` as a sidecar, after [`fit_prefixes`].
pub(super) fn serialize(root: &mut Element) -> Result<Vec<u8>, String> {
    fit_prefixes(root);
    let mut buf = Vec::new();
    root.write(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

/// Give every namespaced element a prefix that names its namespace where xml-rs's emitter
/// writes it (see the module docs). An element that already resolves correctly — every element
/// of a file without a prefix collision — is left as it is.
pub(super) fn fit_prefixes(root: &mut Element) {
    fit(root, &mut NamespaceStack::empty());
}

fn fit(e: &mut Element, scope: &mut NamespaceStack) {
    // What the emitter does on this element's StartElement (xml-rs `EventWriter::write`).
    scope.push_empty();
    if let Some(ns) = &e.namespaces {
        scope.checked_target().extend(ns);
    }
    if let Some(uri) = e.namespace.clone() {
        if scope.get(e.prefix.as_deref().unwrap_or("")) != Some(uri.as_str()) {
            let preferred = e.prefix.clone().filter(|p| !p.is_empty()).unwrap_or_else(|| "ns".into());
            e.prefix = Some(usable_prefix(e, scope, &uri, &preferred));
        }
    }
    for child in &mut e.children {
        if let XMLNode::Element(c) = child {
            fit(c, scope);
        }
    }
    scope.pop();
}

/// A prefix that names `uri` where `e` is written: one in scope already, else `preferred`
/// (numbered while that is bound in scope), declared on `e` and in `scope`'s frame for `e`.
fn usable_prefix(e: &mut Element, scope: &mut NamespaceStack, uri: &str, preferred: &str) -> String {
    let in_scope = scope.squash();
    if let Some((p, _)) = in_scope
        .iter()
        .find(|(p, u)| *u == uri && !p.is_empty() && !matches!(*p, "xml" | "xmlns"))
    {
        return p.to_string();
    }
    let mut p = preferred.to_string();
    let mut n = 1;
    while scope.get(&p).is_some() {
        p = format!("{preferred}{n}");
        n += 1;
    }
    e.namespaces.get_or_insert_with(Namespace::empty).put(p.clone(), uri);
    scope.put(p.clone(), uri);
    p
}

#[cfg(test)]
mod tests {
    use crate::catalog::IptcFields;
    use crate::xmp::ns::{NS_CHAIRPHOTO, NS_EXIF, NS_PHOTOSHOP, NS_RDF, NS_STAREA, NS_XMP};
    use crate::xmp::test_xml::{
        count_elements, element_text, has_attr, namespaced_elements, read, seeded_photo, sized, CAT, NS_FOREIGN,
    };
    use crate::xmp::{
        read_face_regions, read_gps, read_identifier, sidecar_path, write_face_regions, write_gps,
        write_identifier, write_iptc, FaceRegion,
    };

    // ── prefix collisions (#143 item 2) ─────────────────────────────────────

    const NS_OTHER: &str = "urn:example:other";

    /// A foreign sidecar that binds three prefixes ChairPhoto writes with (`xmp`, `exif`,
    /// `stArea`) to other namespaces, with foreign data under each.
    const COLLIDING: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="urn:example:foreign"
    xmlns:exif="urn:example:other"
    xmlns:stArea="urn:example:foreign"
    xmp:Rating="4">
   <xmp:Identifier>foreign-id</xmp:Identifier>
   <exif:GPSLatitude>not-a-coordinate</exif:GPSLatitude>
   <stArea:x>foreign-x</stArea:x>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

    /// Every writer puts its elements in ChairPhoto's namespaces, under a prefix bound to
    /// them, when the file binds the usual prefix to another URI — never into the foreign
    /// namespace — and the foreign data under those prefixes stays where it was.
    #[test]
    fn writers_never_write_into_a_foreign_namespace_bound_to_their_prefix() {
        let (_dir, photo) = seeded_photo("xmp-143-collide", COLLIDING);
        let uuid = "6f1c1f0e-8f5e-4a51-9a51-3c1b2a0d1431";
        write_identifier(&photo, uuid).unwrap();
        write_gps(&photo, 63.4305, 10.3951).unwrap();
        write_iptc(&photo, &IptcFields::default(), &IptcFields { city: "Trondheim".into(), ..Default::default() })
            .unwrap();
        let ann = FaceRegion { face_id: 1, name: "Ann".into(), bbox: (0.1, 0.2, 0.2, 0.3) };
        write_face_regions(&photo, CAT, &[ann], &[], &[], sized(6000, 4000)).unwrap();

        let xml = read(&sidecar_path(&photo));
        let desc = (NS_RDF, "Description");
        // Ours, in our namespaces (an independent namespace-aware reader).
        assert_eq!(element_text(&xml, desc, (NS_XMP, "Identifier")).as_deref(), Some(uuid), "{xml}");
        assert_eq!(element_text(&xml, desc, (NS_EXIF, "GPSLatitude")).as_deref(), Some("63,25.830000N"), "{xml}");
        assert_eq!(element_text(&xml, desc, (NS_PHOTOSHOP, "City")).as_deref(), Some("Trondheim"), "{xml}");
        assert_eq!(count_elements(&xml, NS_CHAIRPHOTO, "LastWrite"), 1, "{xml}");
        assert_eq!(count_elements(&xml, NS_STAREA, "x"), 1, "{xml}");
        // Theirs, untouched and alone in their namespaces.
        assert_eq!(element_text(&xml, desc, (NS_FOREIGN, "Identifier")).as_deref(), Some("foreign-id"), "{xml}");
        assert_eq!(element_text(&xml, desc, (NS_OTHER, "GPSLatitude")).as_deref(), Some("not-a-coordinate"), "{xml}");
        assert_eq!(element_text(&xml, desc, (NS_FOREIGN, "x")).as_deref(), Some("foreign-x"), "{xml}");
        assert!(has_attr(&xml, desc, (NS_FOREIGN, "Rating"), "4"), "{xml}");
        let foreign: Vec<_> = namespaced_elements(&xml)
            .into_iter()
            .filter(|e| e.name.0 == NS_FOREIGN || e.name.0 == NS_OTHER)
            .map(|e| e.name.1)
            .collect();
        assert_eq!(foreign, ["Identifier", "GPSLatitude", "x"], "{xml}");
        // And ChairPhoto's readers find them.
        assert_eq!(read_identifier(&photo).as_deref(), Some(uuid));
        let (lat, lng) = read_gps(&photo).unwrap();
        assert!((lat - 63.4305).abs() < 1e-6 && (lng - 10.3951).abs() < 1e-6, "{lat},{lng}");
        let names: Vec<String> = read_face_regions(&photo).into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["Ann"], "{xml}");
    }
}
