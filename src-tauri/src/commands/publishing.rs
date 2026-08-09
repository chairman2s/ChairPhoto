//! Helpers shared by the publishing modules (Flickr, SmugMug, LocalSend, Instagram):
//! app-settings access for OAuth credentials, and rendering a photo to an upload JPEG.
//!
//! Not commands themselves — `pub(super)` so the sibling publishing modules can use
//! them, but nothing here is re-exported to `lib.rs`.

// The catalog/settings helpers below belong to Flickr and SmugMug; the temp-directory and
// filename helpers are also used by the Instagram and LocalSend renders, so this module
// compiles for those features too — with a narrower set of imports.
#[cfg(any(feature = "flickr", feature = "smugmug"))]
use super::*;
// `Path` is used by the upload-name helpers and by the sweep, so it follows the wider set.
#[cfg(any(feature = "flickr", feature = "smugmug", feature = "instagram", feature = "localsend"))]
use std::path::Path;
use std::path::PathBuf;
#[cfg(any(feature = "flickr", feature = "smugmug"))]
use tauri::{AppHandle, Manager};

/// A temp directory owned by **one** publish/transfer job, deleted when the job's guard
/// drops (success, error, or early return alike).
///
/// Two things forced this over a deterministic per-service path. Concurrency: two publishes
/// of the same photo+version derive the same upload filename, so a shared directory means
/// one job's render overwrites the other's — and the loser uploads the winner's pixels.
/// Multi-user machines: a predictable name under a world-writable `/tmp` is a path another
/// user can pre-create, symlink, or read. The random leaf name, `create` (never
/// `create_dir_all`, so an existing path or a planted symlink is an error rather than a
/// target we adopt), and mode 0700 close both.
///
/// The *upload filename* deliberately stays outside this: it lives inside the job directory
/// and is still derived from the source photo, so the service keeps showing
/// "DSC01234 - Punchy crop.jpg".
#[cfg(any(feature = "flickr", feature = "smugmug", feature = "instagram", feature = "localsend"))]
pub(super) struct JobTempDir {
    path: PathBuf,
}

/// The name every job directory starts with — what the sweep recognizes as ours.
#[cfg(any(feature = "flickr", feature = "smugmug", feature = "instagram", feature = "localsend"))]
const JOB_DIR_PREFIX: &str = "chairphoto-upload-";

/// How stale a job directory must be before the sweep treats it as abandoned. Well beyond
/// any upload, and beyond a supervised Instagram post left on screen for the evening.
#[cfg(any(feature = "flickr", feature = "smugmug", feature = "instagram", feature = "localsend"))]
const ABANDONED_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

#[cfg(any(feature = "flickr", feature = "smugmug", feature = "instagram", feature = "localsend"))]
impl JobTempDir {
    /// Create `<temp>/chairphoto-upload-<service>-<random>/`, private to this user.
    pub(super) fn new(service: &str) -> Result<Self, String> {
        let root = std::env::temp_dir();
        let path = root.join(format!(
            "{JOB_DIR_PREFIX}{service}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .map_err(|e| format!("couldn't create the upload temp directory: {e}"))?;

        // `Drop` is the only cleanup, so a crash, a SIGKILL, or a power cut between the
        // render and the upload strands a directory with a full-resolution JPEG in it
        // forever. Reclaim the stale ones now that we know a publish is happening — and
        // now that we own a directory to read our own uid from, without pulling in libc.
        #[cfg(unix)]
        let owner = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&path).ok().map(|m| m.uid())
        };
        #[cfg(not(unix))]
        let owner = None;
        sweep_abandoned(&root, ABANDONED_AFTER, owner);

        Ok(Self { path })
    }

    /// A path for `name` inside this job's directory.
    pub(super) fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

#[cfg(any(feature = "flickr", feature = "smugmug", feature = "instagram", feature = "localsend"))]
impl Drop for JobTempDir {
    fn drop(&mut self) {
        // Best-effort: a failure here leaves a temp directory behind, which must not turn a
        // successful publish into an error. Takes the sidecars exiftool may have written
        // next to the render with it.
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "publishing: couldn't remove temp dir {}: {e}",
                    self.path.display()
                );
            }
        }
    }
}

