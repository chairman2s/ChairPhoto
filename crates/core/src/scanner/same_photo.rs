//! Whether a file arriving in the library is a photo the library already holds under its
//! name (#246). Card ingest and bundle import copy into `<root>/YYYY/MM/DD/` keeping the
//! camera's filename, so a name collision there is either the same photo imported before or
//! a different photo that happens to share the name — two bodies of one model both writing
//! `DSC01234.ARW` on the same day, at the same byte size because uncompressed RAWs of one
//! model all have one size.
//!
//! The owner's rule (#246, 2026-10-04): a collision is the same photo only when the names
//! and sizes match **and** the EXIF capture time (`DateTimeOriginal`, with
//! `SubSecTimeOriginal`) and the camera serial (`SerialNumber`, `InternalSerialNumber`, each
//! compared when both files have it) agree. Any difference makes it a different photo, which
//! is copied under a free ` (n)` name with its own row and UUID; nothing is ever overwritten.
//! Only when neither file has a capture time to compare (a PNG, a stripped JPEG) are their
//! contents compared, by streamed SHA-256. File mtime is never evidence: a copy changes it.
//!
//! The metadata is read by one exiftool process per [`STAMP_BATCH`] files, over the
//! colliding pairs only, so re-importing a card whose every file collides reads a few KB of
//! each file rather than hashing the card and its library copies. Both sides of a pair are
//! read by the same pass with the same arguments, so one file's bytes always give one
//! answer. A file exiftool cannot read (or no exiftool at all) has no capture time: with the
//! other side's time known that is a difference, and with neither known the contents decide.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

/// Files per exiftool invocation — bounds the command line, as `metadata::extract_batch`.
const STAMP_BATCH: usize = 150;

/// The serial-number tags compared, each only when both files carry it.
const SERIAL_TAGS: &[&str] = &["SerialNumber", "InternalSerialNumber"];

/// What identifies a capture: when, to the sub-second where the camera records it, and by
/// which body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureStamp {
    /// `DateTimeOriginal`, as exiftool prints it.
    pub time: Option<String>,
    /// `SubSecTimeOriginal`.
    pub subsec: Option<String>,
    /// Each of [`SERIAL_TAGS`] the file carries, by tag name.
    pub serials: BTreeMap<String, String>,
}

/// Whether two stamps are one capture: `Some(true)` the same photo, `Some(false)` a
/// different one, `None` when neither has a capture time and the contents must decide.
///
/// A capture time on one side only is a difference: the same bytes read the same way give
/// the same answer. So is a sub-second on one side only. A serial is compared per tag, only
/// where both files carry that tag (the owner's "when both files have it").
pub fn same_capture(a: &CaptureStamp, b: &CaptureStamp) -> Option<bool> {
    match (&a.time, &b.time) {
        (None, None) => None,
        (Some(x), Some(y)) => {
            let serials_agree =
                a.serials.iter().all(|(tag, value)| b.serials.get(tag).is_none_or(|other| other == value));
            Some(x == y && a.subsec == b.subsec && serials_agree)
        }
        _ => Some(false),
    }
}

/// Read the [`CaptureStamp`] of each of `paths` with one exiftool per [`STAMP_BATCH`] files.
/// A file exiftool cannot read is absent (no capture time). `None` once `abort` is set,
/// checked between batches.
pub fn read_capture_stamps(paths: &[PathBuf], abort: &AtomicBool) -> Option<HashMap<PathBuf, CaptureStamp>> {
    let mut out = HashMap::new();
    for chunk in paths.chunks(STAMP_BATCH) {
        if abort.load(Ordering::Relaxed) {
            return None;
        }
        let mut cmd = Command::new("exiftool");
        cmd.args(["-j", "-DateTimeOriginal", "-SubSecTimeOriginal"]);
        for tag in SERIAL_TAGS {
            cmd.arg(format!("-{tag}"));
        }
        cmd.arg("--");
        cmd.args(chunk);
        // No exiftool, or a chunk it could not read: those files have no capture time.
        let Ok(output) = cmd.output() else { continue };
        let Ok(serde_json::Value::Array(objects)) = serde_json::from_slice(&output.stdout) else {
            continue;
        };
        for obj in objects.iter().filter_map(|v| v.as_object()) {
            if let Some((path, stamp)) = parse_stamp(obj) {
                out.insert(path, stamp);
            }
        }
    }
    Some(out)
}

