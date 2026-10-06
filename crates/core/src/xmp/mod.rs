//! Merge-safe XMP sidecar writing — chairphoto's first *write* to photo files.
//!
//! Writes authored IPTC Core fields to `<original>.xmp` using the XMP property
//! mappings from the IPTC Photo Metadata standard. The critical invariant
//! (AGENTS.md): we modify ONLY our managed properties and preserve everything else
//! in the sidecar — darktable/Lightroom develop settings live in the same file and
//! must survive. We parse the existing sidecar into a DOM, replace our properties,
//! and write it back; foreign elements are untouched.
//!
//! # What's where
//!
//! Every public item is re-exported here, so callers use `crate::xmp::<item>`.
//!
//! | Module | Holds |
//! |---|---|
//! | `mod.rs` (this file) | The sidecar path convention: [`sidecar_path`], its backup path, the `LastWrite` clock. |
//! | `document.rs` | `SidecarDocument`: the one read-modify-write path every writer goes through (backup-once, owned-property replacement, `chairphoto:LastWrite`). |
//! | [`lock`] | Per-sidecar serialisation: the file lock and the write order (#149). |
//! | `iptc.rs` | The `MANAGED` IPTC property table, [`write_iptc`], [`write_iptc_fields`], [`read_iptc_present`]. |
//! | `keywords.rs` | [`write_keywords`] (export copies only: `dc:subject`, `lr:hierarchicalSubject`). |
//! | `identity.rs` | [`read_identifier`], [`read_identifiers`], [`write_identifier`], [`overwrite_identifier`], [`overwrite_identifier_checked`], [`write_import_batch`], [`read_import_batch`]. |
//! | `gps.rs` | [`write_gps`], [`read_gps`], [`decimal_to_dms_lat`] / [`decimal_to_dms_lng`]. |
//! | `regions/mod.rs` | Face regions' public API: [`FaceRegion`], [`ReadRegion`], [`RegionSet`], [`RegionWriteError`], [`write_face_regions`], [`write_face_regions_gathered`], [`read_face_regions`], [`read_face_regions_in`], [`region_iou`]. |
//! | `regions/frame.rs` | [`RegionFrame`] and [`FrameDoubt`], EXIF-orientation point maps and their composition, the preview-aspect check, and which frame a `Regions` declares (`region_target`). |
//! | `regions/mwg.rs` | MWG element construction and parsing: `Regions` layout, struct forms, one region `rdf:li`. |
//! | `regions/reconcile.rs` | In-place edit of an existing `Regions`: the `chairphoto:FaceId` marker, claiming, the pre-marker shape. |
//! | `parse.rs` | `parse_xml` (keeps qualified attribute names) and namespace-aware attribute lookup. |
//! | `repair.rs` | Restoring the attribute prefixes pre-#138 releases dropped, when unambiguous (#143). |
//! | `emit.rs` | Serialising the DOM: the pass that writes every element under a prefix bound to its namespace (#143). |
//! | `dom.rs` | Generic element navigation and construction helpers, the empty packet skeleton. |
//! | `ns.rs` | Namespace URI constants. |
//! | `test_fixtures.rs`, `region_fixtures.rs`, `test_xml.rs` | Test-only: foreign sidecars and independent readers. |
//! | `tests.rs` | Test-only: merge-safety checks that run every writer over one sidecar. |

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

mod document;
mod dom;
mod emit;
mod gps;
mod identity;
mod iptc;
mod keywords;
pub mod lock;
mod ns;
mod parse;
mod regions;
mod repair;
#[cfg(test)]
pub(crate) mod region_fixtures;
#[cfg(test)]
pub(crate) mod test_fixtures;
#[cfg(test)]
mod test_xml;
#[cfg(test)]
mod tests;

pub use gps::{decimal_to_dms_lat, decimal_to_dms_lng, read_gps, write_gps};
pub use identity::{
    overwrite_identifier, overwrite_identifier_checked, read_identifier, read_identifiers,
    read_import_batch, write_identifier, write_import_batch, CheckedOverwriteError,
};
pub use iptc::{read_iptc_present, write_iptc, write_iptc_fields};
pub use keywords::write_keywords;
pub use regions::{
    read_face_regions, read_face_regions_in, region_iou, write_face_regions, write_face_regions_gathered,
    FaceRegion, FrameDoubt, ReadRegion, RegionFrame, RegionSet, RegionWriteError,
};
pub(crate) use regions::compose_orientations;

// What `document.rs`, `lock.rs` and the test fixtures reach as `super::…`.
use dom::{child_mut, declare_namespaces, is_xmp_root, new_root, plain, rdf_of_mut};
#[cfg(test)]
use iptc::MANAGED;
#[cfg(test)]
use ns::{NS_DC, NS_EXIF, NS_IPTC, NS_MWG_RS, NS_PHOTOSHOP, NS_STAREA, NS_STDIM, NS_XMP};
use ns::{NS_CHAIRPHOTO, NS_RDF};
use parse::{attr_is, ns_attr, parse_xml};

/// The sidecar path for a photo: `<original_filename>.xmp` (darktable convention).
pub fn sidecar_path(photo_path: &Path) -> PathBuf {
    let mut s = photo_path.as_os_str().to_os_string();
    s.push(".xmp");
    PathBuf::from(s)
}

/// Where this sidecar's pre-chairphoto backup goes. Defined in `companions`, because
/// offload has to recognise these files without deleting them (#82) and a second spelling
/// of the suffix would let the two drift.
fn sidecar_backup_path(sidecar: &Path) -> PathBuf {
    crate::companions::sidecar_backup(sidecar)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The `chairphoto:LastWrite` stamp (Unix seconds) of the sidecar file `sidecar` itself — not
/// a photo's path — in element or attribute form, in whichever top-level `rdf:Description`
/// carries it. `None` when there is no such file, no stamp, or it does not parse. Storage
/// reads it to tell ChairPhoto's own rewrite of a sidecar from another program's (#257).
pub fn read_last_write(sidecar: &Path) -> Option<i64> {
    let file = std::fs::File::open(sidecar).ok()?;
    let root = repair::parse_for_read(file).ok()?;
    let rdf = dom::rdf_of(&root)?;
    rdf.children.iter().find_map(|node| {
        let xmltree::XMLNode::Element(desc) = node else { return None };
        if desc.namespace.as_deref() != Some(NS_RDF) || desc.name != "Description" {
            return None;
        }
        let element = desc.children.iter().find_map(|n| match n {
            xmltree::XMLNode::Element(e) if e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "LastWrite" => {
                dom::first_text(e)
            }
            _ => None,
        });
        element.or_else(|| parse::ns_attr(desc, NS_CHAIRPHOTO, "LastWrite").map(str::to_string))?.trim().parse().ok()
    })
}
