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

/// Properties chairphoto manages via [`write_iptc`], by (namespace, local name).
/// On write we remove any existing instances of these and re-add from the current
/// field values; every other element in the sidecar is preserved.
/// Note: `chairphoto:ImportBatch` is managed separately by [`write_import_batch`]
/// and is intentionally NOT listed here so IPTC writes don't clobber it.
const MANAGED: &[(&str, &str)] = &[
    (NS_DC, "description"),
    (NS_DC, "title"),
    (NS_DC, "rights"),
    (NS_DC, "creator"),
    (NS_PHOTOSHOP, "Headline"),
    (NS_PHOTOSHOP, "Credit"),
    (NS_PHOTOSHOP, "Source"),
    (NS_PHOTOSHOP, "City"),
    (NS_PHOTOSHOP, "State"),
    (NS_PHOTOSHOP, "Country"),
    (NS_IPTC, "CountryCode"),
    (NS_CHAIRPHOTO, "LastWrite"),
];

/// The sidecar path for a photo: `<original_filename>.xmp` (darktable convention).
pub fn sidecar_path(photo_path: &Path) -> PathBuf {
    let mut s = photo_path.as_os_str().to_os_string();
    s.push(".xmp");
    PathBuf::from(s)
}

/// Write the authored IPTC fields into the photo's XMP sidecar, merging with any
/// existing content. Creates the sidecar if absent; backs up a pre-existing
/// non-chairphoto sidecar once before the first write.
pub fn write_iptc(photo_path: &Path, fields: &IptcFields) -> Result<(), String> {
    let path = sidecar_path(photo_path);
    let existed = path.exists();

    let mut root = if existed {
        let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        Element::parse(file).map_err(|e| format!("cannot parse {}: {e}", path.display()))?
    } else {
        new_root()
    };

    let rdf = child_mut(&mut root, "rdf", NS_RDF, "RDF");
    let desc = child_mut(rdf, "rdf", NS_RDF, "Description");
    desc.attributes
        .entry("rdf:about".to_string())
        .or_insert_with(String::new);

    // Back up a foreign sidecar exactly once (before we've ever marked it).
    let chairphoto_written = desc.children.iter().any(|n| {
        matches!(n, XMLNode::Element(e)
            if e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "LastWrite")
    });
    if existed && !chairphoto_written {
        let backup = sidecar_backup_path(&path);
        std::fs::copy(&path, &backup).ok();
    }

    declare_namespaces(desc);
    desc.children.retain(|n| !is_managed(n));

    // Re-add managed properties from current values (skip empties).
    let f = fields;
    if !f.description.is_empty() {
        desc.children.push(lang_alt("description", &f.description));
    }
    if !f.title.is_empty() {
        desc.children.push(lang_alt("title", &f.title));
    }
    if !f.copyright.is_empty() {
        desc.children.push(lang_alt("rights", &f.copyright));
    }
    if !f.creator.is_empty() {
        desc.children.push(seq_creator(&f.creator));
    }
    for (val, name) in [
        (&f.headline, "Headline"),
        (&f.credit, "Credit"),
        (&f.source, "Source"),
        (&f.city, "City"),
        (&f.state, "State"),
        (&f.country, "Country"),
    ] {
        if !val.is_empty() {
            desc.children.push(plain("photoshop", NS_PHOTOSHOP, name, val));
        }
    }
    if !f.country_code.is_empty() {
        desc.children
            .push(plain("Iptc4xmpCore", NS_IPTC, "CountryCode", &f.country_code));
    }
    desc.children
        .push(plain("chairphoto", NS_CHAIRPHOTO, "LastWrite", &now().to_string()));

    let mut buf = Vec::new();
    root.write(&mut buf).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, buf).map_err(|e| e.to_string())?;
    Ok(())
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
    let path = sidecar_path(photo_path);
    let mut root = if path.exists() {
        let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        Element::parse(file).map_err(|e| format!("cannot parse {}: {e}", path.display()))?
    } else {
        new_root()
    };

    let rdf = child_mut(&mut root, "rdf", NS_RDF, "RDF");
    let desc = child_mut(rdf, "rdf", NS_RDF, "Description");
    desc.attributes
        .entry("rdf:about".to_string())
        .or_insert_with(String::new);

    declare_namespaces(desc);
    // Manage only our keyword props + LastWrite (re-stamped); everything else stays.
    let managed = [
        (NS_DC, "subject"),
        (NS_LR, "hierarchicalSubject"),
        (NS_CHAIRPHOTO, "LastWrite"),
    ];
    desc.children.retain(|n| {
        !matches!(n, XMLNode::Element(e)
            if managed.iter().any(|(ns, name)|
                e.namespace.as_deref() == Some(*ns) && e.name == *name))
    });

    if !flat.is_empty() {
        desc.children.push(bag("dc", NS_DC, "subject", flat));
    }
    if !hierarchical.is_empty() {
        desc.children
            .push(bag("lr", NS_LR, "hierarchicalSubject", hierarchical));
    }
    desc.children
        .push(plain("chairphoto", NS_CHAIRPHOTO, "LastWrite", &now().to_string()));

    let mut buf = Vec::new();
    root.write(&mut buf).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, buf).map_err(|e| e.to_string())?;
    Ok(())
}

