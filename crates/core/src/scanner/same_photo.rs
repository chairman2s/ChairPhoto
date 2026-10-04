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
//! A file with no `DateTimeOriginal` — a camera's video — has its `CreateDate` (QuickTime's)
//! as its capture time, compared only against the other file's `CreateDate`.
//! Only when neither file has a capture time to compare (a PNG, a stripped JPEG) are their
//! contents compared, by streamed SHA-256. File mtime is never evidence: a copy changes it.
//!
//! The metadata is read by one exiftool process per [`STAMP_BATCH`] files, over the
//! colliding pairs only, so re-importing a card whose every file collides reads a few KB of
//! each file rather than hashing the card and its library copies. Both sides of a pair are
//! read by the same pass with the same arguments, so one file's bytes always give one
//! answer. A bundle's original arrives as bytes in memory ([`find_in_library`]): a library
//! file holding exactly those bytes is the same photo, and otherwise exiftool reads the
//! bytes from stdin with the same arguments — they are never written to disk to be compared. A file exiftool cannot read (or no exiftool at all) has no capture time: with the
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
    /// `CreateDate`: the capture time of a file with no `DateTimeOriginal` — a camera's
    /// video (QuickTime `CreateDate`), so a re-imported clip is not hashed whole.
    pub created: Option<String>,
}

impl CaptureStamp {
    /// The capture time compared, with the tag it came from: `DateTimeOriginal`, else
    /// `CreateDate`. Two files are compared on the same tag only — a `DateTimeOriginal` on
    /// one side and only a `CreateDate` on the other is a difference.
    fn capture_time(&self) -> Option<(&'static str, &str)> {
        match (&self.time, &self.created) {
            (Some(t), _) => Some(("DateTimeOriginal", t)),
            (None, Some(c)) => Some(("CreateDate", c)),
            (None, None) => None,
        }
    }

    /// Whether there is a capture time to compare.
    pub fn has_capture_time(&self) -> bool {
        self.capture_time().is_some()
    }
}

/// Whether two stamps are one capture: `Some(true)` the same photo, `Some(false)` a
/// different one, `None` when neither has a capture time and the contents must decide.
///
/// The capture time is `DateTimeOriginal`, or `CreateDate` for a file without one (a video),
/// compared tag to tag. A capture time on one side only is a difference: the same bytes read
/// the same way give the same answer. So is a sub-second on one side only. A serial is compared per tag, only
/// where both files carry that tag (the owner's "when both files have it").
pub fn same_capture(a: &CaptureStamp, b: &CaptureStamp) -> Option<bool> {
    match (a.capture_time(), b.capture_time()) {
        (None, None) => None,
        (Some(x), Some(y)) => {
            let serials_agree =
                a.serials.iter().all(|(tag, value)| b.serials.get(tag).is_none_or(|other| other == value));
            Some(x == y && a.subsec == b.subsec && serials_agree)
        }
        _ => Some(false),
    }
}

/// The exiftool command both sides of a comparison are read with, so one file's bytes give
/// one answer whether exiftool reads them from a path or from stdin.
fn stamp_command() -> Command {
    let mut cmd = Command::new("exiftool");
    cmd.args(["-j", "-DateTimeOriginal", "-SubSecTimeOriginal", "-CreateDate"]);
    for tag in SERIAL_TAGS {
        cmd.arg(format!("-{tag}"));
    }
    cmd
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
        let mut cmd = stamp_command();
        cmd.arg("--");
        cmd.args(chunk);
        // No exiftool, or a chunk it could not read: those files have no capture time.
        let Ok(output) = cmd.output() else { continue };
        let Ok(serde_json::Value::Array(objects)) = serde_json::from_slice(&output.stdout) else {
            continue;
        };
        for obj in objects.iter().filter_map(|v| v.as_object()) {
            if let Some(path) = obj.get("SourceFile").and_then(|v| v.as_str()) {
                out.insert(PathBuf::from(path), parse_stamp(obj));
            }
        }
    }
    Some(out)
}

/// The [`CaptureStamp`] of bytes held in memory (a bundle's original), read by exiftool from
/// stdin with the command [`read_capture_stamps`] uses: nothing is written to disk. Bytes
/// exiftool cannot read (or no exiftool) have no capture time.
pub fn read_capture_stamp_of(bytes: &[u8]) -> CaptureStamp {
    use std::io::Write;
    use std::process::Stdio;

    let mut cmd = stamp_command();
    cmd.arg("-").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else { return CaptureStamp::default() };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return CaptureStamp::default();
    };
    // Fed from another thread while this one drains stdout, so neither pipe can fill and
    // stall the other. exiftool may stop reading once it has what it needs; the broken pipe
    // that leaves the writer is not an error. Dropping `stdin` at the end closes it.
    let output = std::thread::scope(|scope| {
        scope.spawn(move || {
            let _ = stdin.write_all(bytes);
        });
        child.wait_with_output()
    });
    let Ok(output) = output else { return CaptureStamp::default() };
    match serde_json::from_slice(&output.stdout) {
        Ok(serde_json::Value::Array(objects)) => {
            objects.first().and_then(|v| v.as_object()).map(parse_stamp).unwrap_or_default()
        }
        _ => CaptureStamp::default(),
    }
}

