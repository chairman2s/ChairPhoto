//! Editing an existing `mwg-rs:Regions` in place: who owns each region (the
//! `chairphoto:FaceId` marker, #135), which incoming face claims it, and the pre-marker shape.

use xmltree::{Element, Namespace, XMLNode};
use crate::xmp::dom::{element_at_mut, element_children, is_rdf, plain};
use crate::xmp::ns::{NS_CHAIRPHOTO, NS_MWG_RS, NS_RDF, NS_STAREA};
use crate::xmp::parse::ns_attr;
use super::mwg::{
    new_dimensions, new_region_list, parse_region_li, region_li, regions_layout, set_region_area,
    set_struct_fields, struct_body, struct_body_mut, struct_field,
};
use super::{AREA_EPSILON, FaceRegion, ReadRegion};

/// Edit an existing, recognised `mwg-rs:Regions` in place, by the rules [`write_face_regions`](super::write_face_regions)
/// documents: claim each existing region for at most one incoming face (its marker first,
/// then a marked region's Name + Area, then an unmarked region on the `legacy` record, then an
/// unmarked region's Name + Area), move what was claimed, remove the marked regions nothing
/// claimed and the legacy ones whose face is gone, and append the faces that claimed nothing,
/// marked. When something is written, `dims` becomes its AppliedToDimensions if it has none
/// (one it has is never rewritten, #145). `incoming` is already in the frame the Regions
/// declares ([`region_target`](super::frame::region_target)); `legacy` is in the display frame its writes used. Everything
/// else on Regions, AppliedToDimensions, RegionList and its container is kept.
pub(super) fn update_regions(
    regions: &mut Element,
    catalog: &str,
    incoming: &[FaceRegion],
    retired: &[i64],
    legacy: &[FaceRegion],
    dims: Option<(u32, u32)>,
) -> Result<(), String> {
    let layout = regions_layout(regions)?;
    let body = struct_body_mut(regions).expect("regions_layout checked the form");
    match layout.list {
        Some((l, c)) => {
            let container = element_at_mut(element_at_mut(body, l), c);
            reconcile_regions(container, catalog, incoming, retired, legacy);
        }
        None if incoming.is_empty() => {}
        None => {
            let lis = incoming.iter().map(|r| marked_li(r, catalog)).collect();
            body.children.push(XMLNode::Element(new_region_list(lis)));
        }
    }
    // Last: inserting moves the indices `layout` recorded.
    if let (None, Some((w, h)), false) = (layout.dims, dims, incoming.is_empty()) {
        body.children.insert(0, XMLNode::Element(new_dimensions(w, h)));
    }
    Ok(())
}

/// One `rdf:li` of a RegionList as the reconciliation sees it.
struct ExistingRegion {
    /// Its index in the container's children.
    node: usize,
    /// Who wrote it, by its `chairphoto:FaceId`.
    owner: Owner,
    /// Whether it has exactly the shape the pre-marker writer gave its regions
    /// ([`is_pre_marker_shape`]): only such an unmarked region can be one it wrote.
    pre_marker_shape: bool,
    /// Its Name and box, in the file's frame; `None` when it does not parse.
    region: Option<ReadRegion>,
}

/// Who an existing region belongs to, by its `chairphoto:FaceId` marker (#135).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Owner {
    /// No marker: another tool's, or written by ChairPhoto before the marker existed.
    Unmarked,
    /// This catalog's marker, for this face id.
    Ours(i64),
    /// A marker of another catalog, or one this build does not recognise: foreign.
    Other,
}

/// The `chairphoto:FaceId` value ChairPhoto writes for face `face_id` of catalog `catalog`:
/// `<catalog>/<face id>`, the catalog's UUID and the face's decimal id. A stable on-disk format
/// (docs/face-tagging.md): reading it is [`marker_owner`].
pub(super) fn face_marker(catalog: &str, face_id: i64) -> String {
    format!("{catalog}/{face_id}")
}

/// Whose marker `marker` is, read from catalog `catalog`'s side: [`Owner::Ours`] only for
/// exactly `<catalog>/<decimal id>` in canonical form (`/007`, `/+7`, `/-7` are not).
fn marker_owner(catalog: &str, marker: Option<&str>) -> Owner {
    let Some(marker) = marker else { return Owner::Unmarked };
    match marker.split_once('/') {
        Some((c, id)) if c == catalog => match id.parse::<i64>() {
            // Exactly the form `face_marker` writes: decimal digits, no sign, no leading zero.
            Ok(n) if n >= 0 && n.to_string() == id => Owner::Ours(n),
            _ => Owner::Other,
        },
        _ => Owner::Other,
    }
}