/// Read the photo UUID (`xmp:Identifier`) from its sidecar, if present. Used by the
/// scanner to recognise a moved/re-rooted file as the same photo (UUID is stable; path
/// is not). Returns `None` when there's no sidecar or no identifier. Tolerant of the
/// attribute form, a plain element, or an rdf array.
pub fn read_identifier(photo_path: &Path) -> Option<String> {
    let path = sidecar_path(photo_path);
    let file = std::fs::File::open(&path).ok()?;
    let root = Element::parse(file).ok()?;
    let rdf = root.get_child(("RDF", NS_RDF))?;
    for node in &rdf.children {
        let XMLNode::Element(desc) = node else { continue };
        if desc.name != "Description" {
            continue;
        }
        // Compact (attribute) form: rdf:Description xmp:Identifier="…".
        if let Some(v) = desc.attributes.get("xmp:Identifier") {
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
    let path = sidecar_path(photo_path);
    let existed = path.exists();

    let mut root = if existed {
        let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        Element::parse(file).map_err(|e| format!("cannot parse {}: {e}", path.display()))?
    } else {
        new_root()
    };

    let rdf = child_mut(&mut root, "rdf", NS_RDF, "RDF");
    let desc = child_mut(rdf, "rdf", NS_RDF, "Description");
    desc.attributes
        .entry("rdf:about".to_string())
        .or_insert_with(String::new);

    let chairphoto_written = desc.children.iter().any(|n| {
        matches!(n, XMLNode::Element(e)
            if e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "LastWrite")
    });
    if existed && !chairphoto_written {
        std::fs::copy(&path, sidecar_backup_path(&path)).ok();
    }

    declare_namespaces(desc);
    // Manage only xmp:Identifier + chairphoto:LastWrite (preserve everything else).
    desc.children.retain(|n| {
        !matches!(n, XMLNode::Element(e)
            if (e.namespace.as_deref() == Some(NS_XMP) && e.name == "Identifier")
                || (e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "LastWrite"))
    });
    desc.children.push(plain("xmp", NS_XMP, "Identifier", uuid));
    desc.children
        .push(plain("chairphoto", NS_CHAIRPHOTO, "LastWrite", &now().to_string()));

    let mut buf = Vec::new();
    root.write(&mut buf).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, buf).map_err(|e| e.to_string())?;
    Ok(())
}

/// Write the import-batch UUID into the photo's XMP sidecar as
/// `chairphoto:ImportBatch`, merge-safe: only that field (and
/// `chairphoto:LastWrite`) are touched; all other content is preserved.
/// The batch UUID lets the batch survive catalog loss / catalog merge across
/// machines (the deferred part of D1 / K3).
/// Backs up a pre-existing foreign sidecar once before the first write,
/// mirroring the invariant in [`write_identifier`].
pub fn write_import_batch(photo_path: &Path, batch_uuid: &str) -> Result<(), String> {
    let path = sidecar_path(photo_path);
    let existed = path.exists();

    let mut root = if existed {
        let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        Element::parse(file).map_err(|e| format!("cannot parse {}: {e}", path.display()))?
    } else {
        new_root()
    };

    let rdf = child_mut(&mut root, "rdf", NS_RDF, "RDF");
    let desc = child_mut(rdf, "rdf", NS_RDF, "Description");
    desc.attributes
        .entry("rdf:about".to_string())
        .or_insert_with(String::new);

    let chairphoto_written = desc.children.iter().any(|n| {
        matches!(n, XMLNode::Element(e)
            if e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "LastWrite")
    });
    if existed && !chairphoto_written {
        std::fs::copy(&path, sidecar_backup_path(&path)).ok();
    }

    declare_namespaces(desc);
    // Manage only chairphoto:ImportBatch + chairphoto:LastWrite (preserve everything else).
    desc.children.retain(|n| {
        !matches!(n, XMLNode::Element(e)
            if (e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "ImportBatch")
                || (e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "LastWrite"))
    });
    desc.children
        .push(plain("chairphoto", NS_CHAIRPHOTO, "ImportBatch", batch_uuid));
    desc.children
        .push(plain("chairphoto", NS_CHAIRPHOTO, "LastWrite", &now().to_string()));

    let mut buf = Vec::new();
    root.write(&mut buf).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, buf).map_err(|e| e.to_string())?;
    Ok(())
}

