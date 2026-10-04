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
//! A file with no `DateTimeOriginal` — a camera's video — has its QuickTime `CreateDate`
//! (read group-qualified, `-QuickTime:CreateDate`) as its capture time, compared only against
//! the other file's. A still's EXIF or XMP `CreateDate` is not read: it is not the capture
//! time the owner's rule names, and it has no sub-second to tell a burst apart, so a still
//! without `DateTimeOriginal` has no capture time.
//! Only when neither file has a capture time to compare (a PNG, a stripped JPEG) are their
//! contents compared, by streamed SHA-256. File mtime is never evidence: a copy changes it.
//!
//! The metadata is read by one exiftool process per [`STAMP_BATCH`] files, over the
//! colliding pairs only, so re-importing a card whose every file collides reads a few KB of
//! each file rather than hashing the card and its library copies. Both sides of a pair are
//! read by the same pass with the same arguments, so one file's bytes always give one
//! answer. A bundle's original arrives as bytes in memory ([`find_in_library`]): exiftool
//! reads them from stdin in the same process that reads the library files — they are never
//! written to disk to be compared — and the stamps decide first; a library file holding
//! exactly those bytes is the same photo all the same. A file exiftool cannot read (or no exiftool at all) has no capture time: with
//! the other side's time known that is a difference, and with neither known the contents
//! decide.

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
    /// QuickTime `CreateDate`: the capture time of a file with no `DateTimeOriginal` — a
    /// camera's video — so a re-imported clip is not hashed whole. Never a still's EXIF or
    /// XMP `CreateDate` ([`stamp_command`] reads the QuickTime group only).
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
/// the same way give the same answer. So is a sub-second on one side only. A serial is
/// compared per tag, only where both files carry that tag (the owner's "when both files have
/// it").
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
    // `-QuickTime:CreateDate` is printed as `CreateDate` (no `-G`), and only from the
    // QuickTime group: a still's EXIF `CreateDate` (DateTimeDigitized) is not read.
    cmd.args(["-j", "-DateTimeOriginal", "-SubSecTimeOriginal", "-QuickTime:CreateDate"]);
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
    read_stamps_with_bytes(bytes, &[]).0
}

