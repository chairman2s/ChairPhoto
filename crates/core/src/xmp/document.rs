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
//!
//! ## One writer at a time, and never a half-written file (issue #149)
//!
//! [`SidecarDocument::open`] takes the sidecar's file lock (`lock::FILE_TURNS`) before it
//! reads the file, and the document holds it until it is committed or dropped: two writers
//! on one sidecar run their read-modify-writes one after the other, so neither's change is
//! lost. [`SidecarDocument::commit`] never writes the sidecar in place: it writes a hidden
//! temp file beside it, syncs it, and renames it over the sidecar, so a reader — another
//! tool, or a crash at any point — sees the old file or the new one, never a mix.
//!
//! ## One folder from read to commit (#155, F1 of the #149 review)
//!
//! [`SidecarDocument::open`] also opens the original's folder ([`Folder`]) and holds it until
//! the commit: the sidecar is read, and the temp file created, renamed and synced, relative to
//! that handle (`openat`/`renameat` on Unix). Checking the original by path and then creating
//! the temp by path left a gap in which an unmount could send the temp file to the disk under
//! the empty mount point; through the handle it can only land in the folder the original was
//! read from. While the handle is open a plain unmount of that volume fails as busy.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use xmltree::{Element, XMLNode};

use super::{attr_is, child_mut, declare_namespaces, is_xmp_root, new_root, now, ns_attr, parse_xml,
    plain, rdf_of_mut, sidecar_backup_path, sidecar_path, NS_CHAIRPHOTO, NS_RDF};

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
/// [`Self::description_mut`] / [`Self::rdf_mut`] / [`Self::replace_owned`] →
/// [`Self::commit`]. Dropping without calling `commit` writes nothing (an error before the
/// commit leaves the sidecar untouched) and releases the sidecar's file lock.
pub(super) struct SidecarDocument {
    /// The original photo the sidecar belongs to. It must still be there at commit.
    original: PathBuf,
    path: PathBuf,
    /// The original's folder, open from before the read to the commit (see the module docs).
    folder: Folder,
    root: Element,
    /// Where this open copied the pre-existing sidecar, if it did. Reported so a
    /// destructive writer can tell the user what it preserved and where.
    backup: Option<PathBuf>,
    /// This sidecar's file lock, held from before the read until the commit's rename (or
    /// the drop). A leaf in the lock order: see `lock`'s module docs.
    _turn: super::lock::Ticket,
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
        // Before the read: the read-modify-write is one turn (issue #149).
        let turn = super::lock::FILE_TURNS.lock(super::lock::key(&path));
        // An unmounted volume makes the sidecar look absent; never start a fresh one then.
        let folder = Folder::open(photo_path)?;
        folder.require_original(photo_path)?;
        let name = file_name(&path)?;
        let existed = folder.exists(name);

        let mut root = if existed {
            let file = folder.open_read(name).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            parse_xml(file).map_err(|e| format!("cannot parse {}: {e}", path.display()))?
        } else {
            new_root()
        };
        // Anything but an XMP packet is not ours to put an `rdf:RDF` into (#147 L4).
        if !is_xmp_root(&root) {
            return Err(format!(
                "{}: not written, its root element {{{}}}{} is not an XMP packet",
                path.display(),
                root.namespace.as_deref().unwrap_or(""),
                root.name
            ));
        }

        let rdf = rdf_of_mut(&mut root);
        // Decided before the writer adds anything: has ChairPhoto written this file before?
        let written_before = has_chairphoto_last_write(rdf);
        let desc = child_mut(rdf, "rdf", NS_RDF, "Description");
        if ns_attr(desc, NS_RDF, "about").is_none() {
            desc.attributes.insert("rdf:about".to_string(), String::new());
        }

        // Back up the existing sidecar before any writer-specific mutation runs — a
        // document-level decision (see module docs), not a per-writer one.
        let wants_backup = existed
            && match backup_policy {
                BackupPolicy::BeforeFirstWrite => !written_before,
                BackupPolicy::Always => true,
                BackupPolicy::Never => false,
            };
        let backup_path = sidecar_backup_path(&path);
        // An existing backup is never replaced: it is the earliest state we ever saw, and
        // `BackupPolicy::Always` must not trade that away for a newer, chairphoto-written
        // one. `BeforeFirstWrite` cannot reach an existing backup twice anyway (the first
        // write stamps `chairphoto:LastWrite`), so this only ever binds for `Always`.
        let backup_name = file_name(&backup_path)?;
        let backup = if wants_backup && !folder.exists(backup_name) {
            folder.copy(name, backup_name).ok().map(|_| backup_path)
        } else {
            None
        };

        declare_namespaces(desc);

