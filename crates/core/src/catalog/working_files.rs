//! The hidden files the storage lifecycle keeps beside photos while it works, and the
//! recovery of ones a crash left behind.
//!
//! - `.<name>.chairphoto-offload-<pid>-<n>` — a local file an offload has moved aside to
//!   re-hash it and then delete it (#256). While it has that name, nothing else can write
//!   into it through the photo's own name: a writer either landed before the move (and the
//!   re-hash sees it) or makes a new file at the photo's name (which the offload sees, and
//!   keeps).
//!
//! A crash can leave such a file behind. Its bytes may be the only copy of a local edit that
//! never reached home, so it is never deleted by a sweep: [`sweep_once`] puts it back under
//! its own name, without replacing anything there.
//!
//! Everything here is file IO: call it off the catalog lock, on a blocking worker.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The tag in the name of a file an offload moved aside.
pub(crate) const ASIDE_TAG: &str = "chairphoto-offload";

/// Move `file` to a new hidden name beside it, unique to this call; `None` when there was no
/// `file` to move. The new name is claimed first by an exclusive create, so the rename only
/// ever replaces that empty file of ours — on every filesystem, with no fallback needed.
pub(crate) fn move_aside(file: &Path) -> std::io::Result<Option<PathBuf>> {
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

/// Give a moved-aside file its name `original` back, never replacing a file there and never by
/// a copy (`same_photo::place_no_replace_without_copy`). `Ok(false)` when it cannot: the name
/// is taken (a new file was written there meanwhile) or the filesystem has neither a
/// no-replace rename nor hard links. The aside file is then still where it was.
pub(crate) fn put_back(aside: &Path, original: &Path) -> std::io::Result<bool> {
    match crate::scanner::same_photo::place_no_replace_without_copy(aside, original) {
        Ok(()) => Ok(true),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::Unsupported) => {
            Ok(false)
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

/// The folders swept in this run: each is listed at most once, so a folder of thousands of
/// photos is not listed on every storage operation.
static SWEPT: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

/// Sweep each folder holding one of `paths` ([`sweep_once`]).
pub(crate) fn sweep_beside<'a>(paths: impl IntoIterator<Item = &'a Path>) {
    for path in paths {
        if let Some(dir) = path.parent() {
            sweep_once(dir);
        }
    }
}

/// Recover what a crashed run left in `dir`, once per folder per run (see the module docs).
/// Only names of the exact pattern, only regular files (a symlink is never followed or
/// touched), and only those whose process is no longer running. Best effort: a folder that
/// cannot be listed is tried again by a later call.
pub(crate) fn sweep_once(dir: &Path) {
    {
        let mut swept = SWEPT.lock().unwrap_or_else(|e| e.into_inner());
        if !swept.get_or_insert_with(HashSet::new).insert(dir.to_path_buf()) {
            return;
        }
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        if let Some(swept) = SWEPT.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            swept.remove(dir);
        }
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(working) = name.to_str().and_then(|n| parse(n, &[ASIDE_TAG])) else { continue };
        // `symlink_metadata`: decided from the entry itself, never through a link.
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
        if !meta.file_type().is_file() || running(working.pid) {
            continue;
        }
        recover_aside(&entry.path(), &dir.join(working.original), meta.len());
    }
}

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

/// Put a crashed offload's file back under its name. When that name has been taken since, an
/// empty file is removed (a name claimed but never filled, or an empty file: no bytes to
/// lose) and any other is left where it is, for a person to look at.
fn recover_aside(aside: &Path, original: &Path, len: u64) {
    match put_back(aside, original) {
        Ok(true) => eprintln!("storage: put back {} left by an interrupted offload", original.display()),
        Ok(false) if len == 0 => {
            let _ = std::fs::remove_file(aside);
        }
        Ok(false) | Err(_) => eprintln!(
            "storage: {} was left by an interrupted offload and {} is taken; left as it is",
            aside.display(),
            original.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestTmpDir;

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

        sweep_once(&folder);

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
