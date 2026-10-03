//! Parsing a sidecar into a DOM, and namespace-aware attribute lookup.

use xmltree::{Element, XMLNode};

/// Parse a sidecar into an `xmltree` DOM, keeping each prefixed attribute under its
/// **qualified** name (`stArea:x`, `xmp:Rating`, `xml:lang`) — issue #138.
///
/// `xmltree::Element::parse` (0.11) keys attributes by local name only and writes those keys
/// back unprefixed, so every read-modify-write turned `digiKam:Confidence` into a
/// no-namespace `Confidence`. This is the same tree xmltree builds (same parser and config:
/// comments kept, whitespace-only text dropped), except for the attribute key. Every parsed
/// element carries its full in-scope namespace map, and the writer emits a key verbatim. That
/// alone does not keep every name in its namespace on the way back out: xml-rs's emitter skips
/// a declaration an enclosing element already made, even where a nearer element rebinds the
/// prefix in between (#143 item 5), and never checks the prefix it writes. `emit::serialize`
/// therefore runs `emit::fit_prefixes` first, which re-prefixes any element or attribute that
/// would otherwise land in another namespace.
///
/// One loss remains, by design: a nested `xmlns=""` (undeclaring the default namespace) is not
/// written back — xml-rs's emitter never emits an empty default namespace — so an element in
/// no namespace inside a default-namespace scope comes back in that default namespace (#143
/// item 6). Fixing it means replacing the emitter, and the case cannot arise in a valid
/// packet: RDF/XML requires every node and property element to be namespaced.
///
/// Look such attributes up with [`ns_attr`], which matches by namespace URI, not prefix.
pub(super) fn parse_xml<R: std::io::Read>(r: R) -> Result<Element, String> {
    use xml::reader::{EventReader, ParserConfig, XmlEvent};
    let reader =
        EventReader::new_with_config(r, ParserConfig::new().ignore_comments(false));
    let mut open: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    for ev in reader {
        let parent = open.last_mut();
        match ev.map_err(|e| e.to_string())? {
            XmlEvent::StartElement { name, attributes, namespace } => {
                let mut e = Element::new(&name.local_name);
                e.prefix = name.prefix;
                e.namespace = name.namespace;
                e.namespaces = (!namespace.is_essentially_empty()).then_some(namespace);
                for a in attributes {
                    let key = match a.name.prefix {
                        Some(prefix) => format!("{prefix}:{}", a.name.local_name),
                        None => a.name.local_name,
                    };
                    e.attributes.insert(key, a.value);
                }
                open.push(e);
            }
            XmlEvent::EndElement { .. } => {
                let done = open.pop().ok_or("unbalanced end element")?;
                match open.last_mut() {
                    Some(parent) => parent.children.push(XMLNode::Element(done)),
                    None => root = root.or(Some(done)),
                }
            }
            XmlEvent::Characters(s) => {
                if let Some(p) = parent {
                    p.children.push(XMLNode::Text(s));
                }
            }
            XmlEvent::CData(s) => {
                if let Some(p) = parent {
                    p.children.push(XMLNode::CData(s));
                }
            }
            XmlEvent::Comment(s) => {
                if let Some(p) = parent {
                    p.children.push(XMLNode::Comment(s));
                }
            }
            XmlEvent::ProcessingInstruction { name, data } => {
                if let Some(p) = parent {
                    p.children.push(XMLNode::ProcessingInstruction(name, data));
                }
            }
            XmlEvent::EndDocument => break,
            XmlEvent::StartDocument { .. } | XmlEvent::Whitespace(_) => {}
        }
    }
    root.ok_or_else(|| "no root element".to_string())
}

/// True when attribute key `key` (as [`parse_xml`] stores it, `prefix:local`) of element `e`
/// names `{ns}local`, resolving the prefix through `e`'s in-scope namespaces.
pub(super) fn attr_is(e: &Element, key: &str, ns: &str, local: &str) -> bool {
    let Some((prefix, name)) = key.split_once(':') else {
        return false; // an unprefixed attribute is in no namespace
    };
    name == local && e.namespaces.as_ref().and_then(|n| n.get(prefix)) == Some(ns)
}

/// The namespace URI of attribute key `key` on `e` (as [`parse_xml`] stores it), or `None` for
/// an unprefixed attribute or an unbound prefix.
pub(super) fn attr_ns<'a>(e: &'a Element, key: &str) -> Option<&'a str> {
    let (prefix, _) = key.split_once(':')?;
    e.namespaces.as_ref()?.get(prefix)
}

/// The value of attribute `{ns}local` on `e`, whatever prefix the file bound `ns` to.
pub(super) fn ns_attr<'a>(e: &'a Element, ns: &str, local: &str) -> Option<&'a str> {
    e.attributes
        .iter()
        .find(|(k, _)| attr_is(e, k, ns, local))
        .map(|(_, v)| v.as_str())
}
