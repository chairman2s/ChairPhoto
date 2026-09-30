//! Internal sidecar-document transaction (issue #14).
//!
//! Every public writer in `xmp::mod` used to hand-roll the same frame: load the sidecar or
//! create a fresh one, ensure `rdf:RDF`/`rdf:Description` exist, decide whether to back up a
//! foreign sidecar before touching it, declare namespaces, mutate the properties it owns, and
//! serialize back to disk. The load/backup/namespace/write plumbing is identical across
//! writers; only *which* nodes a writer owns and what replaces them differs. This module is
//! that shared frame — writers become thin adapters over it (see `mod.rs`).
//!
//! `SidecarDocument` is not `pub`: it is an implementation detail of this module, reached only
//! from `xmp::mod`'s writers.
//!
//! ## Backup-once policy (AGENTS.md, "XMP safety")
//!
//! Before ChairPhoto's first in-library write to an existing sidecar, back it up if it lacks
//! `chairphoto:LastWrite` — i.e. a sidecar chairphoto has never written to before is presumed
//! foreign (hand-authored, or written by darktable/Lightroom/digiKam) and is preserved verbatim
//! at `<sidecar>.chairphoto-backup` before the first mutation. This is a *document-level* fact
//! (has chairphoto ever committed to this file?), not a per-writer one, so it is decided once,
//! here, at [`SidecarDocument::open`] — before any writer-specific mutation happens.
//!
//! Export-only destination copies are not subject to this rule (AGENTS.md): a destination
//! sidecar produced by export is a throwaway copy, not the in-library original, so
//! [`SidecarDocument::open_no_backup`] skips the backup entirely. Only [`super::write_keywords`]
//! (the export keyword writer) uses it today.
//!
//! One writer needs the *stronger* rule: resolving an identity conflict by Overwrite (issue
//! #33) deliberately destroys the `xmp:Identifier` another tool put in the file, which the
//! `chairphoto:LastWrite` test alone would not protect — a sidecar chairphoto has already
//! written can still carry a foreign identifier (a file duplicated after import is the
//! ordinary way that happens). [`SidecarDocument::open_forcing_backup`] therefore backs up
//! regardless of `chairphoto:LastWrite`. It still never *replaces* an existing backup: the
//! backup slot holds the earliest state we ever saw, which is strictly more valuable than
//! the current one.

use std::path::{Path, PathBuf};
use xmltree::{Element, Namespace, XMLNode};

use super::{child_mut, declare_namespaces, new_root, now, plain, sidecar_backup_path,
    sidecar_path, NS_CHAIRPHOTO, NS_RDF};

/// When [`SidecarDocument::open`] copies the existing sidecar to `<sidecar>.chairphoto-backup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackupPolicy {
    /// AGENTS.md "XMP safety": back up an existing sidecar that lacks `chairphoto:LastWrite`
    /// — i.e. one chairphoto has never written — before the first mutation. Every in-library
    /// writer uses this.
    BeforeFirstWrite,
    /// Back up an existing sidecar even if chairphoto has written it before, because this
    /// write destroys data no other writer touches (see the module docs). Never overwrites a
    /// backup that already exists.
    Always,
    /// Never back up — export destination copies only (AGENTS.md).
    Never,
}

/// A parsed (or freshly created) XMP sidecar document, mid-transaction.
///
/// Lifecycle: [`Self::open`] (or [`Self::open_no_backup`]) → zero or more mutations against
/// [`Self::description_mut`] / [`Self::replace_owned`] / [`Self::declare_extra_namespaces`] →
/// [`Self::commit`]. Dropping without calling `commit` writes nothing (matches every existing
/// writer: an error before the final `std::fs::write` leaves the sidecar untouched).
pub(super) struct SidecarDocument {
    path: PathBuf,
    root: Element,
    /// Where this open copied the pre-existing sidecar, if it did. Reported so a
    /// destructive writer can tell the user what it preserved and where.
    backup: Option<PathBuf>,
}

impl SidecarDocument {
    /// Load `photo_path`'s sidecar, or create a fresh one if absent. Ensures `rdf:RDF` and
    /// `rdf:Description` exist (creating them for a fresh document), ensures `rdf:about` is
    /// present (defaulting to `""`), applies the backup-once policy (see module docs), and
    /// declares the base chairphoto namespace set every writer needs.
    pub(super) fn open(photo_path: &Path) -> Result<Self, String> {
        Self::open_impl(photo_path, BackupPolicy::BeforeFirstWrite)
    }

