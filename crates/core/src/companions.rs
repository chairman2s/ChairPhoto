//! Companion files — the files that belong to a photo but are not the photo.
//!
//! `CONTEXT.md` used to define a **copy** as "one file of a photo at one location", and the
//! lifecycle code believed it: `backup_photo` copied the image and nothing else, so
//! darktable's history (`<raw>.xmp`) and RapidRAW's state (`<raw>.rrdata`) never reached
//! home while the app reported the photo backed up (issue #80). This module is the single
//! declared answer to "what else travels with this photo".
//!
//! ## Declared, never guessed
//!
//! An integration names its extension here. The catalog never sweeps arbitrary neighbouring
//! files: a stray `.txt` beside a RAW is the user's business, and silently copying it would
//! make backup's behaviour depend on what happens to share a folder.
//!
//! ## Carrying and edit-attribution are different questions
//!
//! They look like one field and are not. RapidRAW writes an `.rrdata` when it **opens** a
//! file, not when it edits one — measured across a real library, 536 of 547 `.rrdata` files
//! carried no `adjustments` block at all. So `.rrdata` must be carried (it holds masks and
//! slider state when there is any) but must never mark a RAW "edited", or the edited filter
//! fills with photos that were merely looked at. That distinction is why [`Companion`] has
//! both `carry` and [`EditSignal`], and why two existing tests pin `.rrdata` out of editor
//! detection (`scanner/sidecars.rs`, `scanner/mod.rs`).
//!
//! ## Two shapes on disk
//!
//! Sidecars appear either **appended** to the whole filename (`DSC1.ARW.xmp`) or on the
//! **basename** (`DSC1.xmp`, darktable's alternate mode). Both are carried, and the
//! destination is derived from the destination *image* rather than from the source name, so
//! a copy whose relative path differs between volumes still lands correctly.

use std::path::{Path, PathBuf};

/// What a companion's presence says about the photo having been externally edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditSignal {
    /// Presence alone proves an edit, and names the editor.
    ByExtension(&'static str),
    /// The file is written by several tools including ChairPhoto itself; only its
    /// contents can say (see `scanner::sidecars::attribute_xmp`).
    ByContent,
    /// Presence proves nothing — the file is written when the editor opens the photo.
    Never,
}

/// One declared companion kind.
#[derive(Debug, Clone, Copy)]
pub struct Companion {
    /// Extension, without the dot.
    pub ext: &'static str,
    /// Travels with the image on backup and restore.
    pub carry: bool,
    pub edit_signal: EditSignal,
}

/// Every companion kind ChairPhoto knows about. Adding a row here is what "declaring a
/// companion" means; nothing else in the codebase should hardcode one of these extensions.
pub const COMPANIONS: &[Companion] = &[
    // ChairPhoto's own identity/IPTC/GPS/face sidecar, and darktable's and Lightroom's
    // develop history — the same file, so attribution has to read the contents.
    Companion { ext: "xmp", carry: true, edit_signal: EditSignal::ByContent },
    Companion { ext: "pp3", carry: true, edit_signal: EditSignal::ByExtension("RawTherapee") },
    Companion { ext: "arp", carry: true, edit_signal: EditSignal::ByExtension("ART") },
    // Written on open, not on edit — carried, never an edit signal. See the module docs.
    Companion { ext: "rrdata", carry: true, edit_signal: EditSignal::Never },
];

/// How a companion's name is built from the image's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// `DSC1.ARW` → `DSC1.ARW.xmp`
    Appended,
    /// `DSC1.ARW` → `DSC1.xmp` (darktable's alternate mode)
    Basename,
}

/// A companion that exists on disk beside a specific image.
#[derive(Debug, Clone)]
pub struct Found {
    pub path: PathBuf,
    pub ext: &'static str,
    pub form: Form,
}

impl Found {
    /// The file name, which is what the catalog records. Unique within a directory, and
    /// enough to identify the companion again on a later pass.
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Where this companion belongs beside `dest_image`.
    ///
    /// Derived from the destination image rather than from this file's own name: a photo's
    /// `relative_path` may differ between volumes, and copying `DSC1.ARW.xmp` next to a
    /// destination image called something else would produce an orphan that no later pass
    /// could match up.
    pub fn destination(&self, dest_image: &Path) -> PathBuf {
        match self.form {
            Form::Appended => appended_path(dest_image, self.ext),
            Form::Basename => dest_image.with_extension(self.ext),
        }
    }
}