fn parse_stamp(obj: &serde_json::Map<String, serde_json::Value>) -> CaptureStamp {
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
    // An all-zero date ("0000:00:00 00:00:00", a camera or muxer that never set it) is no
    // capture time: two clips both carrying it are not thereby one.
    let date = |key: &str| text(key).filter(|s| s.chars().any(|c| c.is_ascii_digit() && c != '0'));
    CaptureStamp {
        time: date("DateTimeOriginal"),
        subsec: text("SubSecTimeOriginal"),
        serials,
        created: date("CreateDate"),
    }
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

/// Which of `candidates` (the library files a bundle original may already be,
/// [`same_size_candidates`]) is the photo whose bytes are `bytes`, by #246's rule, without
/// writing those bytes anywhere: `Some(Some(path))` the same photo, `Some(None)` a different
/// one, `None` once `abort` is set.
///
/// A candidate holding exactly these bytes is the same photo — under the rule identical
/// bytes always are (one reader gives them one stamp, or their equal contents decide) — and
/// is found by a streamed comparison that stops at the first differing byte, so re-importing
/// a bundle the library already holds starts no exiftool at all. Only when no candidate is
/// byte-identical are the stamps read: the bytes' own by exiftool from stdin
/// ([`read_capture_stamp_of`]), the candidates' from their paths, with the same command. A
/// pair with no capture time on either side is then different: its contents were just
/// compared.
pub fn find_in_library(bytes: &[u8], candidates: &[PathBuf], abort: &AtomicBool) -> Option<Option<PathBuf>> {
    // No collision is no decision: there is nothing for a stop to interrupt (the caller reads
    // `abort` between originals).
    if candidates.is_empty() {
        return Some(None);
    }
    for c in candidates {
        if abort.load(Ordering::Relaxed) {
            return None;
        }
        if holds_bytes(c, bytes) {
            return Some(Some(c.clone()));
        }
    }
    if abort.load(Ordering::Relaxed) {
        return None;
    }
    let arriving = read_capture_stamp_of(bytes);
    if !arriving.has_capture_time() {
        // No capture time: only a candidate without one either could be this photo, and the
        // contents decide that pair — they were just found to differ.
        return Some(None);
    }
    let stamps = read_capture_stamps(candidates, abort)?;
    let none = CaptureStamp::default();
    Some(
        candidates
            .iter()
            .find(|c| same_capture(&arriving, stamps.get(*c).unwrap_or(&none)) == Some(true))
            .cloned(),
    )
}

/// Whether the file at `path` holds exactly `bytes`, read in chunks and stopping at the first
/// difference. A file that cannot be read does not.
fn holds_bytes(path: &Path, bytes: &[u8]) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    if file.metadata().map(|m| m.len()).ok() != Some(bytes.len() as u64) {
        return false;
    }
    let mut buf = vec![0u8; 256 * 1024];
    let mut rest = bytes;
    loop {
        let n = match file.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        };
        if n == 0 {
            return rest.is_empty();
        }
        if n > rest.len() || buf[..n] != rest[..n] {
            return false;
        }
        rest = &rest[n..];
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
            created: None,
        }
    }

    fn created(date: &str) -> CaptureStamp {
        CaptureStamp { created: Some(date.to_string()), ..Default::default() }
    }

    /// L-3 of the #246 review: a file with no `DateTimeOriginal` (a camera's video) is
    /// compared on its `CreateDate`, tag to tag — never one file's `DateTimeOriginal` against
    /// the other's `CreateDate`.
    #[test]
    fn without_date_time_original_the_create_date_is_the_capture_time() {
        let a = created("2026:06:28 12:00:00");
        assert_eq!(same_capture(&a, &a.clone()), Some(true));
        assert_eq!(same_capture(&a, &created("2026:06:28 12:00:01")), Some(false));
        assert_eq!(same_capture(&a, &CaptureStamp::default()), Some(false));
        let dto = stamp(Some("2026:06:28 12:00:00"), None, &[]);
        assert_eq!(same_capture(&a, &dto), Some(false), "another tag is a difference");
        let both = CaptureStamp { created: Some("2026:06:28 13:00:00".into()), ..dto.clone() };
        assert_eq!(same_capture(&dto, &both), Some(true), "DateTimeOriginal wins over CreateDate");
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

    // --- bytes in memory (a bundle's original) -------------------------------------------

    /// exiftool reading bytes from stdin gives the stamp it reads from the file holding them.
    #[test]
    fn the_stamp_of_bytes_is_the_stamp_of_their_file() {
        if !test_files::exiftool_available("the_stamp_of_bytes_is_the_stamp_of_their_file") {
            return;
        }
        let dir = temp("stdin");
        let a = dir.join("a.jpg");
        test_files::stamped_jpeg(&a, "2026:06:28 12:00:00", "123", "4711");
        let from_path = read_capture_stamps(std::slice::from_ref(&a), &AtomicBool::new(false)).unwrap();
        let from_bytes = read_capture_stamp_of(&std::fs::read(&a).unwrap());
        assert_eq!(from_bytes.time.as_deref(), Some("2026:06:28 12:00:00"));
        assert_eq!(Some(&from_bytes), from_path.get(&a));
        assert_eq!(read_capture_stamp_of(b"not an image"), CaptureStamp::default());
    }

    /// Bytes held in memory are matched without being written anywhere: to a byte-identical
    /// library file, or — when the bytes differ — by the stamps, read from stdin on their
    /// side. Another capture, or other bytes with no capture time, is not matched.
    #[test]
    fn bytes_are_matched_against_the_library_without_writing_them() {
        if !test_files::exiftool_available("bytes_are_matched_against_the_library_without_writing_them") {
            return;
        }
        let dir = temp("in-memory");
        let lib = dir.join("lib/DSC1.jpg");
        let other_capture = dir.join("lib/DSC1 (2).jpg");
        test_files::stamped_jpeg(&lib, "2026:06:28 12:00:00", "123", "4711");
        test_files::stamped_jpeg(&other_capture, "2026:06:28 12:00:00", "456", "4711");
        let never = AtomicBool::new(false);
        let candidates = vec![other_capture.clone(), lib.clone()];

        let same = std::fs::read(&lib).unwrap();
        assert_eq!(find_in_library(&same, &candidates, &never), Some(Some(lib.clone())));

        // The same capture with other bytes of the same size (a byte of the pixels changed):
        // the stamps decide.
        let mut retouched = same.clone();
        let last = retouched.len() - 3;
        retouched[last] ^= 0x01;
        assert_eq!(find_in_library(&retouched, &candidates, &never), Some(Some(lib.clone())));

        let mut another = std::fs::read(&other_capture).unwrap();
        let last = another.len() - 3;
        another[last] ^= 0x01;
        assert_eq!(find_in_library(&another, &[lib.clone()], &never), Some(None), "another sub-second");

        let png = dir.join("lib/a.png");
        std::fs::write(&png, b"bytes one").unwrap();
        assert_eq!(find_in_library(b"bytes two", std::slice::from_ref(&png), &never), Some(None));
        assert_eq!(find_in_library(b"bytes one", std::slice::from_ref(&png), &never), Some(Some(png.clone())));
        assert_eq!(find_in_library(&same, &candidates, &AtomicBool::new(true)), None, "an abort stops it");
    }

    /// L-3: exiftool reads `CreateDate` where `DateTimeOriginal` is absent, so two files of
    /// one capture time and different bytes are matched by it, not by their contents. An
    /// all-zero date is no capture time.
    #[test]
    fn a_file_without_date_time_original_is_matched_by_its_create_date() {
        if !test_files::exiftool_available("a_file_without_date_time_original_is_matched_by_its_create_date") {
            return;
        }
        let dir = temp("create-date");
        let write = |name: &str, date: &str, pixel: u8| {
            let p = dir.join(name);
            image::RgbImage::from_pixel(16, 16, image::Rgb([pixel, 80, 40]))
                .save_with_format(&p, image::ImageFormat::Jpeg)
                .unwrap();
            let ok = Command::new("exiftool")
                .args(["-q", "-overwrite_original"])
                .arg(format!("-CreateDate={date}"))
                .arg(&p)
                .status()
                .unwrap()
                .success();
            assert!(ok);
            p
        };
        let lib = write("lib.jpg", "2026:06:28 12:00:00", 120);
        let same = write("same.jpg", "2026:06:28 12:00:00", 10);
        let other = write("other.jpg", "2026:06:28 12:00:01", 10);
        assert_ne!(std::fs::read(&lib).unwrap(), std::fs::read(&same).unwrap());
        let stamps = read_capture_stamps(&[lib.clone()], &AtomicBool::new(false)).unwrap();
        assert_eq!(stamps.get(&lib).map(|s| (s.time.clone(), s.created.clone())), Some((None, Some("2026:06:28 12:00:00".into()))));
        let arrivals = vec![
            Arrival { file: same, candidates: vec![lib.clone()] },
            Arrival { file: other, candidates: vec![lib.clone()] },
        ];
        let found = find_already_imported(&arrivals, &AtomicBool::new(false)).unwrap();
        assert_eq!(found, vec![Some(lib.clone()), None]);
        assert_eq!(
            find_in_library(&std::fs::read(dir.join("same.jpg")).unwrap(), &[lib.clone()], &AtomicBool::new(false)),
            Some(Some(lib)),
            "from stdin too"
        );
    }

    #[test]
    fn an_all_zero_date_is_no_capture_time() {
        let obj = serde_json::json!({
            "SourceFile": "a.mp4",
            "CreateDate": "0000:00:00 00:00:00",
            "DateTimeOriginal": "0000:00:00 00:00:00",
        });
        let stamp = parse_stamp(obj.as_object().unwrap());
        assert!(!stamp.has_capture_time(), "{stamp:?}");
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
