//! Merge-safe XMP sidecar writing — chairphoto's first *write* to photo files.
//!
//! Writes authored IPTC Core fields to `<original>.xmp` using the XMP property
//! mappings from the IPTC Photo Metadata standard. The critical invariant
//! (AGENTS.md): we modify ONLY our managed properties and preserve everything else
//! in the sidecar — darktable/Lightroom develop settings live in the same file and
//! must survive. We parse the existing sidecar into a DOM, replace our properties,
//! and write it back; foreign elements are untouched.

use crate::catalog::IptcFields;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use xmltree::{Element, Namespace, XMLNode};

mod document;
use document::SidecarDocument;
pub mod lock;
#[cfg(test)]
pub(crate) mod test_fixtures;
#[cfg(test)]
pub(crate) mod region_fixtures;

const NS_X: &str = "adobe:ns:meta/";
const NS_RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const NS_DC: &str = "http://purl.org/dc/elements/1.1/";
const NS_PHOTOSHOP: &str = "http://ns.adobe.com/photoshop/1.0/";
const NS_IPTC: &str = "http://iptc.org/std/Iptc4xmpCore/1.0/xmlns/";
const NS_LR: &str = "http://ns.adobe.com/lightroom/1.0/";
const NS_XMP: &str = "http://ns.adobe.com/xap/1.0/";
const NS_CHAIRPHOTO: &str = "https://chairphoto.local/ns/1.0/";
const NS_EXIF: &str = "http://ns.adobe.com/exif/1.0/";
// Metadata Working Group region schema (mwg-rs) + the shared structure namespaces it uses.
// digiKam, Lightroom and Picasa all read/write faces through these.
const NS_MWG_RS: &str = "http://www.metadataworkinggroup.com/schemas/regions/";
const NS_STAREA: &str = "http://ns.adobe.com/xmp/sType/Area#";
const NS_STDIM: &str = "http://ns.adobe.com/xap/1.0/sType/Dimensions#";

/// One property [`write_iptc`] manages: its (namespace, local name) and the catalog field
/// that holds its value, side by side so the mapping cannot drift between two lists.
struct Managed {
    ns: &'static str,
    name: &'static str,
    value: fn(&IptcFields) -> &str,
}

/// Properties chairphoto manages via [`write_iptc`]. A write touches only the ones whose
/// catalog value changed (issue #144): it removes every existing instance of a changed
/// property — element or compact form, in every Description — and re-adds it when the new
/// value is non-empty. Every other element in the sidecar, an unchanged managed property
/// included, is preserved.
/// Note: `chairphoto:ImportBatch` is managed separately by [`write_import_batch`]
/// and is intentionally NOT listed here so IPTC writes don't clobber it. Likewise
/// `chairphoto:LastWrite` is not listed: every writer's completion stamp is applied
/// uniformly by [`SidecarDocument::commit`], not per-writer.
const MANAGED: [Managed; 11] = [
    Managed { ns: NS_DC, name: "description", value: |f| &f.description },
    Managed { ns: NS_DC, name: "title", value: |f| &f.title },
    Managed { ns: NS_DC, name: "rights", value: |f| &f.copyright },
    Managed { ns: NS_DC, name: "creator", value: |f| &f.creator },
    Managed { ns: NS_PHOTOSHOP, name: "Headline", value: |f| &f.headline },
    Managed { ns: NS_PHOTOSHOP, name: "Credit", value: |f| &f.credit },
    Managed { ns: NS_PHOTOSHOP, name: "Source", value: |f| &f.source },
    Managed { ns: NS_PHOTOSHOP, name: "City", value: |f| &f.city },
    Managed { ns: NS_PHOTOSHOP, name: "State", value: |f| &f.state },
    Managed { ns: NS_PHOTOSHOP, name: "Country", value: |f| &f.country },
    Managed { ns: NS_IPTC, name: "CountryCode", value: |f| &f.country_code },
];

/// The sidecar node for one managed property's non-empty value.
fn managed_node(ns: &str, name: &str, value: &str) -> XMLNode {
    match (ns, name) {
        (NS_DC, "creator") => seq_creator(value),
        (NS_DC, _) => lang_alt(name, value),
        (NS_PHOTOSHOP, _) => plain("photoshop", NS_PHOTOSHOP, name, value),
        _ => plain("Iptc4xmpCore", NS_IPTC, name, value),
    }
}

/// The sidecar path for a photo: `<original_filename>.xmp` (darktable convention).
pub fn sidecar_path(photo_path: &Path) -> PathBuf {
    let mut s = photo_path.as_os_str().to_os_string();
    s.push(".xmp");
    PathBuf::from(s)
}

/// Write a change of the authored IPTC fields into the photo's XMP sidecar, merging with
/// any existing content. `before` is the catalog's value before this change and `after`
/// the value now stored; callers read both in the lock hold that stores `after`, so the
/// diff is the change the catalog actually made.
///
/// Only a field whose value changed is written (issue #144): a new non-empty value replaces
/// every existing instance of the property, and a value the user cleared removes them. A
/// field that did not change — empty before and after included — is left exactly as the
/// sidecar has it, so a creator, rights or caption another tool wrote (ChairPhoto never
/// imports them into the catalog) survive a city-only geocode or a title-only save. With
/// nothing changed the sidecar is not opened at all.
///
/// Creates the sidecar if absent; backs up a pre-existing non-chairphoto sidecar once
/// before the first write.
pub fn write_iptc(photo_path: &Path, before: &IptcFields, after: &IptcFields) -> Result<(), String> {
    let mut owned = Vec::new();
    let mut replacements = Vec::new();
    for m in &MANAGED {
        let new = (m.value)(after);
        if (m.value)(before) == new {
            continue;
        }
        owned.push((m.ns, m.name));
        if !new.is_empty() {
            replacements.push(managed_node(m.ns, m.name, new));
        }
    }
    if owned.is_empty() {
        return Ok(());
    }

    let mut doc = SidecarDocument::open(photo_path)?;
    doc.replace_owned(&owned, replacements);
    doc.commit()
}

/// Write export keywords into the photo's XMP sidecar, merging with existing content.
/// Manages ONLY `dc:subject` (flat) and `lr:hierarchicalSubject` — every other element
/// (IPTC fields, develop settings, foreign namespaces) is preserved. Mirrors the
/// merge-safety invariant of [`write_iptc`]. An empty list clears that property.
///
/// This is the **export** path: it writes the destination copy of a sidecar, so unlike
/// [`write_iptc`] it does NOT back up a pre-existing foreign sidecar (the destination is
/// a throwaway copy, and keywords are always re-derivable from the catalog). If this is
/// ever used on an in-library sidecar, add the foreign-backup-once logic back.
pub fn write_keywords(
    photo_path: &Path,
    flat: &[String],
    hierarchical: &[String],
) -> Result<(), String> {
    // Export destination copy: not subject to the in-library backup-once rule (AGENTS.md).
    let mut doc = SidecarDocument::open_no_backup(photo_path)?;

    let owned = [(NS_DC, "subject"), (NS_LR, "hierarchicalSubject")];
    let mut replacements = Vec::new();
    if !flat.is_empty() {
        replacements.push(bag("dc", NS_DC, "subject", flat));
    }
    if !hierarchical.is_empty() {
        replacements.push(bag("lr", NS_LR, "hierarchicalSubject", hierarchical));
    }

    doc.replace_owned(&owned, replacements);
    doc.commit()
}

/// Read the photo UUID (`xmp:Identifier`) from its sidecar, if present. Used by the
/// scanner to recognise a moved/re-rooted file as the same photo (UUID is stable; path
/// is not). Returns `None` when there's no sidecar or no identifier. Tolerant of the
/// attribute form, a plain element, or an rdf array.
pub fn read_identifier(photo_path: &Path) -> Option<String> {
    let path = sidecar_path(photo_path);
    let file = std::fs::File::open(&path).ok()?;
    let root = parse_xml(file).ok()?;
    let rdf = rdf_of(&root)?;
    for node in &rdf.children {
        let XMLNode::Element(desc) = node else { continue };
        if desc.name != "Description" {
            continue;
        }
        // Compact (attribute) form: rdf:Description xmp:Identifier="…".
        if let Some(v) = ns_attr(desc, NS_XMP, "Identifier") {
            if !v.trim().is_empty() {
                return Some(v.trim().to_string());
            }
        }
        // Element form: <xmp:Identifier>… or an rdf:Bag/Alt of li.
        for child in &desc.children {
            if let XMLNode::Element(e) = child {
                if e.namespace.as_deref() == Some(NS_XMP) && e.name == "Identifier" {
                    if let Some(text) = first_text(e) {
                        return Some(text);
                    }
                }
            }
        }
    }
    None
}

/// First non-blank text anywhere under an element (handles plain text and rdf:li wraps).
fn first_text(e: &Element) -> Option<String> {
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

/// Write the photo's UUID into its sidecar as `xmp:Identifier`, merge-safe: only the
/// identifier (and `chairphoto:LastWrite`) are touched; all other content is preserved.
/// This is the binding invariant — the UUID must live on disk so moves/re-roots match.
/// Backs up a pre-existing foreign sidecar once before the first write.
pub fn write_identifier(photo_path: &Path, uuid: &str) -> Result<(), String> {
    let mut doc = SidecarDocument::open(photo_path)?;
    // Manage only xmp:Identifier (preserve everything else).
    doc.replace_owned(
        &[(NS_XMP, "Identifier")],
        vec![plain("xmp", NS_XMP, "Identifier", uuid)],
    );
    doc.commit()
}

/// Replace an `xmp:Identifier` that belongs to somebody else with `uuid` — the Overwrite
/// half of resolving an identity conflict (issue #33), and the ONLY writer allowed to
/// destroy an identifier it did not write. Everything else in the sidecar is preserved,
/// exactly as in [`write_identifier`].
///
/// Returns the path the previous sidecar was copied to, or `None` if nothing was backed up
/// (no sidecar existed yet, or a backup from an earlier write is already there and was
/// deliberately not replaced). Unlike [`write_identifier`], the backup does not depend on
/// `chairphoto:LastWrite`: a sidecar chairphoto has written can still carry a foreign
/// identifier, and that is precisely the case this function exists for. See
/// `document::BackupPolicy`.
pub fn overwrite_identifier(photo_path: &Path, uuid: &str) -> Result<Option<PathBuf>, String> {
    let mut doc = SidecarDocument::open_forcing_backup(photo_path)?;
    let backup = doc.backup().map(|p| p.to_path_buf());
    doc.replace_owned(
        &[(NS_XMP, "Identifier")],
        vec![plain("xmp", NS_XMP, "Identifier", uuid)],
    );
    doc.commit()?;
    Ok(backup)
}

/// Write the import-batch UUID into the photo's XMP sidecar as
/// `chairphoto:ImportBatch`, merge-safe: only that field (and
/// `chairphoto:LastWrite`) are touched; all other content is preserved.
/// The batch UUID lets the batch survive catalog loss / catalog merge across
/// machines.
/// Backs up a pre-existing foreign sidecar once before the first write,
/// mirroring the invariant in [`write_identifier`].
pub fn write_import_batch(photo_path: &Path, batch_uuid: &str) -> Result<(), String> {
    let mut doc = SidecarDocument::open(photo_path)?;
    // Manage only chairphoto:ImportBatch (preserve everything else).
    doc.replace_owned(
        &[(NS_CHAIRPHOTO, "ImportBatch")],
        vec![plain("chairphoto", NS_CHAIRPHOTO, "ImportBatch", batch_uuid)],
    );
    doc.commit()
}

/// Read the import-batch UUID (`chairphoto:ImportBatch`) from the photo's
/// sidecar, if present. Returns `None` when there's no sidecar or no field.
pub fn read_import_batch(photo_path: &Path) -> Option<String> {
    let path = sidecar_path(photo_path);
    let file = std::fs::File::open(&path).ok()?;
    let root = parse_xml(file).ok()?;
    let rdf = rdf_of(&root)?;
    for node in &rdf.children {
        let XMLNode::Element(desc) = node else { continue };
        if desc.name != "Description" {
            continue;
        }
        for child in &desc.children {
            if let XMLNode::Element(e) = child {
                if e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "ImportBatch" {
                    if let Some(text) = first_text(e) {
                        return Some(text);
                    }
                }
            }
        }
    }
    None
}

/// Write GPS coordinates into the photo's XMP sidecar as `exif:GPSLatitude` and
/// `exif:GPSLongitude`, merge-safe: only those two fields (and `chairphoto:LastWrite`)
/// are touched; all other content is preserved.
///
/// Coordinates are encoded in the XMP EXIF DMS+ref format:
/// `"DD,MM.SSS[N|S]"` for latitude and `"DDD,MM.SSS[E|W]"` for longitude, which is
/// the format Lightroom, exiftool, and the XMP spec use for `exif:GPS*`.
///
/// Backs up a pre-existing foreign sidecar once before the first write, mirroring the
/// invariant in [`write_identifier`].
pub fn write_gps(photo_path: &Path, lat: f64, lng: f64) -> Result<(), String> {
    let mut doc = SidecarDocument::open(photo_path)?;
    // Manage only exif:GPSLatitude + exif:GPSLongitude (preserve everything else).
    doc.replace_owned(
        &[(NS_EXIF, "GPSLatitude"), (NS_EXIF, "GPSLongitude")],
        vec![
            plain("exif", NS_EXIF, "GPSLatitude", &decimal_to_dms_lat(lat)),
            plain("exif", NS_EXIF, "GPSLongitude", &decimal_to_dms_lng(lng)),
        ],
    );
    doc.commit()
}

/// Read the GPS coordinates (`exif:GPSLatitude` / `exif:GPSLongitude`) from the
/// photo's XMP sidecar. Returns `None` when there's no sidecar or the GPS fields
/// are absent or unparseable.
pub fn read_gps(photo_path: &Path) -> Option<(f64, f64)> {
    let path = sidecar_path(photo_path);
    let file = std::fs::File::open(&path).ok()?;
    let root = parse_xml(file).ok()?;
    let rdf = rdf_of(&root)?;
    let mut lat_str: Option<String> = None;
    let mut lng_str: Option<String> = None;
    for node in &rdf.children {
        let XMLNode::Element(desc) = node else { continue };
        if desc.name != "Description" {
            continue;
        }
        for child in &desc.children {
            if let XMLNode::Element(e) = child {
                if e.namespace.as_deref() == Some(NS_EXIF) {
                    match e.name.as_str() {
                        "GPSLatitude" => lat_str = first_text(e),
                        "GPSLongitude" => lng_str = first_text(e),
                        _ => {}
                    }
                }
            }
        }
    }
    let lat = dms_to_decimal(&lat_str?)?;
    let lng = dms_to_decimal(&lng_str?)?;
    Some((lat, lng))
}

/// Split an absolute (unsigned, unrefed) decimal degree into a whole-degree/minutes DMS
/// pair, rounding the minutes to the same six decimal places `decimal_to_dms_lat`/
/// `decimal_to_dms_lng` format them to, and carrying into the degree when that rounding
/// lands exactly on 60 minutes.
///
/// The carry decision is made by formatting the minutes with `{:.6}` and comparing the
/// *string* to `"60.000000"`, not by separately rounding the `f64` and trusting it to
/// agree with what `{:.6}` would later print. Two independent roundings (one hand-rolled,
/// one done by the formatter) can disagree at the ULP level — that mismatch is exactly how
/// this bug class survives a naive fix. Reusing the formatter's own output as the carry
/// test makes disagreement impossible: there is only one rounding, done once.
///
/// A carry into 60, 90, or 180 whole degrees is a legitimate DMS value (a pole for
/// latitude, the antimeridian for longitude) and is returned as-is — see the doc comments
/// on `decimal_to_dms_lat`/`decimal_to_dms_lng` for why no special-casing is needed there.
/// This function does not validate that `deg_abs` is within the 0..=90 / 0..=180 range
/// coordinates normally occupy; an out-of-range input (or one that carries past it, e.g.
/// 89.9999995 rounding through 90 becoming 90,0.0) is passed straight through. Range
/// validation, if wanted, belongs in the caller — see the module-level doc comments below.
fn dms_round_and_carry(deg_abs: f64) -> (u32, String) {
    let d = deg_abs.trunc() as u32;
    let m = (deg_abs - d as f64) * 60.0;
    let m_str = format!("{m:.6}");
    if m_str == "60.000000" {
        (d + 1, "0.000000".to_string())
    } else {
        (d, m_str)
    }
}

/// Convert a decimal-degree latitude to XMP EXIF DMS+ref format: `"DD,MM.SSSS[N|S]"`.
///
/// Rounding the minutes to six decimal places can land exactly on `60.000000` for a
/// latitude a few ULPs below a whole degree (e.g. `45.99999999999999289457`); see
/// `dms_round_and_carry` for how that's rounded once and carried into the degree so the
/// output is never `"45,60.000000N"`.
///
/// A carry that reaches 90 degrees is a legitimate result: 90°N/90°S is the pole, a real,
/// representable point, and `"90,0.000000N"` is a well-formed DMS string for it — no
/// different in kind from any other carry, so it needs no special case. This function does
/// not validate or clamp its input: a latitude magnitude above 90 (whether given directly
/// or reached by carrying, e.g. an input already at 90.9999995) is passed through
/// unchecked. Nothing upstream (`write_gps`, `plugins/map/mod.rs::set_photo_gps`) validates
/// latitude range either, so enforcing it here would be new, unrequested scope rather than
/// a rollover fix; a caller that needs a validated coordinate must check it before calling.
pub fn decimal_to_dms_lat(deg: f64) -> String {
    let hemi = if deg >= 0.0 { 'N' } else { 'S' };
    let (d, m_str) = dms_round_and_carry(deg.abs());
    format!("{d},{m_str}{hemi}")
}

/// Convert a decimal-degree longitude to XMP EXIF DMS+ref format: `"DDD,MM.SSSS[E|W]"`.
///
/// Rounding the minutes to six decimal places can land exactly on `60.000000` for a
/// longitude a few ULPs below a whole degree; see `dms_round_and_carry` for how that's
/// rounded once and carried into the degree so the output is never e.g.
/// `"45,60.000000E"`.
///
/// A carry that reaches 180 degrees is a legitimate result: the antimeridian is a real
/// meridian, and `"180,0.000000E"` is a well-formed DMS string for a point on it. This
/// function keeps whatever hemisphere letter the *input's sign* implies (`>= 0.0` is `E`,
/// negative is `W`) rather than picking one for the antimeridian itself — +180 and -180
/// name the same meridian, conventions differ on which letter belongs there, and this
/// function has no basis to prefer one over the other that the caller doesn't already have
/// via the sign it passed in. This function does not validate or clamp its input: a
/// longitude magnitude above 180 (whether given directly or reached by carrying) is passed
/// through unchecked, for the same reason given on `decimal_to_dms_lat` — no caller
/// upstream validates range, so adding it here would be new, unrequested scope.
pub fn decimal_to_dms_lng(deg: f64) -> String {
    let hemi = if deg >= 0.0 { 'E' } else { 'W' };
    let (d, m_str) = dms_round_and_carry(deg.abs());
    format!("{d},{m_str}{hemi}")
}

/// Parse an XMP EXIF DMS+ref string (e.g. `"59,23.456N"` or `"10,45.678E"`) back to
/// a signed decimal degree. Returns `None` on any parse error.
fn dms_to_decimal(s: &str) -> Option<f64> {
    let s = s.trim();
    let (hemi, body) = if let Some(rest) = s.strip_suffix(['N', 'S', 'E', 'W']) {
        let h = s.chars().last()?;
        (h, rest)
    } else {
        return None;
    };
    let mut parts = body.splitn(2, ',');
    let deg: f64 = parts.next()?.trim().parse().ok()?;
    let min: f64 = parts.next().unwrap_or("0").trim().parse().ok()?;
    let decimal = deg + min / 60.0;
    let signed = match hemi {
        'S' | 'W' => -decimal,
        _ => decimal,
    };
    Some(signed)
}

// --- MWG face regions (mwg-rs:Regions) ------------------------------------
//
// Confirmed faces are written to the sidecar as MWG Regions — the Metadata Working
// Group schema that digiKam, Lightroom and Picasa understand. A `mwg-rs:Regions`
// property carries `mwg-rs:AppliedToDimensions` (the pixel size of the stored image the
// region coordinates apply to) plus a `mwg-rs:RegionList` (an rdf:Bag of region structs).
//
// MWG 2.0 § 5.9 measures regions on the **stored** image, before its EXIF Orientation is
// applied ("A Creator or Changer MUST express region coordinates, width and height relative
// to the stored image, prior to the application of the Exif Orientation tag"). ChairPhoto
// measures faces on the EXIF-oriented preview, so the writer turns each box into the stored
// frame and the reader turns it back (#136). See [`RegionFrame`].
// Each region has a `mwg-rs:Name`, `mwg-rs:Type='Face'` and a `mwg-rs:Area` whose
// `stArea:x/y` are the **CENTER** of the rectangle (MWG stores centers, not top-left),
// with `stArea:w/h` and `stArea:unit='normalized'`.
//
// This write is merge-safe like the rest of this module, but with an extra twist: the
// RegionList may already contain regions written by *other tools* (digiKam etc.). We edit the
// existing Regions in place, replace or remove only the regions chairphoto itself wrote and
// preserve every foreign region; a Regions laid out in a way we do not recognise is not
// written. A region chairphoto writes carries its marker, a `chairphoto:FaceId` field (#135);
// a region without one is foreign — or was written before the marker existed, see
// [`write_face_regions`] — and when in doubt we preserve. See AGENTS.md ("XMP safety").

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

/// What the catalog knows of a photo's frames, for converting its face boxes (#136).
///
/// Face boxes live in the **display** frame: the image as its EXIF Orientation turns it.
/// MWG regions and their `AppliedToDimensions` live in the **stored** frame: the pixels as the
/// file holds them, before the Orientation is applied (MWG 2.0 § 5.9). The non-destructive
/// user rotation plays no part in either; it is the catalog's alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegionFrame {
    /// The original's EXIF Orientation code (1-8), `None` when unknown. Unknown is never
    /// guessed: the boxes are written as they are and an existing `AppliedToDimensions` is
    /// left alone.
    pub orientation: Option<u8>,
    /// The stored image's pixel size (`photos.width`/`height`, EXIF `ExifImageWidth`/`Height`),
    /// `None` when unknown — then no `AppliedToDimensions` is written.
    pub stored_size: Option<(u32, u32)>,
}

