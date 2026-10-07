//! MWG region element construction and parsing: the `Regions` / `AppliedToDimensions` /
//! `RegionList` structures, the struct forms other tools write them in, and one region `rdf:li`.

use xmltree::{Element, Namespace, XMLNode};
use crate::xmp::dom::{
    child, el, element_at, element_at_mut, element_children, first_text, has_text, is_rdf, plain,
    declare_prefix, prefix_for_ns,
};
use crate::xmp::ns::{NS_MWG_RS, NS_RDF, NS_STAREA, NS_STDIM};
use crate::xmp::parse::{attr_is, attr_ns, ns_attr};
use super::{FaceRegion, ReadRegion};

/// The prefixes the region elements chairphoto builds are written with, declared on the
/// Description that holds (or will hold) `mwg-rs:Regions`. A prefix the file already binds is
/// left alone; where it binds it to another URI another prefix is used ([`declare_prefix`]).
pub(super) fn declare_region_namespaces(desc: &mut Element) {
    let ns = desc.namespaces.get_or_insert_with(Namespace::empty);
    for (prefix, uri) in [("rdf", NS_RDF), ("mwg-rs", NS_MWG_RS), ("stArea", NS_STAREA), ("stDim", NS_STDIM)] {
        declare_prefix(ns, prefix, uri);
    }
}

/// A new `mwg-rs:Regions` element with AppliedToDimensions (when the size is known) + a
/// RegionList Bag of `lis`, for a sidecar that has none.
pub(super) fn new_regions(dims: Option<(u32, u32)>, lis: Vec<XMLNode>) -> Element {
    let mut regions = el("mwg-rs", NS_MWG_RS, "Regions");
    regions
        .attributes
        .insert("rdf:parseType".to_string(), "Resource".to_string());
    if let Some((w, h)) = dims {
        regions.children.push(XMLNode::Element(new_dimensions(w, h)));
    }
    regions.children.push(XMLNode::Element(new_region_list(lis)));
    regions
}

/// AppliedToDimensions as an rdf:parseType="Resource" struct: stDim:w / stDim:h / stDim:unit.
pub(super) fn new_dimensions(w: u32, h: u32) -> Element {
    let mut dims = el("mwg-rs", NS_MWG_RS, "AppliedToDimensions");
    dims.attributes
        .insert("rdf:parseType".to_string(), "Resource".to_string());
    for (field, value) in dimension_fields(w, h) {
        dims.children.push(plain("stDim", NS_STDIM, field, &value));
    }
    dims
}