        Ok(Self { original: photo_path.to_path_buf(), path, folder, root, backup, _turn: turn })
    }

    /// Where this open copied the pre-existing sidecar, if it did. `None` when nothing was
    /// backed up — a fresh sidecar, a policy that skipped it, or a backup that already
    /// existed and was therefore left alone.
    pub(super) fn backup(&self) -> Option<&Path> {
        self.backup.as_deref()
    }

    /// The `rdf:Description` element every writer mutates.
    pub(super) fn description_mut(&mut self) -> &mut Element {
        let rdf = rdf_of_mut(&mut self.root);
        child_mut(rdf, "rdf", NS_RDF, "Description")
    }

    /// The `rdf:RDF` element, for a writer that has to look past the first Description: the
    /// face-region writer edits `mwg-rs:Regions` in whichever top-level Description holds it.
    pub(super) fn rdf_mut(&mut self) -> &mut Element {
        rdf_of_mut(&mut self.root)
    }

    /// Remove every existing Description child matching one of `owned` (namespace, local-name)
    /// pairs — and the same property in compact attribute form (`xmp:Identifier="…"` on the
    /// Description), which another tool may have written — and append `replacements` in
    /// their place. This is the "declare what I own, add replacements" shape shared by the
    /// plain-property writers (IPTC, keywords, identifier, import batch, GPS): every other
    /// element and attribute — foreign namespaces, develop history, anything this writer
    /// doesn't list in `owned` — is left exactly as parsed.
    ///
    /// Not used by the face-region writer, which has its own Name+Area matching rule for
    /// deciding what to keep (see `write_face_regions` in `mod.rs`).
    ///
    /// The owned properties are removed from **every** top-level `rdf:Description` — exiftool
    /// writes one per namespace, so a stale value can sit in any of them (#142) — and the
    /// replacements go into the first.
    pub(super) fn replace_owned(&mut self, owned: &[(&str, &str)], replacements: Vec<XMLNode>) {
        self.remove_owned_everywhere(owned);
        self.description_mut().children.extend(replacements);
    }

    fn remove_owned_everywhere(&mut self, owned: &[(&str, &str)]) {
        for node in &mut self.rdf_mut().children {
            if let XMLNode::Element(desc) = node {
                if desc.namespace.as_deref() == Some(NS_RDF) && desc.name == "Description" {
                    remove_owned(desc, owned);
                }
            }
        }
    }

    /// Commit the transaction: re-stamp `chairphoto:LastWrite` (removing any prior instance —
    /// this is the single path every writer's completion timestamp goes through), serialize,
    /// and replace the sidecar on disk atomically ([`write_atomically`]). The file lock is
    /// released after the rename.
    ///
    /// The sidecar is written only next to an original that is still there: if the original
    /// or its folder has gone since [`Self::open`] — a volume unmounted while the writer
    /// waited its turn — the commit fails and creates nothing (no directory is ever made, so
    /// a write cannot land on the disk under an empty mount point). The original is checked
    /// at open and again just before the temp file is created, and the temp file is created
    /// and renamed through the folder handle held since open ([`Folder`]), so a change after
    /// that check can at worst put the sidecar beside the original in the folder it was read
    /// from, never in another folder at the same path (#155 F1; on Unix — elsewhere the
    /// operations go by path and the gap between check and create remains).
    ///
    /// A sidecar in a folder this process cannot create files in is refused too, even when
    /// the sidecar itself is writable: the temp file cannot be made, and there is no fallback
    /// to a non-atomic in-place write (decided in the #149 review, F3). The write stays as
    /// debt with the caller — a pending identity field, an IPTC save's error — and the sidecar
    /// stays as it was (docs/storage-and-import.md, "How a sidecar is written").
    pub(super) fn commit(mut self) -> Result<(), String> {
        let stamp = plain("chairphoto", NS_CHAIRPHOTO, "LastWrite", &now().to_string());
        self.replace_owned(&[(NS_CHAIRPHOTO, "LastWrite")], vec![stamp]);

        let mut buf = Vec::new();
        self.root.write(&mut buf).map_err(|e| e.to_string())?;
        let written = write_atomically(&self.folder, &self.original, &self.path, &buf);
        let Self { folder, _turn, .. } = self;
        drop(_turn); // the sweep below needs no file lock: it touches no sidecar
        if written.is_ok() {
            folder.sweep_stale_temps();
        }
        written
    }
}

fn file_name(path: &Path) -> Result<&std::ffi::OsStr, String> {
    path.file_name().ok_or_else(|| format!("{} has no file name", path.display()))
}

fn missing_folder(original: &Path) -> String {
    format!("not writing the sidecar of {}: its folder is missing (offline or moved)", original.display())
}

/// A folder held open for one sidecar transaction: the original's (see the module docs), or
/// a symlinked sidecar's target's. On Unix every operation is relative to the open handle, so
/// it reaches the folder that was opened even if its path now names another one (a volume
/// unmounted, a folder moved); elsewhere it falls back to paths under the folder's path.
struct Folder {
    path: PathBuf,
    #[cfg(unix)]
    fd: rustix::fd::OwnedFd,
}

impl Folder {
    /// The folder `original` is in. A folder that is not there fails as missing, so an
    /// unmounted volume reads as one.
    fn open(original: &Path) -> Result<Self, String> {
        let dir = match original.parent() {
            Some(d) if d.as_os_str().is_empty() => Path::new("."),
            Some(d) => d,
            None => return Err(format!("{} has no folder", original.display())),
        };
        Self::open_dir(dir).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => missing_folder(original),
            _ => format!("cannot open the folder of {}: {e}", original.display()),
        })
    }

    fn open_dir(dir: &Path) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags};
            let fd = rustix::fs::open(dir, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())?;
            Ok(Self { path: dir.to_path_buf(), fd })
        }
        #[cfg(not(unix))]
        {
            if !dir.is_dir() {
                return Err(std::io::ErrorKind::NotFound.into());
            }
            Ok(Self { path: dir.to_path_buf() })
        }
    }

    /// Fail unless the folder's path still names this folder and `original` is a file in it:
    /// a sidecar is only ever written beside its original (AGENTS.md "Sidecars are
    /// `<original_filename>.xmp`, alongside the original"). A folder whose path is gone or now
    /// names another folder (an unmount, a move) is reported missing, as the path check before
    /// the handle was; a removal after this check still cannot redirect the write, because
    /// the write goes through the handle.
    fn require_original(&self, original: &Path) -> Result<(), String> {
        let name = file_name(original)?;
        #[cfg(unix)]
        let (in_place, is_file) = {
            let same = |a: &rustix::fs::Stat, b: &rustix::fs::Stat| a.st_dev == b.st_dev && a.st_ino == b.st_ino;
            let here = rustix::fs::fstat(&self.fd).map_err(|e| e.to_string())?;
            let in_place = rustix::fs::stat(&self.path).is_ok_and(|at_path| same(&at_path, &here));
            let is_file = rustix::fs::statat(&self.fd, name, rustix::fs::AtFlags::empty())
                .is_ok_and(|st| rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::RegularFile);
            (in_place, is_file)
        };
        #[cfg(not(unix))]
        let (in_place, is_file) = (self.path.is_dir(), self.path.join(name).is_file());
        if !in_place {
            return Err(missing_folder(original));
        }
        if !is_file {
            return Err(format!(
                "not writing the sidecar of {}: the original is missing (offline or moved)",
                original.display()
            ));
        }
        Ok(())
    }

    /// Whether `name` exists here (a symlink counts by its target, as `Path::exists`).
    fn exists(&self, name: &std::ffi::OsStr) -> bool {
        #[cfg(unix)]
        return rustix::fs::statat(&self.fd, name, rustix::fs::AtFlags::empty()).is_ok();
        #[cfg(not(unix))]
        return self.path.join(name).exists();
    }

    /// Whether `name` is a symbolic link.
    fn is_symlink(&self, name: &std::ffi::OsStr) -> bool {
        #[cfg(unix)]
        return rustix::fs::statat(&self.fd, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|st| rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::Symlink);
        #[cfg(not(unix))]
        return std::fs::symlink_metadata(self.path.join(name)).is_ok_and(|m| m.file_type().is_symlink());
    }

    /// `name`'s permissions, when it exists.
    fn permissions(&self, name: &std::ffi::OsStr) -> Option<std::fs::Permissions> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let st = rustix::fs::statat(&self.fd, name, rustix::fs::AtFlags::empty()).ok()?;
            Some(std::fs::Permissions::from_mode(u32::from(st.st_mode) & 0o7777))
        }
        #[cfg(not(unix))]
        std::fs::metadata(self.path.join(name)).ok().map(|m| m.permissions())
    }

    fn open_with(&self, name: &std::ffi::OsStr, flags: OpenFor) -> std::io::Result<std::fs::File> {
        #[cfg(unix)]
        {
            use rustix::fs::{Mode, OFlags};
            let (flags, mode) = match flags {
                OpenFor::Read => (OFlags::RDONLY, Mode::empty()),
                OpenFor::WriteExisting => (OFlags::WRONLY, Mode::empty()),
                OpenFor::CreateNew => (OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL, Mode::from_raw_mode(0o666)),
                OpenFor::Replace => (OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC, Mode::from_raw_mode(0o666)),
            };
            Ok(rustix::fs::openat(&self.fd, name, flags | OFlags::CLOEXEC, mode)?.into())
        }
        #[cfg(not(unix))]
        {
            let mut options = std::fs::OpenOptions::new();
            match flags {
                OpenFor::Read => options.read(true),
                OpenFor::WriteExisting => options.write(true),
                OpenFor::CreateNew => options.write(true).create_new(true),
                OpenFor::Replace => options.write(true).create(true).truncate(true),
            };
            options.open(self.path.join(name))
        }
    }

    fn open_read(&self, name: &std::ffi::OsStr) -> std::io::Result<std::fs::File> {
        self.open_with(name, OpenFor::Read)
    }

    /// Copy `from` to `to`, permissions included, replacing `to` (as `std::fs::copy`).
    fn copy(&self, from: &std::ffi::OsStr, to: &std::ffi::OsStr) -> std::io::Result<()> {
        let mut source = self.open_read(from)?;
        let mut copy = self.open_with(to, OpenFor::Replace)?;
        std::io::copy(&mut source, &mut copy)?;
        copy.set_permissions(source.metadata()?.permissions())
    }

    fn rename(&self, from: &std::ffi::OsStr, to: &std::ffi::OsStr) -> std::io::Result<()> {
        #[cfg(unix)]
        return Ok(rustix::fs::renameat(&self.fd, from, &self.fd, to)?);
        #[cfg(not(unix))]
        return std::fs::rename(self.path.join(from), self.path.join(to));
    }

    fn remove(&self, name: &std::ffi::OsStr) -> std::io::Result<()> {
        #[cfg(unix)]
        return Ok(rustix::fs::unlinkat(&self.fd, name, rustix::fs::AtFlags::empty())?);
        #[cfg(not(unix))]
        return std::fs::remove_file(self.path.join(name));
    }

    /// Make a rename in this folder durable. Best effort: not every platform or filesystem
    /// lets a directory be synced, and the new contents are already safe in the file.
    fn sync(&self) {
        #[cfg(unix)]
        let _ = rustix::fs::fsync(&self.fd);
    }
}