/// Where a normalized point `(u, v)` of the stored image lands in the image displayed under
/// EXIF Orientation `o` — the turn the preview the faces are detected on went through.
fn stored_to_display_point(o: u8, (u, v): (f32, f32)) -> (f32, f32) {
    match o {
        2 => (1.0 - u, v),       // mirror horizontal
        3 => (1.0 - u, 1.0 - v), // rotate 180
        4 => (u, 1.0 - v),       // mirror vertical
        5 => (v, u),             // mirror horizontal and rotate 270 CW (transpose)
        6 => (1.0 - v, u),       // rotate 90 CW
        7 => (1.0 - v, 1.0 - u), // mirror horizontal and rotate 90 CW (transverse)
        8 => (v, 1.0 - u),       // rotate 270 CW
        _ => (u, v),
    }
}

/// The inverse of [`stored_to_display_point`]: 6 and 8 undo each other, every other
/// orientation undoes itself.
fn display_to_stored_point(o: u8, p: (f32, f32)) -> (f32, f32) {
    let inverse = match o {
        6 => 8,
        8 => 6,
        o => o,
    };
    stored_to_display_point(inverse, p)
}

/// A top-left bbox mapped point by point: both corners, then the box they span.
fn map_bbox(
    bbox: (f32, f32, f32, f32),
    point: impl Fn((f32, f32)) -> (f32, f32),
) -> (f32, f32, f32, f32) {
    let (x, y, w, h) = bbox;
    let (ax, ay) = point((x, y));
    let (bx, by) = point((x + w, y + h));
    (ax.min(bx), ay.min(by), (ax - bx).abs(), (ay - by).abs())
}

/// A display-frame box in the stored frame of a photo with EXIF Orientation `o`.
fn display_to_stored(o: u8, bbox: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
    map_bbox(bbox, |p| display_to_stored_point(o, p))
}

/// A stored-frame box in the display frame of a photo with EXIF Orientation `o`.
fn stored_to_display(o: u8, bbox: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
    map_bbox(bbox, |p| stored_to_display_point(o, p))
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
/// `chairphoto:FaceId` marker, `<catalog>/<face id>` ([`face_marker`]; `catalog` is the
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
///   An unmarked region in exactly the pre-marker writer's shape ([`is_pre_marker_shape`])
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
    if catalog.is_empty() || catalog.contains('/') {
        return Err(RegionWriteError::Failed(format!("face regions not written: {catalog:?} is no catalog identity")));
    }
    let mut doc = SidecarDocument::open(photo_path)?;
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

/// The frame ChairPhoto's boxes are written in, and read back from, for one `mwg-rs:Regions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionTarget {
    /// The boxes as they are: the orientation is unknown, or the sidecar's own
    /// `AppliedToDimensions` declares the display frame.
    AsIs,
    /// The stored frame of a photo with this EXIF Orientation (MWG 2.0 § 5.9).
    Stored(u8),
}

impl RegionTarget {
    /// A display-frame face as it is written.
    fn write(self, r: &FaceRegion) -> FaceRegion {
        match self {
            Self::AsIs => r.clone(),
            Self::Stored(o) => FaceRegion { bbox: display_to_stored(o, r.bbox), ..r.clone() },
        }
    }

    /// A region as read, in the display frame.
    fn read(self, bbox: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
        match self {
            Self::AsIs => bbox,
            Self::Stored(o) => stored_to_display(o, bbox),
        }
    }
}

/// The size an `AppliedToDimensions` declares, or why it declares none ChairPhoto can use.
fn applied_dimensions(dims: &Element) -> Result<(f64, f64), String> {
    let body = struct_body(dims).ok_or("its AppliedToDimensions is not a struct")?;
    let side = |f| {
        struct_field(body, NS_STDIM, f)
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
    };
    match (side("w"), side("h")) {
        (Some(w), Some(h)) => Ok((w, h)),
        _ => Err("its AppliedToDimensions declares no usable width and height".into()),
    }
}

/// Which frame ChairPhoto's boxes go into a `Regions` whose `AppliedToDimensions` is
/// `declared` (`None`: it has none), never changing that declaration (#145):
///
/// - Unknown orientation: as they are (#136, never guessed).
/// - No declared size, or ChairPhoto's own old `1x1` stand-in, which declares no frame: the
///   stored frame, as MWG requires.
/// - A size with the stored image's aspect (the same image, perhaps resized): the stored frame.
/// - A size with the aspect swapped, on a photo whose orientation turns it a quarter (5-8):
///   the display frame that size describes — the regions already there are measured in it.
/// - Anything else — another aspect, a swapped one the orientation does not explain, or a
///   quarter-turned photo whose stored size is unknown, so which frame is declared cannot be
///   told — is `Err`: the write is refused rather than mixing frames.
fn region_target(
    frame: RegionFrame,
    declared: Option<Result<(f64, f64), String>>,
) -> Result<RegionTarget, String> {
    let Some(o) = frame.orientation else {
        return Ok(RegionTarget::AsIs);
    };
    let stored = RegionTarget::Stored(o);
    let quarter = (5..=8).contains(&o);
    let (dw, dh) = match declared {
        None => return Ok(stored),
        Some(Ok((w, h))) if w == 1.0 && h == 1.0 => return Ok(stored),
        Some(Ok(size)) => size,
        Some(Err(why)) => return Err(why),
    };
    let Some((sw, sh)) = frame.stored_size else {
        return if quarter {
            Err(format!(
                "its AppliedToDimensions {dw}x{dh} may be either frame of a photo turned a quarter \
                 (EXIF Orientation {o}), and the photo's own size is not known"
            ))
        } else {
            Ok(stored)
        };
    };
    let (sw, sh) = (f64::from(sw), f64::from(sh));
    let same_aspect = |w: f64, h: f64| ((dw / dh) / (w / h) - 1.0).abs() <= ASPECT_TOLERANCE;
    if same_aspect(sw, sh) {
        Ok(stored)
    } else if quarter && same_aspect(sh, sw) {
        Ok(RegionTarget::AsIs)
    } else {
        Err(format!(
            "its AppliedToDimensions {dw}x{dh} is not a frame of this {sw}x{sh} image \
             (EXIF Orientation {o})"
        ))
    }
}

/// How far two aspect ratios may differ and still be one image's: a resize rounds each side
/// to a whole pixel, and a RAW's recorded size can differ from a converter's by a few pixels.
const ASPECT_TOLERANCE: f64 = 0.01;

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

// --- MWG region element construction & parsing ----------------------------

/// The prefixes the region elements chairphoto builds are written with, declared on the
/// Description that holds (or will hold) `mwg-rs:Regions`. A prefix the file already binds is
/// left alone.
fn declare_region_namespaces(desc: &mut Element) {
    let ns = desc.namespaces.get_or_insert_with(Namespace::empty);
    ns.put("rdf", NS_RDF);
    ns.put("mwg-rs", NS_MWG_RS);
    ns.put("stArea", NS_STAREA);
    ns.put("stDim", NS_STDIM);
}