/// The [`CaptureStamp`] of `bytes`, read from stdin, and of each of `paths`, read **by the
/// same exiftool process** ([`stamp_command`], `exiftool … -- - <paths>`): one exiftool, one
/// version and one set of arguments for both sides of a comparison. Paths beyond one
/// [`STAMP_BATCH`] are read by [`read_capture_stamps`]'s further processes, with the same
/// command. Whatever exiftool cannot read (or no exiftool) has no capture time.
fn read_stamps_with_bytes(bytes: &[u8], paths: &[PathBuf]) -> (CaptureStamp, HashMap<PathBuf, CaptureStamp>) {
    use std::io::Write;
    use std::process::Stdio;

    let (first, rest) = paths.split_at(paths.len().min(STAMP_BATCH - 1));
    let mut stamps = if rest.is_empty() {
        HashMap::new()
    } else {
        read_capture_stamps(rest, &AtomicBool::new(false)).unwrap_or_default()
    };
    let mut cmd = stamp_command();
    cmd.args(["--", "-"]).args(first);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    let Ok(mut child) = cmd.spawn() else { return (CaptureStamp::default(), stamps) };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return (CaptureStamp::default(), stamps);
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
    let mut arriving = CaptureStamp::default();
    let Ok(output) = output else { return (arriving, stamps) };
    if let Ok(serde_json::Value::Array(objects)) = serde_json::from_slice(&output.stdout) {
        for obj in objects.iter().filter_map(|v| v.as_object()) {
            match obj.get("SourceFile").and_then(|v| v.as_str()) {
                Some("-") => arriving = parse_stamp(obj),
                Some(path) => {
                    stamps.insert(PathBuf::from(path), parse_stamp(obj));
                }
                None => {}
            }
        }
    }
    (arriving, stamps)
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
/// [`same_size_candidates`], so of its size) is the photo whose bytes are `bytes`, by #246's
/// rule, without writing those bytes anywhere: `Some(Some(path))` the same photo,
/// `Some(None)` a different one, `None` once `abort` is set.
///
/// The stamps decide first, as for a card (L-d of the second #246 review): the bytes' own
/// read by exiftool from stdin and the candidates' from their paths, by one exiftool process
/// ([`read_stamps_with_bytes`]) — a few KB of each file, so re-importing 2000 RAWs from a NAS
/// reads 2000 headers, not 120 GB. Only where the stamps do not say "the same capture" are the
/// contents compared, streamed and stopping at the first differing byte, because identical
/// bytes are always the same photo: with no capture time on either side that compare is the
/// rule itself (and reads the whole file when it matches); where the stamps differ it guards
/// against a read that failed on one side only, and stops within the first bytes for two
/// genuinely different captures, whose headers differ.
pub fn find_in_library(bytes: &[u8], candidates: &[PathBuf], abort: &AtomicBool) -> Option<Option<PathBuf>> {
    // No collision is no decision: there is nothing for a stop to interrupt (the caller reads
    // `abort` between originals).
    if candidates.is_empty() {
        return Some(None);
    }
    if abort.load(Ordering::Relaxed) {
        return None;
    }
    let (arriving, stamps) = read_stamps_with_bytes(bytes, candidates);
    let none = CaptureStamp::default();
    let verdict = |c: &PathBuf| same_capture(&arriving, stamps.get(c).unwrap_or(&none));
    if let Some(c) = candidates.iter().find(|c| verdict(c) == Some(true)) {
        return Some(Some(c.clone()));
    }
    for c in candidates {
        if abort.load(Ordering::Relaxed) {
            return None;
        }
        if holds_bytes(c, bytes) {
            return Some(Some(c.clone()));
        }
    }
    Some(None)
}

#[cfg(test)]
thread_local! {
    /// Bytes [`holds_bytes`] read on this thread: what a test counts to tell a stamp decision
    /// from a contents compare.
    static BYTES_COMPARED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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
        #[cfg(test)]
        BYTES_COMPARED.with(|c| c.set(c.get() + n as u64));
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
/// itself and every ` (n)` name beside it — the names an earlier import gave different
/// photos of that name — in order of `n`. So a second import of a card that holds two photos
/// named alike finds the second one at its ` (2)` name and does not copy it a third time.
/// The directory is listed rather than the names probed in turn: a gap in the numbers (a
/// ` (2)` the user deleted) or a missing `dest` does not hide the names after it.
///
/// Lists the folder afresh; a run over many files uses one [`FolderListings`].
pub fn same_size_candidates(dest: &Path, size: u64) -> Vec<PathBuf> {
    FolderListings::default().same_size_candidates(dest, size)
}

/// The ` (n)` names in each library folder, listed once per run (L-c of the second #246
/// review): a card of 5000 files bound for one day's folder would otherwise list that folder
/// 5000 times — quadratic, and on a NAS each listing is a fresh round of READDIRs. Card
/// ingest plans every file before it copies one, so its listings hold for the plan; the
/// bundle importer places files as it goes and records each name it places
/// ([`Self::placed`]). A file another program puts in a folder after it was listed is not a
/// candidate — as before, when it could land just after the listing — and is still never
/// overwritten ([`create_new_file`]).
#[derive(Default)]
pub struct FolderListings {
    /// Per folder: for each `(stem, extension)`, the `n` and path of every
    /// `stem (n).extension` there.
    dirs: HashMap<PathBuf, HashMap<NameKey, Vec<(u32, PathBuf)>>>,
}

/// A file name's stem and extension, as [`numbered`] splits them.
type NameKey = (String, Option<String>);

impl FolderListings {
    /// [`same_size_candidates`], from this run's listing of `dest`'s folder.
    pub fn same_size_candidates(&mut self, dest: &Path, size: u64) -> Vec<PathBuf> {
        let same_size = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() == size);
        let mut out = Vec::new();
        if same_size(dest) {
            out.push(dest.to_path_buf());
        }
        let (Some(dir), Some(stem)) = (dest.parent(), dest.file_stem().and_then(|s| s.to_str())) else {
            return out;
        };
        let key = (stem.to_string(), dest.extension().and_then(|s| s.to_str()).map(str::to_string));
        let Some(names) = self.listing(dir).get(&key) else { return out };
        let mut numbered: Vec<&(u32, PathBuf)> = names.iter().filter(|(_, p)| same_size(p)).collect();
        numbered.sort();
        out.extend(numbered.into_iter().map(|(_, p)| p.clone()));
        out
    }

    /// Record a file this run placed at `path`, so a later file of the run sees it.
    pub fn placed(&mut self, path: &Path) {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|s| s.to_str())) else {
            return;
        };
        // A folder not listed yet will show the file when it is.
        let Some(listing) = self.dirs.get_mut(dir) else { return };
        if let Some((key, n)) = numbered_parts(name) {
            let names = listing.entry(key).or_default();
            if !names.iter().any(|(_, p)| p == path) {
                names.push((n, path.to_path_buf()));
            }
        }
    }

    /// `dir`'s ` (n)` names, listed on first use. A folder that cannot be listed (not created
    /// yet) has none.
    fn listing(&mut self, dir: &Path) -> &HashMap<NameKey, Vec<(u32, PathBuf)>> {
        self.dirs.entry(dir.to_path_buf()).or_insert_with(|| {
            let mut by_key: HashMap<NameKey, Vec<(u32, PathBuf)>> = HashMap::new();
            for entry in std::fs::read_dir(dir).into_iter().flatten().filter_map(|e| e.ok()) {
                let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
                if let Some((key, n)) = numbered_parts(&name) {
                    by_key.entry(key).or_default().push((n, entry.path()));
                }
            }
            by_key
        })
    }
}

