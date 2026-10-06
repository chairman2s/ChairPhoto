//! The hidden files the storage lifecycle keeps beside photos while it works, and the
//! recovery of ones a crash left behind.
//!
//! - `.<name>.chairphoto-offload-<pid>-<n>` — a local file an offload has moved aside to
//!   re-hash it and then delete it (#256). While it has that name, nothing else can write
//!   into it through the photo's own name: a writer either landed before the move (and the
//!   re-hash sees it) or makes a new file at the photo's name (which the offload sees, and
//!   keeps). A name too long to take the ~50 bytes this adds within the 255-byte limit goes
//!   into a hidden folder instead, under its own name: `.chairphoto-offload-<pid>-<n>/<name>`
//!   (review of #256, NIT-4) — never a shortened name, which a crash would leave with
//!   nothing to say what it was called.
//!
//! - `.<name>.chairphoto-part-<pid>-<n>` — a copy being written before it is verified and
//!   given its name (`same_photo::create_part`: a lifecycle copy, an import).
//!
//! A crash can leave either behind. An offload's file may hold the only copy of a local edit
//! that never reached home, so it is never deleted by a sweep: [`sweep_once`] puts it back
//! under its own name, without replacing anything there. A copy's temporary file only ever
//! holds bytes that exist elsewhere, and is removed once it is stale ([`STALE_PART_AGE`]).
//!
//! Everything here is file IO: call it off the catalog lock, on a blocking worker.

use crate::scanner::same_photo::PART_TAG;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

/// The tag in the name of a file an offload moved aside.
pub(crate) const ASIDE_TAG: &str = "chairphoto-offload";

/// The longest file name the filesystems a library lives on take, in bytes (`NAME_MAX`).
const NAME_MAX: usize = 255;

/// What `.<name>.<tag>-<pid>-<n>` adds to a name at most: the two dots, the tag, the dashes,
/// a 10-digit pid and a 20-digit counter.
const ASIDE_OVERHEAD: usize = 2 + ASIDE_TAG.len() + 2 + 10 + 20;