/// What becomes of an existing region.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Claim {
    /// Left as it is.
    Keep,
    /// Ours (marked, or adopted from the legacy record): moved to this incoming face, renamed
    /// to it and marked with its id.
    Ours(usize),
    /// Foreign, the same face as this incoming one: only its Area moves (#140).
    Foreign(usize),
    /// Ours, and no face in the set claims it: removed.
    Remove,
}

/// The heart of [`update_regions`] for a container (`rdf:Bag` / `rdf:Seq`) of regions.
fn reconcile_regions(
    container: &mut Element,
    catalog: &str,
    incoming: &[FaceRegion],
    retired: &[i64],
    legacy: &[FaceRegion],
) {
    // Ours only for a face this catalog knows on this photo: a marker with another id came
    // from a copy of this catalog's file (review N1) and is foreign.
    let known = |id: i64| retired.contains(&id) || incoming.iter().any(|r| r.face_id == id);
    let existing: Vec<ExistingRegion> = element_children(container)
        .filter(|(_, li)| is_rdf(li, "li"))
        .map(|(node, li)| {
            let marker = struct_body(li).and_then(|b| struct_field(b, NS_CHAIRPHOTO, "FaceId"));
            let owner = match marker_owner(catalog, marker.as_deref()) {
                Owner::Ours(id) if !known(id) => Owner::Other,
                owner => owner,
            };
            ExistingRegion {
                node,
                owner,
                pre_marker_shape: is_pre_marker_shape(li),
                region: parse_region_li(li),
            }
        })
        .collect();
    let mut claims = vec![Claim::Keep; existing.len()];
    let mut written = vec![false; incoming.len()];
    let ids: Vec<i64> = incoming.iter().map(|r| r.face_id).collect();

    // 1. A region of ours whose face id is an incoming face's, and that is still recognisably
    //    that face (its Name, or its place): a copied catalog file shares its identity with
    //    the original, so the id alone could name another face.
    for (i, e) in existing.iter().enumerate() {
        let Owner::Ours(id) = e.owner else { continue };
        let k = incoming.iter().enumerate().position(|(k, r)| {
            !written[k]
                && r.face_id == id
                && e.region.as_ref().is_some_and(|p| p.name == r.name || center_close(p.bbox, r.bbox))
        });
        if let Some(k) = k {
            written[k] = true;
            claims[i] = Claim::Ours(k);
        }
    }
    // 2. A region of ours, written for a face id that has changed, that is the same face.
    let free = |claims: &[Claim]| claims.iter().map(|c| *c == Claim::Keep).collect::<Vec<_>>();
    let unwritten = |written: &[bool]| written.iter().map(|w| !w).collect::<Vec<_>>();
    let pairs = closest_pairs(&existing, incoming, &free(&claims), &unwritten(&written), |e, p, r| {
        matches!(e.owner, Owner::Ours(_)) && same_face(p, r)
    });
    for (i, k) in pairs {
        written[k] = true;
        claims[i] = Claim::Ours(k);
    }
    // 3. An unmarked region a pre-marker ChairPhoto wrote for a face still in the set: adopted.
    //    Matched against the record (the box the old writer wrote, in its frame), then
    //    claimed for the record's face.
    let mut legacy_free = vec![true; legacy.len()];
    let in_set: Vec<bool> = legacy
        .iter()
        .map(|l| incoming.iter().zip(&written).any(|(r, w)| !w && r.face_id == l.face_id))
        .collect();
    let pairs = closest_pairs(&existing, legacy, &free(&claims), &in_set, |e, p, l| {
        e.owner == Owner::Unmarked && e.pre_marker_shape && same_face(p, l)
    });
    for (i, j) in pairs {
        let k = incoming
            .iter()
            .zip(&written)
            .position(|(r, w)| !w && r.face_id == legacy[j].face_id)
            .expect("in_set found it unwritten");
        legacy_free[j] = false;
        written[k] = true;
        claims[i] = Claim::Ours(k);
    }
    // 4. A foreign region (unmarked, or another catalog's) that is the same face as one being
    //    written: it already holds it.
    let pairs = closest_pairs(&existing, incoming, &free(&claims), &unwritten(&written), |e, p, r| {
        !matches!(e.owner, Owner::Ours(_)) && same_face(p, r)
    });
    for (i, k) in pairs {
        written[k] = true;
        claims[i] = Claim::Foreign(k);
    }
    // What nothing claimed: a region of ours whose face has left the set is stale; one whose
    // face is still in it but no longer recognisably that face (step 1's guard) is kept. An
    // unmarked one is removed only when it matches a legacy export whose face has left the
    // set. Another catalog's is kept.
    for (i, e) in existing.iter().enumerate() {
        if let (Claim::Keep, Owner::Ours(id)) = (claims[i], e.owner) {
            if retired.contains(&id) {
                claims[i] = Claim::Remove;
            }
        }
    }
    let retired: Vec<bool> =
        legacy.iter().zip(&legacy_free).map(|(l, free)| *free && !ids.contains(&l.face_id)).collect();
    let pairs = closest_pairs(&existing, legacy, &free(&claims), &retired, |e, p, l| {
        e.owner == Owner::Unmarked && e.pre_marker_shape && same_face(p, l)
    });
    for (i, _) in pairs {
        claims[i] = Claim::Remove;
    }

    for (e, claim) in existing.iter().zip(&claims) {
        let XMLNode::Element(li) = &mut container.children[e.node] else { unreachable!() };
        match *claim {
            Claim::Ours(k) => mark_region(li, &incoming[k], catalog),
            Claim::Foreign(k) => set_region_area(li, &incoming[k]),
            Claim::Keep | Claim::Remove => {}
        }
    }
    let doomed: Vec<usize> = existing
        .iter()
        .zip(&claims)
        .filter(|(_, c)| **c == Claim::Remove)
        .map(|(e, _)| e.node)
        .collect();
    for node in doomed.into_iter().rev() {
        container.children.remove(node);
    }
    let new = incoming.iter().zip(&written).filter(|(_, w)| !**w);
    container.children.extend(new.map(|(r, _)| marked_li(r, catalog)));
}