/// Remove job directories under `root` that nothing is coming back for: named like ours,
/// owned by `owner_uid` where the platform reports one, and untouched for `older_than`.
///
/// Deliberately narrow, because `root` is a shared `/tmp`:
/// - only entries whose name starts with `chairphoto-upload-`;
/// - `symlink_metadata`, so a symlink another user planted under one of those names is
///   skipped rather than followed into whatever it points at;
/// - only real directories, and (on Unix) only ones with our uid, so another user's
///   identically named directory is never even attempted;
/// - only ones older than `older_than`, which is what keeps a *live* job — including a
///   supervised Instagram post still on screen — out of reach.
///
/// Errors are silent by design: this is opportunistic housekeeping on someone else's turn,
/// and a directory we cannot remove is not a reason to fail their publish.
#[cfg(any(feature = "flickr", feature = "smugmug", feature = "instagram", feature = "localsend"))]
fn sweep_abandoned(root: &Path, older_than: std::time::Duration, owner_uid: Option<u32>) {
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
        // Never follow a symlink here: `remove_dir_all` on one would be a way for another
        // user to aim our cleanup at a directory of theirs (or ours) that isn't a job dir.
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

/// The filename the *service* sees: the source stem plus the version suffix, sanitized.
/// Unchanged by the move to job-scoped directories — only the directory around it is new.
#[cfg(any(feature = "flickr", feature = "smugmug", feature = "localsend"))]
pub(super) fn upload_file_name(original: &Path, version_name: Option<&str>) -> String {
    let mut name = original
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "photo".into());
    if let Some(v) = version_name {
        name.push_str(" - ");
        name.push_str(v);
    }
    format!("{}.jpg", sanitize_filename(&name))
}

/// A rendered upload: the JPEG plus the job directory holding it. Keep the value alive
/// until the upload finishes — dropping it deletes the render.
#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) struct RenderedUpload {
    _dir: JobTempDir,
    path: PathBuf,
}

#[cfg(any(feature = "flickr", feature = "smugmug"))]
impl RenderedUpload {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) async fn render_export_jpeg(
    app: &AppHandle,
    photo_id: i64,
    version_id: Option<i64>,
    service: &str,
    max_long_edge: Option<u32>,
) -> Result<RenderedUpload, String> {
    let resolved = {
        let state = app.state::<AppState>();
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        crate::export::resolve_originals(catalog, &[photo_id], &[], version_id)
    };
    let item = resolved
        .items
        .into_iter()
        .next()
        .ok_or("Photo is unavailable (original offline?)")?;

    // Name the upload after the source (with the version suffix), so the service shows a
    // meaningful filename (e.g. "DSC01234.jpg", "DSC01234 - Punchy crop.jpg") instead of a
    // temp name. The job-scoped directory keeps that name collision-free.
    let dir = JobTempDir::new(service)?;
    let out = dir.join(&upload_file_name(&item.original, item.version_name.as_deref()));

    let o = out.clone();
    // Render the JPEG, applying the per-module long-edge limit when set (0/None = full
    // resolution, the default for portfolio/archival services like Flickr/SmugMug).
    tauri::async_runtime::spawn_blocking(move || {
        crate::export::write_item_jpeg_with_long_edge(&item, max_long_edge, &o)
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(RenderedUpload { _dir: dir, path: out })
}

/// Read the user-configured max long edge (px) for a publishing module. Setting key is
/// `<prefix>.max_long_edge`. Returns `None` when the setting is absent, empty, or "0"
/// (= full resolution, the default).
#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) fn read_max_long_edge(app: &AppHandle, prefix: &str) -> Option<u32> {
    let raw = read_setting(app, &format!("{prefix}.max_long_edge")).unwrap_or_default();
    let v: u32 = raw.trim().parse().ok()?;
    if v == 0 { None } else { Some(v) }
}

/// Keep a filename to safe ASCII (filename- and HTTP-header-friendly: SmugMug sends it as a
/// header), collapsing anything else to `_`.
#[cfg(any(feature = "flickr", feature = "smugmug", feature = "localsend"))]
pub(super) fn sanitize_filename(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        "photo".into()
    } else {
        s
    }
}

#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) fn read_setting(app: &AppHandle, key: &str) -> Result<String, String> {
    let state = app.state::<AppState>();
    with_catalog(&state, |c| Ok(c.get_setting(key)?.unwrap_or_default()))
}

#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) fn write_setting(app: &AppHandle, key: &str, value: &str) -> Result<(), String> {
    let state = app.state::<AppState>();
    with_catalog(&state, |c| c.set_setting(key, value))
}

/// Read the user-entered app key + secret for a module (settings keys `<prefix>.api_key` /
/// `<prefix>.api_secret`), erroring with a clear hint if they're not set.
#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) fn read_app_keys(app: &AppHandle, prefix: &str) -> Result<(String, String), String> {
    let key = read_setting(app, &format!("{prefix}.api_key"))?;
    let secret = read_setting(app, &format!("{prefix}.api_secret"))?;
    if key.is_empty() || secret.is_empty() {
        return Err(format!(
            "Enter your {prefix} API key and secret in the module settings first."
        ));
    }
    Ok((key, secret))
}

/// Read a connected module's access token + secret, erroring if not connected.
#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub(super) fn read_access(app: &AppHandle, prefix: &str) -> Result<(String, String), String> {
    let token = read_setting(app, &format!("{prefix}.access_token"))?;
    let secret = read_setting(app, &format!("{prefix}.access_secret"))?;
    if token.is_empty() || secret.is_empty() {
        return Err(format!("Connect {prefix} in the module settings first."));
    }
    Ok((token, secret))
}