/// `<image>.<ext>` — the extension appended to the whole filename.
pub fn appended_path(image: &Path, ext: &str) -> PathBuf {
    with_suffix(image, &format!(".{ext}"))
}

/// Every declared companion that currently exists beside `image`.
///
/// Order is stable (registry order, appended before basename) so callers that report or
/// record the result produce the same sequence twice.
pub fn found_beside(image: &Path) -> Vec<Found> {
    let mut out = Vec::new();
    for c in COMPANIONS {
        let appended = appended_path(image, c.ext);
        if appended.is_file() {
            out.push(Found { path: appended, ext: c.ext, form: Form::Appended });
        }
        // `with_extension` on a file that has no extension would collide with the image
        // itself; guard so we never report the photo as its own companion.
        let basename = image.with_extension(c.ext);
        if basename != image && basename.is_file() && !out.iter().any(|f| f.path == basename) {
            out.push(Found { path: basename, ext: c.ext, form: Form::Basename });
        }
    }
    out
}

/// Those of [`found_beside`] that travel with the image.
pub fn carried_beside(image: &Path) -> Vec<Found> {
    found_beside(image)
        .into_iter()
        .filter(|f| {
            COMPANIONS.iter().any(|c| c.ext == f.ext && c.carry)
        })
        .collect()
}

/// The suffix `xmp::SidecarDocument` appends when it preserves a sidecar before its first
/// chairphoto write (AGENTS.md "XMP safety").
pub const SIDECAR_BACKUP_SUFFIX: &str = ".chairphoto-backup";

/// Where a sidecar's pre-chairphoto backup lives: `<sidecar>.chairphoto-backup`.
///
/// One definition, because two halves of the app care about these files — the writer that
/// creates them (`xmp::SidecarDocument::open`) and the offload that must recognise them
/// without deleting them (#82).
pub fn sidecar_backup(sidecar: &Path) -> PathBuf {
    with_suffix(sidecar, SIDECAR_BACKUP_SUFFIX)
}

/// Sidecar backups sitting beside `image`, in both sidecar shapes.
///
/// Deliberately **not** a companion: a backup records what *this copy's* sidecar looked
/// like before chairphoto first touched it, so it is per-copy by construction. Carrying one
/// home would routinely leave two different backups for one photo, which the divergence
/// rule reads as unreconciled edits and refuses to offload over; deleting it would destroy
/// the only record of the pre-chairphoto sidecar during a space-freeing operation. So
/// offload leaves them alone and reports them, and this is how it finds them (#82).
pub fn sidecar_backups_beside(image: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for c in COMPANIONS {
        // Both shapes, whether or not the sidecar itself still exists — a backup outlives
        // the sidecar it was taken from, which is exactly the case offload leaves behind.
        for candidate in [appended_path(image, c.ext), image.with_extension(c.ext)] {
            if candidate == *image {
                continue; // `with_extension` on a bare name would name the image itself
            }
            let backup = sidecar_backup(&candidate);
            if backup.is_file() && !out.contains(&backup) {
                out.push(backup);
            }
        }
    }
    out
}