/// Move `file` to a new hidden name beside it, unique to this call; `None` when there was no
/// `file` to move. The new name is claimed first by an exclusive create, so the rename only
/// ever replaces that empty file of ours — on every filesystem, with no fallback needed.
///
/// A name too long for the hidden name to fit in [`NAME_MAX`] is moved, under its own name,
/// into a new hidden folder beside it instead ([`move_into_aside_folder`]).
pub(crate) fn move_aside(file: &Path) -> std::io::Result<Option<PathBuf>> {
    if file.file_name().map_or(0, |n| n.len()) + ASIDE_OVERHEAD > NAME_MAX {
        return move_into_aside_folder(file);
    }
    let (aside, placeholder) = crate::scanner::same_photo::create_hidden(file, ASIDE_TAG)?;
    drop(placeholder);
    match std::fs::rename(file, &aside) {
        Ok(()) => Ok(Some(aside)),
        Err(e) => {
            let _ = std::fs::remove_file(&aside);
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

/// [`move_aside`] for a long name: a new folder `.chairphoto-offload-<pid>-<n>` beside
/// `file`, created exclusively, and `file` renamed into it under its own name — which can
/// replace nothing, the folder being new and ours. A folder left empty (there was no `file`)
/// is removed again.
fn move_into_aside_folder(file: &Path) -> std::io::Result<Option<PathBuf>> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = file.parent().unwrap_or_else(|| Path::new("."));
    let Some(name) = file.file_name() else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"));
    };
    let folder = loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let folder = dir.join(format!(".{ASIDE_TAG}-{}-{n}", std::process::id()));
        match std::fs::create_dir(&folder) {
            Ok(()) => break folder,
            // Left by a crashed run of a process that had this id: take the next number.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let aside = folder.join(name);
    match std::fs::rename(file, &aside) {
        Ok(()) => Ok(Some(aside)),
        Err(e) => {
            let _ = std::fs::remove_dir(&folder);
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

/// The hidden folder holding `aside`, if [`move_into_aside_folder`] put it in one.
fn aside_folder(aside: &Path) -> Option<&Path> {
    let folder = aside.parent()?;
    let name = folder.file_name()?.to_str()?;
    parse_folder(name).map(|_| folder)
}

/// Remove the hidden folder that held `aside` once it is empty; a folder holding anything
/// else is left as it is.
fn tidy(aside: &Path) {
    if let Some(folder) = aside_folder(aside) {
        let _ = std::fs::remove_dir(folder);
    }
}

/// Delete a confirmed file an offload moved aside, and the hidden folder that held it when
/// it had one.
pub(crate) fn delete_aside(aside: &Path) -> std::io::Result<()> {
    std::fs::remove_file(aside)?;
    tidy(aside);
    Ok(())
}

/// How a refusal names a moved-aside file: its hidden name, or its hidden folder and name.
pub(crate) fn aside_label(aside: &Path) -> String {
    let file = aside.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match aside_folder(aside).and_then(Path::file_name) {
        Some(folder) => format!("{}/{file}", folder.to_string_lossy()),
        None => file,
    }
}

/// Give a moved-aside file its name `original` back, never by a copy, and never replacing a
/// file there (`same_photo::place_no_replace_without_copy`). `Ok(false)` when the name is
/// taken (a new file was written there meanwhile); the aside file is then still where it was.
///
/// On a filesystem with neither a no-replace rename nor hard links (some FUSE mounts; current
/// Linux vfat, exFAT and SMB drivers accept `RENAME_NOREPLACE`) it is put back by a plain
/// rename once the name is seen free. A file created at that exact name between the look and
/// the rename would be replaced there — a window of one syscall, against leaving the photo's
/// file hidden for good.
pub(crate) fn put_back(aside: &Path, original: &Path) -> std::io::Result<bool> {
    let back = put_back_file(aside, original);
    if matches!(back, Ok(true)) {
        tidy(aside);
    }
    back
}

fn put_back_file(aside: &Path, original: &Path) -> std::io::Result<bool> {
    match crate::scanner::same_photo::place_no_replace_without_copy(aside, original) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            match std::fs::symlink_metadata(original) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::rename(aside, original).map(|()| true),
                Ok(_) => Ok(false),
                Err(e) => Err(e),
            }
        }
        Err(e) => Err(e),
    }
}

/// What a hidden name says: which kind of working file, the name of the file it belongs
/// beside, and the process that made it.
#[derive(Debug, PartialEq, Eq)]
struct Working<'a> {
    tag: &'a str,
    original: &'a str,
    pid: u32,
}

/// Parse `.<original>.<tag>-<pid>-<n>` for one of `tags`. Nothing else is ever a working file.
fn parse<'a>(name: &'a str, tags: &[&'a str]) -> Option<Working<'a>> {
    let rest = name.strip_prefix('.')?;
    for &tag in tags {
        let Some((original, numbers)) = rest.rsplit_once(&format!(".{tag}-")) else { continue };
        let Some((pid, n)) = numbers.split_once('-') else { continue };
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if original.is_empty() || !digits(pid) || !digits(n) {
            continue;
        }
        return Some(Working { tag, original, pid: pid.parse().ok()? });
    }
    None
}

/// Parse a hidden folder's name, `.chairphoto-offload-<pid>-<n>` ([`move_into_aside_folder`]),
/// to its pid.
fn parse_folder(name: &str) -> Option<u32> {
    let numbers = name.strip_prefix('.')?.strip_prefix(ASIDE_TAG)?.strip_prefix('-')?;
    let (pid, n) = numbers.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(pid) || !digits(n) {
        return None;
    }
    pid.parse().ok()
}

/// Whether process `pid` is running on this machine. Only Linux can say (`/proc`); elsewhere
/// every process is taken to be running, so nothing is ever swept there.
fn running(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Where each folder's sweep stands in this run: each is listed at most once, so a folder of
/// thousands of photos is not listed on every storage operation. A folder is `Done` only once
/// its sweep has finished, so an operation never plans in a folder whose crashed files are
/// still being put back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sweep {
    Running,
    Done,
}

static SWEPT: Mutex<Option<HashMap<PathBuf, Sweep>>> = Mutex::new(None);
static SWEPT_CHANGED: Condvar = Condvar::new();

/// Sweep each folder holding one of `paths` ([`sweep_once`]).
pub(crate) fn sweep_beside<'a>(paths: impl IntoIterator<Item = &'a Path>) {
    for path in paths {
        if let Some(dir) = path.parent() {
            sweep_once(dir);
        }
    }
}

/// Sweep every folder under `root`, `root` included ([`sweep_once`]): what a crashed run
/// left anywhere in the library is put back at start-up (review of #256, (b)), rather than
/// only once a storage operation happens to plan in that folder — until then the photo's
/// file is missing under its own name. Hidden folders are not entered (a hidden folder a
/// crashed offload made is recovered by the sweep of the folder holding it), and links are
/// not followed. Returns how many folders were swept. File IO only, possibly long on a big
/// library: a blocking worker, never the UI thread.
pub fn sweep_tree(root: &Path) -> usize {
    let mut swept = 0;
    let folders = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_str().is_some_and(|n| n.starts_with('.')))
        .flatten()
        .filter(|e| e.file_type().is_dir());
    for folder in folders {
        sweep_once(folder.path());
        swept += 1;
    }
    swept
}