fn dimension_fields(w: u32, h: u32) -> [(&'static str, String); 3] {
    [("w", w.to_string()), ("h", h.to_string()), ("unit", "pixel".to_string())]
}

pub(super) fn new_region_list(lis: Vec<XMLNode>) -> Element {
    let mut bag = el("rdf", NS_RDF, "Bag");
    bag.children.extend(lis);
    let mut region_list = el("mwg-rs", NS_MWG_RS, "RegionList");
    region_list.children.push(XMLNode::Element(bag));
    region_list
}

/// Where the parts of an existing `mwg-rs:Regions` the writer edits sit, as child indices into
/// the Regions struct's body (see [`struct_body`]). Built by [`regions_layout`], which is
/// also the check that the Regions is one chairphoto recognises.
pub(super) struct RegionsLayout {
    /// `mwg-rs:AppliedToDimensions`, if present.
    pub(super) dims: Option<usize>,
    /// `mwg-rs:RegionList`, if present, and its container (`rdf:Bag` / `rdf:Seq`) within it.
    pub(super) list: Option<(usize, usize)>,
}

/// Check that `regions` is laid out the way chairphoto can edit without losing anything, and
/// say where its parts are. `Err` names what was not recognised.
pub(super) fn regions_layout(regions: &Element) -> Result<RegionsLayout, String> {
    let body = match struct_form(regions) {
        Some(StructForm::Resource | StructForm::Nested(_)) => {
            struct_body(regions).expect("form checked")
        }
        Some(StructForm::Attributes) => return Err("mwg-rs:Regions is in attribute form".into()),
        None => return Err("mwg-rs:Regions is not a struct".into()),
    };
    if body
        .attributes
        .keys()
        .any(|k| attr_is(body, k, NS_MWG_RS, "RegionList")
            || attr_is(body, k, NS_MWG_RS, "AppliedToDimensions"))
    {
        return Err("mwg-rs:Regions carries a list or dimensions as an attribute".into());
    }
    let mut layout = RegionsLayout { dims: None, list: None };
    for (i, node) in body.children.iter().enumerate() {
        let XMLNode::Element(e) = node else { continue };
        if e.namespace.as_deref() != Some(NS_MWG_RS) {
            continue;
        }
        match e.name.as_str() {
            "AppliedToDimensions" => {
                if layout.dims.is_some() {
                    return Err("mwg-rs:Regions has two AppliedToDimensions".into());
                }
                if struct_form(e).is_none() {
                    return Err("its AppliedToDimensions is not a struct".into());
                }
                layout.dims = Some(i);
            }
            "RegionList" => {
                if layout.list.is_some() {
                    return Err("mwg-rs:Regions has two RegionLists".into());
                }
                layout.list = Some((i, region_container(e)?));
            }
            _ => {}
        }
    }
    Ok(layout)
}

/// The index of the `rdf:Bag` (or `rdf:Seq`) a `mwg-rs:RegionList` holds its regions in. Any
/// other shape (no container, another container, a container next to other content) is `Err`.
fn region_container(list: &Element) -> Result<usize, String> {
    if has_text(list) || list.attributes.keys().any(|k| attr_ns(list, k) == Some(NS_RDF)) {
        return Err("its RegionList is not an rdf:Bag".into());
    }
    let elements: Vec<(usize, &Element)> = element_children(list).collect();
    match elements.as_slice() {
        [(i, c)] if is_rdf(c, "Bag") || is_rdf(c, "Seq") => Ok(*i),
        [(_, c)] => Err(format!("its RegionList holds {{{}}}{}, not an rdf:Bag",
            c.namespace.as_deref().unwrap_or(""), c.name)),
        [] => Err("its RegionList holds no rdf:Bag".into()),
        _ => Err("its RegionList holds more than one element".into()),
    }
}

/// How a struct-valued property element (`Regions`, `AppliedToDimensions`, a region `rdf:li`,
/// an `Area`) carries its fields in RDF/XML.
#[derive(Debug, Clone, Copy, PartialEq)]
enum StructForm {
    /// `rdf:parseType="Resource"`: the fields are the element's own children.
    Resource,
    /// The fields are attributes on the element itself, which has no children.
    Attributes,
    /// The fields are on the element's one child, an `rdf:Description` (at this child index).
    Nested(usize),
}

/// How `prop` carries a struct, or `None` if it does not carry one in a form chairphoto reads
/// (text, an `rdf:resource` reference, another parseType, several node elements, …).
fn struct_form(prop: &Element) -> Option<StructForm> {
    if has_text(prop) {
        return None;
    }
    match ns_attr(prop, NS_RDF, "parseType") {
        Some("Resource") => return Some(StructForm::Resource),
        Some(_) => return None,
        None => {}
    }
    let mut rdf_attrs = prop.attributes.keys().filter(|k| attr_ns(prop, k) == Some(NS_RDF));
    if rdf_attrs.next().is_some() {
        return None; // rdf:resource, rdf:nodeID, rdf:datatype: not an inline struct
    }
    let elements: Vec<(usize, &Element)> = element_children(prop).collect();
    match elements.as_slice() {
        [] if prop.attributes.keys().any(|k| attr_ns(prop, k).is_some()) => {
            Some(StructForm::Attributes)
        }
        [(i, d)] if is_rdf(d, "Description") => Some(StructForm::Nested(*i)),
        _ => None,
    }
}

/// The element that holds `prop`'s struct fields: `prop` itself, or its nested Description.
pub(super) fn struct_body(prop: &Element) -> Option<&Element> {
    match struct_form(prop)? {
        StructForm::Resource | StructForm::Attributes => Some(prop),
        StructForm::Nested(i) => Some(element_at(prop, i)),
    }
}

pub(super) fn struct_body_mut(prop: &mut Element) -> Option<&mut Element> {
    match struct_form(prop)? {
        StructForm::Resource | StructForm::Attributes => Some(prop),
        StructForm::Nested(i) => Some(element_at_mut(prop, i)),
    }
}

/// A struct field's text, whether written as an attribute or as a child element.
pub(super) fn struct_field(body: &Element, ns: &str, local: &str) -> Option<String> {
    if let Some(v) = ns_attr(body, ns, local) {
        if !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    child(body, ns, local).and_then(first_text)
}

/// Set `fields` of the struct `prop` carries, in place and in the form each is already written
/// in (attribute or element). A field it lacks is added in the struct's own form. Every other
/// attribute and child is kept. `prop` must be a struct ([`struct_form`] is `Some`).
pub(super) fn set_struct_fields(prop: &mut Element, ns: &str, prefix: &str, fields: &[(&str, String)]) {
    let form = struct_form(prop).expect("caller checked the struct form");
    let body = struct_body_mut(prop).expect("caller checked the struct form");
    for (local, value) in fields {
        let mut found = false;
        let keys: Vec<String> = body
            .attributes
            .keys()
            .filter(|k| attr_is(body, k, ns, local))
            .cloned()
            .collect();
        for key in keys {
            body.attributes.insert(key, value.clone());
            found = true;
        }
        for node in &mut body.children {
            if let XMLNode::Element(e) = node {
                if e.namespace.as_deref() == Some(ns) && e.name == *local {
                    e.children = vec![XMLNode::Text(value.clone())];
                    found = true;
                }
            }
        }
        if !found {
            let p = prefix_for(body, ns, prefix);
            if form == StructForm::Attributes {
                body.attributes.insert(format!("{p}:{local}"), value.clone());
            } else {
                body.children.push(plain(&p, ns, local, value));
            }
        }
    }
}

/// A prefix that names `ns` where `e` is written: one `e`'s in-scope namespaces already bind to
/// it, else `preferred` (or `preferred` plus a number, if that prefix means something else
/// there), declared on `e`.
fn prefix_for(e: &mut Element, ns: &str, preferred: &str) -> String {
    prefix_for_ns(e.namespaces.get_or_insert_with(Namespace::empty), ns, preferred)
}

/// Build one `rdf:li` region struct for a face: Name + Type=Face + center-form Area.
pub(super) fn region_li(r: &FaceRegion) -> XMLNode {
    let (x, y, w, h) = r.bbox;
    // Convert stored top-left (x,y = corner) → MWG center (cx,cy = center).
    let cx = x + w / 2.0;
    let cy = y + h / 2.0;

    let mut area = el("mwg-rs", NS_MWG_RS, "Area");
    area.attributes
        .insert("rdf:parseType".to_string(), "Resource".to_string());
    area.children.push(plain("stArea", NS_STAREA, "x", &fmt_coord(cx)));
    area.children.push(plain("stArea", NS_STAREA, "y", &fmt_coord(cy)));
    area.children.push(plain("stArea", NS_STAREA, "w", &fmt_coord(w)));
    area.children.push(plain("stArea", NS_STAREA, "h", &fmt_coord(h)));
    area.children
        .push(plain("stArea", NS_STAREA, "unit", "normalized"));

    let mut li = el("rdf", NS_RDF, "li");
    li.attributes
        .insert("rdf:parseType".to_string(), "Resource".to_string());
    li.children.push(plain("mwg-rs", NS_MWG_RS, "Name", &r.name));
    li.children.push(plain("mwg-rs", NS_MWG_RS, "Type", "Face"));
    li.children.push(XMLNode::Element(area));
    XMLNode::Element(li)
}

/// Move a matched region to `r`'s geometry, in place (#140). ChairPhoto owns only the Area's
/// `stArea:x/y/w/h/unit`: the Name already equals `r.name` (that is how it matched), and its
/// Type, any other field (`mwg-rs:Rotation`, extensions) and every foreign attribute of the
/// region or its Area (`digiKam:Confidence`, …) are kept. `li` parsed, so its struct and Area
/// forms are ones [`set_struct_fields`] can edit.
pub(super) fn set_region_area(li: &mut Element, r: &FaceRegion) {
    let (x, y, w, h) = r.bbox;
    // Convert stored top-left (x,y = corner) → MWG center (cx,cy = center).
    let fields = [
        ("x", fmt_coord(x + w / 2.0)),
        ("y", fmt_coord(y + h / 2.0)),
        ("w", fmt_coord(w)),
        ("h", fmt_coord(h)),
        ("unit", "normalized".to_string()),
    ];
    let body = struct_body_mut(li).expect("the region parsed");
    let area = body
        .children
        .iter_mut()
        .find_map(|n| match n {
            XMLNode::Element(e) if e.namespace.as_deref() == Some(NS_MWG_RS) && e.name == "Area" => {
                Some(e)
            }
            _ => None,
        })
        .expect("the region parsed, so it has an Area");
    set_struct_fields(area, NS_STAREA, "stArea", &fields);
}

/// Parse one region `rdf:li` into a [`ReadRegion`] (Name + top-left bbox from the center Area).
/// Returns `None` if the Area is missing or its coordinates are unparseable.
pub(super) fn parse_region_li(li: &Element) -> Option<ReadRegion> {
    let body = struct_body(li)?;
    let name = struct_field(body, NS_MWG_RS, "Name").unwrap_or_default();
    let area = struct_body(child(body, NS_MWG_RS, "Area")?)?;
    let coord = |field| struct_field(area, NS_STAREA, field)?.parse::<f32>().ok();
    let cx = coord("x")?;
    let cy = coord("y")?;
    let w = coord("w")?;
    let h = coord("h")?;
    // MWG stores the CENTER; convert to top-left corner.
    let x = cx - w / 2.0;
    let y = cy - h / 2.0;
    Some(ReadRegion {
        name,
        bbox: (x, y, w, h),
    })
}

/// Format a normalized coordinate compactly (trim trailing zeros; keep it deterministic).
fn fmt_coord(v: f32) -> String {
    // Six decimals is plenty for pixel-accurate regions and matches the GPS formatting style.
    let s = format!("{v:.6}");
    // Trim trailing zeros and a dangling dot for tidiness (0.500000 → 0.5).
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}
