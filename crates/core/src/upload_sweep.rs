//! Job-scoped upload temp directories: the sweep that reclaims the ones nothing is coming back
//! for.
//!
//! The publishing modules (Flickr, SmugMug, Instagram, LocalSend) render each upload into its
//! own `<temp>/chairphoto-upload-<service>-<random>/` directory (the Tauri shell's
//! `commands::publishing::JobTempDir`). A crash, a SIGKILL, or a kept Instagram render can
//! strand one; [`sweep_abandoned`] reclaims them on the next publish, and
//! [`sweep_abandoned_uploads_at_startup`] at every start (called by `app::boot`, so every front
//! end runs it). Compiled only with one of those features.

use std::path::Path;

/// The name every job directory starts with — what the sweep recognizes as ours.
pub const JOB_DIR_PREFIX: &str = "chairphoto-upload-";

/// How stale a job directory must be before the sweep treats it as abandoned. Well beyond
/// any upload, and beyond a supervised Instagram post left on screen for the evening.
pub const ABANDONED_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Reclaim abandoned upload renders at startup.
///
/// Without this, a directory kept by `JobTempDir::keep` — a supervised Instagram post, or any
/// publish that errored — is reclaimed only when someone happens to publish again, because the
/// sweep ran solely as a side effect of `JobTempDir::new`. A user who publishes once, hits an
/// error, and never publishes again keeps that render forever.
///
/// Same narrow rules as the on-publish sweep: prefix, uid, real directories, age. Opportunistic
/// and silent; startup must not fail because a temp directory would not delete.
pub fn sweep_abandoned_uploads_at_startup() {
    let root = std::env::temp_dir();
    // Read our uid the same way `JobTempDir::new` does — from a directory we just made —
    // rather than pulling in libc for one call. If the probe cannot be created there is
    // nothing to sweep into anyway, so give up silently.
    #[cfg(unix)]
    let owner = {
        use std::os::unix::fs::MetadataExt;
        let probe = root.join(format!("{JOB_DIR_PREFIX}uidprobe-{}", std::process::id()));
        let uid = std::fs::create_dir(&probe)
            .ok()
            .and_then(|_| std::fs::metadata(&probe).ok())
            .map(|m| m.uid());
        let _ = std::fs::remove_dir(&probe);
        uid
    };
    #[cfg(not(unix))]
    let owner = None;
    sweep_abandoned(&root, ABANDONED_AFTER, owner);
}

/// Remove job directories under `root` that nothing is coming back for: named like ours,
/// owned by `owner_uid` where the platform reports one, and untouched for `older_than`.
///
/// Deliberately narrow, because `root` is a shared `/tmp`:
/// - only entries whose name starts with `chairphoto-upload-`;
/// - `symlink_metadata`, so a symlink another user planted under one of those names is
///   read as a symlink and skipped, rather than as the directory it points at;
/// - only real directories, and (on Unix) only ones with our uid, so another user's
///   identically named directory is never even attempted;
/// - only ones older than `older_than`. That protects a live job only *within* that window:
///   a supervised Instagram composer left open past `ABANDONED_AFTER` has its render swept
///   by the next publish, and the Share click then fails in exactly the way `keep()` exists
///   to prevent. The window is a bound on the leak, not a guarantee for the composer.
///
/// Errors are silent by design: this is opportunistic housekeeping on someone else's turn,
/// and a directory we cannot remove is not a reason to fail their publish.
pub fn sweep_abandoned(root: &Path, older_than: std::time::Duration, owner_uid: Option<u32>) {
    let _ = owner_uid; // read only on Unix, where a uid exists
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(JOB_DIR_PREFIX) {
            continue;
        }
        let path = entry.path();
        // `symlink_metadata`, not `metadata`, so the test below sees a planted symlink for
        // what it is instead of the directory it points at. Measured on this toolchain,
        // `remove_dir_all` on a symlink removes the link and leaves the target intact — so
        // this is not the only thing standing between us and someone else's directory, but
        // deciding a symlink is a job directory of ours is not a judgement to delegate to
        // std's error handling.
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if owner_uid.is_some_and(|uid| meta.uid() != uid) {
                continue;
            }
        }
        // A modification time we can't read, or one in the future, means we can't establish
        // that the directory is stale — so we leave it alone.
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age >= older_than {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A private root to sweep, so a test never reaches the real `/tmp` where live jobs
    /// (and other users' directories) are.
    fn sweep_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "chairphoto-sweep-test-{tag}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn our_uid(path: &Path) -> Option<u32> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            return std::fs::metadata(path).ok().map(|m| m.uid());
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            None
        }
    }

    /// A crash between the render and the upload leaves a directory `Drop` never ran for.
    /// The next publish must reclaim it — with the render still inside.
    #[test]
    fn the_sweep_reclaims_a_directory_a_crash_left_behind() {
        let root = sweep_root("crashed");
        let abandoned = root.join("chairphoto-upload-flickr-deadbeef");
        std::fs::create_dir_all(&abandoned).unwrap();
        std::fs::write(abandoned.join("DSC01234.jpg"), b"a full-resolution render").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        sweep_abandoned(&root, std::time::Duration::from_millis(1), our_uid(&root));
        assert!(!abandoned.exists(), "the abandoned directory survived the sweep");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The age threshold is what protects a job that is still running — including a
    /// supervised Instagram post whose directory is deliberately left for the sweep.
    #[test]
    fn the_sweep_leaves_a_live_job_alone() {
        let root = sweep_root("live");
        let live = root.join("chairphoto-upload-instagram-c0ffee");
        std::fs::create_dir_all(&live).unwrap();

        sweep_abandoned(&root, std::time::Duration::from_secs(3600), our_uid(&root));
        assert!(live.exists(), "a live job's directory was swept");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Anything that isn't one of ours is none of the sweep's business, however stale.
    #[test]
    fn the_sweep_ignores_directories_that_are_not_ours() {
        let root = sweep_root("foreign");
        let foreign = root.join("chairphoto-thumbnail-cache");
        std::fs::create_dir_all(&foreign).unwrap();
        // Right prefix, but another user's uid: skipped without even attempting removal.
        let other_user = root.join("chairphoto-upload-flickr-someoneelse");
        std::fs::create_dir_all(&other_user).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        let not_us = our_uid(&root).map(|uid| uid.wrapping_add(1));
        sweep_abandoned(&root, std::time::Duration::from_millis(1), not_us);
        assert!(foreign.exists(), "a directory with a foreign name was swept");
        #[cfg(unix)]
        assert!(other_user.exists(), "a directory owned by another user was swept");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A symlink planted under one of our names is not a job directory, so the sweep skips
    /// it entirely — link and target both.
    ///
    /// The target surviving is guaranteed twice over: `std::fs::remove_dir_all` on a symlink
    /// removes the link and leaves what it points at (measured on this toolchain), so that
    /// assertion alone would hold even without the `symlink_metadata` check. The assertion
    /// that *the link itself* is untouched is the one that fails when the check goes.
    #[cfg(unix)]
    #[test]
    fn the_sweep_skips_a_planted_symlink() {
        let root = sweep_root("symlink");
        let victim = root.join("precious");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep.txt"), b"not the sweep's to delete").unwrap();
        let planted = root.join("chairphoto-upload-flickr-planted");
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        sweep_abandoned(&root, std::time::Duration::from_millis(1), our_uid(&root));
        assert!(
            std::fs::symlink_metadata(&planted).is_ok(),
            "the sweep treated a symlink as a job directory instead of skipping it"
        );
        assert!(
            victim.join("keep.txt").exists(),
            "the sweep reached through a symlink and deleted its target"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
