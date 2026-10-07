//! What every publish path shares (docs/publications.md): the job-scoped temp directory a
//! render goes to and the filename the receiving service or device sees.
//!
//! Moved here from the Tauri shell's `commands/publishing.rs` so the GPUI app's publish
//! targets run the same code. The render itself is a publish job's (`app::uploads` for
//! Flickr, SmugMug and Instagram; `app::localsend` for a send), each collecting its own
//! export-parity tally for the catalog it read.

use crate::upload_sweep::{sweep_abandoned, ABANDONED_AFTER, JOB_DIR_PREFIX};
use std::path::{Path, PathBuf};

/// A temp directory owned by **one** publish/transfer job, deleted when the job's guard
/// drops (success, error, cancel, panic unwinding or early return alike).
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
pub struct JobTempDir {
    path: PathBuf,
    /// Set by [`JobTempDir::keep`] when something outside this process still needs the
    /// render; the sweep takes over from there.
    #[cfg_attr(not(feature = "instagram"), allow(dead_code))]
    kept: bool,
}

impl JobTempDir {
    /// Create `<temp>/chairphoto-upload-<service>-<random>/`, private to this user.
    pub fn new(service: &str) -> Result<Self, String> {
        let root = std::env::temp_dir();
        let path = root.join(format!("{JOB_DIR_PREFIX}{service}-{}", uuid::Uuid::new_v4().simple()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(|e| format!("couldn't create the upload temp directory: {e}"))?;

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

        Ok(Self { path, kept: false })
    }

    /// A path for `name` inside this job's directory.
    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// The directory itself.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Leave the directory on disk instead of removing it when the guard drops.
    ///
    /// For the one shape of job that is not finished when the command returns: a supervised
    /// Instagram post, where Chrome holds the render as a `File` it reads from disk only
    /// when the user finally clicks Share. Deleting it at return would break that click.
    /// The directory is not leaked — it stays at mode 0700 and `sweep_abandoned` reclaims
    /// it once it is stale.
    ///
    /// Gated on `instagram` because that supervised flow is the only caller: every other
    /// publish path has sent its bytes by the time the command returns, and a general
    /// "leave this behind" method they could reach for would be a way to reintroduce the
    /// leak this sweep exists to clean up.
    #[cfg(feature = "instagram")]
    pub fn keep(mut self) {
        self.kept = true;
    }
}

impl Drop for JobTempDir {
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        // Best-effort: a failure here leaves a temp directory behind, which must not turn a
        // successful publish into an error. Takes the sidecars exiftool may have written
        // next to the render with it.
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("publishing: couldn't remove temp dir {}: {e}", self.path.display());
            }
        }
    }
}

/// The filename the *service* sees: the source stem plus the version suffix, sanitized.
/// Unchanged by the move to job-scoped directories — only the directory around it is new.
pub fn upload_file_name(original: &Path, version_name: Option<&str>) -> String {
    let mut name = original.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "photo".into());
    if let Some(v) = version_name {
        name.push_str(" - ");
        name.push_str(v);
    }
    format!("{}.jpg", sanitize_filename(&name))
}

/// Keep a filename to safe ASCII (filename- and HTTP-header-friendly: SmugMug sends it as a
/// header), collapsing anything else to `_`.
pub fn sanitize_filename(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')') { c } else { '_' })
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        "photo".into()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two jobs publishing the same photo+version derive the *same* upload filename. Under
    /// the old shared per-service directory that was one path, so whichever render finished
    /// second replaced the other's bytes and both jobs uploaded it. Run the two renders
    /// overlapped and require each to still read back its own content.
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
            assert_eq!(read, &format!("render-{i}"), "job {i} read another job's render at {}", path.display());
            assert_eq!(path.file_name().unwrap().to_str().unwrap(), name, "the service-facing filename must not change");
        }
        let distinct: std::collections::HashSet<_> = results.iter().map(|(_, p, _)| p.clone()).collect();
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

    /// A kept guard leaves the render for whoever still needs it (the supervised Instagram
    /// composer), rather than deleting it out from under them.
    #[cfg(feature = "instagram")]
    #[test]
    fn a_kept_guard_leaves_the_render_in_place() {
        let dir = JobTempDir::new("instagram").unwrap();
        let render = dir.join("chairphoto-instagram.jpg");
        std::fs::write(&render, b"jpeg").unwrap();
        dir.keep();
        assert!(render.exists(), "the render Chrome still reads from was deleted at return");
        // Left to `sweep_abandoned` in production; this test does not wait 24 hours for it.
        std::fs::remove_dir_all(render.parent().unwrap()).unwrap();
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
        assert!(!dir_path.exists(), "{} outlived a failed job", dir_path.display());
    }

    /// A guessable path in a shared /tmp is readable by other users on the machine; the job
    /// directory must not be.
    #[cfg(unix)]
    #[test]
    fn the_directory_is_private_to_this_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = JobTempDir::new("flickr").unwrap();
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "job temp dir must be user-only");
    }

    /// The name the service receives is derived from the photo, not from the job.
    #[test]
    fn upload_file_name_is_derived_from_the_photo() {
        let original = Path::new("/photos/2026/DSC01234.ARW");
        assert_eq!(upload_file_name(original, None), "DSC01234.jpg");
        assert_eq!(upload_file_name(original, Some("Punchy crop")), "DSC01234 - Punchy crop.jpg");
        // Non-ASCII and separators collapse to `_` (SmugMug sends this as an HTTP header).
        assert_eq!(upload_file_name(Path::new("/photos/vår/tur:2.jpg"), None), "tur_2.jpg");
    }
}
