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
#[cfg(any(feature = "flickr", feature = "smugmug", feature = "localsend"))]
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

#[cfg(any(feature = "flickr", feature = "smugmug", feature = "instagram", feature = "localsend"))]
impl JobTempDir {
    /// Create `<temp>/chairphoto-upload-<service>-<random>/`, private to this user.
    pub(super) fn new(service: &str) -> Result<Self, String> {
        let path = std::env::temp_dir().join(format!(
            "chairphoto-upload-{service}-{}",
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