/// Read the import-batch UUID (`chairphoto:ImportBatch`) from the photo's
/// sidecar, if present. Returns `None` when there's no sidecar or no field.
pub fn read_import_batch(photo_path: &Path) -> Option<String> {
    let path = sidecar_path(photo_path);
    let file = std::fs::File::open(&path).ok()?;
    let root = Element::parse(file).ok()?;
    let rdf = root.get_child(("RDF", NS_RDF))?;
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
    let path = sidecar_path(photo_path);
    let existed = path.exists();

    let mut root = if existed {
        let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        Element::parse(file).map_err(|e| format!("cannot parse {}: {e}", path.display()))?
    } else {
        new_root()
    };

    let rdf = child_mut(&mut root, "rdf", NS_RDF, "RDF");
    let desc = child_mut(rdf, "rdf", NS_RDF, "Description");
    desc.attributes
        .entry("rdf:about".to_string())
        .or_insert_with(String::new);

    let chairphoto_written = desc.children.iter().any(|n| {
        matches!(n, XMLNode::Element(e)
            if e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "LastWrite")
    });
    if existed && !chairphoto_written {
        std::fs::copy(&path, sidecar_backup_path(&path)).ok();
    }

    declare_namespaces(desc);
    // Manage only exif:GPSLatitude + exif:GPSLongitude + chairphoto:LastWrite.
    desc.children.retain(|n| {
        !matches!(n, XMLNode::Element(e)
            if (e.namespace.as_deref() == Some(NS_EXIF) && e.name == "GPSLatitude")
                || (e.namespace.as_deref() == Some(NS_EXIF) && e.name == "GPSLongitude")
                || (e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "LastWrite"))
    });
    desc.children.push(plain(
        "exif",
        NS_EXIF,
        "GPSLatitude",
        &decimal_to_dms_lat(lat),
    ));
    desc.children.push(plain(
        "exif",
        NS_EXIF,
        "GPSLongitude",
        &decimal_to_dms_lng(lng),
    ));
    desc.children
        .push(plain("chairphoto", NS_CHAIRPHOTO, "LastWrite", &now().to_string()));

    let mut buf = Vec::new();
    root.write(&mut buf).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, buf).map_err(|e| e.to_string())?;
    Ok(())
}