/// Whether the existing region `p` is the same face as `r`: its Name matches AND its center is
/// within [`AREA_EPSILON`] of `r`'s.
fn same_face(p: &ReadRegion, r: &FaceRegion) -> bool {
    r.name == p.name && center_close(r.bbox, p.bbox)
}

/// Pair existing regions with candidates one-to-one, **closest first** (#147 L2): every pair
/// `(existing, candidate)` that `admits`, ordered by the distance between their centers (ties
/// by document order, then candidate order), taken greedily while both sides are free. The
/// first region in document order is not preferred over a closer one, so a foreign region
/// listed before ours is not the one moved. Each candidate claims at most one region and each
/// region at most one candidate (#147 L1).
fn closest_pairs(
    existing: &[ExistingRegion],
    candidates: &[FaceRegion],
    existing_free: &[bool],
    candidate_free: &[bool],
    admits: impl Fn(&ExistingRegion, &ReadRegion, &FaceRegion) -> bool,
) -> Vec<(usize, usize)> {
    let mut pairs: Vec<(f32, usize, usize)> = Vec::new();
    for (i, e) in existing.iter().enumerate() {
        let Some(p) = e.region.as_ref().filter(|_| existing_free[i]) else { continue };
        for (k, r) in candidates.iter().enumerate() {
            if candidate_free[k] && admits(e, p, r) {
                pairs.push((center_distance(p.bbox, r.bbox), i, k));
            }
        }
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let (mut e_used, mut c_used) = (vec![false; existing.len()], vec![false; candidates.len()]);
    let mut out = Vec::new();
    for (_, i, k) in pairs {
        if !e_used[i] && !c_used[k] {
            e_used[i] = true;
            c_used[k] = true;
            out.push((i, k));
        }
    }
    out
}

/// The distance between two top-left bboxes' centers.
fn center_distance(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
    let dx = (a.0 + a.2 / 2.0) - (b.0 + b.2 / 2.0);
    let dy = (a.1 + a.3 / 2.0) - (b.1 + b.3 / 2.0);
    dx.hypot(dy)
}

/// Whether `li` has exactly the shape of a region the pre-marker writer wrote — its
/// [`region_li`] output, unchanged from the first public release until the marker (#135):
///
/// ```xml
/// <rdf:li rdf:parseType="Resource">
///   <mwg-rs:Name>…</mwg-rs:Name> <mwg-rs:Type>Face</mwg-rs:Type>
///   <mwg-rs:Area rdf:parseType="Resource">
///     <stArea:x/> <stArea:y/> <stArea:w/> <stArea:h/> <stArea:unit>normalized</stArea:unit>
///   </mwg-rs:Area>
/// </rdf:li>
/// ```
///
/// Exactly those fields, each once, in elements (any order, whitespace between them), and no
/// other attribute, field or child — no foreign property at all. Only such a region can be on
/// the legacy record (review M2): a region another tool wrote at the recorded place under the
/// recorded name (digiKam's nested `rdf:Description` with `digiKam:Confidence`, a Lightroom
/// region with `mwg-rs:Rotation`, …) has another shape and stays foreign. The old writer's
/// in-place Area update (#140) kept a foreign region's shape, so a region it updated but did
/// not write is not taken for one it wrote.
pub(super) fn is_pre_marker_shape(li: &Element) -> bool {
    /// Only `rdf:parseType="Resource"` among its attributes.
    fn resource_struct(e: &Element) -> bool {
        ns_attr(e, NS_RDF, "parseType") == Some("Resource") && e.attributes.len() == 1
    }
    /// Its child elements, when every other child is whitespace.
    fn fields(e: &Element) -> Option<Vec<&Element>> {
        let mut out = Vec::new();
        for n in &e.children {
            match n {
                XMLNode::Element(c) => out.push(c),
                XMLNode::Text(t) if t.trim().is_empty() => {}
                _ => return None,
            }
        }
        Some(out)
    }
    /// The text of a plain literal field with no attributes, `None` for anything else.
    fn literal(e: &Element) -> Option<String> {
        if !e.attributes.is_empty() {
            return None;
        }
        let mut text = String::new();
        for n in &e.children {
            match n {
                XMLNode::Text(t) => text.push_str(t),
                _ => return None,
            }
        }
        Some(text)
    }
    /// The fields of `e` in namespace `ns`, matched one to one with `names` in any order.
    fn exactly<'a>(e: &'a Element, ns: &str, names: &[&str]) -> Option<Vec<&'a Element>> {
        let kids = fields(e)?;
        if kids.len() != names.len() {
            return None;
        }
        names
            .iter()
            .map(|name| {
                let mut found = kids.iter().filter(|k| k.namespace.as_deref() == Some(ns) && k.name == *name);
                match (found.next(), found.next()) {
                    (Some(k), None) => Some(*k),
                    _ => None,
                }
            })
            .collect()
    }

    if !is_rdf(li, "li") || !resource_struct(li) {
        return false;
    }
    let Some([name, kind, area]) = exactly(li, NS_MWG_RS, &["Name", "Type", "Area"]).map(|v| [v[0], v[1], v[2]])
    else {
        return false;
    };
    if literal(name).is_none() || literal(kind).as_deref() != Some("Face") || !resource_struct(area) {
        return false;
    }
    let Some(coords) = exactly(area, NS_STAREA, &["x", "y", "w", "h", "unit"]) else {
        return false;
    };
    coords[..4].iter().all(|c| literal(c).is_some_and(|v| v.trim().parse::<f32>().is_ok()))
        && literal(coords[4]).as_deref() == Some("normalized")
}