    /// Same as [`Self::open`], but never backs up — for export destination writers, which are
    /// not subject to the in-library backup-once rule (AGENTS.md: "Export-only destination
    /// copies are not subject to this rule").
    pub(super) fn open_no_backup(photo_path: &Path) -> Result<Self, String> {
        Self::open_impl(photo_path, BackupPolicy::Never)
    }

    /// Same as [`Self::open`], but backs up an existing sidecar even when chairphoto has
    /// written it before — for the one writer that destroys a field it does not own
    /// ([`super::overwrite_identifier`], issue #33's Overwrite). See the module docs.
    pub(super) fn open_forcing_backup(photo_path: &Path) -> Result<Self, String> {
        Self::open_impl(photo_path, BackupPolicy::Always)
    }

    fn open_impl(photo_path: &Path, backup_policy: BackupPolicy) -> Result<Self, String> {
        let path = sidecar_path(photo_path);
        let existed = path.exists();

        let mut root = if existed {
            let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
            Element::parse(file).map_err(|e| format!("cannot parse {}: {e}", path.display()))?
        } else {
            new_root()
        };

        let rdf = child_mut(&mut root, "rdf", NS_RDF, "RDF");
        let desc = child_mut(rdf, "rdf", NS_RDF, "Description");
        desc.attributes
            .entry("rdf:about".to_string())
            .or_insert_with(String::new);

        // Back up the existing sidecar before any writer-specific mutation runs — a
        // document-level decision (see module docs), not a per-writer one.
        let wants_backup = existed
            && match backup_policy {
                BackupPolicy::BeforeFirstWrite => !has_chairphoto_last_write(desc),
                BackupPolicy::Always => true,
                BackupPolicy::Never => false,
            };
        let backup_path = sidecar_backup_path(&path);
        // An existing backup is never replaced: it is the earliest state we ever saw, and
        // `BackupPolicy::Always` must not trade that away for a newer, chairphoto-written
        // one. `BeforeFirstWrite` cannot reach an existing backup twice anyway (the first
        // write stamps `chairphoto:LastWrite`), so this only ever binds for `Always`.
        let backup = if wants_backup && !backup_path.exists() {
            std::fs::copy(&path, &backup_path).ok().map(|_| backup_path)
        } else {
            None
        };

        declare_namespaces(desc);

        Ok(Self { path, root, backup })
    }

    /// Where this open copied the pre-existing sidecar, if it did. `None` when nothing was
    /// backed up — a fresh sidecar, a policy that skipped it, or a backup that already
    /// existed and was therefore left alone.
    pub(super) fn backup(&self) -> Option<&Path> {
        self.backup.as_deref()
    }

    /// The `rdf:Description` element every writer mutates.
    pub(super) fn description_mut(&mut self) -> &mut Element {
        let rdf = child_mut(&mut self.root, "rdf", NS_RDF, "RDF");
        child_mut(rdf, "rdf", NS_RDF, "Description")
    }

    /// Declare additional namespace prefixes on the Description, beyond the base set `open`
    /// already declared — used by writers whose properties live outside that base set (the
    /// face-region writer's `mwg-rs`/`stArea`/`stDim`).
    pub(super) fn declare_extra_namespaces(&mut self, extra: &[(&str, &str)]) {
        let desc = self.description_mut();
        let mut ns = desc.namespaces.take().unwrap_or_else(Namespace::empty);
        for (prefix, uri) in extra {
            ns.put(*prefix, *uri);
        }
        desc.namespaces = Some(ns);
    }

    /// Remove every existing Description child matching one of `owned` (namespace, local-name)
    /// pairs and append `replacements` in their place. This is the "declare what I own, add
    /// replacements" shape shared by the plain-property writers (IPTC, keywords, identifier,
    /// import batch, GPS): every other element — foreign namespaces, develop history, anything
    /// this writer doesn't list in `owned` — is left exactly as parsed.
    ///
    /// Not used by the face-region writer, which has its own Name+Area matching rule for
    /// deciding what to keep (see `write_face_regions` in `mod.rs`).
    pub(super) fn replace_owned(&mut self, owned: &[(&str, &str)], replacements: Vec<XMLNode>) {
        let desc = self.description_mut();
        desc.children.retain(|n| !matches_owned(n, owned));
        desc.children.extend(replacements);
    }

