//! Generic DOM helpers: finding and walking elements, and building the elements and
//! packet skeleton the writers emit.

use xmltree::{Element, Namespace, XMLNode};
use super::ns::{NS_CHAIRPHOTO, NS_DC, NS_EXIF, NS_IPTC, NS_LR, NS_PHOTOSHOP, NS_RDF, NS_X, NS_XMP};

/// First non-blank text anywhere under an element (handles plain text and rdf:li wraps).
pub(super) fn first_text(e: &Element) -> Option<String> {
    for node in &e.children {
        match node {
            XMLNode::Text(t) if !t.trim().is_empty() => return Some(t.trim().to_string()),
            XMLNode::Element(child) => {
                if let Some(t) = first_text(child) {
                    return Some(t);
                }
            }
            _ => {}
        }
    }
    None
}

/// Find a direct child element by (namespace, name).
pub(super) fn child<'a>(parent: &'a Element, ns: &str, name: &str) -> Option<&'a Element> {
    parent.children.iter().find_map(|n| match n {
        XMLNode::Element(e) if e.namespace.as_deref() == Some(ns) && e.name == name => Some(e),
        _ => None,
    })
}

/// Every `{ns}name` property element on every top-level `rdf:Description` under `rdf`, as
/// (Description index in `rdf.children`, property index in that Description's children).
pub(super) fn find_description_properties(rdf: &Element, ns: &str, name: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    for (d, desc) in element_children(rdf) {
        if !is_rdf(desc, "Description") {
            continue;
        }
        for (p, prop) in element_children(desc) {
            if prop.namespace.as_deref() == Some(ns) && prop.name == name {
                found.push((d, p));
            }
        }
    }
    found
}

/// The element children of `parent`, with their index in `parent.children`.
pub(super) fn element_children(parent: &Element) -> impl Iterator<Item = (usize, &Element)> {
    parent.children.iter().enumerate().filter_map(|(i, n)| match n {
        XMLNode::Element(e) => Some((i, e)),
        _ => None,
    })
}

/// The element at `parent.children[i]`; `i` came from [`element_children`] on the same tree.
pub(super) fn element_at(parent: &Element, i: usize) -> &Element {
    match &parent.children[i] {
        XMLNode::Element(e) => e,
        _ => unreachable!("index {i} was taken from an element child"),
    }
}

pub(super) fn element_at_mut(parent: &mut Element, i: usize) -> &mut Element {
    match &mut parent.children[i] {
        XMLNode::Element(e) => e,
        _ => unreachable!("index {i} was taken from an element child"),
    }
}

pub(super) fn is_rdf(e: &Element, name: &str) -> bool {
    e.namespace.as_deref() == Some(NS_RDF) && e.name == name
}

/// True when `e` holds non-whitespace character data (it is a literal, not a struct or array).
pub(super) fn has_text(e: &Element) -> bool {
    e.children.iter().any(|n| matches!(n,
        XMLNode::Text(t) | XMLNode::CData(t) if !t.trim().is_empty()))
}

pub(super) fn node_element(n: &XMLNode) -> Option<&Element> {
    match n {
        XMLNode::Element(e) => Some(e),
        _ => None,
    }
}

// --- element construction helpers ----------------------------------------

pub(super) fn el(prefix: &str, ns: &str, name: &str) -> Element {
    let mut e = Element::new(name);
    e.prefix = Some(prefix.to_string());
    e.namespace = Some(ns.to_string());
    e
}

/// A plain text property: `<prefix:name>value</prefix:name>`.
pub(super) fn plain(prefix: &str, ns: &str, name: &str, value: &str) -> XMLNode {
    let mut e = el(prefix, ns, name);
    e.children.push(XMLNode::Text(value.to_string()));
    XMLNode::Element(e)
}

/// A Lang Alt property (dc namespace): `<dc:name><rdf:Alt><rdf:li xml:lang="x-default">v</rdf:li></rdf:Alt></dc:name>`.
pub(super) fn lang_alt(name: &str, value: &str) -> XMLNode {
    let mut li = el("rdf", NS_RDF, "li");
    li.attributes
        .insert("xml:lang".to_string(), "x-default".to_string());
    li.children.push(XMLNode::Text(value.to_string()));
    let mut alt = el("rdf", NS_RDF, "Alt");
    alt.children.push(XMLNode::Element(li));
    let mut e = el("dc", NS_DC, name);
    e.children.push(XMLNode::Element(alt));
    XMLNode::Element(e)
}

