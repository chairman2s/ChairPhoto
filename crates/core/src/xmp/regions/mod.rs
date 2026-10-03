//! MWG face regions (`mwg-rs:Regions`).
//!
//! Confirmed faces are written to the sidecar as MWG Regions — the Metadata Working
//! Group schema that digiKam, Lightroom and Picasa understand. A `mwg-rs:Regions`
//! property carries `mwg-rs:AppliedToDimensions` (the pixel size of the stored image the
//! region coordinates apply to) plus a `mwg-rs:RegionList` (an rdf:Bag of region structs).
//!
//! MWG 2.0 § 5.9 measures regions on the **stored** image, before its EXIF Orientation is
//! applied ("A Creator or Changer MUST express region coordinates, width and height relative
//! to the stored image, prior to the application of the Exif Orientation tag"). ChairPhoto
//! measures faces on the EXIF-oriented preview, so the writer turns each box into the stored
//! frame and the reader turns it back (#136). See [`RegionFrame`].
//! Each region has a `mwg-rs:Name`, `mwg-rs:Type='Face'` and a `mwg-rs:Area` whose
//! `stArea:x/y` are the **CENTER** of the rectangle (MWG stores centers, not top-left),
//! with `stArea:w/h` and `stArea:unit='normalized'`.
//!
//! This write is merge-safe like the rest of this module, but with an extra twist: the
//! RegionList may already contain regions written by *other tools* (digiKam etc.). We edit the
//! existing Regions in place, replace or remove only the regions chairphoto itself wrote and
//! preserve every foreign region; a Regions laid out in a way we do not recognise is not
//! written. A region chairphoto writes carries its marker, a `chairphoto:FaceId` field (#135);
//! a region without one is foreign — or was written before the marker existed, see
//! [`write_face_regions`] — and when in doubt we preserve. See AGENTS.md ("XMP safety").

mod frame;
mod mwg;
mod reconcile;
#[cfg(test)]
mod tests;

pub use frame::RegionFrame;

use std::path::Path;
use xmltree::XMLNode;
use crate::xmp::document::SidecarDocument;
use crate::xmp::dom::{element_at, element_at_mut, find_description_properties, is_rdf, node_element, rdf_of};
use crate::xmp::ns::NS_MWG_RS;
use crate::xmp::parse::parse_xml;
use crate::xmp::sidecar_path;
use frame::{applied_dimensions, region_target, RegionTarget};
use mwg::{
    declare_region_namespaces, new_regions, parse_region_li, regions_layout, struct_body, RegionsLayout,
};
use reconcile::{marked_li, update_regions};

/// Epsilon (in normalized coordinates) for deciding whether an existing region is "the same"
/// as one chairphoto is writing — i.e. previously written by us for that same person. Coarse
/// on purpose: two distinct faces of the same-named person on one photo are never this close,
/// while a re-detection of the same face drifts far less than this.
const AREA_EPSILON: f32 = 0.02;

/// One face region for the MWG writer. `bbox` is the **top-left** normalized rectangle
/// (`x, y` = top-left corner, `w, h` = size, all 0–1 of the EXIF-oriented image, the frame
/// the faces are detected and drawn in) — exactly what `faces__faces.bbox` holds. The writer
/// turns it into the stored frame ([`RegionFrame`]) and MWG's center form.
#[derive(Debug, Clone, PartialEq)]
pub struct FaceRegion {
    /// The face's `faces__faces.id`, written as the region's `chairphoto:FaceId` marker (#135).
    pub face_id: i64,
    /// Person tag leaf name (the region's `mwg-rs:Name`).
    pub name: String,
    /// Top-left-normalized bbox: `(x, y, w, h)`, each 0–1 of the EXIF-oriented image.
    pub bbox: (f32, f32, f32, f32),
}

/// A region parsed back out of a sidecar. Coordinates are converted from MWG's center form
/// back to a **top-left** normalized bbox: in the file's own frame from [`read_face_regions`],
/// in the EXIF-oriented frame ChairPhoto stores from [`read_face_regions_in`].
#[derive(Debug, Clone, PartialEq)]
pub struct ReadRegion {
    /// `mwg-rs:Name` (may be empty for an unnamed region).
    pub name: String,
    /// Top-left-normalized bbox: `(x, y, w, h)`, each 0–1.
    pub bbox: (f32, f32, f32, f32),
}