#[derive(Clone, Copy)]
enum OpenFor {
    Read,
    /// For writing, neither creating nor truncating: asks whether we may write the file.
    WriteExisting,
    CreateNew,
    Replace,
}

/// Replace `path` with `bytes` so that no reader ever sees a partial file: write a temp file
/// in the same directory, sync it, rename it over `path`, then sync the directory.
///
/// * **Same directory** — a rename is atomic only within one filesystem. POSIX, NFS and SMB2+
///   (the Linux CIFS client and Windows' `MoveFileEx` with replace) all replace an existing
///   target in one step. A volume that refuses the rename fails the write; it never falls
///   back to an in-place write.
/// * **The temp file** ([`temp_path`]) is a dotfile ending in `.chairphoto-tmp`, so the
///   scanner's walks (which skip hidden entries and keep only image extensions) never import
///   it, and nothing takes it for a sidecar (`<original>.xmp`). Its name is unique to this
///   write (process id and a random suffix) and it is created exclusively, so a second
///   ChairPhoto process writing the same sidecar — which the in-process file lock cannot
///   stop — never touches this writer's temp file, nor this one its. The writes then race
///   only at the rename: the last one wins whole.
/// * **On failure** this writer's own temp file is removed and the sidecar is untouched — a
///   read-only volume fails at the temp file's creation, before anything changed. No other
///   temp file is removed here: a fresh one left by a crash (killed between the write and the
///   rename) cannot be told apart from another process's write in progress. Once it is a day
///   old it can, and a later successful write to its folder removes it
///   ([`Folder::sweep_stale_temps`]).
/// * **Permissions** of an existing sidecar are carried over (best effort: a filesystem that
///   cannot set them, such as some SMB mounts, keeps its own). A sidecar this process may not
///   write is refused, as the in-place write it replaces was — a rename would otherwise
///   replace it.
///   The owner becomes the writing user, and hard links to the old file keep the old
///   contents: a rename makes a new file.
/// * **A symlinked sidecar** is written through to its target, as the in-place write was;
///   the link stays a link. The temp file then goes beside the target, in the target's folder
///   (opened by path); the original is still checked through `folder`.
/// * **Through the folder handle** (`folder`, the original's, held since the sidecar was read):
///   the original is checked, and the temp file created, renamed over the sidecar and synced,
///   relative to it — see the module docs.
fn write_atomically(folder: &Folder, original: &Path, path: &Path, bytes: &[u8]) -> Result<(), String> {
    let linked;
    let (dir, target) = if folder.is_symlink(file_name(path)?) {
        let target = std::fs::canonicalize(path).map_err(|e| format!("cannot resolve {}: {e}", path.display()))?;
        let target_dir = target.parent().ok_or_else(|| format!("{} has no folder", target.display()))?;
        linked = Folder::open_dir(target_dir).map_err(|e| format!("cannot open {}: {e}", target_dir.display()))?;
        (&linked, target)
    } else {
        (folder, path.to_path_buf())
    };
    let name = file_name(&target)?;
    let existing = dir.permissions(name);
    if existing.is_some() {
        // The in-place write this replaces needed write access to the file itself; a rename
        // needs only the directory's. Ask the filesystem the old question — open for writing,
        // without truncating or writing anything — so a sidecar we may not write (no write bit
        // for us, another user's file in a shared folder, an ACL) is still refused rather than
        // replaced.
        if let Err(e) = dir.open_with(name, OpenFor::WriteExisting) {
            return Err(format!("cannot write {}: the file is read-only for us ({e})", target.display()));
        }
    }
    let temp = temp_path(&target);
    let temp_name = file_name(&temp)?;
    folder.require_original(original)?;
    #[cfg(test)]
    if let Some(hook) = tests::AFTER_THE_LAST_CHECK.with(|h| h.take()) {
        hook();
    }
    let file = dir
        .open_with(temp_name, OpenFor::CreateNew)
        .map_err(|e| format!("cannot create {}: {e}", temp.display()))?;
    // From here the temp file is ours, and only ours is removed on failure.
    let written = write_temp_then_rename(dir, file, &temp, &target, bytes, existing);
    if written.is_err() {
        let _ = dir.remove(temp_name);
    }
    written
}