/// The base name's `(stem, extension)` and `n` when `name` is `stem (n).ext` or `stem (n)`
/// ([`numbered`]'s form, `n` ≥ 2 written without leading zeros), else `None`.
fn numbered_parts(name: &str) -> Option<(NameKey, u32)> {
    let path = Path::new(name);
    let stem = path.file_stem()?.to_str()?;
    let ext = path.extension().and_then(|s| s.to_str());
    let open = stem.rfind(" (")?;
    let digits = stem[open + 2..].strip_suffix(')')?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = digits.parse().ok().filter(|n| *n >= 2)?;
    Some(((stem[..open].to_string(), ext.map(str::to_string)), n))
}

/// A destination path that is free ([`name_free`]): `path`, or `name (2).ext`, …, or `None`
/// if no free name was found — the caller must then NOT copy (never overwrite an existing
/// file).
pub fn unique_dest(path: &Path) -> Option<PathBuf> {
    std::iter::once(path.to_path_buf())
        .chain((2..10_000).map(|n| numbered(path, n)))
        .find(|c| name_free(c))
}

/// Whether a new original may take `path`: nothing is there — a dangling symlink counts as
/// something — and nothing is at its sidecar's name (`<name>.xmp`) either. A sidecar with no
/// original beside it (another tool's, or one whose original was removed) belongs to some
/// other photo: a new original placed beside it would adopt its identity and metadata, and a
/// bundle's sidecar written there would destroy it.
fn name_free(path: &Path) -> bool {
    let absent = |p: &Path| std::fs::symlink_metadata(p).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
    absent(path) && absent(&crate::xmp::sidecar_path(path))
}