/// Write the given confirmed face regions into the photo's XMP sidecar as `mwg-rs:Regions`,
/// merge-safely. The boxes are in the display frame; with a known EXIF Orientation in `frame`
/// they are written in the stored frame MWG measures regions in, and a new
/// `AppliedToDimensions` is the stored size. With an unknown orientation they are written as
/// they are (#136).
///
/// An `AppliedToDimensions` the sidecar already has is **never rewritten** (#145): the regions
/// other tools wrote are normalized against it, so changing it would move them. ChairPhoto's
/// regions are written in the frame it declares instead — see [`region_target`] — and when
/// that frame is not one of this image's (another aspect, or which way it is turned cannot be
/// told), the write is refused and the sidecar left as it was.
///
/// `regions` is the photo's whole current set: every region ChairPhoto writes carries a
/// `chairphoto:FaceId` marker, `<catalog>/<face id>` ([`face_marker`](reconcile::face_marker); `catalog` is the
/// writing catalog's identity, `catalog::CATALOG_UUID_KEY`), and on each write ChairPhoto
/// **replaces or removes only regions carrying this catalog's marker** (#135), and only for a
/// face this catalog knows on this photo: one in `regions`, or one in `retired` — this
/// catalog's faces on this photo that have left the set (rejected, ignored, unnamed). Such a
/// region whose face is retired is removed; one whose face is in the set is moved to the face's
/// geometry and name. Every other region is foreign and kept — unmarked, marked by another
/// catalog (a rebuilt one, a second one over the same folders), marked with a face id this
/// catalog does not know on this photo (a copy of this catalog's file shares its identity,
/// review N1), or carrying a marker this build does not recognise:
///
/// - When its Name and center-Area (within [`AREA_EPSILON`]) match a face being written, it is
///   that face already in the file, and only its Area's coordinates change; its other fields,
///   foreign attributes and children stay (#140), and no marked copy is appended.
/// - `legacy` is the catalog's record of the faces ChairPhoto exported **before the marker
///   existed** (face id, name and box at that time, in the display frame those writes used).
///   An unmarked region in exactly the pre-marker writer's shape ([`is_pre_marker_shape`](reconcile::is_pre_marker_shape))
///   matching such a face by Name + Area was ChairPhoto's: it is adopted (moved and marked)
///   when the face is still in the set, removed when it is not. Any other shape is foreign.
///
/// Each incoming region claims at most one existing region. Merge-safety (binding, AGENTS.md):
/// the existing `mwg-rs:Regions` is edited in place, never rebuilt. Every foreign region in
/// its `RegionList`, every foreign attribute or child of `Regions`, `AppliedToDimensions` and
/// the list, and every foreign XML element anywhere else in the sidecar is preserved. The
/// `Regions` may sit in any top-level
/// `rdf:Description` (exiftool writes one per namespace), and its struct values may be written
/// with `rdf:parseType="Resource"`, as a nested `rdf:Description`, or (for
/// `AppliedToDimensions`) as attributes. The list may be an `rdf:Bag` or an `rdf:Seq`.
///
/// A `Regions` this writer does not recognise (two of them, a list in another container, a
/// struct in a form it cannot read) is **not** written: the write fails with an error and the
/// sidecar is left as it was. Losing a region another tool wrote is worse than missing ours.
///
/// A write that changes nothing in the regions — an empty set and no region of ours, or the
/// set already as the file has it — leaves the sidecar untouched (it creates none).
///
/// Backs up a pre-existing foreign sidecar once before the first write, mirroring the other
/// managed-property writers.
pub fn write_face_regions(
    photo_path: &Path,
    catalog: &str,
    regions: &[FaceRegion],
    retired: &[i64],
    legacy: &[FaceRegion],
    frame: RegionFrame,
) -> Result<(), RegionWriteError> {
    require_catalog_identity(catalog)?;
    let doc = SidecarDocument::open(photo_path)?;
    write_regions_into(doc, photo_path, catalog, regions, retired, legacy, frame)
}

/// What one face-region write writes: [`write_face_regions`]'s arguments past the catalog.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RegionSet {
    pub regions: Vec<FaceRegion>,
    pub retired: Vec<i64>,
    pub legacy: Vec<FaceRegion>,
    pub frame: RegionFrame,
}