fn write_temp_then_rename(
    dir: &Folder,
    mut file: std::fs::File,
    temp: &Path,
    target: &Path,
    bytes: &[u8],
    existing: Option<std::fs::Permissions>,
) -> Result<(), String> {
    let fail = |what: &str, e: std::io::Error| format!("cannot {what} {}: {e}", temp.display());
    file.write_all(bytes).map_err(|e| fail("write", e))?;
    if let Some(permissions) = existing {
        let _ = file.set_permissions(permissions);
    }
    // A network filesystem may report a failed write only here; a filesystem without
    // fsync says so with Unsupported/InvalidInput, which is no reason to fail the write.
    if let Err(e) = file.sync_all() {
        if !matches!(e.kind(), std::io::ErrorKind::Unsupported | std::io::ErrorKind::InvalidInput) {
            return Err(fail("sync", e));
        }
    }
    drop(file);
    #[cfg(test)]
    if tests::FAIL_BEFORE_RENAME.with(|f| f.get()) {
        return Err("simulated failure before the rename".to_string());
    }
    dir.rename(file_name(temp)?, file_name(target)?)
        .map_err(|e| format!("cannot replace {}: {e}", target.display()))?;
    dir.sync();
    Ok(())
}

/// `<dir>/.<sidecar name>.<pid>-<random>.chairphoto-tmp`, a new name on every call — see
/// [`write_atomically`].
fn temp_path(sidecar: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(sidecar.file_name().unwrap_or_default());
    let random = uuid::Uuid::new_v4().simple().to_string();
    name.push(format!(".{}-{}{TEMP_SUFFIX}", std::process::id(), &random[..12]));
    sidecar.with_file_name(name)
}

const TEMP_SUFFIX: &str = ".chairphoto-tmp";

/// Whether `name` is a temp file [`temp_path`] made: `.<name>.<pid>-<12 hex>.chairphoto-tmp`.
/// Nothing else is ever swept.
fn is_temp_name(name: &std::ffi::OsStr) -> bool {
    let Some(rest) = name.to_str().and_then(|n| n.strip_prefix('.')).and_then(|n| n.strip_suffix(TEMP_SUFFIX)) else {
        return false;
    };
    let Some((sidecar, tag)) = rest.rsplit_once('.') else { return false };
    let Some((pid, random)) = tag.split_once('-') else { return false };
    !sidecar.is_empty()
        && !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && random.len() == 12
        && random.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// How old a temp file must be before [`Folder::sweep_stale_temps`] removes it. A write holds
/// its temp file for the time it takes to write and sync one sidecar; a day leaves room for
/// any clock skew between the machines sharing a NAS folder.
const STALE_TEMP_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// The folders this process has swept (by the path they were opened at): each is listed at
/// most once per run, so a folder of thousands of photos is not listed on every save.
static SWEPT: std::sync::Mutex<std::collections::BTreeSet<PathBuf>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

impl Folder {
    /// Remove the temp files ([`is_temp_name`]) older than [`STALE_TEMP_AGE`] in this folder:
    /// left by a write killed between its create and its rename, never another write in
    /// progress (#155 R4). Called after a successful commit, once per folder per run, with no
    /// lock held. Best effort: a file that cannot be read or removed is left.
    fn sweep_stale_temps(&self) {
        if !SWEPT.lock().unwrap_or_else(|e| e.into_inner()).insert(self.path.clone()) {
            return;
        }
        let now = std::time::SystemTime::now();
        for name in self.temp_names() {
            if self.is_stale_temp(&name, now) {
                let _ = self.remove(&name);
            }
        }
    }

    /// Whether `name` is a regular file last modified more than [`STALE_TEMP_AGE`] before
    /// `now`. Decided from its metadata alone (a symlink is not followed), never by opening
    /// it: a FIFO or device node with a temp's name would block an open (#155 review, L1).
    fn is_stale_temp(&self, name: &std::ffi::OsStr, now: std::time::SystemTime) -> bool {
        let old = |modified: std::time::SystemTime| now.duration_since(modified).is_ok_and(|age| age > STALE_TEMP_AGE);
        #[cfg(unix)]
        {
            use rustix::fs::{AtFlags, FileType};
            let Ok(st) = rustix::fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) else { return false };
            let Ok(secs) = u64::try_from(st.st_mtime) else { return false };
            FileType::from_raw_mode(st.st_mode) == FileType::RegularFile
                && old(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
        }
        #[cfg(not(unix))]
        std::fs::symlink_metadata(self.path.join(name))
            .is_ok_and(|m| m.is_file() && m.modified().is_ok_and(old))
    }

    /// The names in this folder that are ChairPhoto temp files.
    fn temp_names(&self) -> Vec<std::ffi::OsString> {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            let Ok(dir) = rustix::fs::Dir::read_from(&self.fd) else { return Vec::new() };
            dir.filter_map(Result::ok)
                .map(|e| std::ffi::OsStr::from_bytes(e.file_name().to_bytes()).to_os_string())
                .filter(|n| is_temp_name(n))
                .collect()
        }
        #[cfg(not(unix))]
        {
            let Ok(entries) = std::fs::read_dir(&self.path) else { return Vec::new() };
            entries.filter_map(Result::ok).map(|e| e.file_name()).filter(|n| is_temp_name(n)).collect()
        }
    }
}

/// Whether any top-level `rdf:Description` carries `chairphoto:LastWrite`: exiftool keeps one
/// Description per namespace, so the stamp need not sit in the first (#147).
fn has_chairphoto_last_write(rdf: &Element) -> bool {
    rdf.children.iter().any(|d| {
        matches!(d, XMLNode::Element(desc)
            if desc.namespace.as_deref() == Some(NS_RDF) && desc.name == "Description"
                && desc.children.iter().any(|n| matches!(n, XMLNode::Element(e)
                    if e.namespace.as_deref() == Some(NS_CHAIRPHOTO) && e.name == "LastWrite")))
    })
}