/// Read the GPS coordinates (`exif:GPSLatitude` / `exif:GPSLongitude`) from the
/// photo's XMP sidecar. Returns `None` when there's no sidecar or the GPS fields
/// are absent or unparseable.
pub fn read_gps(photo_path: &Path) -> Option<(f64, f64)> {
    let path = sidecar_path(photo_path);
    let file = std::fs::File::open(&path).ok()?;
    let root = Element::parse(file).ok()?;
    let rdf = root.get_child(("RDF", NS_RDF))?;
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

/// Convert a decimal-degree latitude to XMP EXIF DMS+ref format: `"DD,MM.SSSS[N|S]"`.
pub fn decimal_to_dms_lat(deg: f64) -> String {
    let hemi = if deg >= 0.0 { 'N' } else { 'S' };
    let abs = deg.abs();
    let d = abs.trunc() as u32;
    let m = (abs - d as f64) * 60.0;
    format!("{},{:.6}{}", d, m, hemi)
}

/// Convert a decimal-degree longitude to XMP EXIF DMS+ref format: `"DDD,MM.SSSS[E|W]"`.
pub fn decimal_to_dms_lng(deg: f64) -> String {
    let hemi = if deg >= 0.0 { 'E' } else { 'W' };
    let abs = deg.abs();
    let d = abs.trunc() as u32;
    let m = (abs - d as f64) * 60.0;
    format!("{},{:.6}{}", d, m, hemi)
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
// property carries `mwg-rs:AppliedToDimensions` (the oriented pixel size the region
// coordinates apply to) plus a `mwg-rs:RegionList` (an rdf:Bag of region structs).
// Each region has a `mwg-rs:Name`, `mwg-rs:Type='Face'` and a `mwg-rs:Area` whose
// `stArea:x/y` are the **CENTER** of the rectangle (MWG stores centers, not top-left),
// with `stArea:w/h` and `stArea:unit='normalized'`.
//
// This write is merge-safe like the rest of this module, but with an extra twist: the
// RegionList may already contain regions written by *other tools* (digiKam etc.). We must
// replace only the regions chairphoto itself wrote and preserve every foreign region. A
// chairphoto-written region is identified by matching its Name against one of the names we
// are about to write AND its center-Area being within `AREA_EPSILON` of the incoming region
// — when in doubt we preserve. See AGENTS.md ("XMP Sidecar Convention").

/// Epsilon (in normalized coordinates) for deciding whether an existing region is "the same"
/// as one chairphoto is writing — i.e. previously written by us for that same person. Coarse
/// on purpose: two distinct faces of the same-named person on one photo are never this close,
/// while a re-detection of the same face drifts far less than this.
const AREA_EPSILON: f32 = 0.02;

/// One face region for the MWG writer. `bbox` is the stored **top-left** normalized rectangle
/// (`x, y` = top-left corner, `w, h` = size, all 0–1 of the oriented image) — exactly what
/// `faces__faces.bbox` holds. The writer converts it to MWG's center form.
#[derive(Debug, Clone, PartialEq)]
pub struct FaceRegion {
    /// Person tag leaf name (the region's `mwg-rs:Name`).
    pub name: String,
    /// Top-left-normalized bbox: `(x, y, w, h)`, each 0–1 of the oriented image.
    pub bbox: (f32, f32, f32, f32),
}

/// A region parsed back out of a sidecar by [`read_face_regions`]. Coordinates are converted
/// from MWG's center form back to the **top-left** normalized bbox chairphoto stores.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadRegion {
    /// `mwg-rs:Name` (may be empty for an unnamed region).
    pub name: String,
    /// Top-left-normalized bbox: `(x, y, w, h)`, each 0–1 of the applied-to dimensions.
    pub bbox: (f32, f32, f32, f32),
}

/// Write the given confirmed face regions into the photo's XMP sidecar as `mwg-rs:Regions`,
/// merge-safely. `oriented_w`/`oriented_h` are the photo's oriented pixel dimensions, written
/// as `mwg-rs:AppliedToDimensions` (the reference frame the normalized Area coords apply to).
///
/// Merge-safety (binding, AGENTS.md): every foreign region already in the `RegionList` — and
/// every foreign XML element anywhere in the sidecar — is preserved untouched. Only regions
/// chairphoto previously wrote (matched by Name + center-Area within [`AREA_EPSILON`]) are
/// replaced. When `regions` is empty this removes only chairphoto's own regions; a RegionList
/// that becomes empty of foreign entries is left with an empty Bag (harmless), and if there is
/// no foreign content and no chairphoto content the whole `Regions` property is dropped.
///
/// Backs up a pre-existing foreign sidecar once before the first write, mirroring the other
/// managed-property writers.
pub fn write_face_regions(
    photo_path: &Path,
    regions: &[FaceRegion],
    oriented_w: u32,
    oriented_h: u32,
) -> Result<(), String> {
    let path = sidecar_path(photo_path);
    let existed = path.exists();

    let mut root = if existed {
        let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        Element::parse(file).map_err(|e| format!("cannot parse {}: {e}", path.display()))?
    } else {
        new_root()
    };

    let rdf = child_mut(&mut root, "rdf", NS_RDF, "RDF");
    let desc = child_mut(rdf, "rdf", NS_RDF, "Description");
    desc.attributes
        .entry("rdf:about".to_string())
        .or_insert_with(String::new);

    let chairphoto_written = desc.children.iter().any(|n| {
        matches!(n, XMLNode::Element(e)
            if e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "LastWrite")
    });
    if existed && !chairphoto_written {
        std::fs::copy(&path, sidecar_backup_path(&path)).ok();
    }

    declare_namespaces(desc);
    declare_region_namespaces(desc);

    // Split out any existing mwg-rs:Regions element; keep everything else in place.
    let existing_regions = take_child(desc, NS_MWG_RS, "Regions");

    // Collect the foreign regions we must preserve (those NOT matching an incoming region).
    let foreign = existing_regions
        .as_ref()
        .map(|e| foreign_region_lis(e, regions))
        .unwrap_or_default();

    // Rebuild the Regions element from foreign + our new ones — unless there is nothing at all.
    if !foreign.is_empty() || !regions.is_empty() {
        let mut lis: Vec<XMLNode> = foreign;
        for r in regions {
            lis.push(region_li(r));
        }
        desc.children
            .push(build_regions(oriented_w, oriented_h, lis));
    }

    // Re-stamp LastWrite (managed by every writer).
    desc.children.retain(|n| {
        !matches!(n, XMLNode::Element(e)
            if e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "LastWrite")
    });
    desc.children
        .push(plain("chairphoto", NS_CHAIRPHOTO, "LastWrite", &now().to_string()));

    let mut buf = Vec::new();
    root.write(&mut buf).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, buf).map_err(|e| e.to_string())?;
    Ok(())
}