/// Create a new file at `wanted`, or at the next free ` (n)` name beside it, and fill it with
/// `fill`; returns where it landed. An existing file is never replaced: the name is claimed
/// by an exclusive create (`O_CREAT|O_EXCL`), so a file that appears there between finding
/// the name free and claiming it — another import, another program — makes this one move on
/// to the next free name instead of overwriting it. The contents are synced before the name
/// is reported placed; a fill that fails removes the file it created (and only that one).
pub fn create_new_file(
    wanted: &Path,
    fill: impl FnMut(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    create_new_with(wanted, unique_dest, fill)
}

/// [`create_new_file`], with the free-name search given — where a test makes a name be taken
/// after it was found free.
fn create_new_with(
    wanted: &Path,
    next_free: impl Fn(&Path) -> Option<PathBuf>,
    mut fill: impl FnMut(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    use std::io::{Error, ErrorKind};
    // Each lost race means another file now holds a name; a bound keeps a pathological
    // directory from spinning here.
    for _ in 0..100 {
        let Some(candidate) = next_free(wanted) else { break };
        let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        return match fill(&mut file).and_then(|()| file.sync_all()) {
            Ok(()) => Ok(candidate),
            Err(e) => {
                drop(file);
                let _ = std::fs::remove_file(&candidate);
                Err(e)
            }
        };
    }
    Err(Error::new(ErrorKind::AlreadyExists, format!("no free name beside {}", wanted.display())))
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
    fn the_candidates_are_the_same_size_numbered_names() {
        let dir = temp("chain");
        let dest = dir.join("DSC1.ARW");
        assert!(same_size_candidates(&dest, 4).is_empty());
        std::fs::write(&dest, b"aaaa").unwrap();
        std::fs::write(dir.join("DSC1 (2).ARW"), b"bbbbb").unwrap(); // another size
        std::fs::write(dir.join("DSC1 (3).ARW"), b"cccc").unwrap();
        std::fs::write(dir.join("DSC1 (12).ARW"), b"eeee").unwrap();
        std::fs::write(dir.join("DSC1 (5).ARW"), b"dddd").unwrap(); // past the gap at (4)
        // Not ` (n)` names of DSC1.ARW.
        for other in ["DSC1 (05).ARW", "DSC1 (1).ARW", "DSC1 (x).ARW", "DSC1 (6).JPG", "DSC10 (2).ARW", "DSC1 (7)"] {
            std::fs::write(dir.join(other), b"ffff").unwrap();
        }
        let n = |k: u32| dir.join(format!("DSC1 ({k}).ARW"));
        assert_eq!(same_size_candidates(&dest, 4), vec![dest.clone(), n(3), n(5), n(12)]);
        assert_eq!(unique_dest(&dest), Some(n(4)));
    }

    /// L-5 of the #246 review: the base name gone (the user deleted it) does not hide the
    /// ` (n)` names beside it.
    #[test]
    fn the_numbered_names_count_without_the_base_name() {
        let dir = temp("chain-no-base");
        let dest = dir.join("DSC1.ARW");
        std::fs::write(dir.join("DSC1 (2).ARW"), b"aaaa").unwrap();
        assert_eq!(same_size_candidates(&dest, 4), vec![dir.join("DSC1 (2).ARW")]);
        let bare = dir.join("README");
        std::fs::write(dir.join("README (3)"), b"bbbb").unwrap();
        assert_eq!(same_size_candidates(&bare, 4), vec![dir.join("README (3)")]);
    }

    /// L-c of the second #246 review: a run lists each folder once. A ` (n)` name that
    /// appears after the listing is a candidate only when the run placed it (`placed`); the
    /// plain name is looked at directly every time.
    #[test]
    fn a_folder_is_listed_once_per_run_and_told_of_what_the_run_places() {
        let dir = temp("listings");
        let dest = dir.join("DSC1.ARW");
        std::fs::write(dir.join("DSC1 (2).ARW"), b"aaaa").unwrap();
        let mut listings = FolderListings::default();
        assert_eq!(listings.same_size_candidates(&dest, 4), vec![dir.join("DSC1 (2).ARW")]);
        std::fs::write(&dest, b"bbbb").unwrap();
        std::fs::write(dir.join("DSC1 (3).ARW"), b"cccc").unwrap();
        std::fs::write(dir.join("DSC1 (4).ARW"), b"dddd").unwrap();
        listings.placed(&dir.join("DSC1 (4).ARW"));
        assert_eq!(
            listings.same_size_candidates(&dest, 4),
            vec![dest.clone(), dir.join("DSC1 (2).ARW"), dir.join("DSC1 (4).ARW")],
            "not listed again: (3) is not seen, the placed (4) is"
        );
        assert_eq!(same_size_candidates(&dest, 4).len(), 4, "a fresh listing sees all");
    }

    // --- placing a new file (L-4) ---------------------------------------------------------

    /// M-a of the second #246 review: a name whose sidecar exists without it (an orphan
    /// sidecar) is not free, nor is one a dangling symlink holds; the plain name is checked
    /// the same way.
    #[test]
    fn a_name_with_an_orphan_sidecar_or_a_dangling_link_is_not_free() {
        let dir = temp("orphan");
        let dest = dir.join("DSC1.ARW");
        std::fs::write(dir.join("DSC1.ARW.xmp"), b"<orphan/>").unwrap();
        std::fs::write(dir.join("DSC1 (2).ARW.xmp"), b"<orphan/>").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("gone"), dir.join("DSC1 (3).ARW")).unwrap();
        #[cfg(not(unix))]
        std::fs::write(dir.join("DSC1 (3).ARW"), b"x").unwrap();
        assert_eq!(unique_dest(&dest), Some(dir.join("DSC1 (4).ARW")));
        let placed = create_new_file(&dest, |f| {
            use std::io::Write;
            f.write_all(b"arriving")
        })
        .unwrap();
        assert_eq!(placed, dir.join("DSC1 (4).ARW"));
        assert_eq!(std::fs::read(dir.join("DSC1 (2).ARW.xmp")).unwrap(), b"<orphan/>");
    }

    /// A name found free but taken before it is claimed (another program wrote it in
    /// between) keeps its file: the new one moves on to the next free name.
    #[test]
    fn a_name_taken_after_it_was_found_free_is_never_overwritten() {
        let dir = temp("no-clobber");
        let wanted = dir.join("DSC1.ARW");
        std::fs::write(&wanted, b"library").unwrap();
        let raced = dir.join("DSC1 (2).ARW");
        std::fs::write(&raced, b"written meanwhile").unwrap();
        let asked = std::cell::Cell::new(0);
        // The first answer is the name as it was before the other writer took it.
        let next_free = |p: &Path| {
            asked.set(asked.get() + 1);
            if asked.get() == 1 { Some(raced.clone()) } else { unique_dest(p) }
        };
        let placed = create_new_with(&wanted, next_free, |f| {
            use std::io::Write;
            f.write_all(b"arriving")
        })
        .unwrap();
        assert_eq!(placed, dir.join("DSC1 (3).ARW"));
        assert_eq!(std::fs::read(&raced).unwrap(), b"written meanwhile");
        assert_eq!(std::fs::read(&wanted).unwrap(), b"library");
        assert_eq!(std::fs::read(&placed).unwrap(), b"arriving");
    }

    /// A fill that fails removes the file it created, and only that one.
    #[test]
    fn a_failed_fill_removes_only_its_own_file() {
        let dir = temp("fill-fails");
        let wanted = dir.join("DSC1.ARW");
        std::fs::write(&wanted, b"library").unwrap();
        let err = create_new_file(&wanted, |_| Err(std::io::Error::other("card pulled"))).unwrap_err();
        assert_eq!(err.to_string(), "card pulled");
        assert!(!dir.join("DSC1 (2).ARW").exists());
        assert_eq!(std::fs::read(&wanted).unwrap(), b"library");
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

    /// L-d of the second #246 review: with a capture time on both sides the stamps decide and
    /// no library file is read whole — a re-import of byte-identical stamped originals
    /// compares no content at all; with none on either side the contents decide, read in
    /// full when they match. The bytes' stamp and the library file's come from one exiftool
    /// process and agree.
    #[test]
    fn the_stamps_decide_before_any_contents_are_compared() {
        if !test_files::exiftool_available("the_stamps_decide_before_any_contents_are_compared") {
            return;
        }
        let dir = temp("stamps-first");
        let lib = dir.join("DSC1.jpg");
        test_files::stamped_jpeg(&lib, "2026:06:28 12:00:00", "123", "4711");
        let bytes = std::fs::read(&lib).unwrap();
        let (arriving, stamps) = read_stamps_with_bytes(&bytes, std::slice::from_ref(&lib));
        assert!(arriving.has_capture_time());
        assert_eq!(stamps.get(&lib), Some(&arriving), "one process, one answer");

        let compared = || BYTES_COMPARED.with(|c| c.get());
        let never = AtomicBool::new(false);
        let before = compared();
        assert_eq!(find_in_library(&bytes, std::slice::from_ref(&lib), &never), Some(Some(lib.clone())));
        assert_eq!(compared(), before, "decided by the stamps, no content read");

        let png = dir.join("a.png");
        std::fs::write(&png, b"bytes one").unwrap();
        let before = compared();
        assert_eq!(find_in_library(b"bytes one", std::slice::from_ref(&png), &never), Some(Some(png.clone())));
        assert_eq!(compared() - before, 9, "no capture time: the contents decide, whole");
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

    /// L-3, narrowed by L-b of the second review: a video with no `DateTimeOriginal` is
    /// matched by its QuickTime `CreateDate` — two clips of one creation time and different
    /// bytes are the same capture, another time is not — from its path and from stdin alike.
    #[test]
    fn a_video_is_matched_by_its_quicktime_create_date() {
        let test = "a_video_is_matched_by_its_quicktime_create_date";
        if !test_files::exiftool_available(test) {
            return;
        }
        if !Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success()) {
            println!("SKIPPED: {test} — ffmpeg is not installed");
            return;
        }
        let dir = temp("quicktime");
        let clip = |name: &str, color: &str, when: &str| {
            let p = dir.join(name);
            let ok = Command::new("ffmpeg")
                .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i"])
                .arg(format!("color=c={color}:s=16x16:d=0.1"))
                .arg("-metadata")
                .arg(format!("creation_time={when}"))
                .arg(&p)
                .status()
                .unwrap()
                .success();
            assert!(ok, "ffmpeg wrote {}", p.display());
            p
        };
        let lib = clip("lib.mp4", "red", "2026-06-28T12:00:00Z");
        let same = clip("same.mp4", "blue", "2026-06-28T12:00:00Z");
        let other = clip("other.mp4", "blue", "2026-06-28T12:00:01Z");
        assert_ne!(std::fs::read(&lib).unwrap(), std::fs::read(&same).unwrap());
        let stamps = read_capture_stamps(std::slice::from_ref(&lib), &AtomicBool::new(false)).unwrap();
        assert_eq!(
            stamps.get(&lib).map(|s| (s.time.clone(), s.created.clone())),
            Some((None, Some("2026:06:28 12:00:00".into())))
        );
        let arrivals = vec![
            Arrival { file: same.clone(), candidates: vec![lib.clone()] },
            Arrival { file: other.clone(), candidates: vec![lib.clone()] },
        ];
        let found = find_already_imported(&arrivals, &AtomicBool::new(false)).unwrap();
        assert_eq!(found, vec![Some(lib.clone()), None]);
        let never = AtomicBool::new(false);
        let from_stdin = |p: &Path| find_in_library(&std::fs::read(p).unwrap(), std::slice::from_ref(&lib), &never);
        assert_eq!(from_stdin(&same), Some(Some(lib.clone())), "from stdin too");
        assert_eq!(from_stdin(&other), Some(None));
    }

    /// L-b of the second #246 review: a still's EXIF `CreateDate` is not a capture time. Two
    /// JPEGs carrying only that date, equal to the second, with different pixels are different
    /// photos (their contents decide); a byte copy is still the same photo.
    #[test]
    fn a_stills_exif_create_date_is_not_a_capture_time() {
        if !test_files::exiftool_available("a_stills_exif_create_date_is_not_a_capture_time") {
            return;
        }
        let dir = temp("exif-create-date");
        let write = |name: &str, pixel: u8| {
            let p = dir.join(name);
            image::RgbImage::from_pixel(16, 16, image::Rgb([pixel, 80, 40]))
                .save_with_format(&p, image::ImageFormat::Jpeg)
                .unwrap();
            let ok = Command::new("exiftool")
                .args(["-q", "-overwrite_original", "-CreateDate=2026:06:28 12:00:00"])
                .arg(&p)
                .status()
                .unwrap()
                .success();
            assert!(ok);
            p
        };
        let lib = write("lib.jpg", 120);
        let other = write("other.jpg", 10);
        let copy = dir.join("copy.jpg");
        std::fs::copy(&lib, &copy).unwrap();
        let stamps = read_capture_stamps(std::slice::from_ref(&lib), &AtomicBool::new(false)).unwrap();
        assert!(!stamps.get(&lib).cloned().unwrap_or_default().has_capture_time(), "{stamps:?}");
        let arrivals = vec![
            Arrival { file: other.clone(), candidates: vec![lib.clone()] },
            Arrival { file: copy.clone(), candidates: vec![lib.clone()] },
        ];
        let found = find_already_imported(&arrivals, &AtomicBool::new(false)).unwrap();
        assert_eq!(found, vec![None, Some(lib.clone())]);
        let never = AtomicBool::new(false);
        assert_eq!(find_in_library(&std::fs::read(&other).unwrap(), std::slice::from_ref(&lib), &never), Some(None));
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