fn parse_stamp(obj: &serde_json::Map<String, serde_json::Value>) -> Option<(PathBuf, CaptureStamp)> {
    let path = PathBuf::from(obj.get("SourceFile")?.as_str()?);
    let text = |key: &str| {
        obj.get(key)
            .and_then(|v| match v {
                serde_json::Value::String(s) => Some(s.trim().to_string()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .filter(|s| !s.is_empty())
    };
    let serials = SERIAL_TAGS
        .iter()
        .filter_map(|tag| text(tag).map(|v| (tag.to_string(), v)))
        .collect();
    Some((path, CaptureStamp { time: text("DateTimeOriginal"), subsec: text("SubSecTimeOriginal"), serials }))
}

/// One file arriving at a name the library already uses: the arriving file, and the
/// same-size files already there that it may be ([`same_size_candidates`]).
pub struct Arrival {
    pub file: PathBuf,
    pub candidates: Vec<PathBuf>,
}

/// For each arrival, the candidate that is the same photo (#246's rule), or `None` when it
/// is a different photo. Reads every file's stamp in one batched pass. `None` once `abort`
/// is set.
pub fn find_already_imported(arrivals: &[Arrival], abort: &AtomicBool) -> Option<Vec<Option<PathBuf>>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut seen = HashSet::new();
    for a in arrivals {
        for p in std::iter::once(&a.file).chain(&a.candidates) {
            if seen.insert(p.clone()) {
                paths.push(p.clone());
            }
        }
    }
    let stamps = read_capture_stamps(&paths, abort)?;
    let none = CaptureStamp::default();
    let stamp = |p: &Path| stamps.get(p).unwrap_or(&none);
    let mut out = Vec::with_capacity(arrivals.len());
    for a in arrivals {
        if abort.load(Ordering::Relaxed) {
            return None;
        }
        out.push(a.candidates.iter().find(|c| is_same_photo(&a.file, stamp(&a.file), c, stamp(c))).cloned());
    }
    Some(out)
}

/// #246's rule for one pair: the stamps decide when either has a capture time; otherwise
/// the contents, by streamed SHA-256. A file that cannot be read is not the same photo —
/// the arriving one is then kept under a new name rather than dropped.
fn is_same_photo(a: &Path, sa: &CaptureStamp, b: &Path, sb: &CaptureStamp) -> bool {
    match same_capture(sa, sb) {
        Some(same) => same,
        None => match (crate::catalog::sha256_file(a), crate::catalog::sha256_file(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        },
    }
}

/// `dir/stem (n).ext`, the name a collision is renamed to.
fn numbered(path: &Path, n: u32) -> PathBuf {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let mut name = format!("{stem} ({n})");
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        name.push('.');
        name.push_str(ext);
    }
    dir.join(name)
}

/// The library files of `size` bytes that a file arriving as `dest` may already be: `dest`
/// itself and the ` (n)` names an earlier import gave different photos of that name, as far
/// as the names run unbroken — the same chain [`unique_dest`] walks to find a free one. So a
/// second import of a card that holds two photos named alike finds the second one at its
/// ` (2)` name and does not copy it a third time.
pub fn same_size_candidates(dest: &Path, size: u64) -> Vec<PathBuf> {
    let same_size = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() == size);
    let mut out = Vec::new();
    if !dest.exists() {
        return out;
    }
    if same_size(dest) {
        out.push(dest.to_path_buf());
    }
    for n in 2..10_000 {
        let candidate = numbered(dest, n);
        if !candidate.exists() {
            break;
        }
        if same_size(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// A destination path that doesn't exist yet (`name (2).ext`, …), or `None` if no free name
/// was found — the caller must then NOT copy (never overwrite an existing file).
pub fn unique_dest(path: &Path) -> Option<PathBuf> {
    if !path.exists() {
        return Some(path.to_path_buf());
    }
    (2..10_000).map(|n| numbered(path, n)).find(|c| !c.exists())
}

/// Test files carrying a capture stamp, shared by the card-ingest and bundle-import tests.
#[cfg(test)]
pub(crate) mod test_files {
    use std::path::Path;
    use std::process::Command;

    /// Whether exiftool runs here. A test that needs it announces the skip otherwise.
    pub(crate) fn exiftool_available(test: &str) -> bool {
        let ok = Command::new("exiftool").arg("-ver").output().is_ok_and(|o| o.status.success());
        if !ok {
            println!("SKIPPED: {test} — exiftool is not installed");
        }
        ok
    }

    /// Write a small JPEG at `path` whose EXIF says it was taken at `time`, sub-second
    /// `subsec`, by the body with serial `serial`.
    pub(crate) fn stamped_jpeg(path: &Path, time: &str, subsec: &str, serial: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::RgbImage::from_pixel(16, 16, image::Rgb([120, 80, 40]))
            .save_with_format(path, image::ImageFormat::Jpeg)
            .unwrap();
        let status = Command::new("exiftool")
            .args(["-q", "-overwrite_original"])
            .arg(format!("-DateTimeOriginal={time}"))
            .arg(format!("-SubSecTimeOriginal={subsec}"))
            .arg(format!("-SerialNumber={serial}"))
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "exiftool wrote {}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(time: Option<&str>, subsec: Option<&str>, serials: &[(&str, &str)]) -> CaptureStamp {
        CaptureStamp {
            time: time.map(str::to_string),
            subsec: subsec.map(str::to_string),
            serials: serials.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    // --- the rule (#246) ----------------------------------------------------------------

    #[test]
    fn the_same_time_subsecond_and_serial_is_the_same_photo() {
        let a = stamp(Some("2026:06:28 12:00:00"), Some("123"), &[("SerialNumber", "4711")]);
        assert_eq!(same_capture(&a, &a.clone()), Some(true));
    }

    #[test]
    fn another_subsecond_or_serial_in_the_same_second_is_another_photo() {
        let a = stamp(Some("2026:06:28 12:00:00"), Some("123"), &[("SerialNumber", "4711")]);
        let subsec = stamp(Some("2026:06:28 12:00:00"), Some("456"), &[("SerialNumber", "4711")]);
        let serial = stamp(Some("2026:06:28 12:00:00"), Some("123"), &[("SerialNumber", "4712")]);
        let internal_a = stamp(Some("2026:06:28 12:00:00"), None, &[("InternalSerialNumber", "aa")]);
        let internal_b = stamp(Some("2026:06:28 12:00:00"), None, &[("InternalSerialNumber", "bb")]);
        assert_eq!(same_capture(&a, &subsec), Some(false));
        assert_eq!(same_capture(&a, &serial), Some(false));
        assert_eq!(same_capture(&internal_a, &internal_b), Some(false));
        let second = stamp(Some("2026:06:28 12:00:01"), Some("123"), &[("SerialNumber", "4711")]);
        assert_eq!(same_capture(&a, &second), Some(false));
    }

    #[test]
    fn a_serial_only_one_file_carries_is_not_compared() {
        let a = stamp(Some("2026:06:28 12:00:00"), None, &[("SerialNumber", "4711")]);
        let b = stamp(Some("2026:06:28 12:00:00"), None, &[("InternalSerialNumber", "x")]);
        assert_eq!(same_capture(&a, &b), Some(true));
    }

    #[test]
    fn a_capture_time_on_one_side_only_is_a_difference_and_none_defers_to_the_contents() {
        let a = stamp(Some("2026:06:28 12:00:00"), None, &[]);
        assert_eq!(same_capture(&a, &CaptureStamp::default()), Some(false));
        assert_eq!(same_capture(&CaptureStamp::default(), &a), Some(false));
        assert_eq!(same_capture(&CaptureStamp::default(), &CaptureStamp::default()), None);
    }

    fn temp(tag: &str) -> crate::test_support::TestTmpDir {
        crate::test_support::TestTmpDir::new(&format!("same-photo-{tag}"))
    }

    /// With no capture time on either side the contents decide: equal bytes are the same
    /// photo, a one-byte difference at equal size is not.
    #[test]
    fn without_a_capture_time_the_contents_decide() {
        let dir = temp("hash");
        let lib = dir.join("lib.png");
        let same = dir.join("same.png");
        let other = dir.join("other.png");
        std::fs::write(&lib, b"not a real png 1").unwrap();
        std::fs::write(&same, b"not a real png 1").unwrap();
        std::fs::write(&other, b"not a real png 2").unwrap();
        let none = CaptureStamp::default();
        assert!(is_same_photo(&same, &none, &lib, &none));
        assert!(!is_same_photo(&other, &none, &lib, &none));
    }

    #[test]
    fn the_candidates_are_the_same_size_names_of_the_unbroken_chain() {
        let dir = temp("chain");
        let dest = dir.join("DSC1.ARW");
        assert!(same_size_candidates(&dest, 4).is_empty());
        std::fs::write(&dest, b"aaaa").unwrap();
        std::fs::write(dir.join("DSC1 (2).ARW"), b"bbbbb").unwrap(); // another size
        std::fs::write(dir.join("DSC1 (3).ARW"), b"cccc").unwrap();
        std::fs::write(dir.join("DSC1 (5).ARW"), b"dddd").unwrap(); // past the gap
        assert_eq!(same_size_candidates(&dest, 4), vec![dest.clone(), dir.join("DSC1 (3).ARW")]);
        assert_eq!(unique_dest(&dest), Some(dir.join("DSC1 (4).ARW")));
    }

    /// The stamps exiftool reads: the capture time, its sub-second and the serial, for the
    /// files it can read, and nothing for one it cannot.
    #[test]
    fn the_stamps_are_read_with_exiftool() {
        if !test_files::exiftool_available("the_stamps_are_read_with_exiftool") {
            return;
        }
        let dir = temp("read");
        let a = dir.join("a.jpg");
        let junk = dir.join("junk.jpg");
        test_files::stamped_jpeg(&a, "2026:06:28 12:00:00", "123", "4711");
        std::fs::write(&junk, b"not an image").unwrap();
        let stamps = read_capture_stamps(&[a.clone(), junk.clone()], &AtomicBool::new(false)).unwrap();
        assert_eq!(
            stamps.get(&a),
            Some(&stamp(Some("2026:06:28 12:00:00"), Some("123"), &[("SerialNumber", "4711")]))
        );
        assert_eq!(stamps.get(&junk).cloned().unwrap_or_default().time, None);
        assert!(read_capture_stamps(&[a], &AtomicBool::new(true)).is_none(), "an abort stops the read");
    }

    /// The first candidate that is the same photo answers; a different one is passed over.
    #[test]
    fn the_matching_candidate_answers() {
        let dir = temp("found");
        let lib = dir.join("lib.png");
        let lib2 = dir.join("lib (2).png");
        let a = dir.join("a.png");
        let b = dir.join("b.png");
        std::fs::write(&lib, b"bytes one").unwrap();
        std::fs::write(&lib2, b"bytes two").unwrap();
        std::fs::write(&a, b"bytes two").unwrap();
        std::fs::write(&b, b"bytes six").unwrap();
        let arrivals = vec![
            Arrival { file: a, candidates: vec![lib.clone(), lib2.clone()] },
            Arrival { file: b, candidates: vec![lib, lib2.clone()] },
        ];
        let found = find_already_imported(&arrivals, &AtomicBool::new(false)).unwrap();
        assert_eq!(found, vec![Some(lib2), None]);
    }
}