#[cfg(test)]
mod job_temp_dir_tests {
    use super::*;

    /// Two jobs publishing the same photo+version derive the *same* upload filename. Under
    /// the old shared per-service directory that was one path, so whichever render finished
    /// second replaced the other's bytes and both jobs uploaded it. Run the two renders
    /// overlapped and require each to still read back its own content.
    ///
    /// Gated on the features that compile `upload_file_name`: the name under test has to be
    /// the one the publish commands really derive, so a change in naming reaches this test
    /// instead of passing against a literal that no longer matches.
    #[cfg(any(feature = "flickr", feature = "smugmug", feature = "localsend"))]
    #[test]
    fn concurrent_jobs_do_not_overwrite_the_same_upload_name() {
        const JOBS: usize = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(JOBS));
        // Every job renders the same photo and version, so the name is identical for all.
        let name = upload_file_name(Path::new("/photos/2026/DSC01234.ARW"), Some("Punchy crop"));

        let handles: Vec<_> = (0..JOBS)
            .map(|i| {
                let barrier = barrier.clone();
                let name = name.clone();
                std::thread::spawn(move || {
                    let dir = JobTempDir::new("flickr").unwrap();
                    let path = dir.join(&name);
                    barrier.wait();
                    // Stand in for the render: each job writes bytes only it should see.
                    std::fs::write(&path, format!("render-{i}")).unwrap();
                    barrier.wait();
                    let read = std::fs::read_to_string(&path).unwrap();
                    // Keep the guard alive until after the read, as the publish commands do
                    // until their upload finishes.
                    (dir, path, read)
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for (i, (_dir, path, read)) in results.iter().enumerate() {
            assert_eq!(
                read,
                &format!("render-{i}"),
                "job {i} read another job's render at {}",
                path.display()
            );
            assert_eq!(
                path.file_name().unwrap().to_str().unwrap(),
                name,
                "the service-facing filename must not change"
            );
        }
        let distinct: std::collections::HashSet<_> =
            results.iter().map(|(_, p, _)| p.clone()).collect();
        assert_eq!(distinct.len(), JOBS, "job temp paths must all differ");
    }

    /// The guard cleans up when the job ends normally.
    #[test]
    fn dropping_the_guard_removes_the_directory() {
        let render = {
            let dir = JobTempDir::new("smugmug").unwrap();
            let render = dir.join("DSC0001.jpg");
            std::fs::write(&render, b"jpeg").unwrap();
            // Something exiftool-shaped left beside the render must go too.
            std::fs::write(dir.join("DSC0001.jpg.xmp"), b"<xmp/>").unwrap();
            assert!(render.exists());
            render
        };
        let dir_path = render.parent().unwrap();
        assert!(!dir_path.exists(), "{} outlived its job", dir_path.display());
    }

    /// …and when the job fails after rendering (the upload errors out).
    #[test]
    fn a_failing_job_still_removes_its_directory() {
        let mut render = PathBuf::new();
        let result: Result<(), String> = (|| {
            let dir = JobTempDir::new("instagram").unwrap();
            render = dir.join("chairphoto-instagram.jpg");
            std::fs::write(&render, b"jpeg").unwrap();
            Err("upload rejected".into())
        })();
        assert!(result.is_err());
        let dir_path = render.parent().unwrap();
        assert!(
            !dir_path.exists(),
            "{} outlived a failed job",
            dir_path.display()
        );
    }

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

    /// A guessable path in a shared /tmp is readable by other users on the machine; the job
    /// directory must not be.
    #[cfg(unix)]
    #[test]
    fn the_directory_is_private_to_this_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = JobTempDir::new("flickr").unwrap();
        let dir_path = dir.join("probe");
        let dir_path = dir_path.parent().unwrap();
        let mode = std::fs::metadata(dir_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "job temp dir must be user-only");
    }

    /// The name the service receives is derived from the photo, not from the job.
    #[cfg(any(feature = "flickr", feature = "smugmug", feature = "localsend"))]
    #[test]
    fn upload_file_name_is_derived_from_the_photo() {
        let original = Path::new("/photos/2026/DSC01234.ARW");
        assert_eq!(upload_file_name(original, None), "DSC01234.jpg");
        assert_eq!(
            upload_file_name(original, Some("Punchy crop")),
            "DSC01234 - Punchy crop.jpg"
        );
        // Non-ASCII and separators collapse to `_` (SmugMug sends this as an HTTP header).
        assert_eq!(
            upload_file_name(Path::new("/photos/vår/tur:2.jpg"), None),
            "tur_2.jpg"
        );
    }
}

// --- Flickr ---

