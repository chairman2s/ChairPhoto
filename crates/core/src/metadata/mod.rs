//! EXIF / IPTC / XMP extraction via exiftool.
//!
//! We read metadata at scan time and persist it, so search and smart albums query
//! the database, never exiftool. Two tiers are produced (see the catalog):
//! - **promoted** fields → indexed columns on `photos` (camera, lens, exposure,
//!   date, dimensions, GPS) for fast filtering/sorting,
//! - **generic** entries → the `photo_metadata` key-value table (everything else).
//!
//! exiftool is invoked ONCE per batch of files (`-j -G`), which keeps bulk scans
//! cheap despite its per-spawn cost. Binary blobs and very long noise values
//! (correction-parameter arrays, etc.) are skipped.

use crate::catalog::{MetadataEntry, PromotedMetadata};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

/// Files per exiftool invocation — bounds command-line length on huge folders.
const BATCH_SIZE: usize = 150;
/// Skip metadata values longer than this (maker-note arrays, WB tables, …).
const MAX_VALUE_LEN: usize = 240;

/// `photo_metadata.key` under which the AF-point makernote (`FocusLocation`) is stored.
/// The sharpness scorer (H16c) reads this to score at the camera's focus point when a
/// photo has no detected faces. Group `MakerNotes` matches exiftool's own namespace.
pub const AF_POINT_KEY: &str = "FocusLocation";
/// `photo_metadata.key` under which the EXIF Orientation code (1–8) is stored, needed to
/// rotate the sensor-frame AF point onto the oriented preview the scorer measures.
pub const AF_ORIENTATION_KEY: &str = "SharpnessAFOrientation";
/// Group name used for the AF-point entries stored above.
pub const AF_GROUP: &str = "MakerNotes";

pub struct PhotoMetadata {
    pub promoted: PromotedMetadata,
    pub entries: Vec<MetadataEntry>,
}

/// Extract metadata for many files, keyed by path. Files exiftool can't read are
/// simply absent from the map.
///
/// Each `BATCH_SIZE`-file chunk is a separate exiftool process; chunks run across
/// several worker threads (bounded, so a huge library doesn't fork hundreds of
/// processes at once) to use the multiple cores a bulk import otherwise leaves idle.
pub fn extract_batch(paths: &[PathBuf]) -> HashMap<PathBuf, PhotoMetadata> {
    use std::sync::atomic::AtomicUsize;

    let chunks: Vec<&[PathBuf]> = paths.chunks(BATCH_SIZE).collect();
    if chunks.is_empty() {
        return HashMap::new();
    }
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8)
        .min(chunks.len());
    let mut out = if workers == 1 {
        run_chunks(&chunks, &AtomicUsize::new(0))
    } else {
        let next = AtomicUsize::new(0);
        let mut out = HashMap::new();
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..workers)
                .map(|_| s.spawn(|| run_chunks(&chunks, &next)))
                .collect();
            for h in handles {
                if let Ok(local) = h.join() {
                    out.extend(local);
                }
            }
        });
        out
    };
    // Runs on every path (single- and multi-worker alike): the AF tier must fire for
    // single-file imports and small (<=BATCH_SIZE) scans too, not only bulk scans.
    enrich_af_points(paths, &mut out);
    out
}

/// Add the MakerNote-only fields (`FocusLocation` + `Orientation`, `Quality`) to
/// already-extracted RAW metadata, via a **separate targeted** exiftool pass.
///
/// # Why a second pass
///
/// The main pass runs `-fast2`, which stops before the MakerNote sub-IFDs — a 3.2× scan-time
/// win on a NAS that we must not give back. `FocusLocation` and `Quality` live in the
/// MakerNotes, so `-fast2` never returns them (verified 2026-07-17 on ARW 6.0). This targeted
/// pass requests only those fields (no `-fast2`, but only three tags), and only for RAW
/// files — the sole format that carries the Sony makernote — so it stays cheap: exiftool
/// reads just enough of each RAW to resolve the MakerNote header, not the whole metadata
/// surface. The result is folded into the existing `entries` so it rides the one
/// `set_photo_metadata` write per photo.
///
/// `Quality` is the only tag that distinguishes Sony's compression variants that share a
/// `SonyRawFileType` ("Compressed (HQ) RAW + Fine" vs "Compressed RAW + Fine" — both read
/// `Sony Compressed RAW 2` everywhere else), which is why the metadata panel needs it.
///
/// Non-RAW files are skipped entirely (no Sony makernote to read). A file missing
/// `FocusLocation` simply gets no entry — the scorer then falls through to tiles.
fn enrich_af_points(paths: &[PathBuf], out: &mut HashMap<PathBuf, PhotoMetadata>) {
    let raws: Vec<&PathBuf> = paths.iter().filter(|p| crate::scanner::is_raw(p)).collect();
    if raws.is_empty() {
        return;
    }
    for chunk in raws.chunks(BATCH_SIZE) {
        let Some(objects) = run_af_exiftool(chunk) else { continue };
        for obj in objects {
            let Some(source) = obj.get("SourceFile").and_then(|v| v.as_str()) else {
                continue;
            };
            let path = PathBuf::from(source);
            let Some(meta) = out.get_mut(&path) else { continue };

            if let Some(loc) = obj.get("MakerNotes:FocusLocation").and_then(value_to_string) {
                if !loc.is_empty() {
                    meta.entries.push(MetadataEntry {
                        key: AF_POINT_KEY.to_string(),
                        group_name: AF_GROUP.to_string(),
                        value: loc,
                    });
                    // The numeric EXIF Orientation code (1–8), needed to rotate the
                    // sensor-frame AF point onto the oriented preview. `-Orientation#`
                    // gives the raw number instead of the human string ("Rotate 90 CW").
                    if let Some(o) = obj.get("EXIF:Orientation").and_then(value_to_string) {
                        meta.entries.push(MetadataEntry {
                            key: AF_ORIENTATION_KEY.to_string(),
                            group_name: AF_GROUP.to_string(),
                            value: o,
                        });
                    }
                }
            }
            if let Some(q) = obj.get("MakerNotes:Quality").and_then(value_to_string) {
                // Guard against a second row when a future main pass also returns it.
                let already = meta
                    .entries
                    .iter()
                    .any(|e| e.key == "Quality" && e.group_name == AF_GROUP);
                if !q.is_empty() && !already {
                    meta.entries.push(MetadataEntry {
                        key: "Quality".to_string(),
                        group_name: AF_GROUP.to_string(),
                        value: q,
                    });
                }
            }
        }
    }
}