/// [`write_face_regions`] with the set read by `gather` once the sidecar's file lock is held
/// (#156), so the set written is never older than one another writer of this sidecar
/// already wrote. A writer that reads the set and only then waits for the lock can be
/// overtaken: another connection changes the set and writes it, then the first writes its
/// older set over it. `gather` runs under the file lock, which is a leaf in the lock order
/// (`app/jobs.rs`): it must not take a lock — a read on a connection the caller already
/// holds (or a WAL read on its own) only. Its error fails the write, with nothing written.
pub fn write_face_regions_gathered(
    photo_path: &Path,
    catalog: &str,
    gather: impl FnOnce() -> Result<RegionSet, String>,
) -> Result<(), RegionWriteError> {
    require_catalog_identity(catalog)?;
    let doc = SidecarDocument::open(photo_path)?;
    let set = gather()?;
    write_regions_into(doc, photo_path, catalog, &set.regions, &set.retired, &set.legacy, set.frame)
}

fn require_catalog_identity(catalog: &str) -> Result<(), RegionWriteError> {
    if catalog.is_empty() || catalog.contains('/') {
        return Err(RegionWriteError::Failed(format!("face regions not written: {catalog:?} is no catalog identity")));
    }
    Ok(())
}

fn write_regions_into(
    mut doc: SidecarDocument,
    photo_path: &Path,
    catalog: &str,
    regions: &[FaceRegion],
    retired: &[i64],
    legacy: &[FaceRegion],
    frame: RegionFrame,
) -> Result<(), RegionWriteError> {
    // Written only into a Regions that has no AppliedToDimensions of its own.
    let dims = frame.stored_size;

    // Regions don't fit `replace_owned`'s "strip a fixed set of (ns, name) pairs, push flat
    // replacements" shape: which existing `rdf:li` entries to keep is decided per-entry by
    // marker, Name + center-Area matching (AGENTS.md), not by a static owned-node list. So this
    // writer reaches into the Description directly instead.
    let rdf = doc.rdf_mut();
    let found = find_description_properties(rdf, NS_MWG_RS, "Regions");
    match found.as_slice() {
        [] if regions.is_empty() => return Ok(()),
        [] => {
            let target = region_target(frame, None).expect("no dimensions, no conflict");
            let desc = doc.description_mut();
            declare_region_namespaces(desc);
            let lis = regions.iter().map(|r| marked_li(&target.write(r), catalog)).collect();
            desc.children.push(XMLNode::Element(new_regions(dims, lis)));
        }
        [(d, p)] => {
            let existing = element_at(element_at(rdf, *d), *p);
            let layout = regions_layout(existing)
                .map_err(|why| unrecognised_regions(photo_path, &why))?;
            let declared = layout.dims.map(|i| {
                let body = struct_body(existing).expect("regions_layout checked the form");
                applied_dimensions(element_at(body, i))
            });
            // With nothing to write, the frame only matters for what is removed, which is
            // found by its marker or in the legacy frame.
            let target = match regions {
                [] => RegionTarget::AsIs,
                _ => region_target(frame, declared)
                    .map_err(|why| unrecognised_frame(photo_path, &why))?,
            };
            let regions: Vec<FaceRegion> = regions.iter().map(|r| target.write(r)).collect();
            let mut edited = existing.clone();
            update_regions(&mut edited, catalog, &regions, retired, legacy, dims)
                .map_err(|why| unrecognised_regions(photo_path, &why))?;
            if edited == *existing {
                return Ok(());
            }
            let desc = element_at_mut(rdf, *d);
            declare_region_namespaces(desc);
            *element_at_mut(desc, *p) = edited;
        }
        more => {
            let why = format!("it holds {} mwg-rs:Regions properties", more.len());
            return Err(unrecognised_regions(photo_path, &why));
        }
    }

    Ok(doc.commit()?)
}

fn unrecognised_frame(photo_path: &Path, why: &str) -> RegionWriteError {
    RegionWriteError::Refused(format!(
        "{}: face regions not written, sidecar left unchanged: {why}",
        sidecar_path(photo_path).display()
    ))
}

/// Why [`write_face_regions`] wrote nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionWriteError {
    /// The sidecar's own content rules the write out — a `Regions` laid out in a way this
    /// writer does not recognise, or declaring a frame it cannot place its boxes in — and the
    /// sidecar was left as it was. Writing again changes nothing until the file does.
    Refused(String),
    /// Anything else: the sidecar could not be read, parsed or written (IO, a volume gone, an
    /// unparseable file), or the caller passed no catalog identity. Trying again may succeed.
    Failed(String),
}