/// A new region for `r`, carrying catalog `catalog`'s marker.
pub(super) fn marked_li(r: &FaceRegion, catalog: &str) -> XMLNode {
    let XMLNode::Element(mut li) = region_li(r) else { unreachable!("region_li builds an element") };
    // Declared on the field itself: the Description it lands in may bind `chairphoto` to
    // something else, or not at all.
    let XMLNode::Element(mut marker) =
        plain("chairphoto", NS_CHAIRPHOTO, "FaceId", &face_marker(catalog, r.face_id))
    else {
        unreachable!("plain builds an element")
    };
    let mut ns = Namespace::empty();
    ns.put("chairphoto", NS_CHAIRPHOTO);
    marker.namespaces = Some(ns);
    li.children.push(XMLNode::Element(marker));
    XMLNode::Element(li)
}

/// Make an existing region ChairPhoto's write of `r`: its Area moved ([`set_region_area`]), its
/// Name the face's, and its marker this catalog's for the face. Every other field, attribute
/// and child stays.
fn mark_region(li: &mut Element, r: &FaceRegion, catalog: &str) {
    set_region_area(li, r);
    set_struct_fields(li, NS_MWG_RS, "mwg-rs", &[("Name", r.name.clone())]);
    set_struct_fields(li, NS_CHAIRPHOTO, "chairphoto", &[("FaceId", face_marker(catalog, r.face_id))]);
}

/// True if two top-left bboxes have centers within [`AREA_EPSILON`] on both axes.
fn center_close(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    let acx = a.0 + a.2 / 2.0;
    let acy = a.1 + a.3 / 2.0;
    let bcx = b.0 + b.2 / 2.0;
    let bcy = b.1 + b.3 / 2.0;
    (acx - bcx).abs() <= AREA_EPSILON && (acy - bcy).abs() <= AREA_EPSILON
}