/// Targeted exiftool pass for the MakerNote-only fields. `-Orientation#` makes Orientation
/// a raw number (1–8) rather than a phrase — per-tag, NOT global `-n`, so `Quality` keeps
/// its human-readable string ("Compressed (HQ) RAW + Fine", not "5 2"). No `-fast2`:
/// MakerNotes are exactly what we need here.
fn run_af_exiftool(paths: &[&PathBuf]) -> Option<Vec<serde_json::Map<String, serde_json::Value>>> {
    let mut cmd = Command::new("exiftool");
    cmd.args(["-j", "-G", "-FocusLocation", "-Orientation#", "-Quality", "--"]);
    for p in paths {
        cmd.arg(p);
    }
    let output = cmd.output().ok()?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let array = value.as_array()?;
    Some(array.iter().filter_map(|v| v.as_object().cloned()).collect())
}

/// Pull chunk indices off the shared cursor and run exiftool on each until exhausted.
fn run_chunks(
    chunks: &[&[PathBuf]],
    next: &std::sync::atomic::AtomicUsize,
) -> HashMap<PathBuf, PhotoMetadata> {
    use std::sync::atomic::Ordering;
    let mut out = HashMap::new();
    loop {
        let i = next.fetch_add(1, Ordering::Relaxed);
        let Some(chunk) = chunks.get(i) else { break };
        if let Some(objects) = run_exiftool(chunk) {
            for obj in objects {
                if let Some((path, meta)) = parse_object(&obj) {
                    out.insert(path, meta);
                }
            }
        }
    }
    out
}

/// Run `exiftool -j -G -fast2` over a chunk and return the parsed JSON objects.
///
/// `-fast2` tells exiftool to stop reading each file after the front metadata blocks
/// (EXIF IFDs, ICC profile, IPTC, XMP, Photoshop) without scanning deeper into the
/// file body for maker-note sub-IFDs or trailer blocks.  This is the dominant scan-time
/// win for RAWs served over a NAS: measured at 12.9 s → 4.0 s wall-clock for a 150-file
/// ARW batch on a Samba NAS (3.2× faster).
///
/// Field parity (verified 2026-07-07 on ARW + JPEG + HEIC + MP4):
/// Every field consumed by `parse_object` (all keys in the `promoted` struct +
/// the generic `entries`) is present with `-fast2` **except** `MakerNotes:LensSpec`.
/// That key is the third fallback in the lens chain
/// `[Composite:LensID, EXIF:LensModel, MakerNotes:LensSpec, XMP:Lens]`; the two
/// higher-priority keys (`Composite:LensID` / `EXIF:LensModel`) are still returned
/// by `-fast2`, so lens identification is unaffected in practice.
///
/// `Composite:LensID` and `Composite:ShutterSpeed` are still present; without
/// `-fast2` exiftool resolves them with MakerNote data which can produce a slightly
/// different string ("Sony FE 85mm F1.8" vs "FE 85mm F1.8") or — for shutter speed
/// on Sony bodies — an erroneous value derived from `MakerNotes:ExposureTime`.
/// The `-fast2` values are equally correct (or more so: shutter speed matches
/// `EXIF:ExposureTime`, which is the primary key anyway).
fn run_exiftool(paths: &[PathBuf]) -> Option<Vec<serde_json::Map<String, serde_json::Value>>> {
    let mut cmd = Command::new("exiftool");
    cmd.args(["-j", "-G", "-c", "%+.6f", "-fast2", "--"]);
    for p in paths {
        cmd.arg(p);
    }
    let output = cmd.output().ok()?;
    // exiftool exits non-zero if any file has a minor warning, but still emits JSON.
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let array = value.as_array()?;
    Some(
        array
            .iter()
            .filter_map(|v| v.as_object().cloned())
            .collect(),
    )
}

