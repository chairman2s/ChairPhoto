//! GPS coordinates: `exif:GPSLatitude` / `exif:GPSLongitude` and their DMS+ref encoding.

use std::path::Path;
use xmltree::XMLNode;
use super::document::SidecarDocument;
use super::dom::{first_text, is_rdf, plain, rdf_of};
use super::ns::NS_EXIF;
use super::parse::{ns_attr, parse_xml};
use super::sidecar_path;

/// Write GPS coordinates into the photo's XMP sidecar as `exif:GPSLatitude` and
/// `exif:GPSLongitude`, merge-safe: only those two fields (and `chairphoto:LastWrite`)
/// are touched; all other content is preserved.
///
/// Coordinates are encoded in the XMP EXIF DMS+ref format:
/// `"DD,MM.SSS[N|S]"` for latitude and `"DDD,MM.SSS[E|W]"` for longitude, which is
/// the format Lightroom, exiftool, and the XMP spec use for `exif:GPS*`.
///
/// Backs up a pre-existing foreign sidecar once before the first write, mirroring the
/// invariant in [`write_identifier`](super::write_identifier).
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
///
/// Each field is read in either RDF form: a property element (what [`write_gps`] writes) or a
/// compact attribute on the `rdf:Description` (`exif:GPSLatitude="…"`, what darktable and
/// exiftool write, #143). Both are matched by namespace URI, whatever prefix the file uses.
pub fn read_gps(photo_path: &Path) -> Option<(f64, f64)> {
    let path = sidecar_path(photo_path);
    let file = std::fs::File::open(&path).ok()?;
    let root = parse_xml(file).ok()?;
    let rdf = rdf_of(&root)?;
    let mut lat_str: Option<String> = None;
    let mut lng_str: Option<String> = None;
    for node in &rdf.children {
        let XMLNode::Element(desc) = node else { continue };
        if !is_rdf(desc, "Description") {
            continue;
        }
        if let Some(v) = ns_attr(desc, NS_EXIF, "GPSLatitude") {
            lat_str = Some(v.trim().to_string());
        }
        if let Some(v) = ns_attr(desc, NS_EXIF, "GPSLongitude") {
            lng_str = Some(v.trim().to_string());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmp::sidecar_backup_path;
    use crate::xmp::test_xml::read;

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

    // ── compact GPS attributes (#143 item 1) ────────────────────────────────

    /// A darktable/exiftool-style sidecar: GPS as compact attributes on the Description,
    /// under a prefix that is not `exif`, next to a decoy `GPSLatitude` in another namespace.
    const COMPACT_GPS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:darktable="http://darktable.sf.net/"
    xmlns:ex="http://ns.adobe.com/exif/1.0/"
    xmlns:foo="urn:example:foreign"
    foo:GPSLatitude="1,0.0N"
    ex:GPSLatitude="59,54.834000N"
    ex:GPSLongitude="10,45.132000W"
    darktable:history_end="5"/>
 </rdf:RDF>
</x:xmpmeta>"#;

    /// `read_gps` reads the compact attribute form other tools write, by namespace URI — not
    /// the decoy `foo:GPSLatitude`, and whatever prefix the file binds the EXIF namespace to.
    #[test]
    fn read_gps_reads_compact_attributes_by_namespace() {
        use crate::xmp::test_xml::{has_attr, seeded_photo, NS_FOREIGN};
        use crate::xmp::ns::NS_RDF;
        let (_dir, photo) = seeded_photo("xmp-gps-compact", COMPACT_GPS);
        // The fixture is what it claims, per an independent namespace-aware reader.
        let desc = (NS_RDF, "Description");
        assert!(has_attr(COMPACT_GPS, desc, (NS_EXIF, "GPSLatitude"), "59,54.834000N"));
        assert!(has_attr(COMPACT_GPS, desc, (NS_FOREIGN, "GPSLatitude"), "1,0.0N"));

        let (lat, lng) = read_gps(&photo).expect("compact GPS must be read");
        assert!((lat - 59.9139).abs() < 1e-9, "lat {lat}");
        assert!((lng + 10.7522).abs() < 1e-9, "lng {lng} (W is negative)");
    }

    /// Writing GPS over the compact form leaves one value per field, in element form, and the
    /// foreign attributes (the decoy, darktable's) as they were.
    #[test]
    fn write_gps_replaces_compact_attributes() {
        use crate::xmp::test_xml::{count_elements, has_attr, namespaced_attributes, seeded_photo, NS_FOREIGN};
        use crate::xmp::ns::NS_RDF;
        let (_dir, photo) = seeded_photo("xmp-gps-compact-write", COMPACT_GPS);
        write_gps(&photo, -33.4489, -70.6693).unwrap();

        let xml = read(&sidecar_path(&photo));
        let attrs = namespaced_attributes(&xml);
        assert!(!attrs.iter().any(|(_, _, ns, _, _)| ns == NS_EXIF), "compact GPS left behind:\n{xml}");
        assert_eq!(count_elements(&xml, NS_EXIF, "GPSLatitude"), 1, "{xml}");
        assert_eq!(count_elements(&xml, NS_EXIF, "GPSLongitude"), 1, "{xml}");
        let desc = (NS_RDF, "Description");
        assert!(has_attr(&xml, desc, (NS_FOREIGN, "GPSLatitude"), "1,0.0N"), "{xml}");
        assert!(has_attr(&xml, desc, ("http://darktable.sf.net/", "history_end"), "5"), "{xml}");
        let (lat, lng) = read_gps(&photo).unwrap();
        assert!((lat + 33.4489).abs() < 1e-6 && (lng + 70.6693).abs() < 1e-6, "{lat},{lng}");
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
}