/// `<path><suffix>` — a suffix appended to the whole file name, extension included.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// The registry entry for an extension, if it is declared.
pub fn lookup(ext: &str) -> Option<&'static Companion> {
    COMPANIONS.iter().find(|c| c.ext == ext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestTmpDir;

    fn touch(p: &Path) {
        std::fs::write(p, b"x").unwrap();
    }

    #[test]
    fn finds_both_the_appended_and_basename_sidecar_shapes() {
        let dir = TestTmpDir::new("companions-shapes");
        let img = dir.join("DSC1.ARW");
        touch(&img);
        touch(&dir.join("DSC1.ARW.xmp")); // appended
        touch(&dir.join("DSC1.xmp")); // basename — darktable's alternate mode

        let found = found_beside(&img);

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].form, Form::Appended);
        assert_eq!(found[1].form, Form::Basename);
    }

    #[test]
    fn an_undeclared_neighbour_is_not_a_companion() {
        // Backup must not depend on what happens to share a folder with the photo.
        let dir = TestTmpDir::new("companions-undeclared");
        let img = dir.join("DSC1.ARW");
        touch(&img);
        touch(&dir.join("DSC1.ARW.txt"));
        touch(&dir.join("notes.md"));

        assert!(found_beside(&img).is_empty());
    }

    #[test]
    fn a_sidecar_backup_is_found_but_is_not_a_companion() {
        // The two halves of #82's second finding: offload has to *recognise* these files
        // to report them, and must never carry them (a backup is per copy, so carrying
        // one produces two different backups for one photo).
        let dir = TestTmpDir::new("companions-sidecar-backup");
        let img = dir.join("DSC1.ARW");
        touch(&img);
        touch(&dir.join("DSC1.ARW.xmp"));
        touch(&dir.join("DSC1.ARW.xmp.chairphoto-backup"));

        assert_eq!(sidecar_backups_beside(&img).len(), 1);
        assert!(
            carried_beside(&img).iter().all(|f| f.ext == "xmp"),
            "the backup is not carried — only the sidecar itself is"
        );
    }

    #[test]
    fn a_sidecar_backup_outlives_the_sidecar_it_was_taken_from() {
        // Exactly what offload leaves behind: the sidecar went home and was freed, the
        // backup did not. Deriving the path from the image rather than from what is on
        // disk is what lets it still be counted.
        let dir = TestTmpDir::new("companions-orphan-backup");
        let img = dir.join("DSC1.ARW");
        touch(&img);
        touch(&dir.join("DSC1.xmp.chairphoto-backup")); // basename shape, no sidecar left

        assert_eq!(sidecar_backups_beside(&img).len(), 1);
        assert!(found_beside(&img).is_empty());
    }

    #[test]
    fn the_image_is_never_its_own_companion() {
        // `with_extension` on a bare name would otherwise produce the image's own path.
        let dir = TestTmpDir::new("companions-self");
        let img = dir.join("xmp"); // pathological: a file literally named "xmp"
        touch(&img);

        assert!(found_beside(&img).iter().all(|f| f.path != img));
    }

    #[test]
    fn the_destination_follows_the_destination_image_not_the_source_name() {
        // A photo's relative_path can differ between volumes; deriving from the source
        // name would leave an orphan beside a differently-named destination image.
        let dir = TestTmpDir::new("companions-dest");
        let src = dir.join("DSC1.ARW");
        touch(&src);
        touch(&dir.join("DSC1.ARW.rrdata"));

        let found = found_beside(&src);
        let dest = found[0].destination(Path::new("/nas/2026/RENAMED.ARW"));

        assert_eq!(dest, Path::new("/nas/2026/RENAMED.ARW.rrdata"));
    }

    #[test]
    fn a_basename_companion_keeps_its_shape_at_the_destination() {
        let dir = TestTmpDir::new("companions-dest-basename");
        let src = dir.join("DSC1.ARW");
        touch(&src);
        touch(&dir.join("DSC1.xmp"));

        let found = found_beside(&src);

        assert_eq!(found[0].form, Form::Basename);
        assert_eq!(found[0].destination(Path::new("/nas/R.ARW")), Path::new("/nas/R.xmp"));
    }

    #[test]
    fn rrdata_is_carried_but_never_signals_an_edit() {
        // Measured: RapidRAW writes it on open, so 536 of 547 in a real library carry no
        // adjustments. Carrying it is right; letting it mark a RAW "edited" is not.
        let c = lookup("rrdata").expect("rrdata is declared");
        assert!(c.carry);
        assert_eq!(c.edit_signal, EditSignal::Never);
    }

    #[test]
    fn xmp_is_attributed_by_content_because_chairphoto_writes_it_too() {
        assert_eq!(lookup("xmp").unwrap().edit_signal, EditSignal::ByContent);
        assert_eq!(
            lookup("pp3").unwrap().edit_signal,
            EditSignal::ByExtension("RawTherapee")
        );
        assert_eq!(lookup("arp").unwrap().edit_signal, EditSignal::ByExtension("ART"));
    }

    #[test]
    fn every_declared_companion_is_carried() {
        // Nothing is declared purely for edit-detection today. If that changes, this test
        // is the place to notice, because `carried_beside` silently filters.
        assert!(COMPANIONS.iter().all(|c| c.carry));
    }
}