fn parse_object(obj: &serde_json::Map<String, serde_json::Value>) -> Option<(PathBuf, PhotoMetadata)> {
    let source = obj.get("SourceFile")?.as_str()?;
    let path = PathBuf::from(source);

    // Flatten every tag to a string, keeping a lookup for promoted-field resolution.
    let mut fields: HashMap<String, String> = HashMap::new();
    let mut entries: Vec<MetadataEntry> = Vec::new();
    for (key, value) in obj {
        if key == "SourceFile" {
            continue;
        }
        let Some(text) = value_to_string(value) else {
            continue;
        };
        fields.insert(key.clone(), text.clone());

        // Generic entry: skip binary blobs and noisy long values.
        if text.starts_with("(Binary data") || text.len() > MAX_VALUE_LEN {
            continue;
        }
        let (group, tag) = key.split_once(':').unwrap_or(("Other", key.as_str()));
        entries.push(MetadataEntry {
            key: tag.to_string(),
            group_name: group.to_string(),
            value: text,
        });
    }

    let get = |k: &str| fields.get(k).map(String::as_str);
    let first = |keys: &[&str]| keys.iter().find_map(|k| get(k)).map(str::to_string);

    let promoted = PromotedMetadata {
        camera_make: first(&["EXIF:Make"]),
        camera_model: first(&["EXIF:Model"]),
        lens: first(&["Composite:LensID", "EXIF:LensModel", "MakerNotes:LensSpec", "XMP:Lens"]),
        focal_length: first(&["EXIF:FocalLength"]).and_then(|v| parse_leading_f64(&v)),
        aperture: first(&["EXIF:FNumber", "Composite:Aperture"]).and_then(|v| parse_leading_f64(&v)),
        shutter_speed: first(&["EXIF:ExposureTime", "Composite:ShutterSpeed"]),
        iso: first(&["EXIF:ISO"]).and_then(|v| parse_leading_i64(&v)),
        width: first(&["EXIF:ExifImageWidth", "EXIF:ImageWidth"]).and_then(|v| parse_leading_i64(&v)),
        height: first(&["EXIF:ExifImageHeight", "EXIF:ImageHeight"])
            .and_then(|v| parse_leading_i64(&v)),
        capture_time: first(&[
            "EXIF:DateTimeOriginal",
            "EXIF:CreateDate",
            "EXIF:DateTimeDigitized",
        ])
        .and_then(|v| normalize_datetime(&v)),
        gps_latitude: first(&["Composite:GPSLatitude", "EXIF:GPSLatitude"])
            .and_then(|v| parse_leading_f64(&v)),
        gps_longitude: first(&["Composite:GPSLongitude", "EXIF:GPSLongitude"])
            .and_then(|v| parse_leading_f64(&v)),
    };

    Some((path, PhotoMetadata { promoted, entries }))
}

/// Convert a JSON scalar to a display string; ignore arrays/objects/null.
fn value_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// First float in a string: "85.0 mm" -> 85.0, "+59.267823" -> 59.267823.
fn parse_leading_f64(s: &str) -> Option<f64> {
    let s = s.trim();
    let mut end = 0;
    let bytes = s.as_bytes();
    if matches!(bytes.first(), Some(b'+') | Some(b'-')) {
        end = 1;
    }
    let mut seen_dot = false;
    while end < bytes.len() {
        match bytes[end] {
            b'0'..=b'9' => end += 1,
            b'.' if !seen_dot => {
                seen_dot = true;
                end += 1;
            }
            _ => break,
        }
    }
    s[..end].parse().ok()
}

/// First integer in a string: "400" -> 400, "10240" -> 10240.
fn parse_leading_i64(s: &str) -> Option<i64> {
    parse_leading_f64(s).map(|f| f as i64)
}

/// "2026:06:06 14:35:59[.sss±tz]" -> "2026-06-06T14:35:59" (ISO-ish, sortable).
fn normalize_datetime(s: &str) -> Option<String> {
    let s = s.trim();
    let (date, rest) = s.split_once(' ')?;
    let date = date.replace(':', "-");
    let time: String = rest.chars().take(8).collect(); // HH:MM:SS
    if date.len() < 8 || time.len() < 8 {
        return None;
    }
    Some(format!("{date}T{time}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numbers_and_dates() {
        assert_eq!(parse_leading_f64("85.0 mm"), Some(85.0));
        assert_eq!(parse_leading_f64("+59.267823"), Some(59.267823));
        assert_eq!(parse_leading_i64("400"), Some(400));
        assert_eq!(
            normalize_datetime("2026:06:06 14:35:59"),
            Some("2026-06-06T14:35:59".to_string())
        );
        assert_eq!(
            normalize_datetime("2026:06:06 14:35:59.677+02:00"),
            Some("2026-06-06T14:35:59".to_string())
        );
    }
}