impl std::fmt::Display for RegionWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(why) | Self::Failed(why) => f.write_str(why),
        }
    }
}

impl From<String> for RegionWriteError {
    fn from(why: String) -> Self {
        Self::Failed(why)
    }
}

fn unrecognised_regions(photo_path: &Path, why: &str) -> RegionWriteError {
    RegionWriteError::Refused(format!(
        "{}: face regions not written, sidecar left unchanged: {why}, a layout ChairPhoto \
         does not recognise",
        sidecar_path(photo_path).display()
    ))
}

/// The photo's MWG face regions in the display frame ChairPhoto stores face boxes in (#136):
/// each `Regions` property's boxes turned out of the frame its `AppliedToDimensions` declares
/// ([`region_target`], the rule the writer follows). With an unknown orientation the boxes are
/// returned as the file has them. A `Regions` whose frame is not one of this image's is
/// skipped: a region read in the wrong frame would name the wrong face.
pub fn read_face_regions_in(photo_path: &Path, frame: RegionFrame) -> Vec<ReadRegion> {
    let mut out = Vec::new();
    for (declared, regions) in read_regions(photo_path) {
        let Ok(target) = region_target(frame, declared) else { continue };
        out.extend(regions.into_iter().map(|r| ReadRegion { bbox: target.read(r.bbox), ..r }));
    }
    out
}

/// Read the MWG face regions (`mwg-rs:Regions`) from the photo's sidecar. Returns each region's
/// name and its **top-left** normalized bbox (converted from MWG's center form), in the frame
/// the file measures them in (the stored image, for a file that follows MWG). Regions whose
/// Area is missing/unparseable are skipped. Returns an empty vec when there is no sidecar or no
/// Regions property. Reads the layouts [`write_face_regions`] accepts: `Regions` in any
/// top-level Description; `Regions`, each region and its `Area` as `rdf:parseType="Resource"`,
/// a nested `rdf:Description`, or attributes; the list as an `rdf:Bag` or `rdf:Seq`. A
/// `Regions` in any other layout is skipped.
pub fn read_face_regions(photo_path: &Path) -> Vec<ReadRegion> {
    read_regions(photo_path).into_iter().flat_map(|(_, regions)| regions).collect()
}

/// Every recognised `Regions` property's declared `AppliedToDimensions` (`None` when it has
/// none) and its regions, in the file's frame.
#[allow(clippy::type_complexity)]
fn read_regions(photo_path: &Path) -> Vec<(Option<Result<(f64, f64), String>>, Vec<ReadRegion>)> {
    let path = sidecar_path(photo_path);
    let Ok(file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let Ok(root) = parse_xml(file) else {
        return Vec::new();
    };
    let Some(rdf) = rdf_of(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (d, p) in find_description_properties(rdf, NS_MWG_RS, "Regions") {
        let regions = element_at(element_at(rdf, d), p);
        let Ok(RegionsLayout { list: Some((l, c)), dims }) = regions_layout(regions) else {
            continue;
        };
        let body = struct_body(regions).expect("regions_layout checked the form");
        let declared = dims.map(|i| applied_dimensions(element_at(body, i)));
        let container = element_at(element_at(body, l), c);
        let lis = container.children.iter().filter_map(node_element).filter(|li| is_rdf(li, "li"));
        out.push((declared, lis.filter_map(parse_region_li).collect()));
    }
    out
}

/// Compute IoU between two top-left-normalized bboxes `(x, y, w, h)`. Shared by the region
/// importer to match parsed regions against detected faces (H13f import path).
pub fn region_iou(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
    let (ax1, ay1, aw, ah) = a;
    let (bx1, by1, bw, bh) = b;
    let (ax2, ay2) = (ax1 + aw, ay1 + ah);
    let (bx2, by2) = (bx1 + bw, by1 + bh);
    let ix1 = ax1.max(bx1);
    let iy1 = ay1.max(by1);
    let ix2 = ax2.min(bx2);
    let iy2 = ay2.min(by2);
    let iw = (ix2 - ix1).max(0.0);
    let ih = (iy2 - iy1).max(0.0);
    let inter = iw * ih;
    let uni = aw * ah + bw * bh - inter;
    if uni <= 0.0 {
        0.0
    } else {
        inter / uni
    }
}