/// A new `mwg-rs:Regions` element with AppliedToDimensions (when the size is known) + a
/// RegionList Bag of `lis`, for a sidecar that has none.
fn new_regions(dims: Option<(u32, u32)>, lis: Vec<XMLNode>) -> Element {
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
fn new_dimensions(w: u32, h: u32) -> Element {
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

fn new_region_list(lis: Vec<XMLNode>) -> Element {
    let mut bag = el("rdf", NS_RDF, "Bag");
    bag.children.extend(lis);
    let mut region_list = el("mwg-rs", NS_MWG_RS, "RegionList");
    region_list.children.push(XMLNode::Element(bag));
    region_list
}

/// Where the parts of an existing `mwg-rs:Regions` the writer edits sit, as child indices into
/// the Regions struct's body (see [`struct_body`]). Built by [`regions_layout`], which is
/// also the check that the Regions is one chairphoto recognises.
struct RegionsLayout {
    /// `mwg-rs:AppliedToDimensions`, if present.
    dims: Option<usize>,
    /// `mwg-rs:RegionList`, if present, and its container (`rdf:Bag` / `rdf:Seq`) within it.
    list: Option<(usize, usize)>,
}

/// Check that `regions` is laid out the way chairphoto can edit without losing anything, and
/// say where its parts are. `Err` names what was not recognised.
fn regions_layout(regions: &Element) -> Result<RegionsLayout, String> {
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

/// Edit an existing, recognised `mwg-rs:Regions` in place, by the rules [`write_face_regions`]
/// documents: claim each existing region for at most one incoming face (its marker first,
/// then a marked region's Name + Area, then an unmarked region on the `legacy` record, then an
/// unmarked region's Name + Area), move what was claimed, remove the marked regions nothing
/// claimed and the legacy ones whose face is gone, and append the faces that claimed nothing,
/// marked. When something is written, `dims` becomes its AppliedToDimensions if it has none
/// (one it has is never rewritten, #145). `incoming` is already in the frame the Regions
/// declares ([`region_target`]); `legacy` is in the display frame its writes used. Everything
/// else on Regions, AppliedToDimensions, RegionList and its container is kept.
fn update_regions(
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
fn face_marker(catalog: &str, face_id: i64) -> String {
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
fn struct_body(prop: &Element) -> Option<&Element> {
    match struct_form(prop)? {
        StructForm::Resource | StructForm::Attributes => Some(prop),
        StructForm::Nested(i) => Some(element_at(prop, i)),
    }
}

fn struct_body_mut(prop: &mut Element) -> Option<&mut Element> {
    match struct_form(prop)? {
        StructForm::Resource | StructForm::Attributes => Some(prop),
        StructForm::Nested(i) => Some(element_at_mut(prop, i)),
    }
}

/// A struct field's text, whether written as an attribute or as a child element.
fn struct_field(body: &Element, ns: &str, local: &str) -> Option<String> {
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
fn set_struct_fields(prop: &mut Element, ns: &str, prefix: &str, fields: &[(&str, String)]) {
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
    let map = e.namespaces.get_or_insert_with(Namespace::empty);
    if let Some((p, _)) = map.iter().find(|(p, uri)| *uri == ns && !p.is_empty()) {
        return p.to_string();
    }
    let mut p = preferred.to_string();
    let mut n = 1;
    while map.get(&p).is_some() {
        p = format!("{preferred}{n}");
        n += 1;
    }
    map.put(p.clone(), ns);
    p
}

/// Build one `rdf:li` region struct for a face: Name + Type=Face + center-form Area.
fn region_li(r: &FaceRegion) -> XMLNode {
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
fn is_pre_marker_shape(li: &Element) -> bool {
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
fn marked_li(r: &FaceRegion, catalog: &str) -> XMLNode {
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

/// Move a matched region to `r`'s geometry, in place (#140). ChairPhoto owns only the Area's
/// `stArea:x/y/w/h/unit`: the Name already equals `r.name` (that is how it matched), and its
/// Type, any other field (`mwg-rs:Rotation`, extensions) and every foreign attribute of the
/// region or its Area (`digiKam:Confidence`, …) are kept. `li` parsed, so its struct and Area
/// forms are ones [`set_struct_fields`] can edit.
fn set_region_area(li: &mut Element, r: &FaceRegion) {
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

/// True if two top-left bboxes have centers within [`AREA_EPSILON`] on both axes.
fn center_close(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    let acx = a.0 + a.2 / 2.0;
    let acy = a.1 + a.3 / 2.0;
    let bcx = b.0 + b.2 / 2.0;
    let bcy = b.1 + b.3 / 2.0;
    (acx - bcx).abs() <= AREA_EPSILON && (acy - bcy).abs() <= AREA_EPSILON
}

/// Parse one region `rdf:li` into a [`ReadRegion`] (Name + top-left bbox from the center Area).
/// Returns `None` if the Area is missing or its coordinates are unparseable.
fn parse_region_li(li: &Element) -> Option<ReadRegion> {
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

/// Find a direct child element by (namespace, name).
fn child<'a>(parent: &'a Element, ns: &str, name: &str) -> Option<&'a Element> {
    parent.children.iter().find_map(|n| match n {
        XMLNode::Element(e) if e.namespace.as_deref() == Some(ns) && e.name == name => Some(e),
        _ => None,
    })
}

/// Every `{ns}name` property element on every top-level `rdf:Description` under `rdf`, as
/// (Description index in `rdf.children`, property index in that Description's children).
fn find_description_properties(rdf: &Element, ns: &str, name: &str) -> Vec<(usize, usize)> {
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
fn element_children(parent: &Element) -> impl Iterator<Item = (usize, &Element)> {
    parent.children.iter().enumerate().filter_map(|(i, n)| match n {
        XMLNode::Element(e) => Some((i, e)),
        _ => None,
    })
}

/// The element at `parent.children[i]`; `i` came from [`element_children`] on the same tree.
fn element_at(parent: &Element, i: usize) -> &Element {
    match &parent.children[i] {
        XMLNode::Element(e) => e,
        _ => unreachable!("index {i} was taken from an element child"),
    }
}

fn element_at_mut(parent: &mut Element, i: usize) -> &mut Element {
    match &mut parent.children[i] {
        XMLNode::Element(e) => e,
        _ => unreachable!("index {i} was taken from an element child"),
    }
}

fn is_rdf(e: &Element, name: &str) -> bool {
    e.namespace.as_deref() == Some(NS_RDF) && e.name == name
}

/// True when `e` holds non-whitespace character data (it is a literal, not a struct or array).
fn has_text(e: &Element) -> bool {
    e.children.iter().any(|n| matches!(n,
        XMLNode::Text(t) | XMLNode::CData(t) if !t.trim().is_empty()))
}

fn node_element(n: &XMLNode) -> Option<&Element> {
    match n {
        XMLNode::Element(e) => Some(e),
        _ => None,
    }
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

// --- parsing and attribute lookup -----------------------------------------

/// Parse a sidecar into an `xmltree` DOM, keeping each prefixed attribute under its
/// **qualified** name (`stArea:x`, `xmp:Rating`, `xml:lang`) — issue #138.
///
/// `xmltree::Element::parse` (0.11) keys attributes by local name only and writes those keys
/// back unprefixed, so every read-modify-write turned `digiKam:Confidence` into a
/// no-namespace `Confidence`. This is the same tree xmltree builds (same parser and config:
/// comments kept, whitespace-only text dropped), except for the attribute key. Writing it
/// back with `Element::write` stays well-formed: the writer emits a key verbatim, and every
/// parsed element carries its full in-scope namespace map, so the writer re-declares any
/// prefix that is not already in scope where the element lands.
///
/// Look such attributes up with [`ns_attr`], which matches by namespace URI, not prefix.
fn parse_xml<R: std::io::Read>(r: R) -> Result<Element, String> {
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
fn attr_is(e: &Element, key: &str, ns: &str, local: &str) -> bool {
    let Some((prefix, name)) = key.split_once(':') else {
        return false; // an unprefixed attribute is in no namespace
    };
    name == local && e.namespaces.as_ref().and_then(|n| n.get(prefix)) == Some(ns)
}

/// The namespace URI of attribute key `key` on `e` (as [`parse_xml`] stores it), or `None` for
/// an unprefixed attribute or an unbound prefix.
fn attr_ns<'a>(e: &'a Element, key: &str) -> Option<&'a str> {
    let (prefix, _) = key.split_once(':')?;
    e.namespaces.as_ref()?.get(prefix)
}

/// The value of attribute `{ns}local` on `e`, whatever prefix the file bound `ns` to.
fn ns_attr<'a>(e: &'a Element, ns: &str, local: &str) -> Option<&'a str> {
    e.attributes
        .iter()
        .find(|(k, _)| attr_is(e, k, ns, local))
        .map(|(_, v)| v.as_str())
}

// --- element construction helpers ----------------------------------------

fn el(prefix: &str, ns: &str, name: &str) -> Element {
    let mut e = Element::new(name);
    e.prefix = Some(prefix.to_string());
    e.namespace = Some(ns.to_string());
    e
}

/// A plain text property: `<prefix:name>value</prefix:name>`.
fn plain(prefix: &str, ns: &str, name: &str, value: &str) -> XMLNode {
    let mut e = el(prefix, ns, name);
    e.children.push(XMLNode::Text(value.to_string()));
    XMLNode::Element(e)
}

/// A Lang Alt property (dc namespace): `<dc:name><rdf:Alt><rdf:li xml:lang="x-default">v</rdf:li></rdf:Alt></dc:name>`.
fn lang_alt(name: &str, value: &str) -> XMLNode {
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
fn bag(prefix: &str, ns: &str, name: &str, items: &[String]) -> XMLNode {
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
fn seq_creator(value: &str) -> XMLNode {
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
fn rdf_of(root: &Element) -> Option<&Element> {
    if is_rdf(root, "RDF") {
        return Some(root);
    }
    root.get_child(("RDF", NS_RDF))
}

/// [`rdf_of`] for writing: a wrapper without one gets an `rdf:RDF`, a bare `rdf:RDF` root is
/// used as it is — never a second `rdf:RDF` nested inside the first.
fn rdf_of_mut(root: &mut Element) -> &mut Element {
    if is_rdf(root, "RDF") {
        return root;
    }
    child_mut(root, "rdf", NS_RDF, "RDF")
}

/// Whether `root` is an element a sidecar can be rooted at: the `x:xmpmeta` wrapper (or the
/// older `x:xapmeta`), or a bare `rdf:RDF`.
fn is_xmp_root(root: &Element) -> bool {
    root.namespace.as_deref() == Some(NS_X) || is_rdf(root, "RDF")
}

/// Find a child element by (namespace, name) or create it, returning a mut ref.
fn child_mut<'a>(parent: &'a mut Element, prefix: &str, ns: &str, name: &str) -> &'a mut Element {
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

fn declare_namespaces(desc: &mut Element) {
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

fn new_root() -> Element {
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

fn sidecar_backup_path(sidecar: &Path) -> PathBuf {
    let mut s = sidecar.as_os_str().to_os_string();
    s.push(".chairphoto-backup");
    PathBuf::from(s)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    /// The writing catalog's identity in these tests (`catalog::CATALOG_UUID_KEY`).
    const CAT: &str = "0b7c6e2a-1d3f-4e5a-9b8c-7d6e5f4a3b2c";

    /// This catalog's marker for face `id`, as the file holds it.
    fn ours(id: i64) -> String {
        format!("{CAT}/{id}")
    }

    use super::*;

    fn read(path: &Path) -> String {
        String::from_utf8(std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn creates_sidecar_with_iptc() {
        let dir = crate::test_support::TestTmpDir::new("xmp-create");
        let photo = dir.join("DSC1.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        let fields = IptcFields {
            description: "A ferry".into(),
            creator: "Andreas".into(),
            copyright: "(c) 2026".into(),
            city: "Trondheim".into(),
            ..Default::default()
        };
        write_iptc(&photo, &IptcFields::default(), &fields).unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("A ferry"));
        assert!(xmp.contains("Andreas"));
        assert!(xmp.contains("Trondheim"));
        assert!(xmp.contains("photoshop:City"));
        assert!(xmp.contains("dc:description"));
    }

    #[test]
    fn merge_preserves_foreign_elements() {
        let dir = crate::test_support::TestTmpDir::new("xmp-merge");
        let photo = dir.join("DSC2.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        // Simulate an existing darktable sidecar with its own namespace + element.
        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/">
   <darktable:history_end>7</darktable:history_end>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();

        let fields = IptcFields {
            description: "Edited photo".into(),
            ..Default::default()
        };
        write_iptc(&photo, &IptcFields::default(), &fields).unwrap();

        let xmp = read(&sidecar_path(&photo));
        // Our field is present AND darktable's element survived.
        assert!(xmp.contains("Edited photo"), "IPTC not written");
        assert!(xmp.contains("history_end"), "darktable data clobbered!");
        assert!(xmp.contains("darktable"), "darktable namespace lost!");
        // A backup of the original was made on first write.
        assert!(sidecar_backup_path(&sidecar_path(&photo)).exists());
    }

    #[test]
    fn identifier_round_trips_and_preserves_foreign() {
        let dir = crate::test_support::TestTmpDir::new("xmp-uuid");
        let photo = dir.join("DSC9.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        // No sidecar yet → no identifier.
        assert_eq!(read_identifier(&photo), None);

        write_identifier(&photo, "uuid-abc-123").unwrap();
        assert_eq!(read_identifier(&photo).as_deref(), Some("uuid-abc-123"));

        // A later IPTC write must not drop the identifier, and re-writing the identifier
        // keeps a single instance.
        write_iptc(&photo, &IptcFields::default(), &IptcFields { description: "x".into(), ..Default::default() }).unwrap();
        assert_eq!(read_identifier(&photo).as_deref(), Some("uuid-abc-123"));
        write_identifier(&photo, "uuid-abc-123").unwrap();
        let xmp = read(&sidecar_path(&photo));
        assert_eq!(xmp.matches("xmp:Identifier").count(), 2, "one open + one close tag only");
        assert!(xmp.contains("x"), "IPTC field survived the identifier rewrite");
    }

    /// Overwrite (issue #33) destroys an identifier chairphoto did not write, so the
    /// pre-existing sidecar must be preserved verbatim first — and everything else in that
    /// sidecar (here darktable's element) must survive the rewrite, exactly as
    /// `write_identifier` guarantees.
    #[test]
    fn overwrite_identifier_backs_up_the_foreign_sidecar_and_preserves_the_rest() {
        let dir = crate::test_support::TestTmpDir::new("xmp-overwrite-foreign");
        let photo = dir.join("DSC20.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/"
    xmlns:xmp="http://ns.adobe.com/xap/1.0/">
   <darktable:history_end>7</darktable:history_end>
   <xmp:Identifier>somebody-elses-uuid</xmp:Identifier>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();

        let backup = overwrite_identifier(&photo, "our-uuid").unwrap();
        assert_eq!(backup.as_deref(), Some(sidecar_backup_path(&sidecar_path(&photo)).as_path()));
        assert_eq!(std::fs::read_to_string(backup.unwrap()).unwrap(), existing,
            "the destroyed identifier must survive verbatim in the backup");

        assert_eq!(read_identifier(&photo).as_deref(), Some("our-uuid"));
        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("history_end"), "foreign element clobbered by the overwrite");
        assert!(!xmp.contains("somebody-elses-uuid"), "the conflicting identifier must be gone");
        assert_eq!(xmp.matches("xmp:Identifier").count(), 2, "one open + one close tag only");
    }

    /// The case plain `write_identifier` would NOT back up: a sidecar chairphoto has already
    /// written (it carries `chairphoto:LastWrite`) that nevertheless holds a foreign
    /// identifier — the ordinary result of duplicating a file after import. Overwrite must
    /// still preserve it, because it is about to destroy that identifier.
    #[test]
    fn overwrite_identifier_backs_up_even_a_chairphoto_written_sidecar() {
        let dir = crate::test_support::TestTmpDir::new("xmp-overwrite-ours");
        let photo = dir.join("DSC21.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        // chairphoto writes it once (stamping LastWrite), then the file is duplicated and
        // ends up under a row whose catalog uuid is different.
        write_identifier(&photo, "uuid-of-the-original").unwrap();
        let backup_path = sidecar_backup_path(&sidecar_path(&photo));
        assert!(!backup_path.exists(), "no backup yet: nothing existed before the first write");
        let before = read(&sidecar_path(&photo));

        let backup = overwrite_identifier(&photo, "uuid-of-the-duplicate").unwrap();
        assert_eq!(backup.as_deref(), Some(backup_path.as_path()),
            "a chairphoto-written sidecar must still be backed up before Overwrite destroys \
             an identifier — `chairphoto:LastWrite` does not make the identifier ours");
        assert_eq!(std::fs::read_to_string(&backup_path).unwrap(), before);
        assert_eq!(read_identifier(&photo).as_deref(), Some("uuid-of-the-duplicate"));
    }

    /// A backup that already exists is the earliest state we ever saw. A later Overwrite
    /// must not trade it away for a newer, chairphoto-written one.
    #[test]
    fn overwrite_identifier_never_replaces_an_existing_backup() {
        let dir = crate::test_support::TestTmpDir::new("xmp-overwrite-keeps-backup");
        let photo = dir.join("DSC22.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        let original = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/">
   <xmp:Identifier>first-foreign-uuid</xmp:Identifier>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), original).unwrap();

        overwrite_identifier(&photo, "second-uuid").unwrap();
        let backup_path = sidecar_backup_path(&sidecar_path(&photo));
        assert_eq!(std::fs::read_to_string(&backup_path).unwrap(), original);

        // A second Overwrite reports no new backup and leaves the original snapshot intact.
        let backup = overwrite_identifier(&photo, "third-uuid").unwrap();
        assert_eq!(backup, None, "an existing backup is left alone, and reported as such");
        assert_eq!(std::fs::read_to_string(&backup_path).unwrap(), original,
            "the earliest snapshot must survive later overwrites");
        assert_eq!(read_identifier(&photo).as_deref(), Some("third-uuid"));
    }

    #[test]
    fn rewrites_not_duplicates_on_second_write() {
        let dir = crate::test_support::TestTmpDir::new("xmp-rewrite");
        let photo = dir.join("DSC3.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        let first = IptcFields { headline: "First".into(), ..Default::default() };
        write_iptc(&photo, &IptcFields::default(), &first).unwrap();
        write_iptc(&photo, &first, &IptcFields { headline: "Second".into(), ..Default::default() }).unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("Second"));
        assert!(!xmp.contains("First"), "old value should be replaced, not duplicated");
        assert_eq!(xmp.matches("photoshop:Headline").count(), 2); // open + close tag, once
    }

    /// Issue #144: the geocoder fills an empty city, and writes nothing else. The creator,
    /// rights, caption, title, headline and country code another tool wrote — values the
    /// catalog never imported, so they are empty there before and after — must survive, in
    /// a Lightroom single-Description sidecar and in exiftool's one-Description-per-namespace
    /// layout alike. The city itself changed, so the foreign one is replaced.
    #[test]
    fn a_city_only_write_keeps_every_other_foreign_iptc_field() {
        use test_fixtures::{assert_non_iptc_intact, foreign_iptc, iptc, with, FOREIGN};
        for (layout, sidecar) in FOREIGN {
            let dir = crate::test_support::TestTmpDir::new("xmp-144-city-only");
            let photo = dir.join("DSC144.ARW");
            std::fs::write(&photo, b"raw").unwrap();
            std::fs::write(sidecar_path(&photo), sidecar).unwrap();

            let filled = IptcFields { city: "Trondheim".into(), ..Default::default() };
            write_iptc(&photo, &IptcFields::default(), &filled).unwrap();

            let xml = read(&sidecar_path(&photo));
            assert_eq!(iptc(&xml), with(foreign_iptc(), "photoshop:City", &["Trondheim"]),
                "{layout}:\n{xml}");
            assert_non_iptc_intact(&xml, layout);
        }
    }

    /// A write that changes no field does not open the sidecar: no stamp, no backup, the
    /// file byte-identical.
    #[test]
    fn an_unchanged_write_leaves_the_sidecar_alone() {
        let dir = crate::test_support::TestTmpDir::new("xmp-144-unchanged");
        let photo = dir.join("DSC144.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        std::fs::write(sidecar_path(&photo), test_fixtures::LIGHTROOM).unwrap();

        let same = IptcFields { title: "Mine".into(), ..Default::default() };
        write_iptc(&photo, &same, &same).unwrap();

        assert_eq!(read(&sidecar_path(&photo)), test_fixtures::LIGHTROOM);
        assert!(!sidecar_backup_path(&sidecar_path(&photo)).exists());
    }

    /// Every catalog field lands in its own IPTC property — checked against a table written
    /// out here, not derived from `MANAGED`, so a swapped mapping (Credit written as Source,
    /// say) fails. Each field carries a distinct value.
    #[test]
    fn every_iptc_field_maps_to_its_own_property() {
        let dir = crate::test_support::TestTmpDir::new("xmp-144-mapping");
        let photo = dir.join("DSC144.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        let fields = IptcFields {
            description: "v-description".into(),
            headline: "v-headline".into(),
            title: "v-title".into(),
            creator: "v-creator".into(),
            copyright: "v-copyright".into(),
            credit: "v-credit".into(),
            source: "v-source".into(),
            city: "v-city".into(),
            state: "v-state".into(),
            country: "v-country".into(),
            country_code: "v-country_code".into(),
        };
        write_iptc(&photo, &IptcFields::default(), &fields).unwrap();

        let xml = read(&sidecar_path(&photo));
        let expected = [
            (NS_DC, "description", "v-description"),
            (NS_DC, "title", "v-title"),
            (NS_DC, "rights", "v-copyright"),
            (NS_DC, "creator", "v-creator"),
            (NS_PHOTOSHOP, "Headline", "v-headline"),
            (NS_PHOTOSHOP, "Credit", "v-credit"),
            (NS_PHOTOSHOP, "Source", "v-source"),
            (NS_PHOTOSHOP, "City", "v-city"),
            (NS_PHOTOSHOP, "State", "v-state"),
            (NS_PHOTOSHOP, "Country", "v-country"),
            (NS_IPTC, "CountryCode", "v-country_code"),
        ];
        for (ns, name, value) in expected {
            assert_eq!(test_fixtures::property_values(&xml, ns, name), [value], "{name}:\n{xml}");
        }
    }

    #[test]
    fn import_batch_round_trips_and_preserves_foreign() {
        let dir = crate::test_support::TestTmpDir::new("xmp-batch");
        let photo = dir.join("DSC10.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        // No sidecar yet → no batch.
        assert_eq!(read_import_batch(&photo), None);

        write_import_batch(&photo, "batch-uuid-001").unwrap();
        assert_eq!(read_import_batch(&photo).as_deref(), Some("batch-uuid-001"));

        // Writing the batch UUID a second time replaces, not duplicates.
        write_import_batch(&photo, "batch-uuid-001").unwrap();
        let xmp = read(&sidecar_path(&photo));
        assert_eq!(
            xmp.matches("chairphoto:ImportBatch").count(),
            2, // open + close tag, once
            "batch field must not be duplicated"
        );

        // A later IPTC write must not drop the batch field.
        write_iptc(&photo, &IptcFields::default(), &IptcFields { description: "boat".into(), ..Default::default() }).unwrap();
        assert_eq!(read_import_batch(&photo).as_deref(), Some("batch-uuid-001"),
            "batch uuid must survive an IPTC write");

        // And a new write_identifier must not drop the batch field either.
        write_identifier(&photo, "uuid-photo-001").unwrap();
        assert_eq!(read_import_batch(&photo).as_deref(), Some("batch-uuid-001"),
            "batch uuid must survive an identifier write");
    }

    #[test]
    fn import_batch_preserves_foreign_elements() {
        let dir = crate::test_support::TestTmpDir::new("xmp-batch-foreign");
        let photo = dir.join("DSC11.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        // Simulate an existing darktable sidecar.
        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/">
   <darktable:history_end>3</darktable:history_end>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();

        write_import_batch(&photo, "batch-uuid-002").unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("batch-uuid-002"), "batch UUID not written");
        assert!(xmp.contains("history_end"), "darktable data clobbered by batch write!");
        assert!(xmp.contains("darktable"), "darktable namespace lost by batch write!");
        // A backup must have been made on first-ever write to a foreign sidecar.
        assert!(sidecar_backup_path(&sidecar_path(&photo)).exists());
    }

    // ── MWG face regions (H13f) ────────────────────────────────────────────────

    fn region_dir(tag: &str) -> crate::test_support::TestTmpDir {
        crate::test_support::TestTmpDir::new(tag)
    }

    /// An upright photo (EXIF Orientation 1) of a known stored size.
    fn sized(w: u32, h: u32) -> RegionFrame {
        RegionFrame { orientation: Some(1), stored_size: Some((w, h)) }
    }

    /// Write regions, read them back: names + top-left bboxes survive the center↔corner
    /// conversion round-trip, and AppliedToDimensions is written with the stored pixel size.
    #[test]
    fn face_regions_round_trip() {
        let dir = region_dir("xmp-regions-rt");
        let photo = dir.join("DSC20.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        let regions = vec![
            FaceRegion { face_id: 0, name: "Alice".into(), bbox: (0.10, 0.20, 0.30, 0.40) },
            FaceRegion { face_id: 0, name: "Bob".into(), bbox: (0.60, 0.10, 0.20, 0.25) },
        ];
        write_face_regions(&photo, CAT, &regions, &[], &[], sized(6000, 4000)).unwrap();

        let xmp = read(&sidecar_path(&photo));
        // Center coords are written (x = 0.10 + 0.30/2 = 0.25), unit=normalized, Type=Face.
        assert!(xmp.contains("mwg-rs:Regions"), "Regions element missing");
        assert!(xmp.contains("stArea:unit"), "Area unit missing");
        assert!(xmp.contains("6000"), "AppliedToDimensions width missing");
        assert!(xmp.contains("4000"), "AppliedToDimensions height missing");
        assert!(xmp.contains(">Face<") || xmp.contains("Face"), "Type=Face missing");

        let back = read_face_regions(&photo);
        assert_eq!(back.len(), 2, "both regions read back");
        let alice = back.iter().find(|r| r.name == "Alice").unwrap();
        for (a, b) in [alice.bbox.0, alice.bbox.1, alice.bbox.2, alice.bbox.3]
            .iter()
            .zip([0.10f32, 0.20, 0.30, 0.40].iter())
        {
            assert!((a - b).abs() < 1e-4, "Alice bbox drift: {a} vs {b}");
        }
        let bob = back.iter().find(|r| r.name == "Bob").unwrap();
        assert!((bob.bbox.0 - 0.60).abs() < 1e-4);
        assert!((bob.bbox.1 - 0.10).abs() < 1e-4);
    }

    /// The center↔top-left conversion is exact: a region's stArea:x/y equal the bbox center.
    #[test]
    fn face_region_center_conversion() {
        let dir = region_dir("xmp-regions-center");
        let photo = dir.join("DSC21.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        // Top-left (0.2, 0.3), size (0.4, 0.2) → center (0.4, 0.4).
        let regions = vec![FaceRegion { face_id: 0, name: "Cara".into(), bbox: (0.2, 0.3, 0.4, 0.2) }];
        write_face_regions(&photo, CAT, &regions, &[], &[], sized(1000, 1000)).unwrap();

        let xmp = read(&sidecar_path(&photo));
        // The literal center coordinates must be present (0.4 for both x and y).
        assert!(xmp.contains("stArea:x"));
        assert!(xmp.contains("stArea:y"));
        // Read-back reconstructs the exact top-left corner.
        let back = read_face_regions(&photo);
        assert_eq!(back.len(), 1);
        assert!((back[0].bbox.0 - 0.2).abs() < 1e-5, "corner x");
        assert!((back[0].bbox.1 - 0.3).abs() < 1e-5, "corner y");
        assert!((back[0].bbox.2 - 0.4).abs() < 1e-5, "w");
        assert!((back[0].bbox.3 - 0.2).abs() < 1e-5, "h");
    }

    /// A sidecar carrying darktable foreign namespaces AND a foreign region entry (a face named
    /// by another tool): both survive a chairphoto region write. Our region is added; the foreign
    /// region and darktable data are untouched.
    #[test]
    fn face_regions_preserve_foreign_content() {
        let dir = region_dir("xmp-regions-foreign");
        let photo = dir.join("DSC22.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        // Existing sidecar: darktable develop history + a digiKam-style foreign face region.
        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:darktable="http://darktable.sf.net/"
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
    xmlns:stDim="http://ns.adobe.com/xap/1.0/sType/Dimensions#">
   <darktable:history_end>9</darktable:history_end>
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:AppliedToDimensions rdf:parseType="Resource">
     <stDim:w>6000</stDim:w>
     <stDim:h>4000</stDim:h>
     <stDim:unit>pixel</stDim:unit>
    </mwg-rs:AppliedToDimensions>
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Stranger</mwg-rs:Name>
       <mwg-rs:Type>Face</mwg-rs:Type>
       <mwg-rs:Area rdf:parseType="Resource">
        <stArea:x>0.8</stArea:x>
        <stArea:y>0.8</stArea:y>
        <stArea:w>0.1</stArea:w>
        <stArea:h>0.1</stArea:h>
        <stArea:unit>normalized</stArea:unit>
       </mwg-rs:Area>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();

        // Write one chairphoto region (a different face).
        let regions = vec![FaceRegion { face_id: 0, name: "Alice".into(), bbox: (0.10, 0.10, 0.20, 0.20) }];
        write_face_regions(&photo, CAT, &regions, &[], &[], sized(6000, 4000)).unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("history_end"), "darktable data clobbered!");
        assert!(xmp.contains("darktable"), "darktable namespace lost!");

        // A backup of the foreign sidecar was made on first write.
        assert!(sidecar_backup_path(&sidecar_path(&photo)).exists());

        // Both the foreign "Stranger" region and our "Alice" region are present.
        let back = read_face_regions(&photo);
        let names: Vec<&str> = back.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"Stranger"), "foreign region lost! got {names:?}");
        assert!(names.contains(&"Alice"), "our region missing! got {names:?}");
        assert_eq!(back.len(), 2, "exactly the foreign + our region");

        // The foreign region's geometry is unchanged (center 0.8,0.8 → corner 0.75,0.75).
        let stranger = back.iter().find(|r| r.name == "Stranger").unwrap();
        assert!((stranger.bbox.0 - 0.75).abs() < 1e-4, "foreign corner x moved");
        assert!((stranger.bbox.2 - 0.1).abs() < 1e-4, "foreign w moved");
    }

    /// Re-writing updates only chairphoto's own region: a second write with a moved Alice bbox
    /// replaces Alice (no duplicate) and still preserves the foreign region.
    #[test]
    fn face_regions_rewrite_updates_only_ours() {
        let dir = region_dir("xmp-regions-rewrite");
        let photo = dir.join("DSC23.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        // First write: a foreign region (simulated by pre-writing) + our Alice.
        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#">
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Stranger</mwg-rs:Name>
       <mwg-rs:Area rdf:parseType="Resource">
        <stArea:x>0.9</stArea:x><stArea:y>0.9</stArea:y>
        <stArea:w>0.05</stArea:w><stArea:h>0.05</stArea:h>
        <stArea:unit>normalized</stArea:unit>
       </mwg-rs:Area>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();

        // Write Alice at (0.10, 0.10, 0.20, 0.20), center (0.20, 0.20).
        write_face_regions(
            &photo, CAT,
            &[FaceRegion { face_id: 0, name: "Alice".into(), bbox: (0.10, 0.10, 0.20, 0.20) }], &[],
            &[],
            sized(1000, 1000),
        )
        .unwrap();

        // Second write: Alice moved slightly (well within AREA_EPSILON so it's recognised as
        // "our" region and replaced, not duplicated).
        write_face_regions(
            &photo, CAT,
            &[FaceRegion { face_id: 0, name: "Alice".into(), bbox: (0.105, 0.105, 0.20, 0.20) }], &[],
            &[],
            sized(1000, 1000),
        )
        .unwrap();

        let back = read_face_regions(&photo);
        // Exactly two regions: the foreign one + one Alice (not two Alices).
        let alice_count = back.iter().filter(|r| r.name == "Alice").count();
        assert_eq!(alice_count, 1, "Alice must be replaced, not duplicated");
        assert!(back.iter().any(|r| r.name == "Stranger"), "foreign region lost on rewrite");
        assert_eq!(back.len(), 2);
        // The kept Alice is the newest (moved) position.
        let alice = back.iter().find(|r| r.name == "Alice").unwrap();
        assert!((alice.bbox.0 - 0.105).abs() < 1e-3, "Alice not updated to new position");

        // Exactly one Regions/AppliedToDimensions block survives (no stacking).
        let xmp = read(&sidecar_path(&photo));
        assert_eq!(xmp.matches("<mwg-rs:Regions").count(), 1, "one Regions block only");
    }

    /// region_iou returns 1.0 for identical boxes and 0.0 for disjoint boxes.
    #[test]
    fn region_iou_basic() {
        let a = (0.1, 0.1, 0.2, 0.2);
        assert!((region_iou(a, a) - 1.0).abs() < 1e-6);
        let b = (0.7, 0.7, 0.2, 0.2);
        assert!(region_iou(a, b) < 1e-6);
        // Half-overlap-ish box has IoU strictly between 0 and 1.
        let c = (0.2, 0.1, 0.2, 0.2);
        let iou = region_iou(a, c);
        assert!(iou > 0.0 && iou < 1.0, "partial overlap IoU = {iou}");
    }

    /// Reading regions from a sidecar with no Regions property (or no sidecar) yields empty.
    #[test]
    fn read_face_regions_absent_is_empty() {
        let dir = region_dir("xmp-regions-absent");
        let photo = dir.join("DSC24.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        // No sidecar.
        assert!(read_face_regions(&photo).is_empty());
        // Sidecar with only IPTC, no regions.
        write_iptc(&photo, &IptcFields::default(), &IptcFields { description: "x".into(), ..Default::default() }).unwrap();
        assert!(read_face_regions(&photo).is_empty());
    }

    // ── write_gps (issue #62) ───────────────────────────────────────────────
    //
    // These pin `write_gps`'s DMS+ref string formatting directly against the raw sidecar
    // text — NOT via `read_gps`/`dms_to_decimal` — because a round-trip through our own
    // parser would still pass if both the write and read directions had the same sign or
    // hemisphere bug. Expected strings below were computed by running the actual
    // `decimal_to_dms_lat`/`decimal_to_dms_lng` formulas (not by hand) to avoid encoding an
    // arithmetic mistake as the "expected" value.

    fn gps_dir(tag: &str) -> crate::test_support::TestTmpDir {
        crate::test_support::TestTmpDir::new(tag)
    }

    /// A northern + eastern coordinate (Oslo): the emitted `exif:GPSLatitude` /
    /// `exif:GPSLongitude` strings must match exactly, hemisphere letters included.
    #[test]
    fn write_gps_pins_north_east_dms() {
        let dir = gps_dir("xmp-gps-ne");
        let photo = dir.join("DSC30.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        write_gps(&photo, 59.9139, 10.7522).unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(
            xmp.contains("<exif:GPSLatitude>59,54.834000N</exif:GPSLatitude>"),
            "unexpected latitude encoding in: {xmp}"
        );
        assert!(
            xmp.contains("<exif:GPSLongitude>10,45.132000E</exif:GPSLongitude>"),
            "unexpected longitude encoding in: {xmp}"
        );
    }

    /// A southern + western coordinate (Santiago): catches a sign or hemisphere-reference
    /// error that a northern/eastern-only test cannot — e.g. a flipped `>= 0.0` check would
    /// still pass `write_gps_pins_north_east_dms` but fail here.
    #[test]
    fn write_gps_pins_south_west_dms() {
        let dir = gps_dir("xmp-gps-sw");
        let photo = dir.join("DSC31.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        write_gps(&photo, -33.4489, -70.6693).unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(
            xmp.contains("<exif:GPSLatitude>33,26.934000S</exif:GPSLatitude>"),
            "unexpected latitude encoding in: {xmp}"
        );
        assert!(
            xmp.contains("<exif:GPSLongitude>70,40.158000W</exif:GPSLongitude>"),
            "unexpected longitude encoding in: {xmp}"
        );
    }

    /// The equator / prime-meridian origin: both components are exactly zero, and the sign
    /// check (`>= 0.0`) must still resolve them to N/E, not leave them unsigned or flip them.
    #[test]
    fn write_gps_pins_equator_and_prime_meridian() {
        let dir = gps_dir("xmp-gps-origin");
        let photo = dir.join("DSC32.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        write_gps(&photo, 0.0, 0.0).unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("<exif:GPSLatitude>0,0.000000N</exif:GPSLatitude>"));
        assert!(xmp.contains("<exif:GPSLongitude>0,0.000000E</exif:GPSLongitude>"));
    }

    /// A coordinate whose minutes round awkwardly: `10.1` degrees is not exactly
    /// representable in `f64`, so `(0.1 * 60)` lands on `5.999999999999978`, not `6.0`. The
    /// `{:.6}` formatter must still round that up to a clean `"6.000000"` rather than
    /// truncating or emitting the raw float noise.
    #[test]
    fn write_gps_pins_awkward_rounding() {
        let dir = gps_dir("xmp-gps-awkward");
        let photo = dir.join("DSC33.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        write_gps(&photo, 10.1, 20.1).unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(
            xmp.contains("<exif:GPSLatitude>10,6.000000N</exif:GPSLatitude>"),
            "unexpected latitude encoding in: {xmp}"
        );
        assert!(
            xmp.contains("<exif:GPSLongitude>20,6.000000E</exif:GPSLongitude>"),
            "unexpected longitude encoding in: {xmp}"
        );
    }

    /// `write_gps` is a plain-property writer like `write_iptc`/`write_import_batch`: it must
    /// go through the standard merge-safe path (foreign elements + namespaces preserved,
    /// pre-existing foreign sidecar backed up once) and touch only its own two fields on a
    /// rewrite — no duplication, no clobbering of the previous coordinate's stale value.
    #[test]
    fn write_gps_preserves_foreign_elements_backs_up_and_rewrites_cleanly() {
        let dir = gps_dir("xmp-gps-merge");
        let photo = dir.join("DSC34.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/">
   <darktable:history_end>5</darktable:history_end>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();

        write_gps(&photo, 63.4305, 10.3951).unwrap(); // Trondheim (N/E)

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("<exif:GPSLatitude>63,25.830000N</exif:GPSLatitude>"));
        assert!(xmp.contains("<exif:GPSLongitude>10,23.706000E</exif:GPSLongitude>"));
        assert!(xmp.contains("history_end"), "darktable data clobbered by GPS write!");
        assert!(xmp.contains("darktable"), "darktable namespace lost by GPS write!");
        assert!(
            sidecar_backup_path(&sidecar_path(&photo)).exists(),
            "foreign sidecar must be backed up on first write"
        );

        // Rewrite with a different coordinate: the old value must be gone, the new one
        // present exactly once, and darktable's data still untouched.
        write_gps(&photo, -1.0, -1.0).unwrap();
        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("<exif:GPSLatitude>1,0.000000S</exif:GPSLatitude>"));
        assert!(xmp.contains("<exif:GPSLongitude>1,0.000000W</exif:GPSLongitude>"));
        assert!(!xmp.contains("63,25.830000N"), "stale latitude must not survive a rewrite");
        assert!(!xmp.contains("10,23.706000E"), "stale longitude must not survive a rewrite");
        assert_eq!(xmp.matches("exif:GPSLatitude").count(), 2, "one open + one close tag only");
        assert_eq!(xmp.matches("exif:GPSLongitude").count(), 2, "one open + one close tag only");
        assert!(xmp.contains("history_end"), "darktable data clobbered by GPS rewrite!");
    }

    // ── write_keywords (issue #62) ──────────────────────────────────────────

    fn keywords_dir(tag: &str) -> crate::test_support::TestTmpDir {
        crate::test_support::TestTmpDir::new(tag)
    }

    /// Parse the sidecar and return the `rdf:li` text values of the `rdf:Bag` under the
    /// (namespace, name) property on `rdf:Description` — used to check that flat keywords
    /// land under `dc:subject` and hierarchical ones under `lr:hierarchicalSubject`, not
    /// swapped or merged together.
    fn bag_items(xmp: &str, ns: &str, name: &str) -> Vec<String> {
        let root = parse_xml(xmp.as_bytes()).unwrap();
        let rdf = root.get_child(("RDF", NS_RDF)).unwrap();
        for node in &rdf.children {
            let XMLNode::Element(desc) = node else { continue };
            if desc.name != "Description" {
                continue;
            }
            if let Some(prop) = child(desc, ns, name) {
                if let Some(bag_el) = prop.get_child(("Bag", NS_RDF)) {
                    return bag_el
                        .children
                        .iter()
                        .filter_map(|n| match n {
                            XMLNode::Element(li) => first_text(li),
                            _ => None,
                        })
                        .collect();
                }
            }
        }
        Vec::new()
    }

    /// Flat keywords land in `dc:subject` and hierarchical keywords land in
    /// `lr:hierarchicalSubject` — each in its own element, not swapped or merged.
    #[test]
    fn write_keywords_flat_and_hierarchical_land_in_right_elements() {
        let dir = keywords_dir("xmp-kw-elements");
        let photo = dir.join("DSC40.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        let flat = vec!["Sunset".to_string(), "Beach".to_string()];
        let hierarchical = vec![
            "Nature|Landscape".to_string(),
            "Nature|Landscape|Sunset".to_string(),
        ];
        write_keywords(&photo, &flat, &hierarchical).unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("dc:subject"), "dc:subject missing");
        assert!(xmp.contains("lr:hierarchicalSubject"), "lr:hierarchicalSubject missing");

        assert_eq!(bag_items(&xmp, NS_DC, "subject"), flat, "dc:subject content mismatch");
        assert_eq!(
            bag_items(&xmp, NS_LR, "hierarchicalSubject"),
            hierarchical,
            "lr:hierarchicalSubject content mismatch"
        );
    }

    /// A pre-existing foreign sidecar's elements and namespace survive a keyword write —
    /// same merge-safety invariant as the other managed-property writers.
    #[test]
    fn write_keywords_foreign_elements_survive() {
        let dir = keywords_dir("xmp-kw-foreign");
        let photo = dir.join("DSC41.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/">
   <darktable:history_end>4</darktable:history_end>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();

        write_keywords(
            &photo,
            &["Ferry".to_string()],
            &["Places|Norway".to_string()],
        )
        .unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("Ferry"), "flat keyword not written");
        assert!(xmp.contains("Places|Norway"), "hierarchical keyword not written");
        assert!(xmp.contains("history_end"), "darktable data clobbered by keyword write!");
        assert!(xmp.contains("darktable"), "darktable namespace lost by keyword write!");
    }

    /// The documented exemption (AGENTS.md "Export-only destination copies are not subject
    /// to this rule", `document.rs:24`): `write_keywords` uses `open_no_backup`, so even a
    /// foreign, never-before-seen sidecar must NOT be backed up. This is the one property
    /// that was previously only a comment, not a checked behavior.
    #[test]
    fn write_keywords_does_not_back_up_export_destination() {
        let dir = keywords_dir("xmp-kw-no-backup");
        let photo = dir.join("DSC42.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        let existing = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/">
   <darktable:history_end>2</darktable:history_end>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        std::fs::write(sidecar_path(&photo), existing).unwrap();
        let backup = sidecar_backup_path(&sidecar_path(&photo));
        assert!(!backup.exists());

        write_keywords(&photo, &["Ferry".to_string()], &[]).unwrap();

        assert!(
            !backup.exists(),
            "export-destination write must never create a .chairphoto-backup file"
        );
        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("Ferry"), "keyword still must be written");
    }

    /// Re-writing keywords replaces, not duplicates, both properties.
    #[test]
    fn write_keywords_rewrite_replaces_not_duplicates() {
        let dir = keywords_dir("xmp-kw-rewrite");
        let photo = dir.join("DSC43.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        write_keywords(
            &photo,
            &["First".to_string()],
            &["Old|Path".to_string()],
        )
        .unwrap();
        write_keywords(
            &photo,
            &["Second".to_string()],
            &["New|Path".to_string()],
        )
        .unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("Second"));
        assert!(!xmp.contains("First"), "old flat keyword should be replaced, not duplicated");
        assert!(xmp.contains("New|Path"));
        assert!(!xmp.contains("Old|Path"), "old hierarchical keyword should be replaced, not duplicated");
        assert_eq!(xmp.matches("dc:subject").count(), 2, "one open + one close tag only");
        assert_eq!(xmp.matches("lr:hierarchicalSubject").count(), 2, "one open + one close tag only");
    }

    // ── decimal_to_dms_lat / decimal_to_dms_lng minute rollover (issue #65) ────────────
    //
    // `decimal_to_dms_lat(45.99999999999999289457)` used to format as `"45,60.000000N"`:
    // the degree is taken by `trunc()` before the minutes are rounded by `{:.6}`, so a
    // value a few ULPs below a whole degree rounded its minutes up to 60 with no path back
    // to the degree. Sixty minutes is one degree, so that string was malformed, not merely
    // imprecise. These pin the carry directly, plus the poles/antimeridian decision, plus
    // that #62's pins (asserted above, in the write_gps block) are unaffected.

    /// The exact repro from issue #65: a latitude a few ULPs below 46 degrees must carry,
    /// not emit `60.000000` minutes.
    #[test]
    fn decimal_to_dms_lat_carries_minutes_into_degree() {
        assert_eq!(decimal_to_dms_lat(45.99999999999999289457), "46,0.000000N");
    }

    /// The longitude twin of the carry case: `decimal_to_dms_lng` has the identical
    /// trunc-before-round shape and needs its own coverage, not just its sibling's.
    #[test]
    fn decimal_to_dms_lng_carries_minutes_into_degree() {
        assert_eq!(decimal_to_dms_lng(45.99999999999999289457), "46,0.000000E");
    }

    /// A carry in the southern hemisphere must still carry the degree, and must not flip
    /// or drop the hemisphere letter while doing it.
    #[test]
    fn decimal_to_dms_lat_carries_in_southern_hemisphere() {
        assert_eq!(decimal_to_dms_lat(-45.99999999999999289457), "46,0.000000S");
    }

    /// A carry in the western hemisphere must still carry the degree, and must not flip
    /// or drop the hemisphere letter while doing it.
    #[test]
    fn decimal_to_dms_lng_carries_in_western_hemisphere() {
        assert_eq!(decimal_to_dms_lng(-45.99999999999999289457), "46,0.000000W");
    }

    /// A latitude a few ULPs below 90 must carry cleanly into the pole: 90°N is a real,
    /// representable point, so `"90,0.000000N"` is the correct output, not a value to
    /// reject or clamp away from.
    #[test]
    fn decimal_to_dms_lat_carries_into_north_pole() {
        assert_eq!(decimal_to_dms_lat(89.99999999999999289457), "90,0.000000N");
    }

    /// Same carry, southern pole: must land on `S`, not `N`.
    #[test]
    fn decimal_to_dms_lat_carries_into_south_pole() {
        assert_eq!(decimal_to_dms_lat(-89.99999999999999289457), "90,0.000000S");
    }

    /// A longitude a few ULPs below 180 must carry cleanly onto the antimeridian.
    /// `decimal_to_dms_lng` keeps whatever hemisphere the input's sign implied — `E` for a
    /// non-negative input — rather than special-casing 180 to a fixed letter.
    ///
    /// `179.99999999999997` is the literal used deliberately: the ULP near 180 is coarser
    /// than near 46 or 90, so the naive next-most-precise literal
    /// (`179.99999999999999289457`, following the same digit pattern as the lat/45 and
    /// lat/90 cases) actually parses to exactly `180.0_f64` — it would exercise the
    /// "already at the boundary" path, not the carry, and would pass even against the
    /// un-fixed code. This value is `f64::from_bits(180.0_f64.to_bits() - 1)`, confirmed
    /// by bit-walk to be the largest `f64` strictly less than 180.0, so the un-fixed code
    /// truncates it to `179,60.000000E`.
    #[test]
    fn decimal_to_dms_lng_carries_into_antimeridian_east() {
        assert_eq!(decimal_to_dms_lng(179.99999999999997), "180,0.000000E");
    }

    /// Same carry from the negative side: keeps `W`, matching the input's sign.
    #[test]
    fn decimal_to_dms_lng_carries_into_antimeridian_west() {
        assert_eq!(decimal_to_dms_lng(-179.99999999999997), "180,0.000000W");
    }

    /// #62's awkward-rounding pin, re-asserted directly against the conversion functions
    /// (not just via `write_gps`'s sidecar text): `(0.1 * 60)` lands on
    /// `5.999999999999978`, which must still round to a clean `"6.000000"` and must NOT be
    /// mistaken for a carry by the new rollover logic. This is the case #65 warns a naive
    /// fix could regress.
    #[test]
    fn decimal_to_dms_lat_awkward_rounding_does_not_spuriously_carry() {
        assert_eq!(decimal_to_dms_lat(10.1), "10,6.000000N");
    }

    // ── prefixed attributes survive every writer (issue #138) ───────────────

    const NS_XML: &str = "http://www.w3.org/XML/1998/namespace";
    const NS_DARKTABLE: &str = "http://darktable.sf.net/";
    const NS_CRS: &str = "http://ns.adobe.com/camera-raw-settings/1.0/";
    const NS_DIGIKAM: &str = "http://www.digikam.org/ns/1.0/";

    /// A foreign sidecar written by hand — not by ChairPhoto — whose foreign data is carried
    /// in prefixed *attributes*: Exiv2's `x:xmptk`, compact `xmp:`/`darktable:`/`crs:`
    /// properties on `rdf:Description`, an MWG region whose Area is in the attribute
    /// (shorthand) form with a `digiKam:` extra, and a foreign Lang Alt's `xml:lang`.
    const FOREIGN_PREFIXED_ATTRS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="XMP Core 4.4.0-Exiv2">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:darktable="http://darktable.sf.net/"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    xmlns:xmpRights="http://ns.adobe.com/xap/1.0/rights/"
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
    xmlns:digiKam="http://www.digikam.org/ns/1.0/"
    xmp:Rating="1"
    darktable:xmp_version="5"
    darktable:auto_presets_applied="1"
    crs:Exposure2012="+0.50">
   <xmpRights:UsageTerms>
    <rdf:Alt>
     <rdf:li xml:lang="x-default">All rights reserved</rdf:li>
    </rdf:Alt>
   </xmpRights:UsageTerms>
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Bob</mwg-rs:Name>
       <mwg-rs:Type>Face</mwg-rs:Type>
       <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2"
         stArea:unit="normalized" digiKam:Confidence="87"/>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

    /// Every attribute in `xml`, namespace-resolved by an independent namespace-aware reader
    /// (xml-rs's event reader, not ChairPhoto's parser): `(element ns, element local,
    /// attr ns, attr local, value)`, `""` meaning "no namespace". Panics if `xml` is not
    /// well-formed, which includes using a prefix that is not declared in scope.
    fn namespaced_attributes(xml: &str) -> Vec<(String, String, String, String, String)> {
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

    fn assert_foreign_attributes_intact(xml: &str, after: &str) {
        let attrs = namespaced_attributes(xml);
        let expected = [
            (NS_X, "xmpmeta", NS_X, "xmptk", "XMP Core 4.4.0-Exiv2"),
            (NS_RDF, "Description", NS_RDF, "about", ""),
            (NS_RDF, "Description", NS_XMP, "Rating", "1"),
            (NS_RDF, "Description", NS_DARKTABLE, "xmp_version", "5"),
            (NS_RDF, "Description", NS_DARKTABLE, "auto_presets_applied", "1"),
            (NS_RDF, "Description", NS_CRS, "Exposure2012", "+0.50"),
            (NS_RDF, "li", NS_XML, "lang", "x-default"),
            (NS_MWG_RS, "Area", NS_STAREA, "x", "0.8"),
            (NS_MWG_RS, "Area", NS_STAREA, "y", "0.7"),
            (NS_MWG_RS, "Area", NS_STAREA, "w", "0.1"),
            (NS_MWG_RS, "Area", NS_STAREA, "h", "0.2"),
            (NS_MWG_RS, "Area", NS_STAREA, "unit", "normalized"),
            (NS_MWG_RS, "Area", NS_DIGIKAM, "Confidence", "87"),
        ];
        for (ens, el_name, ans, an, v) in expected {
            let found = attrs.iter().any(|(e_ns, e_l, a_ns, a_l, val)| {
                e_ns == ens && e_l == el_name && a_ns == ans && a_l == an && val == v
            });
            assert!(found, "after {after}: {{{ans}}}{an}=\"{v}\" on {{{ens}}}{el_name} was lost \
                or moved to another namespace\nattributes: {attrs:#?}\n{xml}");
        }
        // No attribute was demoted to no-namespace (the #138 symptom: `x`, `Rating`, …).
        // Every attribute in the fixture and in ChairPhoto's own output is prefixed.
        let stripped: Vec<_> = attrs.iter().filter(|(_, _, a_ns, _, _)| a_ns.is_empty()).collect();
        assert!(stripped.is_empty(), "after {after}: attributes lost their namespace: {stripped:?}");
    }

    /// Issue #138: every ChairPhoto sidecar writer read-modify-writes the sidecar, and must
    /// keep each foreign attribute's namespace. A hand-written foreign sidecar carrying
    /// prefixed attributes from several namespaces goes through every writer in turn; after
    /// each one an independent namespace-aware reader must still find every foreign
    /// attribute under its original namespace URI with its original value, and ChairPhoto's
    /// own reader must still read the attribute-form (`stArea:x="…"`) region.
    #[test]
    fn writers_preserve_prefixed_foreign_attributes() {
        let dir = crate::test_support::TestTmpDir::new("xmp-prefixed-attrs");
        let photo = dir.join("DSC138.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        std::fs::write(sidecar_path(&photo), FOREIGN_PREFIXED_ATTRS).unwrap();
        assert_foreign_attributes_intact(FOREIGN_PREFIXED_ATTRS, "nothing (fixture self-check)");

        let bob = |regions: &[ReadRegion], after: &str| {
            let r = regions.iter().find(|r| r.name == "Bob").unwrap_or_else(|| {
                panic!("after {after}: Bob's attribute-form region unread: {regions:?}")
            });
            // Center (0.8, 0.7), size 0.1×0.2 → top-left (0.75, 0.6).
            let (x, y, w, h) = r.bbox;
            assert!((x - 0.75).abs() < 1e-4 && (y - 0.6).abs() < 1e-4, "after {after}: {r:?}");
            assert!((w - 0.1).abs() < 1e-4 && (h - 0.2).abs() < 1e-4, "after {after}: {r:?}");
        };
        bob(&read_face_regions(&photo), "nothing");

        let uuid = "6f1c1f0e-8f5e-4a51-9a51-3c1b2a0d1388";
        let steps: Vec<(&str, Box<dyn Fn()>)> = vec![
            ("write_iptc", Box::new(|| {
                let fields = IptcFields {
                    title: "Ferry".into(),
                    city: "Trondheim".into(),
                    ..Default::default()
                };
                write_iptc(&photo, &IptcFields::default(), &fields).unwrap();
            })),
            ("write_keywords", Box::new(|| {
                write_keywords(&photo, &["boat".into()], &["Places|Norway".into()]).unwrap();
            })),
            ("write_identifier", Box::new(|| write_identifier(&photo, uuid).unwrap())),
            ("write_import_batch", Box::new(|| write_import_batch(&photo, uuid).unwrap())),
            ("write_gps", Box::new(|| write_gps(&photo, 63.43, 10.39).unwrap())),
            ("write_face_regions", Box::new(|| {
                let ours = [FaceRegion { face_id: 0, name: "Alice".into(), bbox: (0.1, 0.1, 0.2, 0.2) }];
                write_face_regions(&photo, CAT, &ours, &[], &[], sized(6000, 4000)).unwrap();
            })),
            ("overwrite_identifier", Box::new(|| {
                overwrite_identifier(&photo, uuid).unwrap();
            })),
        ];
        for (name, step) in steps {
            step();
            assert_foreign_attributes_intact(&read(&sidecar_path(&photo)), name);
            bob(&read_face_regions(&photo), name);
        }

        // ChairPhoto's own data from those writers reads back too.
        assert_eq!(read_identifier(&photo).as_deref(), Some(uuid));
        assert_eq!(read_import_batch(&photo).as_deref(), Some(uuid));
        assert!(read_gps(&photo).is_some());
        let names: Vec<String> = read_face_regions(&photo).into_iter().map(|r| r.name).collect();
        assert!(names.contains(&"Alice".to_string()), "{names:?}");
    }

    /// The other half of #138: once prefixed attributes survive parsing, a property a writer
    /// owns can arrive in compact attribute form (`xmp:Identifier="…"`, `photoshop:City="…"`
    /// on the Description, as exiftool and darktable write them). The writer owns that form
    /// too: it must be replaced, not left beside the new element as a second value — and for
    /// the identifier, `read_identifier` (which checks the attribute first) must then see the
    /// new UUID, or an Overwrite (issue #33) silently would not take.
    #[test]
    fn writers_replace_owned_properties_in_compact_attribute_form() {
        let dir = crate::test_support::TestTmpDir::new("xmp-compact-owned");
        let photo = dir.join("DSC139.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        std::fs::write(
            sidecar_path(&photo),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/"
    xmp:Identifier="foreign-id" photoshop:City="Oslo" xmp:Rating="3"/>
 </rdf:RDF>
</x:xmpmeta>"#,
        )
        .unwrap();
        assert_eq!(read_identifier(&photo).as_deref(), Some("foreign-id"), "compact form is read");

        let uuid = "0b7d5f7e-4d0c-4c55-8a3e-1f5e6f0a1390";
        overwrite_identifier(&photo, uuid).unwrap();
        write_iptc(&photo, &IptcFields::default(), &IptcFields { city: "Trondheim".into(), ..Default::default() })
            .unwrap();

        let xml = read(&sidecar_path(&photo));
        assert_eq!(read_identifier(&photo).as_deref(), Some(uuid), "{xml}");
        let attrs = namespaced_attributes(&xml);
        let has = |ns: &str, local: &str| attrs.iter().any(|a| a.2 == ns && a.3 == local);
        assert!(!has(NS_XMP, "Identifier"), "stale compact identifier kept:\n{xml}");
        assert!(!has(NS_PHOTOSHOP, "City"), "stale compact City kept:\n{xml}");
        assert!(has(NS_XMP, "Rating"), "a property no writer owns must survive:\n{xml}");
        assert_eq!(xml.matches("Oslo").count(), 0, "{xml}");
        assert_eq!(xml.matches("<photoshop:City>Trondheim</photoshop:City>").count(), 1, "{xml}");
    }

    /// Issue #142: exiftool writes one `rdf:Description` per namespace, so an owned property
    /// can sit in any of them, as an element or in compact attribute form. The identifier,
    /// IPTC and GPS writers must remove every stale instance, not only the first
    /// Description's, and write exactly one value — while every property they do not own,
    /// in every Description, survives.
    #[test]
    fn writers_replace_owned_properties_in_every_description() {
        let dir = crate::test_support::TestTmpDir::new("xmp-142-multi-desc");
        let photo = dir.join("DSC142.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        // Hand-written in exiftool's shape: single quotes, one Description per namespace.
        std::fs::write(sidecar_path(&photo), r#"<?xpacket begin='' id='W5M0MpCehiHzreSzNTczkc9d'?>
<x:xmpmeta xmlns:x='adobe:ns:meta/' x:xmptk='Image::ExifTool 12.76'>
<rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'>
 <rdf:Description rdf:about='' xmlns:dc='http://purl.org/dc/elements/1.1/'>
  <dc:format>image/x-sony-arw</dc:format>
 </rdf:Description>
 <rdf:Description rdf:about='' xmlns:exif='http://ns.adobe.com/exif/1.0/'>
  <exif:ExposureTime>1/250</exif:ExposureTime>
  <exif:GPSLatitude>59,54.834000N</exif:GPSLatitude>
  <exif:GPSLongitude>10,45.132000E</exif:GPSLongitude>
 </rdf:Description>
 <rdf:Description rdf:about='' xmlns:photoshop='http://ns.adobe.com/photoshop/1.0/'
  photoshop:City='Oslo' photoshop:Instructions='keep'/>
 <rdf:Description rdf:about='' xmlns:xmp='http://ns.adobe.com/xap/1.0/'>
  <xmp:Identifier>
   <rdf:Bag><rdf:li>dam:asset/4711</rdf:li></rdf:Bag>
  </xmp:Identifier>
  <xmp:Rating>3</xmp:Rating>
 </rdf:Description>
</rdf:RDF>
</x:xmpmeta>
<?xpacket end='w'?>"#).unwrap();

        let uuid = "6f1c1f0e-8f5e-4a51-9a51-3c1b2a0d1420";
        overwrite_identifier(&photo, uuid).unwrap();
        write_iptc(&photo, &IptcFields::default(), &IptcFields { city: "Trondheim".into(), ..Default::default() })
            .unwrap();
        write_gps(&photo, 63.4305, 10.3951).unwrap();

        let xml = read(&sidecar_path(&photo));
        let attrs = namespaced_attributes(&xml);
        let compact = |ns: &str, local: &str| attrs.iter().filter(|a| a.2 == ns && a.3 == local).count();
        let elements = namespaced_elements(&xml);
        let texts = |ns: &str, local: &str| -> Vec<String> {
            elements
                .iter()
                .filter(|e| e.name.0 == ns && e.name.1 == local)
                .map(|e| e.text.clone())
                .collect()
        };
        // Exactly one value of each owned property, ChairPhoto's, in element form.
        assert_eq!(texts(NS_XMP, "Identifier"), [uuid], "{xml}");
        assert_eq!(compact(NS_XMP, "Identifier"), 0, "{xml}");
        assert!(!xml.contains("dam:asset/4711"), "the overwritten identifier lingers:\n{xml}");
        assert_eq!(texts(NS_PHOTOSHOP, "City"), ["Trondheim"], "{xml}");
        assert_eq!(compact(NS_PHOTOSHOP, "City"), 0, "stale compact City kept:\n{xml}");
        assert_eq!(texts(NS_EXIF, "GPSLatitude"), ["63,25.830000N"], "{xml}");
        assert_eq!(texts(NS_EXIF, "GPSLongitude"), ["10,23.706000E"], "{xml}");
        assert_eq!(read_identifier(&photo).as_deref(), Some(uuid));
        let (lat, lng) = read_gps(&photo).unwrap();
        assert!((lat - 63.4305).abs() < 1e-6 && (lng - 10.3951).abs() < 1e-6, "{lat},{lng}");
        // What no writer owns survives in its own Description.
        assert_eq!(texts(NS_DC, "format"), ["image/x-sony-arw"], "{xml}");
        assert_eq!(texts(NS_EXIF, "ExposureTime"), ["1/250"], "{xml}");
        assert_eq!(texts(NS_XMP, "Rating"), ["3"], "{xml}");
        assert_eq!(compact(NS_PHOTOSHOP, "Instructions"), 1, "{xml}");
        assert_eq!(texts(NS_CHAIRPHOTO, "LastWrite").len(), 1, "{xml}");
    }

    /// Issue #143 item 4: owned compact properties are recognised by namespace URI, never by
    /// local name or prefix. A foreign namespace may use an owned local name (`foo:Identifier`,
    /// `foo:City`): those attributes are not ours and must survive. An owned namespace may be
    /// bound to a non-canonical prefix (`xap:` for xmp, `ps:` for photoshop): those attributes
    /// are ours and must go. Before this test, `attr_is` ignoring the namespace passed every
    /// test in this module.
    #[test]
    fn compact_owned_properties_are_matched_by_namespace_not_local_name() {
        let dir = crate::test_support::TestTmpDir::new("xmp-143-attr-ns");
        let photo = dir.join("DSC143.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        std::fs::write(sidecar_path(&photo), r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:foo="urn:example:foreign"
    xmlns:xap="http://ns.adobe.com/xap/1.0/"
    xmlns:ps="http://ns.adobe.com/photoshop/1.0/"
    foo:Identifier="keep-me" foo:City="keep-city"
    xap:Identifier="drop-me" ps:City="Oslo"/>
 </rdf:RDF>
</x:xmpmeta>"#).unwrap();
        assert_eq!(read_identifier(&photo).as_deref(), Some("drop-me"),
            "the identifier is read by namespace, whatever its prefix");

        let uuid = "6f1c1f0e-8f5e-4a51-9a51-3c1b2a0d1430";
        overwrite_identifier(&photo, uuid).unwrap();
        write_iptc(&photo, &IptcFields::default(), &IptcFields { city: "Trondheim".into(), ..Default::default() })
            .unwrap();

        let xml = read(&sidecar_path(&photo));
        let desc = (NS_RDF, "Description");
        assert!(has_attr(&xml, desc, (NS_FOREIGN, "Identifier"), "keep-me"),
            "a foreign attribute with an owned local name was removed:\n{xml}");
        assert!(has_attr(&xml, desc, (NS_FOREIGN, "City"), "keep-city"),
            "a foreign attribute with an owned local name was removed:\n{xml}");
        let attrs = namespaced_attributes(&xml);
        let compact = |ns: &str, local: &str| attrs.iter().any(|a| a.2 == ns && a.3 == local);
        assert!(!compact(NS_XMP, "Identifier"), "xap:Identifier is ours and was kept:\n{xml}");
        assert!(!compact(NS_PHOTOSHOP, "City"), "ps:City is ours and was kept:\n{xml}");
        assert_eq!(read_identifier(&photo).as_deref(), Some(uuid), "{xml}");
        assert_eq!(element_text(&xml, desc, (NS_PHOTOSHOP, "City")).as_deref(),
            Some("Trondheim"), "{xml}");
    }

    // ── face regions in layouts other tools write (issue #139) ─────────────

    const NS_FOREIGN: &str = "urn:example:foreign";

    /// One element as an independent namespace-aware reader (xml-rs, not ChairPhoto's parser)
    /// sees it: its parent's `{ns}local`, its own, and its direct non-blank text.
    #[derive(Debug)]
    struct SeenElement {
        parent: (String, String),
        name: (String, String),
        text: String,
    }

    fn namespaced_elements(xml: &str) -> Vec<SeenElement> {
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

    fn count_elements(xml: &str, ns: &str, local: &str) -> usize {
        namespaced_elements(xml)
            .iter()
            .filter(|e| e.name.0 == ns && e.name.1 == local)
            .count()
    }

    /// The text of the one `{ns}local` element whose parent is `{parent_ns}parent`.
    fn element_text(xml: &str, parent: (&str, &str), name: (&str, &str)) -> Option<String> {
        let seen = namespaced_elements(xml);
        let mut hits = seen.iter().filter(|e| {
            e.parent.0 == parent.0 && e.parent.1 == parent.1 && e.name.0 == name.0
                && e.name.1 == name.1
        });
        let hit = hits.next()?;
        assert!(hits.next().is_none(), "more than one {name:?} under {parent:?}\n{xml}");
        Some(hit.text.clone())
    }

    fn has_attr(xml: &str, el: (&str, &str), attr: (&str, &str), value: &str) -> bool {
        namespaced_attributes(xml).iter().any(|(e_ns, e_l, a_ns, a_l, v)| {
            e_ns == el.0 && e_l == el.1 && a_ns == attr.0 && a_l == attr.1 && v == value
        })
    }

    fn region_names(photo: &Path) -> Vec<String> {
        let mut names: Vec<String> = read_face_regions(photo).into_iter().map(|r| r.name).collect();
        names.sort();
        names
    }

    fn seeded_photo(tag: &str, sidecar: &str) -> (crate::test_support::TestTmpDir, PathBuf) {
        let dir = crate::test_support::TestTmpDir::new(tag);
        let photo = dir.join("DSC.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        std::fs::write(sidecar_path(&photo), sidecar).unwrap();
        (dir, photo)
    }

    fn alice() -> Vec<FaceRegion> {
        vec![FaceRegion { face_id: 0, name: "Alice".into(), bbox: (0.1, 0.1, 0.2, 0.2) }]
    }

    /// A hand-written sidecar whose Regions value, each region and their Areas are nested
    /// `rdf:Description`s or attributes (no `rdf:parseType="Resource"` anywhere), with foreign
    /// attributes and a foreign child on the Regions struct and a foreign attribute on
    /// AppliedToDimensions. Before #139 the writer found no RegionList here, so it deleted Bob.
    const NESTED_DESCRIPTION_REGIONS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
    xmlns:stDim="http://ns.adobe.com/xap/1.0/sType/Dimensions#"
    xmlns:f="urn:example:foreign">
   <mwg-rs:Regions>
    <rdf:Description f:keep="regions-attr">
     <mwg-rs:AppliedToDimensions stDim:w="4000" stDim:h="3000" stDim:unit="pixel"
       f:dimkeep="dims-attr"/>
     <mwg-rs:RegionList>
      <rdf:Bag>
       <rdf:li>
        <rdf:Description mwg-rs:Name="Bob" mwg-rs:Type="Face">
         <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2"
           stArea:unit="normalized"/>
        </rdf:Description>
       </rdf:li>
      </rdf:Bag>
     </mwg-rs:RegionList>
     <f:extra>regions-child</f:extra>
    </rdf:Description>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

    #[test]
    fn face_regions_nested_description_layout_keeps_foreign_regions_and_content() {
        let (_dir, photo) = seeded_photo("xmp-139-nested", NESTED_DESCRIPTION_REGIONS);
        assert_eq!(region_names(&photo), ["Bob"], "the fixture's region is read");

        write_face_regions(&photo, CAT, &alice(), &[], &[], sized(8000, 6000)).unwrap();

        let xml = read(&sidecar_path(&photo));
        assert_eq!(region_names(&photo), ["Alice", "Bob"], "{xml}");
        assert_eq!(count_elements(&xml, NS_MWG_RS, "Regions"), 1, "{xml}");
        assert_eq!(count_elements(&xml, NS_MWG_RS, "RegionList"), 1, "{xml}");
        assert!(has_attr(&xml, (NS_RDF, "Description"), (NS_FOREIGN, "keep"), "regions-attr"),
            "foreign attribute on the Regions struct lost:\n{xml}");
        assert_eq!(element_text(&xml, (NS_RDF, "Description"), (NS_FOREIGN, "extra")).as_deref(),
            Some("regions-child"), "foreign child of the Regions struct lost:\n{xml}");
        let dims = (NS_MWG_RS, "AppliedToDimensions");
        assert!(has_attr(&xml, dims, (NS_FOREIGN, "dimkeep"), "dims-attr"),
            "foreign attribute on AppliedToDimensions lost:\n{xml}");
        // The dimensions are the file's own, untouched (#145): a 4000x3000 frame of this
        // 8000x6000 image.
        assert!(has_attr(&xml, dims, (NS_STDIM, "w"), "4000"), "{xml}");
        assert!(has_attr(&xml, dims, (NS_STDIM, "h"), "3000"), "{xml}");
        assert_eq!(count_elements(&xml, NS_STDIM, "w"), 0, "no second, element-form width:\n{xml}");
        // Bob is untouched, attributes and all.
        assert!(has_attr(&xml, (NS_RDF, "Description"), (NS_MWG_RS, "Name"), "Bob"), "{xml}");
        assert!(has_attr(&xml, (NS_MWG_RS, "Area"), (NS_STAREA, "x"), "0.8"), "{xml}");
    }

    /// A RegionList held in an `rdf:Seq` (some tools order their regions) keeps its container:
    /// Bob survives, Alice joins him in the same Seq, and no Bag appears.
    #[test]
    fn face_regions_seq_region_list_keeps_its_regions_and_container() {
        let (_dir, photo) = seeded_photo("xmp-139-seq", r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#">
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:RegionList>
     <rdf:Seq>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Bob</mwg-rs:Name>
       <mwg-rs:Type>Face</mwg-rs:Type>
       <mwg-rs:Area rdf:parseType="Resource">
        <stArea:x>0.8</stArea:x><stArea:y>0.7</stArea:y>
        <stArea:w>0.1</stArea:w><stArea:h>0.2</stArea:h>
        <stArea:unit>normalized</stArea:unit>
       </mwg-rs:Area>
      </rdf:li>
     </rdf:Seq>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#);
        assert_eq!(region_names(&photo), ["Bob"], "the fixture's region is read");

        write_face_regions(&photo, CAT, &alice(), &[], &[], sized(6000, 4000)).unwrap();

        let xml = read(&sidecar_path(&photo));
        assert_eq!(region_names(&photo), ["Alice", "Bob"], "{xml}");
        assert_eq!(count_elements(&xml, NS_RDF, "Seq"), 1, "{xml}");
        assert_eq!(count_elements(&xml, NS_RDF, "Bag"), 0, "{xml}");
        let lis = namespaced_elements(&xml)
            .into_iter()
            .filter(|e| e.parent == (NS_RDF.to_string(), "Seq".to_string()))
            .count();
        assert_eq!(lis, 2, "both regions in the one Seq:\n{xml}");
    }

    /// The `rdf:parseType="Resource"` layout ChairPhoto itself writes, with foreign content
    /// where the old writer rebuilt the elements from scratch: an attribute on Regions, an
    /// attribute and a child on AppliedToDimensions, and a child of Regions.
    #[test]
    fn face_regions_keep_foreign_content_of_regions_and_dimensions() {
        let (_dir, photo) = seeded_photo("xmp-139-resource", r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
    xmlns:stDim="http://ns.adobe.com/xap/1.0/sType/Dimensions#"
    xmlns:f="urn:example:foreign">
   <mwg-rs:Regions rdf:parseType="Resource" f:keep="regions-attr">
    <mwg-rs:AppliedToDimensions rdf:parseType="Resource" f:keep2="dims-attr">
     <stDim:w>4000</stDim:w>
     <stDim:h>3000</stDim:h>
     <stDim:unit>pixel</stDim:unit>
     <f:dimextra>dims-child</f:dimextra>
    </mwg-rs:AppliedToDimensions>
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Bob</mwg-rs:Name>
       <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2"
         stArea:unit="normalized"/>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
    <f:extra>regions-child</f:extra>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#);

        write_face_regions(&photo, CAT, &alice(), &[], &[], sized(8000, 6000)).unwrap();

        let xml = read(&sidecar_path(&photo));
        assert_eq!(region_names(&photo), ["Alice", "Bob"], "{xml}");
        let regions = (NS_MWG_RS, "Regions");
        let dims = (NS_MWG_RS, "AppliedToDimensions");
        assert!(has_attr(&xml, regions, (NS_FOREIGN, "keep"), "regions-attr"), "{xml}");
        assert!(has_attr(&xml, dims, (NS_FOREIGN, "keep2"), "dims-attr"), "{xml}");
        assert_eq!(element_text(&xml, dims, (NS_FOREIGN, "dimextra")).as_deref(),
            Some("dims-child"), "{xml}");
        assert_eq!(element_text(&xml, regions, (NS_FOREIGN, "extra")).as_deref(),
            Some("regions-child"), "{xml}");
        assert_eq!(element_text(&xml, dims, (NS_STDIM, "w")).as_deref(), Some("4000"), "{xml}");
        assert_eq!(element_text(&xml, dims, (NS_STDIM, "h")).as_deref(), Some("3000"), "{xml}");
        assert_eq!(element_text(&xml, dims, (NS_STDIM, "unit")).as_deref(), Some("pixel"));
    }

    /// exiftool writes one Description per namespace, so Regions may sit in a later one. The
    /// writer edits it there instead of adding a second Regions property to the first.
    #[test]
    fn face_regions_are_edited_in_the_description_that_holds_them() {
        let (_dir, photo) = seeded_photo("xmp-139-second-desc", r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/">
   <xmp:Rating>3</xmp:Rating>
  </rdf:Description>
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#">
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Bob</mwg-rs:Name>
       <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2"/>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#);

        write_face_regions(&photo, CAT, &alice(), &[], &[], sized(6000, 4000)).unwrap();

        let xml = read(&sidecar_path(&photo));
        assert_eq!(region_names(&photo), ["Alice", "Bob"], "{xml}");
        assert_eq!(count_elements(&xml, NS_MWG_RS, "Regions"), 1, "{xml}");
        assert_eq!(count_elements(&xml, NS_RDF, "Bag"), 1, "{xml}");
    }

    /// Issue #140: a region another tool wrote that matches one ChairPhoto writes by Name +
    /// Area is updated in place. Only its Area coordinates change, in the form they are
    /// written in; its Type, `mwg-rs:Rotation`, foreign children and foreign attributes stay.
    /// Bob is Lightroom-style (attribute-form Area with a `digiKam:Confidence`), Carol is a
    /// nested-Description region with a foreign attribute and a foreign child in her Area.
    #[test]
    fn face_regions_matched_foreign_region_keeps_its_foreign_content() {
        let (_dir, photo) = seeded_photo("xmp-140-in-place", r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
    xmlns:digiKam="http://www.digikam.org/ns/1.0/"
    xmlns:f="urn:example:foreign">
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li rdf:parseType="Resource">
       <mwg-rs:Name>Bob</mwg-rs:Name>
       <mwg-rs:Type>Face</mwg-rs:Type>
       <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2"
         stArea:unit="normalized" digiKam:Confidence="87"/>
       <mwg-rs:Rotation>0.25</mwg-rs:Rotation>
       <f:note>bob-child</f:note>
      </rdf:li>
      <rdf:li>
       <rdf:Description mwg-rs:Name="Carol" f:tag="carol-attr">
        <mwg-rs:Area rdf:parseType="Resource">
         <stArea:x>0.3</stArea:x><stArea:y>0.3</stArea:y>
         <stArea:w>0.1</stArea:w><stArea:h>0.1</stArea:h>
         <f:areanote>carol-area-child</f:areanote>
        </mwg-rs:Area>
       </rdf:Description>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#);
        assert_eq!(region_names(&photo), ["Bob", "Carol"], "the fixture's regions are read");

        // Both moved by 0.01, well inside AREA_EPSILON: ChairPhoto's write of the same faces.
        let ours = [
            FaceRegion { face_id: 0, name: "Bob".into(), bbox: (0.76, 0.61, 0.1, 0.2) },
            FaceRegion { face_id: 0, name: "Carol".into(), bbox: (0.26, 0.26, 0.1, 0.1) },
        ];
        write_face_regions(&photo, CAT, &ours, &[], &[], sized(6000, 4000)).unwrap();

        let xml = read(&sidecar_path(&photo));
        let back = read_face_regions(&photo);
        assert_eq!(back.len(), 2, "updated, not duplicated: {back:?}\n{xml}");
        for want in &ours {
            let got = back.iter().find(|r| r.name == want.name).unwrap();
            let (a, b) = (got.bbox, want.bbox);
            assert!((a.0 - b.0).abs() < 1e-4 && (a.1 - b.1).abs() < 1e-4,
                "{} not moved to the written geometry: {a:?}\n{xml}", want.name);
        }

        // Bob: the Area stays in attribute form, with its foreign attribute.
        let area = (NS_MWG_RS, "Area");
        assert!(has_attr(&xml, area, (NS_DIGIKAM, "Confidence"), "87"), "{xml}");
        assert!(has_attr(&xml, area, (NS_STAREA, "x"), "0.81"), "{xml}");
        assert!(has_attr(&xml, area, (NS_STAREA, "unit"), "normalized"), "{xml}");
        let li = (NS_RDF, "li");
        assert_eq!(element_text(&xml, li, (NS_MWG_RS, "Rotation")).as_deref(), Some("0.25"),
            "{xml}");
        assert_eq!(element_text(&xml, li, (NS_MWG_RS, "Type")).as_deref(), Some("Face"));
        assert_eq!(element_text(&xml, li, (NS_FOREIGN, "note")).as_deref(), Some("bob-child"));
        // Carol: the nested Description keeps its attribute, the Area its foreign child, and
        // her coordinates stay elements.
        assert!(has_attr(&xml, (NS_RDF, "Description"), (NS_FOREIGN, "tag"), "carol-attr"));
        assert_eq!(element_text(&xml, area, (NS_FOREIGN, "areanote")).as_deref(),
            Some("carol-area-child"), "{xml}");
        assert_eq!(element_text(&xml, area, (NS_STAREA, "x")).as_deref(), Some("0.31"), "{xml}");
        assert_eq!(element_text(&xml, area, (NS_STAREA, "unit")).as_deref(),
            Some("normalized"), "the missing unit is added in the Area's own form:\n{xml}");
    }

    /// A Regions the writer does not recognise is never rebuilt: the write is refused with an
    /// error naming the sidecar, and the file is left byte for byte as it was.
    #[test]
    fn face_regions_refuse_a_layout_they_do_not_recognise() {
        let wrap = |desc_body: &str| format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#">
{desc_body}
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#);
        let bob = r#"<rdf:li rdf:parseType="Resource"><mwg-rs:Name>Bob</mwg-rs:Name>
       <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2"/></rdf:li>"#;
        let cases = [
            ("alt container", wrap(&format!(
                r#"<mwg-rs:Regions rdf:parseType="Resource"><mwg-rs:RegionList><rdf:Alt>{bob}</rdf:Alt></mwg-rs:RegionList></mwg-rs:Regions>"#))),
            ("no container", wrap(&format!(
                r#"<mwg-rs:Regions rdf:parseType="Resource"><mwg-rs:RegionList>{bob}</mwg-rs:RegionList></mwg-rs:Regions>"#))),
            ("two lists", wrap(&format!(
                r#"<mwg-rs:Regions rdf:parseType="Resource"><mwg-rs:RegionList><rdf:Bag>{bob}</rdf:Bag></mwg-rs:RegionList><mwg-rs:RegionList><rdf:Bag/></mwg-rs:RegionList></mwg-rs:Regions>"#))),
            ("two Regions", wrap(&format!(
                r#"<mwg-rs:Regions rdf:parseType="Resource"><mwg-rs:RegionList><rdf:Bag>{bob}</rdf:Bag></mwg-rs:RegionList></mwg-rs:Regions><mwg-rs:Regions rdf:parseType="Resource"/>"#))),
            ("Regions by reference", wrap(r#"<mwg-rs:Regions rdf:resource="urn:example:regions"/>"#)),
        ];
        for (case, sidecar) in cases {
            let (_dir, photo) = seeded_photo("xmp-139-refuse", &sidecar);
            let err = write_face_regions(&photo, CAT, &alice(), &[], &[], sized(6000, 4000))
                .expect_err(&format!("{case}: an unrecognised Regions must not be written"));
            assert!(matches!(err, RegionWriteError::Refused(_)), "{case}: {err:?}");
            assert!(err.to_string().contains(&sidecar_path(&photo).display().to_string()), "{case}: {err}");
            assert_eq!(read(&sidecar_path(&photo)), sidecar, "{case}: sidecar changed");
        }
    }

    // ── #136: the stored frame ─────────────────────────────────────────────────

    use super::region_fixtures as rf;

    fn near4(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
        [(a.0, b.0), (a.1, b.1), (a.2, b.2), (a.3, b.3)].iter().all(|(x, y)| (x - y).abs() < 1e-4)
    }

    /// MWG's center form of a top-left box.
    fn center(b: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
        (b.0 + b.2 / 2.0, b.1 + b.3 / 2.0, b.2, b.3)
    }

    fn turned(o: u8, w: u32, h: u32) -> RegionFrame {
        RegionFrame { orientation: Some(o), stored_size: Some((w, h)) }
    }

    /// The point maps are checked against an independent implementation of all eight EXIF
    /// orientations, the image crate's `apply_orientation` (the one the previews go through):
    /// a block of pixels marked in a stored 12x8 image must land where `stored_to_display`
    /// says, and `display_to_stored` must bring it back.
    #[test]
    fn orientation_maps_agree_with_the_image_crate() {
        let (sw, sh) = (12u32, 8u32);
        let stored = (2.0 / 12.0, 1.0 / 8.0, 3.0 / 12.0, 2.0 / 8.0);
        for o in 1..=8u8 {
            let mut img = image::RgbImage::new(sw, sh);
            for x in 2..5 {
                for y in 1..3 {
                    img.put_pixel(x, y, image::Rgb([255, 255, 255]));
                }
            }
            let mut img = image::DynamicImage::ImageRgb8(img);
            img.apply_orientation(image::metadata::Orientation::from_exif(o).unwrap());
            let img = img.to_rgb8();
            let (dw, dh) = img.dimensions();
            assert_eq!((dw, dh), if o >= 5 { (sh, sw) } else { (sw, sh) }, "orientation {o}");
            let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
            for (x, y, p) in img.enumerate_pixels() {
                if p[0] == 255 {
                    (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
                }
            }
            let (dw, dh) = (dw as f32, dh as f32);
            let want = (x0 as f32 / dw, y0 as f32 / dh, (x1 - x0) as f32 / dw, (y1 - y0) as f32 / dh);
            let got = stored_to_display(o, stored);
            assert!(near4(got, want), "orientation {o}: {got:?} vs the image crate's {want:?}");
            assert!(near4(display_to_stored(o, got), stored), "orientation {o} does not invert");
        }
    }

    /// #136: in all eight orientations the written region is the face's box turned into the
    /// stored frame, `AppliedToDimensions` is the stored size (never swapped), and reading it
    /// back for the same photo gives the face's box again. The stored-frame centers of four
    /// orientations are worked out by hand, independently of the code.
    #[test]
    fn face_regions_round_trip_in_every_orientation() {
        let display = (0.1, 0.2, 0.3, 0.4);
        let by_hand = [
            (1, (0.25, 0.4, 0.3, 0.4)),
            (3, (0.75, 0.6, 0.3, 0.4)),
            (6, (0.4, 0.75, 0.4, 0.3)),
            (8, (0.6, 0.25, 0.4, 0.3)),
        ];
        for o in 1..=8u8 {
            let dir = region_dir("xmp-136-orientations");
            let photo = dir.join("P.JPG");
            std::fs::write(&photo, b"jpeg").unwrap();
            let frame = turned(o, 6000, 4000);
            write_face_regions(&photo, CAT, &[FaceRegion { face_id: 0, name: "Alice".into(), bbox: display }], &[], &[], frame)
                .unwrap();

            let xml = read(&sidecar_path(&photo));
            let got = rf::mwg(&xml);
            assert_eq!(got.dims, [Some(("6000".into(), "4000".into()))], "orientation {o}:\n{xml}");
            assert_eq!(got.regions.len(), 1, "orientation {o}:\n{xml}");
            let area = got.regions[0].area;
            assert!(near4(area, center(display_to_stored(o, display))), "orientation {o}: {area:?}");
            if let Some((_, want)) = by_hand.iter().find(|(h, _)| *h == o) {
                assert!(near4(area, *want), "orientation {o}: {area:?}, by hand {want:?}");
            }
            let back = read_face_regions_in(&photo, frame);
            assert_eq!(back.len(), 1);
            assert!(near4(back[0].bbox, display), "orientation {o}: read back {:?}", back[0].bbox);
        }
    }

    /// #136 on Lightroom's sidecar of a portrait shot (Orientation 6): Bob's region, in the
    /// stored frame as MWG requires, comes through ChairPhoto's write of Alice unchanged, and
    /// so do the dimensions and every other foreign structure; Alice lands in the same frame;
    /// and the importer reads Bob turned into the display frame the detections are in.
    #[test]
    fn face_regions_keep_a_foreign_region_on_a_rotated_photo() {
        let (_dir, photo) = seeded_photo("xmp-136-lightroom", rf::LIGHTROOM_ROTATED);
        let bob_before = rf::subtree(rf::LIGHTROOM_ROTATED, NS_RDF, "li");
        let alice = (0.5, 0.1, 0.2, 0.1);
        let frame = turned(6, 6000, 4000);
        write_face_regions(&photo, CAT, &[FaceRegion { face_id: 0, name: "Alice".into(), bbox: alice }], &[], &[], frame).unwrap();

        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        assert_eq!(got.dims, [Some(("6000".into(), "4000".into()))], "{xml}");
        let bob = rf::named(&got, "Bob");
        assert_eq!(bob.len(), 1, "{xml}");
        assert_eq!(bob[0].area, (0.3, 0.25, 0.15, 0.1), "Bob moved:\n{xml}");
        assert!(rf::subtree(&xml, NS_RDF, "li").contains(&bob_before[0]), "Bob's li changed:\n{xml}");
        assert_eq!(rf::foreign_structures(&xml), rf::foreign_structures(rf::LIGHTROOM_ROTATED));
        let ours = rf::named(&got, "Alice");
        assert!(near4(ours[0].area, center(display_to_stored(6, alice))), "{:?}", ours[0].area);

        let read = read_face_regions_in(&photo, frame);
        let bob = read.iter().find(|r| r.name == "Bob").unwrap();
        // Stored top-left (0.225, 0.2, 0.15, 0.1), turned 90° clockwise.
        assert!(near4(bob.bbox, (0.7, 0.225, 0.1, 0.15)), "{:?}", bob.bbox);
        let alice_back = read.iter().find(|r| r.name == "Alice").unwrap();
        assert!(near4(alice_back.bbox, alice), "{:?}", alice_back.bbox);
    }

    /// #136: an unknown orientation is never guessed. The boxes are written as they are, and
    /// dimensions the sidecar already has stay, even when the catalog's size disagrees; a
    /// Regions ChairPhoto creates gets the recorded size, and with no recorded size no
    /// `AppliedToDimensions` at all (no `1x1` stand-in).
    #[test]
    fn face_regions_with_an_unknown_orientation_convert_nothing() {
        let box_ = (0.1, 0.1, 0.2, 0.2);
        let alice = [FaceRegion { face_id: 0, name: "Alice".into(), bbox: box_ }];
        let unknown = |size| RegionFrame { orientation: None, stored_size: size };

        let (_dir, photo) = seeded_photo("xmp-136-unknown-existing", rf::DIGIKAM);
        write_face_regions(&photo, CAT, &alice, &[], &[], unknown(Some((4000, 6000)))).unwrap();
        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        assert_eq!(got.dims, [Some(("6000".into(), "4000".into()))], "{xml}");
        assert!(near4(rf::named(&got, "Alice")[0].area, center(box_)), "{xml}");
        assert_eq!(rf::named(&got, "Bob")[0].area, (0.8, 0.7, 0.1, 0.2));

        let dir = region_dir("xmp-136-unknown-fresh");
        let photo = dir.join("U.JPG");
        std::fs::write(&photo, b"jpeg").unwrap();
        write_face_regions(&photo, CAT, &alice, &[], &[], unknown(Some((6000, 4000)))).unwrap();
        let got = rf::mwg(&read(&sidecar_path(&photo)));
        assert_eq!(got.dims, [Some(("6000".into(), "4000".into()))]);
        assert!(near4(got.regions[0].area, center(box_)));

        let photo = dir.join("V.JPG");
        std::fs::write(&photo, b"jpeg").unwrap();
        write_face_regions(&photo, CAT, &alice, &[], &[], RegionFrame { orientation: Some(6), stored_size: None })
            .unwrap();
        let got = rf::mwg(&read(&sidecar_path(&photo)));
        assert_eq!(got.dims, [None], "no size, no AppliedToDimensions");
        assert!(near4(got.regions[0].area, center(display_to_stored(6, box_))));
    }

    // ── #145: a foreign AppliedToDimensions is never rewritten ─────────────────

    /// Lightroom's Orientation-6 sidecar with its `AppliedToDimensions` declaring `w`x`h`.
    fn lightroom_declaring(w: &str, h: &str) -> String {
        let fixture = r#"stDim:w="6000" stDim:h="4000""#;
        assert!(rf::LIGHTROOM_ROTATED.contains(fixture), "the fixture's dims moved");
        rf::LIGHTROOM_ROTATED.replace(fixture, &format!(r#"stDim:w="{w}" stDim:h="{h}""#))
    }

    const BOB_STORED: (f32, f32, f32, f32) = (0.3, 0.25, 0.15, 0.1);

    /// Probe P01's shapes (claude-139-142.log M1): a write never changes the frame foreign
    /// regions are measured in. Where the sidecar declares the stored frame, ChairPhoto writes
    /// in it; where it declares the display frame (the stored size swapped, on a photo turned
    /// a quarter), ChairPhoto's regions follow it there instead of the declaration following
    /// ChairPhoto; a resized image keeps its own size. Bob and the dims are untouched each time.
    #[test]
    fn face_regions_follow_the_frame_the_sidecar_declares() {
        let alice = (0.5, 0.1, 0.2, 0.1);
        let cases = [
            ("stored frame", ("6000", "4000"), RegionTarget::Stored(6)),
            ("display frame", ("4000", "6000"), RegionTarget::AsIs),
            ("resized stored frame", ("3000", "2000"), RegionTarget::Stored(6)),
            ("ChairPhoto's old 1x1", ("1", "1"), RegionTarget::Stored(6)),
        ];
        for (case, (w, h), target) in cases {
            let sidecar = lightroom_declaring(w, h);
            let (_dir, photo) = seeded_photo("xmp-145-frames", &sidecar);
            let frame = turned(6, 6000, 4000);
            write_face_regions(&photo, CAT, &[FaceRegion { face_id: 0, name: "Alice".into(), bbox: alice }], &[], &[], frame)
                .unwrap_or_else(|e| panic!("{case}: {e}"));

            let xml = read(&sidecar_path(&photo));
            let got = rf::mwg(&xml);
            assert_eq!(got.dims, [Some((w.to_string(), h.to_string()))], "{case}:\n{xml}");
            assert_eq!(rf::named(&got, "Bob")[0].area, BOB_STORED, "{case}:\n{xml}");
            let ours = rf::named(&got, "Alice")[0].area;
            let want = match target {
                RegionTarget::AsIs => alice,
                RegionTarget::Stored(o) => display_to_stored(o, alice),
            };
            assert!(near4(ours, center(want)), "{case}: Alice at {ours:?}\n{xml}");
            let back = read_face_regions_in(&photo, frame);
            let alice_back = back.iter().find(|r| r.name == "Alice").unwrap();
            assert!(near4(alice_back.bbox, alice), "{case}: read back {:?}", alice_back.bbox);
        }
    }

    /// Probe P02's shape and its neighbours: a sidecar whose declared frame is not one ChairPhoto
    /// can place its boxes in is not written at all — byte for byte as it was — and reading it
    /// for import yields nothing rather than regions in the wrong frame. That covers an unknown
    /// size on a photo turned a quarter (before #145 the declaration became `1x1`), an aspect
    /// that is neither the image's nor swapped, a swap the orientation does not explain, and a
    /// declaration without a usable size.
    #[test]
    fn face_regions_refuse_a_frame_they_cannot_place_their_boxes_in() {
        let alice = [FaceRegion { face_id: 0, name: "Alice".into(), bbox: (0.5, 0.1, 0.2, 0.1) }];
        let no_size = |o| RegionFrame { orientation: Some(o), stored_size: None };
        let cases = [
            ("unknown size, turned a quarter", lightroom_declaring("6000", "4000"), no_size(6)),
            ("another aspect", lightroom_declaring("5000", "5000"), turned(6, 6000, 4000)),
            ("swapped but upright", lightroom_declaring("4000", "6000"), turned(1, 6000, 4000)),
            ("no usable size", lightroom_declaring("6000", "wide"), turned(6, 6000, 4000)),
            ("zero size", lightroom_declaring("0", "4000"), turned(6, 6000, 4000)),
        ];
        for (case, sidecar, frame) in cases {
            let (_dir, photo) = seeded_photo("xmp-145-refuse", &sidecar);
            let err = write_face_regions(&photo, CAT, &alice, &[], &[], frame)
                .expect_err(&format!("{case}: the write must be refused"));
            assert!(matches!(err, RegionWriteError::Refused(_)), "{case}: {err:?}");
            assert!(err.to_string().contains(&sidecar_path(&photo).display().to_string()), "{case}: {err}");
            assert_eq!(read(&sidecar_path(&photo)), sidecar, "{case}: sidecar changed");
            assert!(read_face_regions_in(&photo, frame).is_empty(), "{case}: imported anyway");
        }
    }

    /// P02's other halves: an unknown size never replaces a declaration. Upright (or mirrored
    /// without a quarter turn) the stored and display frames share their aspect, so the boxes
    /// go into the stored frame MWG prescribes; with an unknown orientation they go as they
    /// are (#136). Either way the declared 6000x4000 stays.
    #[test]
    fn face_regions_with_an_unknown_size_keep_the_declared_dimensions() {
        let alice = (0.5, 0.1, 0.2, 0.1);
        for (case, orientation, want) in [
            ("upright", Some(1), alice),
            ("rotated 180", Some(3), display_to_stored(3, alice)),
            ("unknown orientation", None, alice),
        ] {
            let (_dir, photo) = seeded_photo("xmp-145-unsized", &lightroom_declaring("6000", "4000"));
            let frame = RegionFrame { orientation, stored_size: None };
            write_face_regions(&photo, CAT, &[FaceRegion { face_id: 0, name: "Alice".into(), bbox: alice }], &[], &[], frame)
                .unwrap_or_else(|e| panic!("{case}: {e}"));
            let xml = read(&sidecar_path(&photo));
            let got = rf::mwg(&xml);
            assert_eq!(got.dims, [Some(("6000".into(), "4000".into()))], "{case}:\n{xml}");
            assert!(near4(rf::named(&got, "Alice")[0].area, center(want)), "{case}:\n{xml}");
            assert_eq!(rf::named(&got, "Bob")[0].area, BOB_STORED, "{case}");
        }
    }

    // ── #135: ChairPhoto's marker ──────────────────────────────────────────────

    fn face(face_id: i64, name: &str, bbox: (f32, f32, f32, f32)) -> FaceRegion {
        FaceRegion { face_id, name: name.into(), bbox }
    }

    /// #135: every region ChairPhoto writes carries `chairphoto:FaceId` in ChairPhoto's
    /// namespace — even where the Description binds the `chairphoto` prefix to something else —
    /// and a face that leaves the set takes its region with it, while every foreign region and
    /// structure in the digiKam, Lightroom (Orientation 6) and MS Photo sidecars stays.
    #[test]
    fn face_regions_mark_ours_and_remove_them_when_their_face_leaves() {
        for (layout, sidecar) in rf::FOREIGN_REGIONS {
            let (_dir, photo) = seeded_photo("xmp-135-marker", sidecar);
            let frame = if layout == "lightroom-o6" { turned(6, 6000, 4000) } else { sized(6000, 4000) };
            let before = rf::mwg(sidecar);
            let alice = face(41, "Alice", (0.1, 0.1, 0.2, 0.2));
            let carl = face(42, "Carl", (0.4, 0.4, 0.1, 0.1));
            write_face_regions(&photo, CAT, &[alice.clone(), carl.clone()], &[], &[], frame).unwrap();

            let xml = read(&sidecar_path(&photo));
            let got = rf::mwg(&xml);
            assert_eq!(rf::named(&got, "Alice")[0].face_id.as_deref(), Some(ours(41).as_str()), "{layout}:\n{xml}");
            assert_eq!(rf::named(&got, "Carl")[0].face_id.as_deref(), Some(ours(42).as_str()), "{layout}:\n{xml}");
            assert_eq!(got.regions.len(), before.regions.len() + 2, "{layout}:\n{xml}");

            // Alice is rejected, Carl ignored: the set is empty now.
            write_face_regions(&photo, CAT, &[carl.clone()], &[41], &[], frame).unwrap();
            let got = rf::mwg(&read(&sidecar_path(&photo)));
            assert!(rf::named(&got, "Alice").is_empty(), "{layout}: Alice stayed");
            write_face_regions(&photo, CAT, &[], &[41, 42], &[], frame).unwrap();
            let xml = read(&sidecar_path(&photo));
            let got = rf::mwg(&xml);
            assert_eq!(got.regions, before.regions, "{layout}: only the foreign regions are left:\n{xml}");
            if !before.dims.is_empty() {
                assert_eq!(got.dims, before.dims, "{layout}");
            } // else the Regions ChairPhoto added stays, empty: nothing marks it as ours alone.
            assert_eq!(rf::foreign_structures(&xml), rf::foreign_structures(sidecar), "{layout}:\n{xml}");
        }

        // A Description binding `chairphoto` to another namespace.
        let (_dir, photo) = seeded_photo("xmp-135-prefix", &rf::DIGIKAM.replace(
            r#"xmlns:digiKam="http://www.digikam.org/ns/1.0/""#,
            r#"xmlns:digiKam="http://www.digikam.org/ns/1.0/" xmlns:chairphoto="urn:example:not-ours""#,
        ));
        write_face_regions(&photo, CAT, &[face(7, "Alice", (0.1, 0.1, 0.2, 0.2))], &[], &[], sized(6000, 4000))
            .unwrap();
        let got = rf::mwg(&read(&sidecar_path(&photo)));
        assert_eq!(rf::named(&got, "Alice")[0].face_id.as_deref(), Some(ours(7).as_str()));
    }

    /// Review L3 / P05: a marked region follows its face — renamed, then moved further than
    /// `AREA_EPSILON` by a re-detection or an edited box — instead of a stale copy piling up.
    /// Its foreign additions survive. (Renamed *and* moved in one write, the marker alone does
    /// not prove it is the same face — ids are catalog-local — so it is replaced instead.)
    #[test]
    fn face_regions_follow_a_renamed_and_moved_face_by_its_marker() {
        let (_dir, photo) = seeded_photo("xmp-135-follow", rf::DIGIKAM);
        write_face_regions(&photo, CAT, &[face(9, "Alice", (0.1, 0.1, 0.2, 0.2))], &[], &[], sized(6000, 4000))
            .unwrap();
        // Another tool annotates our region.
        let xml = read(&sidecar_path(&photo)).replace(
            "<mwg-rs:Type>Face</mwg-rs:Type>",
            r#"<mwg-rs:Type>Face</mwg-rs:Type><digiKam:FaceEngine>dnn</digiKam:FaceEngine>"#,
        );
        std::fs::write(sidecar_path(&photo), &xml).unwrap();
        write_face_regions(&photo, CAT, &[face(9, "Alicia", (0.1, 0.1, 0.2, 0.2))], &[], &[], sized(6000, 4000))
            .unwrap();
        write_face_regions(&photo, CAT, &[face(9, "Alicia", (0.3, 0.2, 0.2, 0.2))], &[], &[], sized(6000, 4000))
            .unwrap();

        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        assert!(rf::named(&got, "Alice").is_empty(), "the stale name stayed:\n{xml}");
        let ours = rf::named(&got, "Alicia");
        assert_eq!(ours.len(), 1, "{xml}");
        assert!(near4(ours[0].area, center((0.3, 0.2, 0.2, 0.2))), "{xml}");
        assert_eq!(rf::subtree(&xml, rf::NS_DIGIKAM, "FaceEngine").len(), 1, "{xml}");
        assert_eq!(got.regions.len(), 2, "Alicia and digiKam's Bob:\n{xml}");
    }

    /// A foreign region that is the same face as one ChairPhoto writes (by Name + Area) is that
    /// face already in the file: it is updated in place (#140), never marked, and no marked
    /// copy is appended. When the face leaves the set it stays — it is not ours to remove.
    #[test]
    fn face_regions_never_remove_a_foreign_region() {
        let (_dir, photo) = seeded_photo("xmp-135-foreign", rf::DIGIKAM);
        // Bob as ChairPhoto has him (imported from this very region, source 'xmp').
        let bob = face(3, "Bob", (0.755, 0.6, 0.1, 0.2));
        write_face_regions(&photo, CAT, &[bob], &[], &[], sized(6000, 4000)).unwrap();
        let got = rf::mwg(&read(&sidecar_path(&photo)));
        assert_eq!(got.regions.len(), 1, "{got:?}");
        assert_eq!(got.regions[0].face_id, None, "a foreign region is never marked");

        write_face_regions(&photo, CAT, &[], &[], &[], sized(6000, 4000)).unwrap();
        let got = rf::mwg(&read(&sidecar_path(&photo)));
        assert_eq!(rf::named(&got, "Bob").len(), 1, "a foreign region was removed");
    }

    /// The decision's rule for regions written before the marker existed: an unmarked region
    /// matching (Name + Area, in the display frame the old writer used) a face on the
    /// pre-marker record was ChairPhoto's. While its face is in the set it is adopted — moved
    /// into the frame and marked — and once the face has left it is removed. An unmarked region
    /// the record does not describe is foreign and kept, even with a recorded face's name.
    #[test]
    fn face_regions_adopt_or_remove_what_a_pre_marker_chairphoto_wrote() {
        // What the pre-marker writer left on a portrait shot (Orientation 6): Alice and Dora in
        // the display frame, unmarked, plus a foreign Alice elsewhere and Lightroom's Bob.
        let pre_marker = |name: &str, (x, y, w, h): (f32, f32, f32, f32)| {
            format!(
                r#"<rdf:li rdf:parseType="Resource"><mwg-rs:Name>{name}</mwg-rs:Name><mwg-rs:Type>Face</mwg-rs:Type><mwg-rs:Area rdf:parseType="Resource"><stArea:x>{}</stArea:x><stArea:y>{}</stArea:y><stArea:w>{w}</stArea:w><stArea:h>{h}</stArea:h><stArea:unit>normalized</stArea:unit></mwg-rs:Area></rdf:li>"#,
                x + w / 2.0,
                y + h / 2.0
            )
        };
        let alice_then = (0.1, 0.1, 0.2, 0.2);
        let dora_then = (0.5, 0.5, 0.1, 0.1);
        let lis = [
            pre_marker("Alice", alice_then),
            pre_marker("Dora", dora_then),
            pre_marker("Alice", (0.7, 0.05, 0.1, 0.1)),
        ]
        .concat();
        let sidecar = rf::LIGHTROOM_ROTATED.replace("     </rdf:Bag>", &format!("{lis}</rdf:Bag>"));
        let (_dir, photo) = seeded_photo("xmp-135-legacy", &sidecar);
        assert_eq!(rf::mwg(&sidecar).regions.len(), 4);

        let record = [face(1, "Alice", alice_then), face(2, "Dora", dora_then)];
        // Alice is still confirmed (re-detected a little off); Dora was rejected.
        let alice_now = face(1, "Alice", (0.105, 0.1, 0.2, 0.2));
        write_face_regions(&photo, CAT, &[alice_now.clone()], &[], &record, turned(6, 6000, 4000)).unwrap();

        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        assert!(rf::named(&got, "Dora").is_empty(), "the rejected face's region stayed:\n{xml}");
        let alices = rf::named(&got, "Alice");
        assert_eq!(alices.len(), 2, "adopted, not duplicated; the foreign Alice kept:\n{xml}");
        let ours: Vec<_> = alices.iter().filter(|r| r.face_id.is_some()).collect();
        assert_eq!(ours.len(), 1, "{xml}");
        assert_eq!(ours[0].face_id.as_deref(), Some(super::tests::ours(1).as_str()));
        assert!(near4(ours[0].area, center(display_to_stored(6, alice_now.bbox))), "{xml}");
        let foreign: Vec<_> = alices.iter().filter(|r| r.face_id.is_none()).collect();
        assert!(near4(foreign[0].area, (0.75, 0.1, 0.1, 0.1)), "{xml}");
        assert_eq!(rf::named(&got, "Bob")[0].area, BOB_STORED);
    }

    /// A write that changes nothing in the regions leaves the sidecar alone: with nothing to
    /// write and nothing of ours to remove no sidecar is created, and repeating a write leaves
    /// the file byte for byte as the first one did.
    #[test]
    fn face_regions_that_change_nothing_write_nothing() {
        let dir = region_dir("xmp-135-noop");
        let photo = dir.join("N.JPG");
        std::fs::write(&photo, b"jpeg").unwrap();
        write_face_regions(&photo, CAT, &[], &[], &[], sized(6000, 4000)).unwrap();
        assert!(!sidecar_path(&photo).exists(), "an empty set created a sidecar");

        let (_dir, photo) = seeded_photo("xmp-135-noop-again", rf::DIGIKAM);
        write_face_regions(&photo, CAT, &[], &[], &[], sized(6000, 4000)).unwrap();
        assert_eq!(read(&sidecar_path(&photo)), rf::DIGIKAM, "nothing of ours, nothing written");
        let alice = [face(5, "Alice", (0.1, 0.1, 0.2, 0.2))];
        write_face_regions(&photo, CAT, &alice, &[], &[], sized(6000, 4000)).unwrap();
        let first = read(&sidecar_path(&photo));
        write_face_regions(&photo, CAT, &alice, &[], &[], sized(6000, 4000)).unwrap();
        assert_eq!(read(&sidecar_path(&photo)), first);
    }

    // ── #147: matching and the bare rdf:RDF root ───────────────────────────────

    /// Two unmarked Bobs, the one listed first at center x 0.515, the second at 0.50. With the
    /// Bob being written at 0.505 both are within `AREA_EPSILON`.
    fn two_foreign_bobs() -> String {
        let bob = |x: &str| {
            format!(
                r#"<rdf:li><rdf:Description mwg-rs:Name="Bob" mwg-rs:Type="Face"><mwg-rs:Area stArea:x="{x}" stArea:y="0.5" stArea:w="0.1" stArea:h="0.1" stArea:unit="normalized" digiKam:Confidence="{x}"/></rdf:Description></rdf:li>"#
            )
        };
        let digikam_bob = r#"      <rdf:li>
       <rdf:Description mwg-rs:Name="Bob" mwg-rs:Type="Face">
        <mwg-rs:Area stArea:x="0.8" stArea:y="0.7" stArea:w="0.1" stArea:h="0.2" stArea:unit="normalized"/>
       </rdf:Description>
      </rdf:li>"#;
        assert!(rf::DIGIKAM.contains(digikam_bob));
        rf::DIGIKAM.replace(digikam_bob, &[bob("0.515"), bob("0.5")].concat())
    }

    /// Review L1 (probe P04): one incoming region updates at most one existing region, and
    /// L2: the one it updates is the closest, not the first in document order. The other Bob,
    /// listed first, is left exactly as it was.
    #[test]
    fn face_regions_update_only_the_closest_matching_region() {
        let sidecar = two_foreign_bobs();
        assert_eq!(rf::named(&rf::mwg(&sidecar), "Bob").len(), 2, "fixture:\n{sidecar}");
        let (_dir, photo) = seeded_photo("xmp-147-closest", &sidecar);
        let bob = face(3, "Bob", (0.455, 0.45, 0.1, 0.1)); // center (0.505, 0.5)
        write_face_regions(&photo, CAT, &[bob], &[], &[], sized(6000, 4000)).unwrap();

        let xml = read(&sidecar_path(&photo));
        let bobs = rf::named(&rf::mwg(&xml), "Bob").into_iter().cloned().collect::<Vec<_>>();
        assert_eq!(bobs.len(), 2, "no Bob appended or removed:\n{xml}");
        assert_eq!(bobs[0].area.0, 0.515, "the farther Bob, listed first, moved:\n{xml}");
        assert!((bobs[1].area.0 - 0.505).abs() < 1e-5, "the closest Bob did not move:\n{xml}");
        assert!(bobs.iter().all(|b| b.face_id.is_none()), "a foreign region was marked:\n{xml}");
    }

    /// L2 with ChairPhoto's own region in the file: a digiKam Bob listed first and closer to
    /// the old position never takes the update meant for ChairPhoto's marked Bob.
    #[test]
    fn face_regions_update_their_own_region_before_a_foreign_one() {
        let (_dir, photo) = seeded_photo("xmp-147-ours-first", &two_foreign_bobs());
        // ChairPhoto's own Bob, marked, at center (0.51, 0.5): written as Zed far away, then
        // edited into place, so no foreign Bob is matched on the way.
        write_face_regions(&photo, CAT, &[face(99, "Zed", (0.1, 0.1, 0.1, 0.1))], &[], &[], sized(6000, 4000))
            .unwrap();
        let xml = read(&sidecar_path(&photo)).replace(
            "<mwg-rs:Name>Zed</mwg-rs:Name>",
            "<mwg-rs:Name>Bob</mwg-rs:Name>",
        )
        .replace(&format!("<chairphoto:FaceId>{}</chairphoto:FaceId>", ours(99)), &format!("<chairphoto:FaceId>{}</chairphoto:FaceId>", ours(3)))
        .replace("<stArea:x>0.15</stArea:x><stArea:y>0.15</stArea:y>", "<stArea:x>0.51</stArea:x><stArea:y>0.5</stArea:y>");
        assert!(xml.contains("<stArea:x>0.51</stArea:x>"), "{xml}");
        std::fs::write(sidecar_path(&photo), &xml).unwrap();

        write_face_regions(&photo, CAT, &[face(3, "Bob", (0.455, 0.45, 0.1, 0.1))], &[], &[], sized(6000, 4000))
            .unwrap();
        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        let bobs = rf::named(&got, "Bob");
        assert_eq!(bobs.len(), 3, "{xml}");
        assert_eq!((bobs[0].area.0, bobs[1].area.0), (0.515, 0.5), "a foreign Bob moved:\n{xml}");
        assert_eq!(bobs[2].face_id.as_deref(), Some(ours(3).as_str()));
        assert!((bobs[2].area.0 - 0.505).abs() < 1e-5, "ours did not move:\n{xml}");
    }

    /// Review L4 (probe P12): a sidecar whose root is a bare `rdf:RDF` — no `x:xmpmeta`, which
    /// the XMP spec allows — is read and written in place: one `rdf:RDF`, one `Regions`, the
    /// foreign region kept and ChairPhoto's added; the identifier reads and writes too. A root
    /// that is no XMP packet at all is refused and left as it was.
    #[test]
    fn a_bare_rdf_root_is_written_in_place() {
        let start = rf::DIGIKAM.find("<rdf:RDF").unwrap();
        let end = rf::DIGIKAM.find("</x:xmpmeta>").unwrap();
        let bare = format!(r#"<?xml version="1.0" encoding="UTF-8"?>{}"#, &rf::DIGIKAM[start..end]);
        let (_dir, photo) = seeded_photo("xmp-147-bare", &bare);
        assert_eq!(region_names(&photo), ["Bob"], "the bare root is read");

        write_face_regions(&photo, CAT, &[face(4, "Alice", (0.1, 0.1, 0.2, 0.2))], &[], &[], sized(6000, 4000))
            .unwrap();
        write_identifier(&photo, "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f").unwrap();

        let xml = read(&sidecar_path(&photo));
        let got = rf::mwg(&xml);
        assert_eq!(got.rdf_elements, 1, "a second rdf:RDF:\n{xml}");
        assert_eq!(got.dims.len(), 1, "a second Regions:\n{xml}");
        assert_eq!(rf::named(&got, "Bob")[0].area, (0.8, 0.7, 0.1, 0.2));
        assert_eq!(rf::named(&got, "Alice")[0].face_id.as_deref(), Some(ours(4).as_str()));
        assert_eq!(read_identifier(&photo).as_deref(), Some("6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f"));
        assert_eq!(rf::foreign_structures(&xml), rf::foreign_structures(rf::DIGIKAM));

        let (_dir, photo) = seeded_photo("xmp-147-not-xmp", "<foo><bar/></foo>");
        assert!(write_face_regions(&photo, CAT, &[face(4, "Alice", (0.1, 0.1, 0.2, 0.2))], &[], &[], sized(6000, 4000))
            .is_err());
        assert!(write_identifier(&photo, "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f").is_err());
        assert_eq!(read(&sidecar_path(&photo)), "<foo><bar/></foo>");
    }

    /// The #147 nit: a `chairphoto:LastWrite` exiftool moved into a later Description still
    /// says ChairPhoto has written the file, so no backup of a ChairPhoto-written state is
    /// taken as if it were the foreign original.
    #[test]
    fn last_write_in_a_later_description_counts() {
        let sidecar = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="2"/>
  <rdf:Description rdf:about="" xmlns:chairphoto="https://chairphoto.local/ns/1.0/">
   <chairphoto:LastWrite>1700000000</chairphoto:LastWrite>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        let (_dir, photo) = seeded_photo("xmp-147-lastwrite", sidecar);
        write_face_regions(&photo, CAT, &[face(4, "Alice", (0.1, 0.1, 0.2, 0.2))], &[], &[], sized(6000, 4000))
            .unwrap();
        assert!(!sidecar_backup_path(&sidecar_path(&photo)).exists(), "backed up a file ChairPhoto wrote");
    }

    // ── #135 M1: the marker is scoped to its catalog ───────────────────────────

    /// A `rdf:li` region in ChairPhoto's own shape, carrying `marker` as its FaceId.
    fn marked_region(marker: &str, name: &str, (x, y, w, h): (f32, f32, f32, f32)) -> String {
        format!(
            r#"<rdf:li rdf:parseType="Resource"><mwg-rs:Name>{name}</mwg-rs:Name><mwg-rs:Type>Face</mwg-rs:Type><mwg-rs:Area rdf:parseType="Resource"><stArea:x>{}</stArea:x><stArea:y>{}</stArea:y><stArea:w>{w}</stArea:w><stArea:h>{h}</stArea:h><stArea:unit>normalized</stArea:unit></mwg-rs:Area><chairphoto:FaceId xmlns:chairphoto="https://chairphoto.local/ns/1.0/">{marker}</chairphoto:FaceId></rdf:li>"#,
            x + w / 2.0,
            y + h / 2.0
        )
    }

    /// digiKam's sidecar with `lis` added to its RegionList.
    fn digikam_with(lis: &str) -> String {
        let end = "     </rdf:Bag>\n    </mwg-rs:RegionList>";
        assert!(rf::DIGIKAM.contains(end), "the fixture's list moved");
        rf::DIGIKAM.replacen(end, &format!("{lis}{end}"), 1)
    }

    /// Review M1 / probe Q5: a region another catalog marked — or a marker this build does not
    /// recognise — is foreign. A write of this catalog's set never removes it, never marks it
    /// as ours, and never renames or moves it, even when its face id is one this catalog
    /// writes; a rejected face of ours beside it still goes.
    #[test]
    fn another_catalogs_regions_are_foreign() {
        const OTHER: &str = "5d0e9a77-3c2b-4f1e-8a6d-2b9c0e1f4a33";
        let carol = (0.8, 0.8, 0.1, 0.1);
        for (case, marker) in [
            ("another catalog", format!("{OTHER}/7")),
            ("another catalog, our face id", format!("{OTHER}/1")),
            ("the pre-scope bare id", "1".to_string()),
            ("not a face id", format!("{CAT}/x1")),
            ("no face id", format!("{CAT}/")),
            ("a longer path", format!("{CAT}/1/2")),
            ("upper-cased catalog", format!("{}/1", CAT.to_uppercase())),
        ] {
            let lis = [marked_region(&marker, "Carol", carol), marked_region(&ours(2), "Dora", (0.5, 0.5, 0.1, 0.1))];
            let sidecar = digikam_with(&lis.concat());
            let (_dir, photo) = seeded_photo("xmp-135-scope", &sidecar);
            let carol_before = rf::named(&rf::mwg(&sidecar), "Carol")[0].clone();
            // This catalog writes Alice as face 1; Dora (face 2) has been rejected.
            write_face_regions(&photo, CAT, &[face(1, "Alice", (0.1, 0.1, 0.2, 0.2))], &[2], &[], sized(6000, 4000))
                .unwrap();
            let xml = read(&sidecar_path(&photo));
            let got = rf::mwg(&xml);
            assert_eq!(rf::named(&got, "Carol"), [&carol_before], "{case}: Carol changed:\n{xml}");
            assert!(rf::named(&got, "Dora").is_empty(), "{case}: our rejected Dora stayed:\n{xml}");
            assert_eq!(rf::named(&got, "Alice")[0].face_id.as_deref(), Some(ours(1).as_str()), "{case}");
            assert_eq!(rf::named(&got, "Bob").len(), 1, "{case}");

            write_face_regions(&photo, CAT, &[], &[1, 2], &[], sized(6000, 4000)).unwrap();
            let got = rf::mwg(&read(&sidecar_path(&photo)));
            assert_eq!(rf::named(&got, "Carol"), [&carol_before], "{case}: removed with an empty set");
        }
    }

    /// Another catalog's region that is the same face as one this catalog writes holds that
    /// face already: as for an unmarked foreign region (#140), only its Area moves, its marker
    /// stays the other catalog's, and no copy of ours is appended — so this catalog's reject
    /// later leaves it in place.
    #[test]
    fn another_catalogs_region_of_the_same_face_is_updated_in_place_only() {
        const OTHER: &str = "5d0e9a77-3c2b-4f1e-8a6d-2b9c0e1f4a33";
        let theirs = format!("{OTHER}/7");
        let sidecar = digikam_with(&marked_region(&theirs, "Alice", (0.1, 0.1, 0.2, 0.2)));
        let (_dir, photo) = seeded_photo("xmp-135-scope-same", &sidecar);
        write_face_regions(&photo, CAT, &[face(1, "Alice", (0.105, 0.1, 0.2, 0.2))], &[], &[], sized(6000, 4000))
            .unwrap();
        let xml = read(&sidecar_path(&photo));
        let alices = rf::named(&rf::mwg(&xml), "Alice").into_iter().cloned().collect::<Vec<_>>();
        assert_eq!(alices.len(), 1, "a copy was appended:\n{xml}");
        assert_eq!(alices[0].face_id.as_deref(), Some(theirs.as_str()), "re-marked as ours:\n{xml}");
        assert!(near4(alices[0].area, center((0.105, 0.1, 0.2, 0.2))), "{xml}");
        write_face_regions(&photo, CAT, &[], &[], &[], sized(6000, 4000)).unwrap();
        assert_eq!(rf::named(&rf::mwg(&read(&sidecar_path(&photo))), "Alice").len(), 1);
    }

    // ── #135 M2: only the pre-marker writer's exact shape is legacy ────────────

    /// Review M2 / probes Q4, Q4b: an unmarked region at the recorded place under the recorded
    /// name is taken for the pre-marker writer's only when it has exactly that writer's shape.
    /// digiKam's (nested `rdf:Description`, `digiKam:Confidence`) and every other variant —
    /// an extra field, a foreign child or attribute, another struct form — stays foreign: a
    /// reject never removes it, a confirmed face never marks it (only its Area moves, #140).
    /// The control, the exact shape, is adopted and then removed.
    #[test]
    fn only_the_pre_marker_shape_is_adopted_or_removed() {
        const AREA: &str = r#"<mwg-rs:Area rdf:parseType="Resource"><stArea:x>0.2</stArea:x><stArea:y>0.2</stArea:y><stArea:w>0.2</stArea:w><stArea:h>0.2</stArea:h><stArea:unit>normalized</stArea:unit></mwg-rs:Area>"#;
        let exact = |li_attrs: &str, extra: &str| {
            format!(
                r#"<rdf:li rdf:parseType="Resource"{li_attrs}><mwg-rs:Name>Alice</mwg-rs:Name><mwg-rs:Type>Face</mwg-rs:Type>{AREA}{extra}</rdf:li>"#
            )
        };
        let foreign = [
            (
                "digiKam (Q4)",
                r#"<rdf:li><rdf:Description mwg-rs:Name="Alice" mwg-rs:Type="Face" digiKam:Confidence="0.97"><mwg-rs:Area stArea:x="0.2" stArea:y="0.2" stArea:w="0.2" stArea:h="0.2" stArea:unit="normalized"/></rdf:Description></rdf:li>"#.to_string(),
            ),
            ("an extra MWG field", exact("", "<mwg-rs:Rotation>0</mwg-rs:Rotation>")),
            ("a foreign child", exact("", "<digiKam:FaceEngine>dnn</digiKam:FaceEngine>")),
            ("a foreign attribute", exact(r#" digiKam:Confidence="0.97""#, "")),
            (
                "an Area in attribute form",
                r#"<rdf:li rdf:parseType="Resource"><mwg-rs:Name>Alice</mwg-rs:Name><mwg-rs:Type>Face</mwg-rs:Type><mwg-rs:Area stArea:x="0.2" stArea:y="0.2" stArea:w="0.2" stArea:h="0.2" stArea:unit="normalized"/></rdf:li>"#.to_string(),
            ),
            (
                "a foreign Area field",
                exact("", "").replace("<stArea:unit>", r#"<digiKam:Source>user</digiKam:Source><stArea:unit>"#),
            ),
        ];
        let alice = face(1, "Alice", (0.1, 0.1, 0.2, 0.2));
        let record = [alice.clone()];
        for (case, li) in &foreign {
            let sidecar = digikam_with(li);
            let before = rf::named(&rf::mwg(&sidecar), "Alice")[0].clone();
            assert_eq!(before.area, (0.2, 0.2, 0.2, 0.2), "{case}: fixture");

            // Q4: the face was rejected.
            let (_dir, photo) = seeded_photo("xmp-135-shape-reject", &sidecar);
            write_face_regions(&photo, CAT, &[], &[], &record, sized(6000, 4000)).unwrap();
            let xml = read(&sidecar_path(&photo));
            assert_eq!(rf::named(&rf::mwg(&xml), "Alice"), [&before], "{case}: deleted:\n{xml}");

            // Q4b: the face is still confirmed (moved a little).
            let (_dir, photo) = seeded_photo("xmp-135-shape-keep", &sidecar);
            let moved = face(1, "Alice", (0.105, 0.1, 0.2, 0.2));
            write_face_regions(&photo, CAT, &[moved], &[], &record, sized(6000, 4000)).unwrap();
            let xml = read(&sidecar_path(&photo));
            let alices = rf::named(&rf::mwg(&xml), "Alice").into_iter().cloned().collect::<Vec<_>>();
            assert_eq!(alices.len(), 1, "{case}: duplicated:\n{xml}");
            assert_eq!(alices[0].face_id, None, "{case}: adopted as ours:\n{xml}");
            write_face_regions(&photo, CAT, &[], &[], &record, sized(6000, 4000)).unwrap();
            assert_eq!(rf::named(&rf::mwg(&read(&sidecar_path(&photo))), "Alice").len(), 1, "{case}");
        }

        // The control: the exact shape is the old writer's.
        let sidecar = digikam_with(&exact("", ""));
        let (_dir, photo) = seeded_photo("xmp-135-shape-exact", &sidecar);
        write_face_regions(&photo, CAT, &[alice.clone()], &[], &record, sized(6000, 4000)).unwrap();
        let got = rf::mwg(&read(&sidecar_path(&photo)));
        assert_eq!(rf::named(&got, "Alice")[0].face_id.as_deref(), Some(ours(1).as_str()), "not adopted");
        let (_dir, photo) = seeded_photo("xmp-135-shape-exact-reject", &sidecar);
        write_face_regions(&photo, CAT, &[], &[], &record, sized(6000, 4000)).unwrap();
        assert!(rf::named(&rf::mwg(&read(&sidecar_path(&photo))), "Alice").is_empty(), "not removed");
    }

    // ── review L5: the name-or-place guard on a FaceId match ───────────────────

    /// Step 1 follows our marker only while the region is still recognisably that face — its
    /// Name, or its place. A region carrying our marker for face 5 whose name *and* place
    /// both differ from face 5's (a copy of this catalog's file that diverged, sharing its
    /// identity) is not renamed and moved: since face 5 is still in the set it has not left
    /// it either, so the region is kept exactly as it is (review N1) and face 5 is written
    /// beside it. With either the name or the place still matching, the marker is followed
    /// and the annotation kept.
    #[test]
    fn a_marker_is_followed_only_while_the_name_or_place_still_matches() {
        let annotated = |name: &str, bbox| {
            marked_region(&ours(5), name, bbox)
                .replace("</rdf:li>", "<digiKam:FaceEngine>dnn</digiKam:FaceEngine></rdf:li>")
        };
        let carl_place = (0.7, 0.7, 0.1, 0.1);
        let alice_place = (0.1, 0.1, 0.2, 0.2);
        for (case, existing, follows) in [
            ("name and place differ", annotated("Carl", carl_place), false),
            ("same name, moved", annotated("Alice", carl_place), true),
            ("renamed, same place", annotated("Carl", alice_place), true),
        ] {
            let (_dir, photo) = seeded_photo("xmp-l5-guard", &digikam_with(&existing));
            write_face_regions(&photo, CAT, &[face(5, "Alice", alice_place)], &[], &[], sized(6000, 4000)).unwrap();
            let xml = read(&sidecar_path(&photo));
            let got = rf::mwg(&xml);
            let carls = rf::named(&got, "Carl");
            if follows {
                assert!(carls.is_empty(), "{case}: Carl stayed:\n{xml}");
            } else {
                let before = rf::named(&rf::mwg(&digikam_with(&existing)), "Carl")[0].clone();
                assert_eq!(carls, [&before], "{case}: Carl changed:\n{xml}");
            }
            let alices = rf::named(&got, "Alice");
            assert_eq!(alices.len(), 1, "{case}:\n{xml}");
            assert_eq!(alices[0].face_id.as_deref(), Some(ours(5).as_str()), "{case}");
            assert!(near4(alices[0].area, center(alice_place)), "{case}:\n{xml}");
            let kept = rf::subtree(&xml, rf::NS_DIGIKAM, "FaceEngine").len();
            assert_eq!(kept, 1, "{case}: the annotation:\n{xml}");
        }
    }

    // ── review N1: only a face this catalog knows on the photo ─────────────────

    /// Probe R2: a region carrying this catalog's marker for face 901, which this catalog
    /// does not know on this photo (a copy of the catalog file wrote it), is foreign — kept
    /// exactly as it is by a write of another face, by a write whose own face 901 is someone
    /// else elsewhere (the copies' id counters collide), and by an empty write. Only when 901
    /// is one of this photo's faces that left the set (`retired`) is it removed.
    #[test]
    fn a_marker_for_a_face_unknown_on_the_photo_is_kept() {
        let carol = marked_region(&ours(901), "Carol", (0.7, 0.7, 0.1, 0.1));
        let sidecar = digikam_with(&carol);
        let before = rf::named(&rf::mwg(&sidecar), "Carol")[0].clone();
        let alice = face(1, "Alice", (0.1, 0.1, 0.2, 0.2));
        let dave = face(901, "Dave", (0.4, 0.1, 0.1, 0.1));
        for (case, regions, retired, kept) in [
            ("unknown id", vec![alice.clone()], vec![], true),
            ("colliding id, another face", vec![dave], vec![], true),
            ("empty write", vec![], vec![1], true),
            ("unknown id, others retired", vec![alice.clone()], vec![2, 3], true),
            ("retired here", vec![alice.clone()], vec![901], false),
        ] {
            let (_dir, photo) = seeded_photo("xmp-n1-unknown", &sidecar);
            write_face_regions(&photo, CAT, &regions, &retired, &[], sized(6000, 4000)).unwrap();
            let xml = read(&sidecar_path(&photo));
            let carols = rf::named(&rf::mwg(&xml), "Carol").into_iter().cloned().collect::<Vec<_>>();
            if kept {
                assert_eq!(carols, [before.clone()], "{case}:\n{xml}");
            } else {
                assert!(carols.is_empty(), "{case}: not removed:\n{xml}");
            }
        }

        // The same face as one this catalog writes (Carol, a little off): like any foreign
        // region it holds her already — only its Area moves, the copy's marker stays, and no
        // region of ours is appended beside it.
        let (_dir, photo) = seeded_photo("xmp-n1-unknown-same", &sidecar);
        let carol_here = face(3, "Carol", (0.705, 0.7, 0.1, 0.1));
        write_face_regions(&photo, CAT, &[carol_here], &[], &[], sized(6000, 4000)).unwrap();
        let xml = read(&sidecar_path(&photo));
        let carols = rf::named(&rf::mwg(&xml), "Carol").into_iter().cloned().collect::<Vec<_>>();
        assert_eq!(carols.len(), 1, "{xml}");
        assert_eq!(carols[0].face_id.as_deref(), Some(ours(901).as_str()), "re-marked:\n{xml}");
        assert!(near4(carols[0].area, center((0.705, 0.7, 0.1, 0.1))), "{xml}");
    }

    // ── review round 2 nit: only the canonical face id is ours ─────────────────

    /// A marker is ours only in exactly the form ChairPhoto writes. `<catalog>/02` names
    /// face 2 to a lenient parser, but ChairPhoto never wrote it, so it is foreign: kept even
    /// though this photo's face 2 has been retired. The canonical `<catalog>/2` is removed.
    #[test]
    fn only_the_canonical_face_id_is_ours() {
        for (id, removed) in [("2", true), ("02", false), ("002", false), ("+2", false), ("-2", false)] {
            let marker = format!("{CAT}/{id}");
            let sidecar = digikam_with(&marked_region(&marker, "Dora", (0.5, 0.5, 0.1, 0.1)));
            let (_dir, photo) = seeded_photo("xmp-nit-canonical", &sidecar);
            write_face_regions(&photo, CAT, &[], &[2], &[], sized(6000, 4000)).unwrap();
            let xml = read(&sidecar_path(&photo));
            let doras = rf::named(&rf::mwg(&xml), "Dora").len();
            assert_eq!(doras, usize::from(!removed), "{marker}:\n{xml}");
        }
    }
}