/// Recover what a crashed run left in `dir`, once per folder per run (see the module docs).
/// Only names of the exact pattern, only regular files (a symlink is never followed or
/// touched), and only those whose process is no longer running. Best effort: a folder that
/// cannot be listed is tried again by a later call.
///
/// Returns only once `dir` has been swept in this run: a caller that finds another thread's
/// sweep of it running waits for that sweep to finish (or, if it could not list the folder,
/// sweeps it itself). Nothing else is held while it waits.
pub(crate) fn sweep_once(dir: &Path) {
    {
        let mut swept = SWEPT.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            match swept.get_or_insert_with(HashMap::new).get(dir) {
                Some(Sweep::Done) => return,
                Some(Sweep::Running) => {
                    swept = SWEPT_CHANGED.wait(swept).unwrap_or_else(|e| e.into_inner());
                }
                None => break,
            }
        }
        swept.get_or_insert_with(HashMap::new).insert(dir.to_path_buf(), Sweep::Running);
    }
    let listed = sweep(dir);
    let mut swept = SWEPT.lock().unwrap_or_else(|e| e.into_inner());
    let swept = swept.get_or_insert_with(HashMap::new);
    if listed {
        swept.insert(dir.to_path_buf(), Sweep::Done);
    } else {
        swept.remove(dir);
    }
    SWEPT_CHANGED.notify_all();
}

/// One sweep of `dir`; `false` when it could not be listed. A panic here would leave the
/// folder `Running` for good, so nothing in it panics: every step is best effort.
fn sweep(dir: &Path) -> bool {
    #[cfg(test)]
    tests::before_listing(dir);
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if let Some(pid) = name.to_str().and_then(parse_folder) {
            recover_aside_folder(&entry.path(), dir, pid);
            continue;
        }
        let Some(working) = name.to_str().and_then(|n| parse(n, &[ASIDE_TAG, PART_TAG])) else { continue };
        // `symlink_metadata`: decided from the entry itself, never through a link.
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
        if !meta.file_type().is_file() || running(working.pid) {
            continue;
        }
        if working.tag == ASIDE_TAG {
            recover_aside(&entry.path(), &dir.join(working.original), meta.len());
        } else if meta.modified().is_ok_and(|m| now.duration_since(m).is_ok_and(|age| age > STALE_PART_AGE)) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    true
}

/// How long a copy's temporary file must have gone unwritten before a sweep removes it, on
/// top of its process not running here. A backup folder on a NAS is shared: the pid in the
/// name may be a live process on another machine, whose copy is writing that file (its mtime
/// moves) or about to place it (within seconds of the last write). An hour leaves room for
/// clock skew between the machines; a temp file is only ever a copy of bytes that exist
/// elsewhere, so a late removal costs disk space, never data.
const STALE_PART_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Let `dir` be swept again in this run — a test standing in for the next run after a crash.
#[cfg(test)]
pub(crate) fn forget_swept(dir: &Path) {
    if let Some(swept) = SWEPT.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        swept.remove(dir);
    }
}