/// Drop the `owned` (namespace, local-name) properties from `desc`, in element form and in
/// compact attribute form alike. Leaving the attribute form behind would give the property two
/// values once the replacement element lands — and readers such as `read_identifier` check
/// the attribute first, so an Overwrite would not take.
fn remove_owned(desc: &mut Element, owned: &[(&str, &str)]) {
    desc.children.retain(|n| !matches_owned(n, owned));
    let doomed: Vec<String> = desc
        .attributes
        .keys()
        .filter(|k| owned.iter().any(|(ns, name)| attr_is(desc, k, ns, name)))
        .cloned()
        .collect();
    for key in doomed {
        desc.attributes.remove(&key);
    }
}

fn matches_owned(node: &XMLNode, owned: &[(&str, &str)]) -> bool {
    matches!(node, XMLNode::Element(e)
        if owned.iter().any(|(ns, name)| e.namespace.as_deref() == Some(*ns) && e.name == *name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xmp::sidecar_backup_path;
    use std::time::Duration;

    thread_local! {
        /// Makes this thread's next commits fail after the temp file is written and synced,
        /// before the rename — where a crash or a full disk would stop it.
        pub(super) static FAIL_BEFORE_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// Runs once in this thread's next commit, after its last check that the original is
        /// there and before the temp file is created — the gap #155 F1 is about.
        pub(super) static AFTER_THE_LAST_CHECK: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::Cell::new(None) };
    }

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

    /// Issue #149 F1 (with #148): a sidecar is never written where the original is not. With
    /// the original's folder gone (an unmounted volume) or the original itself gone, `open`
    /// refuses, and a `commit` whose original vanished after `open` refuses too — and no
    /// directory is created in either case.
    #[test]
    fn commit_refuses_when_the_original_or_its_directory_is_missing() {
        let dir = crate::test_support::TestTmpDir::new("doc-mkdir");
        let photo = dir.join("nested/deep/G.ARW");
        let folder = photo.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(&photo, b"raw").unwrap();

        // The folder vanishes between open and commit.
        let doc = SidecarDocument::open(&photo).unwrap();
        std::fs::rename(&folder, dir.join("away")).unwrap();
        let err = doc.commit().unwrap_err();
        assert!(err.contains("folder is missing"), "{err}");
        assert!(!folder.exists(), "commit must not recreate the original's folder");

        // Opened with the folder already gone.
        let err = SidecarDocument::open(&photo).err().expect("open must refuse a missing folder");
        assert!(err.contains("folder is missing"), "{err}");
        assert!(!folder.exists());

        // The folder is back but the original is not: refused at open and at commit.
        std::fs::rename(dir.join("away"), &folder).unwrap();
        let doc = SidecarDocument::open(&photo).unwrap();
        std::fs::remove_file(&photo).unwrap();
        let err = doc.commit().unwrap_err();
        assert!(err.contains("original is missing"), "{err}");
        assert!(SidecarDocument::open(&photo).is_err());
        assert!(!sidecar_path(&photo).exists(), "no sidecar for a missing original");
    }

    /// Every writer refuses a missing original and creates nothing — identifier, import batch,
    /// Overwrite, GPS, faces and IPTC — while the export keyword writer and the bundle
    /// importer's identifier write, whose targets exist, still write.
    #[test]
    fn every_writer_refuses_a_missing_original_and_existing_targets_still_work() {
        use crate::catalog::IptcFields;
        use crate::xmp::FaceRegion;
        let dir = crate::test_support::TestTmpDir::new("doc-149-writers");
        let gone = dir.join("unmounted").join("A.ARW");
        let uuid = "8d0a2c1e-4f5b-4c6d-9e7f-0a1b2c3d4e5f";
        let face = [FaceRegion { face_id: 1, name: "Ada".into(), bbox: (0.1, 0.1, 0.2, 0.2) }];
        let frame = crate::xmp::RegionFrame { orientation: None, stored_size: Some((600, 400)) };
        let title = IptcFields { title: "T".into(), ..Default::default() };
        let writes: Vec<(&str, Box<dyn Fn(&Path) -> Result<(), String>>)> = vec![
            ("identifier", Box::new(|p| crate::xmp::write_identifier(p, uuid))),
            ("import batch", Box::new(|p| crate::xmp::write_import_batch(p, uuid))),
            ("overwrite", Box::new(|p| crate::xmp::overwrite_identifier(p, uuid).map(|_| ()))),
            ("gps", Box::new(|p| crate::xmp::write_gps(p, 59.9, 10.7))),
            ("faces", Box::new(move |p| {
                let written = crate::xmp::write_face_regions(p, uuid, &face, &[], &[], frame);
                // Missing originals are offline, not a sidecar that refuses: tried again later.
                if let Err(e) = &written {
                    assert!(matches!(e, crate::xmp::RegionWriteError::Failed(_)), "{e:?}");
                }
                written.map_err(|e| e.to_string())
            })),
            ("iptc", Box::new(move |p| crate::xmp::write_iptc(p, &IptcFields::default(), &title))),
        ];
        for (name, write) in &writes {
            assert!(write(&gone).is_err(), "{name}: a missing folder must be refused");
            assert!(!gone.parent().unwrap().exists(), "{name}: the folder was created");
        }
        std::fs::create_dir_all(gone.parent().unwrap()).unwrap();
        for (name, write) in &writes {
            assert!(write(&gone).is_err(), "{name}: a missing original must be refused");
            assert!(!sidecar_path(&gone).exists(), "{name}: a sidecar was created");
        }

        // An export destination and a bundle-imported photo exist before their sidecar write.
        let exported = dir.join("export").join("A.jpg");
        std::fs::create_dir_all(exported.parent().unwrap()).unwrap();
        std::fs::write(&exported, b"jpeg").unwrap();
        crate::xmp::write_keywords(&exported, &["a".into()], &["x|a".into()]).unwrap();
        crate::xmp::write_identifier(&exported, uuid).unwrap();
        let xml = std::fs::read_to_string(sidecar_path(&exported)).unwrap();
        assert!(xml.contains(uuid) && xml.contains(">a<"), "{xml}");
    }

    fn set_prop(doc: &mut SidecarDocument, name: &str, value: &str) {
        doc.replace_owned(&[(NS_CHAIRPHOTO, name)], vec![plain("chairphoto", NS_CHAIRPHOTO, name, value)]);
    }

    /// The temp files in `sidecar`'s directory.
    fn temps_beside(sidecar: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(sidecar.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(TEMP_SUFFIX))
            .collect();
        names.sort();
        names
    }

    /// Review F2 of #149: two processes write one sidecar. The in-process file lock does not
    /// reach across processes, so two threads call `write_atomically` directly, as two
    /// processes would. No commit may fail (a shared temp name made the loser's rename fail
    /// with ENOENT), the sidecar always parses, and no temp file is left.
    #[test]
    fn two_writers_past_the_lock_never_fail_or_leave_a_partial_sidecar() {
        use std::sync::{Arc, Barrier};
        let (_dir, p) = photo("doc-149-procs", "S.ARW");
        let xmp = sidecar_path(&p);
        std::fs::write(&xmp, FOREIGN).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let writers: Vec<_> = (0..2)
            .map(|w| {
                let (p, xmp, barrier) = (p.clone(), xmp.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let body = FOREIGN.replace("<darktable:history_end>7", &format!(
                        "<darktable:history_end>{}", "7".repeat(256 * 1024 + w)));
                    barrier.wait();
                    let folder = Folder::open(&p).unwrap();
                    for i in 0..60 {
                        write_atomically(&folder, &p, &xmp, body.as_bytes())
                            .unwrap_or_else(|e| panic!("writer {w}, commit {i}: {e}"));
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        assert_parses(&std::fs::read_to_string(&xmp).unwrap());
        assert_eq!(temps_beside(&xmp), Vec::<String>::new());
    }

    fn assert_parses(xml: &str) {
        parse_xml(xml.as_bytes()).unwrap_or_else(|e| panic!("sidecar does not parse ({e}):\n{xml}"));
    }

    /// Issue #149: a commit that fails after the temp file is written but before the rename
    /// leaves the sidecar byte-identical and no temp file behind.
    #[test]
    fn a_failure_before_the_rename_leaves_the_sidecar_untouched_and_no_temp() {
        let (_dir, p) = photo("doc-149-crash", "K.ARW");
        let xmp = sidecar_path(&p);
        std::fs::write(&xmp, FOREIGN).unwrap();
        let mut doc = SidecarDocument::open(&p).unwrap();
        set_prop(&mut doc, "Foo", "bar");

        FAIL_BEFORE_RENAME.with(|f| f.set(true));
        let err = doc.commit().unwrap_err();
        FAIL_BEFORE_RENAME.with(|f| f.set(false));

        assert!(err.contains("before the rename"), "{err}");
        assert_eq!(std::fs::read(&xmp).unwrap(), FOREIGN.as_bytes(), "the sidecar must be untouched");
        assert_eq!(temps_beside(&xmp), Vec::<String>::new(), "the temp file must be removed");
        // The lock went with the failed document: the next write goes through.
        let mut doc = SidecarDocument::open(&p).unwrap();
        set_prop(&mut doc, "Foo", "bar");
        doc.commit().unwrap();
        assert!(std::fs::read_to_string(&xmp).unwrap().contains("<chairphoto:Foo>bar</chairphoto:Foo>"));
    }

    /// A temp file a crash left behind (killed between write and rename) — or another
    /// process's write in progress, which looks the same — neither blocks the next write nor is
    /// read as the sidecar, and while fresh is left alone (F2 of the #149 review; a day-old one
    /// is swept, see `a_write_sweeps_day_old_temps_of_its_folder_once`).
    #[test]
    fn a_temp_left_by_a_crash_is_left_alone_and_does_not_block_the_next_write() {
        let (_dir, p) = photo("doc-149-sweep", "L.ARW");
        let xmp = sidecar_path(&p);
        std::fs::write(&xmp, FOREIGN).unwrap();
        let stale = temp_path(&xmp);
        std::fs::write(&stale, "<x:xmpmeta><half").unwrap();

        let mut doc = SidecarDocument::open(&p).unwrap();
        set_prop(&mut doc, "Foo", "bar");
        doc.commit().unwrap();

        assert_eq!(std::fs::read_to_string(&stale).unwrap(), "<x:xmpmeta><half", "another writer's temp was touched");
        assert_eq!(temps_beside(&xmp).len(), 1, "only the stale temp may remain");
        let xml = std::fs::read_to_string(&xmp).unwrap();
        assert_parses(&xml);
        assert!(xml.contains("history_end") && xml.contains("<chairphoto:Foo>bar</chairphoto:Foo>"), "{xml}");
    }

    /// The temp file is invisible to the scanner (a dotfile — its walks skip hidden entries —
    /// with no image extension), is not a sidecar name (`<original>.xmp`), and is new for
    /// every write.
    #[test]
    fn the_temp_file_is_neither_scanned_nor_a_sidecar() {
        let xmp = sidecar_path(Path::new("/library/2026/DSC1.ARW"));
        let temp = temp_path(&xmp);
        assert_ne!(temp, temp_path(&xmp), "two writes must not share a temp name");
        let name = temp.file_name().unwrap().to_str().unwrap();
        let pid = format!(".DSC1.ARW.xmp.{}-", std::process::id());
        assert!(name.starts_with(&pid) && name.ends_with(TEMP_SUFFIX), "{name}");
        assert_eq!(temp.parent(), xmp.parent(), "same directory, so the rename stays on one volume");
        let name = temp.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with('.'), "{name}");
        assert!(!crate::scanner::is_supported_image(&temp), "{name}");
        assert_ne!(temp.extension().and_then(|e| e.to_str()), Some("xmp"), "{name}");
    }

    /// Issue #149: while one writer holds a sidecar open, a second waits; it then reads the
    /// first one's committed file, so both changes survive.
    #[test]
    fn a_second_writer_waits_for_the_first_to_commit() {
        let (_dir, p) = photo("doc-149-wait", "M.ARW");
        std::fs::write(sidecar_path(&p), FOREIGN).unwrap();
        let mut first = SidecarDocument::open(&p).unwrap();
        set_prop(&mut first, "First", "1");

        let second = {
            let p = p.clone();
            std::thread::spawn(move || {
                let mut doc = SidecarDocument::open(&p).unwrap();
                set_prop(&mut doc, "Second", "2");
                doc.commit().unwrap();
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!second.is_finished(), "the second writer must wait for the first's commit");
        first.commit().unwrap();
        second.join().unwrap();

        let xml = std::fs::read_to_string(sidecar_path(&p)).unwrap();
        assert_parses(&xml);
        assert!(xml.contains("<chairphoto:First>1</chairphoto:First>"), "first change lost:\n{xml}");
        assert!(xml.contains("<chairphoto:Second>2</chairphoto:Second>"), "second change lost:\n{xml}");
        assert!(xml.contains("history_end"), "{xml}");
    }

    /// Issue #149, probe P3: two IPTC writes with disjoint changes ({title}, {city}), released
    /// together by a barrier on a Lightroom sidecar, many times over. Both changes survive
    /// every round and the file always parses.
    #[test]
    fn concurrent_disjoint_iptc_writes_both_survive_and_the_file_parses() {
        use crate::catalog::IptcFields;
        use crate::xmp::test_fixtures::{property_values, LIGHTROOM};
        use std::sync::{Arc, Barrier};

        let (_dir, p) = photo("doc-149-race", "N.ARW");
        let xmp = sidecar_path(&p);
        for round in 0..200 {
            std::fs::write(&xmp, LIGHTROOM).unwrap();
            let barrier = Arc::new(Barrier::new(2));
            let writers: Vec<_> = [
                IptcFields { title: format!("T{round}"), ..Default::default() },
                IptcFields { city: format!("C{round}"), ..Default::default() },
            ]
            .into_iter()
            .map(|after| {
                let (p, barrier) = (p.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    crate::xmp::write_iptc(&p, &IptcFields::default(), &after)
                })
            })
            .collect();
            for w in writers {
                w.join().unwrap().unwrap();
            }
            let xml = std::fs::read_to_string(&xmp).unwrap();
            assert_parses(&xml);
            assert_eq!(property_values(&xml, super::super::NS_DC, "title"), vec![format!("T{round}")],
                "round {round}: title lost:\n{xml}");
            assert_eq!(property_values(&xml, super::super::NS_PHOTOSHOP, "City"), vec![format!("C{round}")],
                "round {round}: city lost:\n{xml}");
        }
    }

    /// Readers take no lock (`read_identifier`, `read_gps`, a scan, another tool), so the
    /// sidecar must never be visible half-written: a reader racing a stream of commits always
    /// finds a complete document — the old one or the new one.
    #[test]
    fn a_reader_racing_commits_never_sees_a_partial_file() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let (_dir, p) = photo("doc-149-reader", "R.ARW");
        let xmp = sidecar_path(&p);
        std::fs::write(&xmp, FOREIGN).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let reader = {
            let (xmp, done) = (xmp.clone(), done.clone());
            std::thread::spawn(move || {
                let mut reads = 0usize;
                while !done.load(Ordering::Relaxed) {
                    let xml = std::fs::read_to_string(&xmp).unwrap();
                    parse_xml(xml.as_bytes())
                        .unwrap_or_else(|e| panic!("read {reads}: a partial sidecar ({e}):\n{xml}"));
                    reads += 1;
                }
                reads
            })
        };
        for i in 0..150 {
            let mut doc = SidecarDocument::open(&p).unwrap();
            set_prop(&mut doc, "Foo", &"x".repeat(i * 37));
            doc.commit().unwrap();
        }
        done.store(true, Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0, "the reader never ran");
    }

    /// The rename makes a new file; the old one's permissions carry over to it.
    #[cfg(unix)]
    #[test]
    fn a_commit_keeps_the_sidecars_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, p) = photo("doc-149-mode", "O.ARW");
        let xmp = sidecar_path(&p);
        std::fs::write(&xmp, FOREIGN).unwrap();
        std::fs::set_permissions(&xmp, std::fs::Permissions::from_mode(0o640)).unwrap();
        SidecarDocument::open(&p).unwrap().commit().unwrap();
        assert_eq!(std::fs::metadata(&xmp).unwrap().permissions().mode() & 0o777, 0o640);
    }

    /// A sidecar the user made read-only is refused, as the in-place write was — the rename
    /// must not replace it — and no temp file is left.
    #[cfg(unix)]
    #[test]
    fn a_read_only_sidecar_is_refused_and_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, p) = photo("doc-149-ro", "P.ARW");
        let xmp = sidecar_path(&p);
        std::fs::write(&xmp, FOREIGN).unwrap();
        std::fs::set_permissions(&xmp, std::fs::Permissions::from_mode(0o444)).unwrap();
        let err = SidecarDocument::open(&p).unwrap().commit().unwrap_err();
        assert!(err.contains("read-only"), "{err}");
        assert_eq!(std::fs::read(&xmp).unwrap(), FOREIGN.as_bytes());
        assert_eq!(temps_beside(&xmp), Vec::<String>::new());
    }

    /// Review F4 of #149: a sidecar with a write bit, but not one that lets us write it — mode
    /// 0464, group-writable, not owner-writable, owned by us — is refused, as the in-place
    /// write was. `Permissions::readonly()` alone (no write bit at all) let the rename replace
    /// it.
    #[cfg(unix)]
    #[test]
    fn a_sidecar_writable_only_by_others_is_refused_and_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, p) = photo("doc-149-0464", "P2.ARW");
        let xmp = sidecar_path(&p);
        std::fs::write(&xmp, FOREIGN).unwrap();
        std::fs::set_permissions(&xmp, std::fs::Permissions::from_mode(0o464)).unwrap();
        if std::fs::OpenOptions::new().write(true).open(&xmp).is_ok() {
            println!("SKIPPED: a_sidecar_writable_only_by_others_is_refused_and_left_alone — running with privileges that ignore the mode");
            return;
        }
        let err = crate::xmp::write_identifier(&p, "8d0a2c1e-4f5b-4c6d-9e7f-0a1b2c3d4e5f").unwrap_err();
        assert!(err.contains("read-only"), "{err}");
        assert_eq!(std::fs::read(&xmp).unwrap(), FOREIGN.as_bytes());
        assert_eq!(std::fs::metadata(&xmp).unwrap().permissions().mode() & 0o777, 0o464);
        assert_eq!(temps_beside(&xmp), Vec::<String>::new());
    }

    /// #155 F1: the volume is swapped out in the gap between the commit's last check that the
    /// original is there and the temp file's creation — the library folder moved away and an
    /// empty folder left at its path, as an unmount leaves its mount point. The write goes
    /// through the folder handle held since open, so it lands beside the original in the
    /// folder it was read from; nothing is created in the folder now at the old path. (By
    /// path, the temp file and then the sidecar were created in that empty folder.)
    #[cfg(unix)]
    #[test]
    fn a_folder_swapped_after_the_last_check_cannot_redirect_the_write() {
        let dir = crate::test_support::TestTmpDir::new("doc-155-f1");
        let library = dir.join("library");
        std::fs::create_dir_all(&library).unwrap();
        let p = library.join("R.ARW");
        std::fs::write(&p, b"raw").unwrap();
        std::fs::write(sidecar_path(&p), FOREIGN).unwrap();
        let mut doc = SidecarDocument::open(&p).unwrap();
        set_prop(&mut doc, "Foo", "bar");

        let away = dir.join("away");
        let (from, to) = (library.clone(), away.clone());
        AFTER_THE_LAST_CHECK.with(|h| h.set(Some(Box::new(move || {
            std::fs::rename(&from, &to).unwrap();
            std::fs::create_dir(&from).unwrap(); // the empty mount point
        }))));
        doc.commit().unwrap();

        let stub: Vec<_> = std::fs::read_dir(&library).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(stub.is_empty(), "the write went to the folder now at the old path: {stub:?}");
        let xml = std::fs::read_to_string(sidecar_path(&away.join("R.ARW"))).unwrap();
        assert!(xml.contains("history_end") && xml.contains("<chairphoto:Foo>bar</chairphoto:Foo>"), "{xml}");
        assert_eq!(temps_beside(&sidecar_path(&away.join("R.ARW"))), Vec::<String>::new());
    }

    /// #155 R4: a successful write removes the temp files a crash left in its folder once they
    /// are a day old — only names of the temp pattern, never a fresh one (another write may be
    /// in progress), never anything else — and lists a folder only once per run.
    #[test]
    fn a_write_sweeps_day_old_temps_of_its_folder_once() {
        let (dir, p) = photo("doc-155-sweep", "T.ARW");
        let xmp = sidecar_path(&p);
        std::fs::write(&xmp, FOREIGN).unwrap();
        let aged = |path: &Path| {
            std::fs::write(path, "<x:xmpmeta><half").unwrap();
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_modified(std::time::SystemTime::now() - STALE_TEMP_AGE - std::time::Duration::from_secs(60)).unwrap();
        };
        let crashed = temp_path(&xmp);
        aged(&crashed);
        let other_sidecars = temp_path(&dir.join("U.ARW.xmp"));
        aged(&other_sidecars);
        let fresh = temp_path(&xmp);
        std::fs::write(&fresh, "<x:xmpmeta><in progress").unwrap();
        let not_ours = [dir.join(".T.ARW.xmp.chairphoto-tmp"), dir.join(".T.ARW.xmp.12-notahexsuffix.chairphoto-tmp"),
            dir.join("T.ARW.xmp.12-0123456789ab.chairphoto-tmp")];
        for path in &not_ours {
            aged(path);
        }

        let mut doc = SidecarDocument::open(&p).unwrap();
        set_prop(&mut doc, "Foo", "bar");
        doc.commit().unwrap();

        assert!(!crashed.exists() && !other_sidecars.exists(), "a day-old temp stayed");
        assert!(fresh.exists(), "a fresh temp (a write in progress?) was removed");
        for path in &not_ours {
            assert!(path.exists(), "{} is not a temp name of ours", path.display());
        }

        // Listed once per run: a stale temp that appears later stays through the next write.
        let later = temp_path(&xmp);
        aged(&later);
        SidecarDocument::open(&p).unwrap().commit().unwrap();
        assert!(later.exists(), "the folder was listed again");
    }

    /// #155 review L1: a FIFO named like a day-old temp neither hangs the commit's sweep (an
    /// open of it would block until a writer came) nor is removed: only regular files are.
    #[cfg(unix)]
    #[test]
    fn a_fifo_named_like_a_temp_neither_hangs_the_sweep_nor_is_removed() {
        let (dir, p) = photo("doc-155-fifo", "V.ARW");
        let fifo = dir.join(".V.ARW.xmp.1-abcdefabcdef.chairphoto-tmp");
        let folder = rustix::fs::open(&*dir, rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY, rustix::fs::Mode::empty()).unwrap();
        rustix::fs::mknodat(&folder, fifo.file_name().unwrap(), rustix::fs::FileType::Fifo, rustix::fs::Mode::from_raw_mode(0o644), 0).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let committed = SidecarDocument::open(&p).and_then(|d| d.commit());
            let _ = tx.send(committed);
        });
        let finished = rx.recv_timeout(Duration::from_secs(5));
        if finished.is_err() {
            let _ = std::fs::OpenOptions::new().write(true).open(&fifo); // release the stuck open
        }
        finished.expect("the commit's sweep hung on a FIFO named like a temp").unwrap();
        let kind = std::fs::symlink_metadata(&fifo).expect("the FIFO was removed").file_type();
        assert!(std::os::unix::fs::FileTypeExt::is_fifo(&kind));
    }

    #[test]
    fn only_the_temp_pattern_is_a_temp_name() {
        let made = temp_path(Path::new("/library/DSC1.ARW.xmp"));
        assert!(is_temp_name(made.file_name().unwrap()), "{made:?}");
        for name in [".DSC1.ARW.xmp.chairphoto-tmp", ".DSC1.ARW.xmp.1-0123456789ab.chairphoto-tmpx",
            ".DSC1.ARW.xmp.x1-0123456789ab.chairphoto-tmp", ".DSC1.ARW.xmp.1-0123456789AB.chairphoto-tmp",
            "..1-0123456789ab.chairphoto-tmp", "DSC1.ARW.xmp", ".DSC1.ARW.xmp.-0123456789ab.chairphoto-tmp"] {
            assert!(!is_temp_name(std::ffi::OsStr::new(name)), "{name}");
        }
    }

    /// A symlinked sidecar is written through to its target; the link stays a link.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_sidecar_is_written_through() {
        let (dir, p) = photo("doc-149-link", "Q.ARW");
        let real = dir.join("elsewhere").join("Q.xmp");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, FOREIGN).unwrap();
        let xmp = sidecar_path(&p);
        std::os::unix::fs::symlink(&real, &xmp).unwrap();

        let mut doc = SidecarDocument::open(&p).unwrap();
        set_prop(&mut doc, "Foo", "bar");
        doc.commit().unwrap();

        assert!(std::fs::symlink_metadata(&xmp).unwrap().file_type().is_symlink(), "the link was replaced");
        let xml = std::fs::read_to_string(&real).unwrap();
        assert!(xml.contains("history_end") && xml.contains("<chairphoto:Foo>bar</chairphoto:Foo>"), "{xml}");
        assert!(temps_beside(&real).is_empty() && temps_beside(&xmp).is_empty());
    }
}