    /// Commit the transaction: re-stamp `chairphoto:LastWrite` (removing any prior instance —
    /// this is the single path every writer's completion timestamp goes through), serialize,
    /// and write the sidecar to disk, creating parent directories as needed.
    pub(super) fn commit(mut self) -> Result<(), String> {
        let desc = self.description_mut();
        desc.children
            .retain(|n| !matches_owned(n, &[(NS_CHAIRPHOTO, "LastWrite")]));
        desc.children
            .push(plain("chairphoto", NS_CHAIRPHOTO, "LastWrite", &now().to_string()));

        let mut buf = Vec::new();
        self.root.write(&mut buf).map_err(|e| e.to_string())?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&self.path, buf).map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn has_chairphoto_last_write(desc: &Element) -> bool {
    desc.children.iter().any(|n| {
        matches!(n, XMLNode::Element(e)
            if e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "LastWrite")
    })
}

fn matches_owned(node: &XMLNode, owned: &[(&str, &str)]) -> bool {
    matches!(node, XMLNode::Element(e)
        if owned.iter().any(|(ns, name)| e.namespace.as_deref() == Some(*ns) && e.name == *name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmp::sidecar_backup_path;

    const FOREIGN: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:darktable="http://darktable.sf.net/">
   <darktable:history_end>7</darktable:history_end>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

    fn photo(tag: &str, name: &str) -> (crate::test_support::TestTmpDir, PathBuf) {
        let dir = crate::test_support::TestTmpDir::new(tag);
        let photo = dir.join(name);
        std::fs::write(&photo, b"raw").unwrap();
        (dir, photo)
    }

    /// A fresh (non-existent) sidecar: `open` creates RDF/Description with `rdf:about=""` and
    /// does not attempt a backup (there is nothing to back up).
    #[test]
    fn open_creates_description_for_absent_sidecar() {
        let (_dir, p) = photo("doc-fresh", "A.ARW");
        let mut doc = SidecarDocument::open(&p).unwrap();
        let desc = doc.description_mut();
        assert_eq!(desc.name, "Description");
        assert_eq!(desc.attributes.get("rdf:about").map(String::as_str), Some(""));
        assert!(!sidecar_backup_path(&sidecar_path(&p)).exists());
    }

    /// A pre-existing sidecar that chairphoto has never written to (no `chairphoto:LastWrite`)
    /// is backed up byte-for-byte before `open` returns.
    #[test]
    fn open_backs_up_foreign_sidecar_once() {
        let (_dir, p) = photo("doc-backup", "B.ARW");
        std::fs::write(sidecar_path(&p), FOREIGN).unwrap();
        let _doc = SidecarDocument::open(&p).unwrap();

        let backup = sidecar_backup_path(&sidecar_path(&p));
        assert!(backup.exists(), "foreign sidecar must be backed up on first open");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), FOREIGN);
    }

    /// A sidecar chairphoto has already written to (carries `chairphoto:LastWrite`) is not
    /// re-backed-up on a later open — the once-only half of "backup-once".
    #[test]
    fn open_does_not_reback_already_written_sidecar() {
        let (_dir, p) = photo("doc-no-reback", "C.ARW");
        SidecarDocument::open(&p).unwrap().commit().unwrap(); // first write stamps LastWrite
        let backup = sidecar_backup_path(&sidecar_path(&p));
        std::fs::remove_file(&backup).unwrap_or(());

        let _doc = SidecarDocument::open(&p).unwrap();
        assert!(!backup.exists(), "a sidecar chairphoto already wrote must not be re-backed-up");
    }

    /// `open_forcing_backup` backs up a sidecar chairphoto has already written — the case
    /// `BeforeFirstWrite` deliberately skips — and reports where. This is what makes
    /// Overwrite (issue #33) safe when the conflicting identifier sits in a sidecar we wrote.
    #[test]
    fn open_forcing_backup_backs_up_even_after_chairphoto_wrote() {
        let (_dir, p) = photo("doc-force-backup", "H.ARW");
        SidecarDocument::open(&p).unwrap().commit().unwrap(); // stamps LastWrite
        let backup = sidecar_backup_path(&sidecar_path(&p));
        assert!(!backup.exists());
        let written = std::fs::read_to_string(sidecar_path(&p)).unwrap();

        let doc = SidecarDocument::open_forcing_backup(&p).unwrap();
        assert_eq!(doc.backup(), Some(backup.as_path()), "must report the backup it took");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), written);
    }

    /// `open_forcing_backup` never replaces an existing backup — the earliest state we saw
    /// outranks the current one — and says so by reporting `None`.
    #[test]
    fn open_forcing_backup_keeps_an_existing_backup() {
        let (_dir, p) = photo("doc-force-keep", "I.ARW");
        std::fs::write(sidecar_path(&p), FOREIGN).unwrap();
        SidecarDocument::open(&p).unwrap().commit().unwrap(); // backs FOREIGN up
        let backup = sidecar_backup_path(&sidecar_path(&p));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), FOREIGN);

        let doc = SidecarDocument::open_forcing_backup(&p).unwrap();
        assert_eq!(doc.backup(), None, "no NEW backup was taken");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), FOREIGN,
            "the pre-chairphoto snapshot must not be traded for a newer one");
    }

    /// Nothing to preserve, nothing to report: forcing a backup on a photo with no sidecar
    /// yet must not invent an empty one.
    #[test]
    fn open_forcing_backup_does_nothing_without_an_existing_sidecar() {
        let (_dir, p) = photo("doc-force-fresh", "J.ARW");
        let doc = SidecarDocument::open_forcing_backup(&p).unwrap();
        assert_eq!(doc.backup(), None);
        assert!(!sidecar_backup_path(&sidecar_path(&p)).exists());
    }

    /// `open_no_backup` never backs up, even when the sidecar is foreign — the export
    /// destination-copy exemption (AGENTS.md).
    #[test]
    fn open_no_backup_never_backs_up() {
        let (_dir, p) = photo("doc-export", "D.ARW");
        std::fs::write(sidecar_path(&p), FOREIGN).unwrap();
        let _doc = SidecarDocument::open_no_backup(&p).unwrap();
        assert!(!sidecar_backup_path(&sidecar_path(&p)).exists());
    }

    /// `replace_owned` removes only the listed (namespace, name) pairs and leaves every other
    /// element — including foreign namespaces — untouched.
    #[test]
    fn replace_owned_touches_only_listed_fields() {
        let (_dir, p) = photo("doc-replace", "E.ARW");
        std::fs::write(sidecar_path(&p), FOREIGN).unwrap();
        let mut doc = SidecarDocument::open(&p).unwrap();
        doc.replace_owned(
            &[(NS_CHAIRPHOTO, "Foo")],
            vec![plain("chairphoto", NS_CHAIRPHOTO, "Foo", "bar")],
        );
        let desc = doc.description_mut();
        assert!(desc.children.iter().any(|n| matches!(n, XMLNode::Element(e) if e.name == "history_end")),
            "foreign element must survive replace_owned");
        doc.commit().unwrap();

        let xmp = std::fs::read_to_string(sidecar_path(&p)).unwrap();
        assert!(xmp.contains("history_end"), "darktable data clobbered by commit");
        assert!(xmp.contains("<chairphoto:Foo>bar</chairphoto:Foo>"));
    }

    /// `commit` stamps `chairphoto:LastWrite` exactly once even across repeated commits — no
    /// duplication, and the field is always present after a commit.
    #[test]
    fn commit_stamps_last_write_exactly_once_across_rewrites() {
        let (_dir, p) = photo("doc-stamp", "F.ARW");
        SidecarDocument::open(&p).unwrap().commit().unwrap();
        SidecarDocument::open(&p).unwrap().commit().unwrap();
        SidecarDocument::open(&p).unwrap().commit().unwrap();

        let xmp = std::fs::read_to_string(sidecar_path(&p)).unwrap();
        assert_eq!(xmp.matches("chairphoto:LastWrite").count(), 2, "one open + one close tag only");
    }

    /// `commit` creates the sidecar's parent directory if it doesn't exist yet.
    #[test]
    fn commit_creates_parent_directories() {
        let dir = crate::test_support::TestTmpDir::new("doc-mkdir");
        let photo = dir.join("nested/deep/G.ARW");
        std::fs::create_dir_all(photo.parent().unwrap()).unwrap();
        std::fs::write(&photo, b"raw").unwrap();
        // Delete the dir again to prove `commit` recreates it.
        std::fs::remove_dir_all(photo.parent().unwrap()).unwrap();

        let doc = SidecarDocument::open(&photo).unwrap();
        doc.commit().unwrap();
        assert!(sidecar_path(&photo).exists());
    }
}