/// A process id that has exited (reaped), for a "crashed run" left-over in tests.
#[cfg(test)]
pub(crate) fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// Put back every file a crashed offload moved into the hidden folder `folder` (a long name,
/// [`move_into_aside_folder`]) under its own name in `dir`, as [`recover_aside`] does for a
/// hidden name, and remove the folder once it is empty. Only a real folder (never through a
/// link) of a process that is no longer running, and only the regular files in it.
fn recover_aside_folder(folder: &Path, dir: &Path, pid: u32) {
    let is_folder = std::fs::symlink_metadata(folder).is_ok_and(|m| m.file_type().is_dir());
    if !is_folder || running(pid) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(folder) else { return };
    for entry in entries.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
        if meta.file_type().is_file() {
            recover_aside(&entry.path(), &dir.join(entry.file_name()), meta.len());
        }
    }
    let _ = std::fs::remove_dir(folder);
}

/// Put a crashed offload's file back under its name, never replacing a file there.
///
/// An empty one is not put back: it is most likely the name an offload claimed and crashed
/// before filling (`move_aside` creates it empty, then renames the file onto it), and putting
/// it back would make a 0-byte original out of nothing. It is removed only when a non-empty
/// file holds its name — then it is that claim, with nothing to lose — and otherwise left.
/// Any other file whose name is taken is left where it is, for a person to look at.
fn recover_aside(aside: &Path, original: &Path, len: u64) {
    if len == 0 {
        if std::fs::symlink_metadata(original).is_ok_and(|m| m.len() > 0) {
            let _ = std::fs::remove_file(aside);
        }
        return;
    }
    match put_back(aside, original) {
        Ok(true) => eprintln!("storage: put back {} left by an interrupted offload", original.display()),
        Ok(false) => eprintln!(
            "storage: {} was left by an interrupted offload and {} is taken; left as it is",
            aside.display(),
            original.display()
        ),
        Err(e) => eprintln!(
            "storage: {} was left by an interrupted offload and could not be put back ({e}); left as it is",
            aside.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestTmpDir;

    type Pause = Box<dyn FnOnce() + Send>;
    /// Run by a sweep of the named folder just before it lists it, once — where a test holds
    /// one sweep while another caller arrives. Global, keyed by a test's own folder.
    static BEFORE_LISTING: Mutex<Option<HashMap<PathBuf, Pause>>> = Mutex::new(None);

    pub(super) fn before_listing(dir: &Path) {
        let hook = BEFORE_LISTING.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|m| m.remove(dir));
        if let Some(hook) = hook {
            hook();
        }
    }

    /// **Forced interleaving (review LOW-4).** A second operation in a folder whose sweep is
    /// still running does not go ahead before that sweep has put a crashed offload's file
    /// back: it waits, and finds the file where the catalog says it is.
    #[test]
    fn a_second_caller_waits_for_a_running_sweep() {
        if !cfg!(target_os = "linux") {
            println!("SKIPPED: a_second_caller_waits_for_a_running_sweep — needs /proc");
            return;
        }
        let dir = TestTmpDir::new("working-files-concurrent-sweep");
        let folder = dir.to_path_buf();
        std::fs::write(folder.join(format!(".A.ARW.{ASIDE_TAG}-{}-0", dead_pid())), b"A").unwrap();
        let (paused_tx, paused_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        BEFORE_LISTING.lock().unwrap().get_or_insert_with(HashMap::new).insert(
            folder.clone(),
            Box::new(move || {
                paused_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }),
        );
        let first = {
            let folder = folder.clone();
            std::thread::spawn(move || sweep_once(&folder))
        };
        paused_rx.recv().unwrap();
        let second = {
            let folder = folder.clone();
            std::thread::spawn(move || {
                sweep_once(&folder);
                folder.join("A.ARW").exists()
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        let returned_early = second.is_finished();
        release_tx.send(()).unwrap();
        first.join().unwrap();

        assert!(!returned_early, "the second caller returned while the sweep was still running");
        assert!(second.join().unwrap(), "and found the file put back when it returned");
    }

    // ── #256 (b): the library is swept at start-up ────────────────────────────────────

    /// Every folder under the root is swept, however deep: a crashed offload's file goes back
    /// under its name without any storage operation touching that folder. Hidden folders are
    /// not entered.
    #[test]
    fn a_tree_sweep_puts_back_every_crashed_file_under_the_root() {
        if !cfg!(target_os = "linux") {
            println!("SKIPPED: a_tree_sweep_puts_back_every_crashed_file_under_the_root — needs /proc");
            return;
        }
        let dir = TestTmpDir::new("working-files-tree");
        let root = dir.join("library");
        let deep = root.join("2026/08/01");
        let hidden = root.join(".cache");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(&hidden).unwrap();
        let dead = dead_pid();
        let aside = |folder: &Path, name: &str| folder.join(format!(".{name}.{ASIDE_TAG}-{dead}-0"));
        std::fs::write(aside(&root, "TOP.ARW"), b"top").unwrap();
        std::fs::write(aside(&deep, "DSC1.ARW"), b"deep").unwrap();
        std::fs::write(aside(&hidden, "X.ARW"), b"hidden").unwrap();

        assert_eq!(sweep_tree(&root), 4, "the root, 2026, 08, 01 — not .cache");

        assert_eq!(std::fs::read(root.join("TOP.ARW")).unwrap(), b"top");
        assert_eq!(std::fs::read(deep.join("DSC1.ARW")).unwrap(), b"deep");
        assert!(aside(&hidden, "X.ARW").exists(), "a hidden folder is not entered");
    }

    #[test]
    fn only_the_exact_pattern_is_a_working_file() {
        let tags = [ASIDE_TAG];
        assert_eq!(
            parse(".DSC1.ARW.chairphoto-offload-12-3", &tags),
            Some(Working { tag: ASIDE_TAG, original: "DSC1.ARW", pid: 12 })
        );
        assert_eq!(parse(".a.b-c.xmp.chairphoto-offload-1-0", &tags).unwrap().original, "a.b-c.xmp");
        for not in [
            "DSC1.ARW.chairphoto-offload-12-3",
            "..chairphoto-offload-12-3",
            ".DSC1.ARW.chairphoto-offload-12",
            ".DSC1.ARW.chairphoto-offload-x12-3",
            ".DSC1.ARW.chairphoto-offload-12-3x",
            ".DSC1.ARW.chairphoto-offload--3",
            ".DSC1.ARW.chairphoto-tmp-12-3",
            ".DSC1.ARW",
        ] {
            assert_eq!(parse(not, &tags), None, "{not}");
        }
    }

    /// A crashed offload's file goes back under its name; one whose name is taken is kept
    /// (non-empty) or removed (empty); one of a live process, or a symlink, is not touched.
    #[test]
    fn a_crashed_offloads_file_is_put_back_never_deleted() {
        if !cfg!(target_os = "linux") {
            println!("SKIPPED: a_crashed_offloads_file_is_put_back_never_deleted — needs /proc");
            return;
        }
        let dir = TestTmpDir::new("working-files-aside");
        let folder = dir.join("2026/08");
        std::fs::create_dir_all(&folder).unwrap();
        let dead = dead_pid();
        let aside = |name: &str, pid: u32| folder.join(format!(".{name}.{ASIDE_TAG}-{pid}-0"));
        std::fs::write(aside("A.ARW", dead), b"only copy of A").unwrap();
        std::fs::write(folder.join("B.ARW"), b"newer B").unwrap();
        std::fs::write(aside("B.ARW", dead), b"older B").unwrap();
        std::fs::write(folder.join("C.ARW"), b"C").unwrap();
        std::fs::write(aside("C.ARW", dead), b"").unwrap();
        std::fs::write(aside("D.ARW", std::process::id()), b"in progress").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(folder.join("C.ARW"), aside("E.ARW", dead)).unwrap();
        // Review S8: an empty claim whose name is free is not made into a 0-byte original.
        std::fs::write(aside("F.ARW", dead), b"").unwrap();

        sweep_once(&folder);

        assert!(!folder.join("F.ARW").exists(), "no 0-byte original out of an empty claim");
        assert!(aside("F.ARW", dead).exists(), "it is left as it was");

        assert_eq!(std::fs::read(folder.join("A.ARW")).unwrap(), b"only copy of A");
        assert!(!aside("A.ARW", dead).exists());
        assert_eq!(std::fs::read(folder.join("B.ARW")).unwrap(), b"newer B", "never replaced");
        assert_eq!(std::fs::read(aside("B.ARW", dead)).unwrap(), b"older B", "and the other kept");
        assert!(!aside("C.ARW", dead).exists(), "an empty claim on a taken name is removed");
        assert!(aside("D.ARW", std::process::id()).exists(), "a running process's file is its own");
        assert!(!folder.join("D.ARW").exists());
        #[cfg(unix)]
        {
            assert!(std::fs::symlink_metadata(aside("E.ARW", dead)).is_ok(), "a symlink is left");
            assert!(!folder.join("E.ARW").exists());
        }
    }

    /// A copy's temporary file is removed once its process is gone and it has gone an hour
    /// unwritten — and nothing else: not a fresh one (another machine's copy in progress on a
    /// shared NAS folder), not a running process's, not a symlink or a folder of that name,
    /// not a name off the pattern.
    #[test]
    fn a_stale_copy_temp_of_a_dead_process_is_removed() {
        if !cfg!(target_os = "linux") {
            println!("SKIPPED: a_stale_copy_temp_of_a_dead_process_is_removed — needs /proc");
            return;
        }
        let dir = TestTmpDir::new("working-files-parts");
        let dead = dead_pid();
        let part = |name: &str, pid: u32| dir.join(format!(".{name}.{PART_TAG}-{pid}-7"));
        let old = std::time::SystemTime::now() - STALE_PART_AGE - std::time::Duration::from_secs(60);
        let write_old = |path: &Path| {
            std::fs::write(path, b"partial").unwrap();
            std::fs::File::options().write(true).open(path).unwrap().set_modified(old).unwrap();
        };
        write_old(&part("A.ARW", dead));
        std::fs::write(part("B.ARW", dead), b"being written elsewhere").unwrap();
        write_old(&part("C.ARW", std::process::id()));
        write_old(&dir.join(format!(".D.ARW.{PART_TAG}-{dead}")));
        std::fs::create_dir(part("E.ARW", dead)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(part("C.ARW", std::process::id()), part("F.ARW", dead)).unwrap();

        sweep_once(&dir);

        assert!(!part("A.ARW", dead).exists(), "stale and its process gone: removed");
        assert!(part("B.ARW", dead).exists(), "recently written: kept");
        assert!(part("C.ARW", std::process::id()).exists(), "a running process's: kept");
        assert!(dir.join(format!(".D.ARW.{PART_TAG}-{dead}")).exists(), "off the pattern: kept");
        assert!(part("E.ARW", dead).is_dir(), "a folder: kept");
        #[cfg(unix)]
        assert!(std::fs::symlink_metadata(part("F.ARW", dead)).is_ok(), "a symlink: kept");
    }

    /// Where the filesystem has neither a no-replace rename nor hard links (forced here), a
    /// moved file still goes back while its name is free, and still never over a file there.
    #[test]
    fn putting_back_without_one_step_placement_takes_a_free_name_only() {
        let dir = TestTmpDir::new("working-files-no-noreplace");
        let file = dir.join("DSC1.ARW");
        std::fs::write(&file, b"raw").unwrap();
        let aside = move_aside(&file).unwrap().unwrap();
        let _neither = crate::scanner::same_photo::copy_fallback::force(|_| {});
        std::fs::write(&file, b"new").unwrap();
        assert!(!put_back(&aside, &file).unwrap(), "the name is taken");
        assert_eq!(std::fs::read(&file).unwrap(), b"new");
        std::fs::remove_file(&file).unwrap();
        assert!(put_back(&aside, &file).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), b"raw");
        assert!(!aside.exists());
    }

    // ── #256 NIT-4: a name too long for a hidden name ────────────────────────────────

    /// A 250-byte name cannot take `.<name>.chairphoto-offload-<pid>-<n>` within 255 bytes.
    /// It is moved under its own name into a hidden folder instead; it goes back, or is
    /// deleted, and the folder goes with it.
    #[test]
    fn a_long_name_moves_aside_into_a_hidden_folder() {
        let dir = TestTmpDir::new("working-files-long");
        let long = format!("{}.ARW", "L".repeat(246));
        assert_eq!(long.len(), 250);
        let file = dir.join(&long);
        std::fs::write(&file, b"raw").unwrap();

        let aside = move_aside(&file).unwrap().expect("moved");
        assert!(!file.exists());
        assert_eq!(aside.file_name().unwrap(), long.as_str(), "under its own name");
        assert!(aside_folder(&aside).is_some(), "in a hidden folder: {}", aside.display());
        assert!(aside_label(&aside).starts_with(&format!(".{ASIDE_TAG}-")), "{}", aside_label(&aside));
        assert!(put_back(&aside, &file).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), b"raw");

        let aside = move_aside(&file).unwrap().unwrap();
        delete_aside(&aside).unwrap();
        assert!(!file.exists());
        let left: Vec<_> = std::fs::read_dir(&*dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(left.is_empty(), "no folder left: {left:?}");
        assert_eq!(move_aside(&file).unwrap(), None, "nothing to move");
        assert_eq!(std::fs::read_dir(&*dir).unwrap().count(), 0, "and no folder made for it");
    }

    /// A crash with a long name moved aside: the next sweep puts it back under its own name
    /// and removes the folder; a folder of a running process, or holding a file whose name is
    /// taken, is left.
    #[test]
    fn a_crashed_long_names_folder_is_put_back() {
        if !cfg!(target_os = "linux") {
            println!("SKIPPED: a_crashed_long_names_folder_is_put_back — needs /proc");
            return;
        }
        let dir = TestTmpDir::new("working-files-long-crash");
        let folder = dir.to_path_buf();
        let long = |c: &str| format!("{}.ARW", c.repeat(246));
        let dead = dead_pid();
        let crashed = folder.join(format!(".{ASIDE_TAG}-{dead}-0"));
        std::fs::create_dir(&crashed).unwrap();
        std::fs::write(crashed.join(long("A")), b"only copy of A").unwrap();
        let taken = folder.join(format!(".{ASIDE_TAG}-{dead}-1"));
        std::fs::create_dir(&taken).unwrap();
        std::fs::write(taken.join(long("B")), b"older B").unwrap();
        std::fs::write(folder.join(long("B")), b"newer B").unwrap();
        let live = folder.join(format!(".{ASIDE_TAG}-{}-2", std::process::id()));
        std::fs::create_dir(&live).unwrap();
        std::fs::write(live.join(long("C")), b"in progress").unwrap();

        sweep_once(&folder);

        assert_eq!(std::fs::read(folder.join(long("A"))).unwrap(), b"only copy of A");
        assert!(!crashed.exists(), "the emptied folder is removed");
        assert_eq!(std::fs::read(folder.join(long("B"))).unwrap(), b"newer B", "never replaced");
        assert_eq!(std::fs::read(taken.join(long("B"))).unwrap(), b"older B", "and the other kept");
        assert!(live.join(long("C")).exists() && !folder.join(long("C")).exists(), "a running process's");
    }

    #[test]
    fn moving_aside_and_back_never_replaces() {
        let dir = TestTmpDir::new("working-files-move");
        let file = dir.join("DSC1.ARW");
        std::fs::write(&file, b"raw").unwrap();
        let aside = move_aside(&file).unwrap().unwrap();
        assert!(!file.exists());
        assert_eq!(std::fs::read(&aside).unwrap(), b"raw");
        std::fs::write(&file, b"new").unwrap();
        assert!(!put_back(&aside, &file).unwrap(), "the name is taken");
        assert_eq!(std::fs::read(&file).unwrap(), b"new");
        std::fs::remove_file(&file).unwrap();
        assert!(put_back(&aside, &file).unwrap());
        assert_eq!(std::fs::read(&file).unwrap(), b"raw");
        assert!(!aside.exists());
        assert_eq!(move_aside(&dir.join("absent")).unwrap(), None);
        let left: Vec<_> = std::fs::read_dir(&*dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "no placeholder left: {left:?}");
    }
}