/// An unordered set property: `<prefix:name><rdf:Bag><rdf:li>v</rdf:li>…</rdf:Bag></prefix:name>`.
pub(super) fn bag(prefix: &str, ns: &str, name: &str, items: &[String]) -> XMLNode {
    let mut b = el("rdf", NS_RDF, "Bag");
    for item in items {
        let mut li = el("rdf", NS_RDF, "li");
        li.children.push(XMLNode::Text(item.clone()));
        b.children.push(XMLNode::Element(li));
    }
    let mut e = el(prefix, ns, name);
    e.children.push(XMLNode::Element(b));
    XMLNode::Element(e)
}

/// dc:creator as an ordered sequence. Multiple creators may be separated by `;`.
pub(super) fn seq_creator(value: &str) -> XMLNode {
    let mut seq = el("rdf", NS_RDF, "Seq");
    for name in value.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let mut li = el("rdf", NS_RDF, "li");
        li.children.push(XMLNode::Text(name.to_string()));
        seq.children.push(XMLNode::Element(li));
    }
    let mut e = el("dc", NS_DC, "creator");
    e.children.push(XMLNode::Element(seq));
    XMLNode::Element(e)
}

/// The sidecar's `rdf:RDF`: the root itself when the file has no `x:xmpmeta` wrapper (the XMP
/// spec allows a bare `rdf:RDF`), else the root's `rdf:RDF` child (#147 L4).
pub(super) fn rdf_of(root: &Element) -> Option<&Element> {
    if is_rdf(root, "RDF") {
        return Some(root);
    }
    root.get_child(("RDF", NS_RDF))
}

/// [`rdf_of`] for writing: a wrapper without one gets an `rdf:RDF`, a bare `rdf:RDF` root is
/// used as it is — never a second `rdf:RDF` nested inside the first.
pub(super) fn rdf_of_mut(root: &mut Element) -> &mut Element {
    if is_rdf(root, "RDF") {
        return root;
    }
    child_mut(root, "rdf", NS_RDF, "RDF")
}

/// Whether `root` is an element a sidecar can be rooted at: the `x:xmpmeta` wrapper (or the
/// older `x:xapmeta`), or a bare `rdf:RDF`.
pub(super) fn is_xmp_root(root: &Element) -> bool {
    root.namespace.as_deref() == Some(NS_X) || is_rdf(root, "RDF")
}

/// Find a child element by (namespace, name) or create it, returning a mut ref.
pub(super) fn child_mut<'a>(parent: &'a mut Element, prefix: &str, ns: &str, name: &str) -> &'a mut Element {
    let pos = parent.children.iter().position(|n| {
        matches!(n, XMLNode::Element(e)
            if e.namespace.as_deref() == Some(ns) && e.name == name)
    });
    let idx = match pos {
        Some(i) => i,
        None => {
            parent.children.push(XMLNode::Element(el(prefix, ns, name)));
            parent.children.len() - 1
        }
    };
    match &mut parent.children[idx] {
        XMLNode::Element(e) => e,
        _ => unreachable!(),
    }
}

pub(super) fn declare_namespaces(desc: &mut Element) {
    let mut ns = desc.namespaces.take().unwrap_or_else(Namespace::empty);
    ns.put("rdf", NS_RDF);
    ns.put("dc", NS_DC);
    ns.put("photoshop", NS_PHOTOSHOP);
    ns.put("Iptc4xmpCore", NS_IPTC);
    ns.put("lr", NS_LR);
    ns.put("xmp", NS_XMP);
    ns.put("chairphoto", NS_CHAIRPHOTO);
    ns.put("exif", NS_EXIF);
    desc.namespaces = Some(ns);
}

pub(super) fn new_root() -> Element {
    let mut desc = el("rdf", NS_RDF, "Description");
    desc.attributes.insert("rdf:about".to_string(), String::new());

    let mut rdf = el("rdf", NS_RDF, "RDF");
    let mut rdf_ns = Namespace::empty();
    rdf_ns.put("rdf", NS_RDF);
    rdf.namespaces = Some(rdf_ns);
    rdf.children.push(XMLNode::Element(desc));

    let mut root = el("x", NS_X, "xmpmeta");
    let mut x_ns = Namespace::empty();
    x_ns.put("x", NS_X);
    root.namespaces = Some(x_ns);
    root.children.push(XMLNode::Element(rdf));
    root
}