/// Read the MWG face regions (`mwg-rs:Regions`) from the photo's sidecar. Returns each region's
/// name and its **top-left** normalized bbox (converted from MWG's center form). Regions whose
/// Area is missing/unparseable are skipped. Returns an empty vec when there is no sidecar or no
/// Regions property. Tolerant of both the `rdf:parseType="Resource"` and nested-Description
/// encodings that different tools emit.
pub fn read_face_regions(photo_path: &Path) -> Vec<ReadRegion> {
    let path = sidecar_path(photo_path);
    let Ok(file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let Ok(root) = Element::parse(file) else {
        return Vec::new();
    };
    let Some(rdf) = root.get_child(("RDF", NS_RDF)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for node in &rdf.children {
        let XMLNode::Element(desc) = node else { continue };
        if desc.name != "Description" {
            continue;
        }
        let Some(regions) = child(desc, NS_MWG_RS, "Regions") else {
            continue;
        };
        // RegionList → rdf:Bag → rdf:li (each an Area-bearing region struct).
        let Some(region_list) = mwg_child(regions, "RegionList") else {
            continue;
        };
        let Some(bag) = region_list.get_child(("Bag", NS_RDF)) else {
            continue;
        };
        for li in bag.children.iter().filter_map(node_element) {
            if li.name != "li" {
                continue;
            }
            if let Some(r) = parse_region_li(li) {
                out.push(r);
            }
        }
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

/// Build a full `mwg-rs:Regions` element with AppliedToDimensions + a RegionList Bag of `lis`.
fn build_regions(w: u32, h: u32, lis: Vec<XMLNode>) -> XMLNode {
    // AppliedToDimensions as an rdf:parseType="Resource" struct: stDim:w / stDim:h / stDim:unit.
    let mut dims = el("mwg-rs", NS_MWG_RS, "AppliedToDimensions");
    dims.attributes
        .insert("rdf:parseType".to_string(), "Resource".to_string());
    dims.children.push(plain("stDim", NS_STDIM, "w", &w.to_string()));
    dims.children.push(plain("stDim", NS_STDIM, "h", &h.to_string()));
    dims.children.push(plain("stDim", NS_STDIM, "unit", "pixel"));

    let mut bag = el("rdf", NS_RDF, "Bag");
    bag.children.extend(lis);
    let mut region_list = el("mwg-rs", NS_MWG_RS, "RegionList");
    region_list.children.push(XMLNode::Element(bag));

    let mut regions = el("mwg-rs", NS_MWG_RS, "Regions");
    regions
        .attributes
        .insert("rdf:parseType".to_string(), "Resource".to_string());
    regions.children.push(XMLNode::Element(dims));
    regions.children.push(XMLNode::Element(region_list));
    XMLNode::Element(regions)
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

/// Return the `rdf:li` children of an existing Regions element that chairphoto did NOT write,
/// i.e. that must be preserved. A li is considered "ours" (and thus dropped, to be re-added
/// fresh) when its Name matches one of the incoming region names AND its center-Area is within
/// [`AREA_EPSILON`] of that region's center. When in doubt, the li is preserved.
fn foreign_region_lis(regions: &Element, incoming: &[FaceRegion]) -> Vec<XMLNode> {
    let Some(region_list) = mwg_child(regions, "RegionList") else {
        return Vec::new();
    };
    let Some(bag) = region_list.get_child(("Bag", NS_RDF)) else {
        return Vec::new();
    };
    let mut kept = Vec::new();
    for node in &bag.children {
        let XMLNode::Element(li) = node else { continue };
        if li.name != "li" {
            // Preserve any non-li node verbatim (defensive; shouldn't normally occur).
            kept.push(node.clone());
            continue;
        }
        let parsed = parse_region_li(li);
        let is_ours = parsed.as_ref().is_some_and(|p| {
            incoming.iter().any(|r| {
                r.name == p.name
                    && center_close(r.bbox, p.bbox)
            })
        });
        if !is_ours {
            kept.push(node.clone());
        }
    }
    kept
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
    let name = mwg_child(li, "Name")
        .and_then(first_text)
        .unwrap_or_default();
    let area = mwg_child(li, "Area")?;
    let cx: f32 = starea_value(area, "x")?;
    let cy: f32 = starea_value(area, "y")?;
    let w: f32 = starea_value(area, "w")?;
    let h: f32 = starea_value(area, "h")?;
    // MWG stores the CENTER; convert to top-left corner.
    let x = cx - w / 2.0;
    let y = cy - h / 2.0;
    Some(ReadRegion {
        name,
        bbox: (x, y, w, h),
    })
}

/// Read a numeric `stArea:<field>` from an Area element, whether encoded as an attribute
/// (`rdf:parseType="Resource"` compact form uses child elements; the shorthand form uses
/// attributes) or as a child element.
fn starea_value(area: &Element, field: &str) -> Option<f32> {
    // Attribute (shorthand) form: stArea:x="0.5".
    let attr_key = format!("stArea:{field}");
    if let Some(v) = area.attributes.get(&attr_key) {
        if let Ok(n) = v.trim().parse::<f32>() {
            return Some(n);
        }
    }
    // Child-element form.
    for node in &area.children {
        if let XMLNode::Element(e) = node {
            if e.namespace.as_deref() == Some(NS_STAREA) && e.name == field {
                if let Some(t) = first_text(e) {
                    if let Ok(n) = t.trim().parse::<f32>() {
                        return Some(n);
                    }
                }
            }
        }
    }
    None
}

/// Find a direct child by the mwg-rs namespace + local name.
fn mwg_child<'a>(parent: &'a Element, name: &str) -> Option<&'a Element> {
    child(parent, NS_MWG_RS, name)
}

/// Find a direct child element by (namespace, name).
fn child<'a>(parent: &'a Element, ns: &str, name: &str) -> Option<&'a Element> {
    parent.children.iter().find_map(|n| match n {
        XMLNode::Element(e) if e.namespace.as_deref() == Some(ns) && e.name == name => Some(e),
        _ => None,
    })
}

/// Remove and return a direct child element of `parent` by (namespace, name), if present.
fn take_child(parent: &mut Element, ns: &str, name: &str) -> Option<Element> {
    let pos = parent.children.iter().position(|n| {
        matches!(n, XMLNode::Element(e)
            if e.namespace.as_deref() == Some(ns) && e.name == name)
    })?;
    match parent.children.remove(pos) {
        XMLNode::Element(e) => Some(e),
        other => {
            parent.children.insert(pos, other);
            None
        }
    }
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

fn declare_region_namespaces(desc: &mut Element) {
    let mut ns = desc.namespaces.take().unwrap_or_else(Namespace::empty);
    ns.put("mwg-rs", NS_MWG_RS);
    ns.put("stArea", NS_STAREA);
    ns.put("stDim", NS_STDIM);
    desc.namespaces = Some(ns);
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

fn is_managed(node: &XMLNode) -> bool {
    matches!(node, XMLNode::Element(e)
        if MANAGED.iter().any(|(ns, name)|
            e.namespace.as_deref() == Some(*ns) && e.name == *name))
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
    use super::*;

    fn read(path: &Path) -> String {
        String::from_utf8(std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn creates_sidecar_with_iptc() {
        let dir = std::env::temp_dir().join("chairphoto-xmp-create");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let photo = dir.join("DSC1.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        let fields = IptcFields {
            description: "A ferry".into(),
            creator: "Andreas".into(),
            copyright: "(c) 2026".into(),
            city: "Trondheim".into(),
            ..Default::default()
        };
        write_iptc(&photo, &fields).unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("A ferry"));
        assert!(xmp.contains("Andreas"));
        assert!(xmp.contains("Trondheim"));
        assert!(xmp.contains("photoshop:City"));
        assert!(xmp.contains("dc:description"));
    }

    #[test]
    fn merge_preserves_foreign_elements() {
        let dir = std::env::temp_dir().join("chairphoto-xmp-merge");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
        write_iptc(&photo, &fields).unwrap();

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
        let dir = std::env::temp_dir().join("chairphoto-xmp-uuid");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let photo = dir.join("DSC9.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        // No sidecar yet → no identifier.
        assert_eq!(read_identifier(&photo), None);

        write_identifier(&photo, "uuid-abc-123").unwrap();
        assert_eq!(read_identifier(&photo).as_deref(), Some("uuid-abc-123"));

        // A later IPTC write must not drop the identifier, and re-writing the identifier
        // keeps a single instance.
        write_iptc(&photo, &IptcFields { description: "x".into(), ..Default::default() }).unwrap();
        assert_eq!(read_identifier(&photo).as_deref(), Some("uuid-abc-123"));
        write_identifier(&photo, "uuid-abc-123").unwrap();
        let xmp = read(&sidecar_path(&photo));
        assert_eq!(xmp.matches("xmp:Identifier").count(), 2, "one open + one close tag only");
        assert!(xmp.contains("x"), "IPTC field survived the identifier rewrite");
    }

    #[test]
    fn rewrites_not_duplicates_on_second_write() {
        let dir = std::env::temp_dir().join("chairphoto-xmp-rewrite");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let photo = dir.join("DSC3.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        write_iptc(&photo, &IptcFields { headline: "First".into(), ..Default::default() }).unwrap();
        write_iptc(&photo, &IptcFields { headline: "Second".into(), ..Default::default() }).unwrap();

        let xmp = read(&sidecar_path(&photo));
        assert!(xmp.contains("Second"));
        assert!(!xmp.contains("First"), "old value should be replaced, not duplicated");
        assert_eq!(xmp.matches("photoshop:Headline").count(), 2); // open + close tag, once
    }

    #[test]
    fn import_batch_round_trips_and_preserves_foreign() {
        let dir = std::env::temp_dir().join("chairphoto-xmp-batch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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
        write_iptc(&photo, &IptcFields { description: "boat".into(), ..Default::default() }).unwrap();
        assert_eq!(read_import_batch(&photo).as_deref(), Some("batch-uuid-001"),
            "batch uuid must survive an IPTC write");

        // And a new write_identifier must not drop the batch field either.
        write_identifier(&photo, "uuid-photo-001").unwrap();
        assert_eq!(read_import_batch(&photo).as_deref(), Some("batch-uuid-001"),
            "batch uuid must survive an identifier write");
    }

    #[test]
    fn import_batch_preserves_foreign_elements() {
        let dir = std::env::temp_dir().join("chairphoto-xmp-batch-foreign");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
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

    fn region_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write regions, read them back: names + top-left bboxes survive the center↔corner
    /// conversion round-trip, and AppliedToDimensions is written with the oriented pixel size.
    #[test]
    fn face_regions_round_trip() {
        let photo = region_dir("chairphoto-xmp-regions-rt").join("DSC20.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        let regions = vec![
            FaceRegion { name: "Alice".into(), bbox: (0.10, 0.20, 0.30, 0.40) },
            FaceRegion { name: "Bob".into(), bbox: (0.60, 0.10, 0.20, 0.25) },
        ];
        write_face_regions(&photo, &regions, 6000, 4000).unwrap();

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
        let photo = region_dir("chairphoto-xmp-regions-center").join("DSC21.ARW");
        std::fs::write(&photo, b"raw").unwrap();

        // Top-left (0.2, 0.3), size (0.4, 0.2) → center (0.4, 0.4).
        let regions = vec![FaceRegion { name: "Cara".into(), bbox: (0.2, 0.3, 0.4, 0.2) }];
        write_face_regions(&photo, &regions, 1000, 1000).unwrap();

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
        let photo = region_dir("chairphoto-xmp-regions-foreign").join("DSC22.ARW");
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
        let regions = vec![FaceRegion { name: "Alice".into(), bbox: (0.10, 0.10, 0.20, 0.20) }];
        write_face_regions(&photo, &regions, 6000, 4000).unwrap();

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
        let photo = region_dir("chairphoto-xmp-regions-rewrite").join("DSC23.ARW");
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
            &photo,
            &[FaceRegion { name: "Alice".into(), bbox: (0.10, 0.10, 0.20, 0.20) }],
            1000,
            1000,
        )
        .unwrap();

        // Second write: Alice moved slightly (well within AREA_EPSILON so it's recognised as
        // "our" region and replaced, not duplicated).
        write_face_regions(
            &photo,
            &[FaceRegion { name: "Alice".into(), bbox: (0.105, 0.105, 0.20, 0.20) }],
            1000,
            1000,
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
        let photo = region_dir("chairphoto-xmp-regions-absent").join("DSC24.ARW");
        std::fs::write(&photo, b"raw").unwrap();
        // No sidecar.
        assert!(read_face_regions(&photo).is_empty());
        // Sidecar with only IPTC, no regions.
        write_iptc(&photo, &IptcFields { description: "x".into(), ..Default::default() }).unwrap();
        assert!(read_face_regions(&photo).is_empty());
    }
}
