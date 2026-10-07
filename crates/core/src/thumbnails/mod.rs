//! Thumbnail and preview generation, with on-disk caching.
//!
//! Raster images are decoded directly by the `image` crate. RAW files have an
//! embedded preview JPEG extracted via `exiv2` — chosen because it is ~10x faster
//! to spawn than exiftool and exposes ALL embedded previews (Sony ARW embeds a
//! tiny, a medium ~1616px, and a full-resolution ~9984px preview). We pick the
//! smallest preview large enough for the requested size, so grid thumbnails decode
//! a small preview while the loupe gets the sharp full-resolution one.
//!
//! Results are cached on disk keyed by absolute path + mtime + size + target size,
//! so each version is generated at most once.

use crate::scanner::{is_raw, is_video};
use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, RgbImage};
use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const THUMB_MAX: u32 = 512;
/// The preview tier's long edge. Before #245 a decode smaller than this was enlarged to it,
/// which is what makes a sharpness score from then stale (`sharpness_indexer`).
pub(crate) const PREVIEW_MAX: u32 = 2048;
/// Zoom uses the embedded preview at native resolution (no downscale below this);
/// Sony's full embedded preview is ~9984px, so this keeps it intact.
const ZOOM_MAX: u32 = 10000;

/// Bump when generation logic changes in a way that affects output (orientation,
/// preview selection, …), so stale cached images are regenerated, not reused. A bump orphans
/// the previous directories; [`cleanup_stale_caches`] removes the ones it names.
///
/// v5: single-decode downscale chain — a preview/zoom decode now opportunistically
/// derives (and caches) the smaller sizes from the one in-hand decode. Thumbnails
/// derived from a larger decode differ pixel-for-pixel from an independently
/// extracted small embedded preview, so old caches must not be reused.
///
/// v6 (#245): a tier never upscales (2497fa2, #168). Before it, `image`'s `thumbnail` fitted
/// every decode to the tier's box both ways, so an original smaller than a tier was enlarged
/// into it: a 1200×800 photo had a 2048×1365 preview and a 300×200 one a 512×341 thumbnail.
/// Those files are not told apart from a real downscale by their size (every v5 preview's
/// long edge is 2048), so the whole of `t512v5` and `p2048v5` is left behind and both tiers
/// regenerate lazily, on the image pool, at their native size. The face-region writer's
/// preview cross-check keeps the old previews' sizes meanwhile ([`cached_preview_size`]).
///
/// Shared by thumb and preview only — see [`ZOOM_VERSION`] for why zoom keeps its own.
const CACHE_VERSION: u32 = 6;

/// Zoom's own cache-directory version, independent of [`CACHE_VERSION`], so a change that
/// affects one tier's output does not regenerate the others. The no-upscale fix (#168) moved
/// zoom to v6 first, orphaning `z10000v5` (whose files were blown up to 10 000 px); thumb and
/// preview followed in #245, when `CACHE_VERSION` went to 6 for the same fix. The two numbers
/// being equal now is a coincidence: the directory names differ by tier tag (`z`, `p`, `t`).
const ZOOM_VERSION: u32 = 6;

/// One cache size: its longest-edge cap, on-disk tag, cache-directory version, and JPEG
/// quality.
#[derive(Clone, Copy)]
struct Size {
    max: u32,
    tag: &'static str,
    version: u32,
    quality: u8,
}

const THUMB: Size = Size { max: THUMB_MAX, tag: "t", version: CACHE_VERSION, quality: 80 };
const PREVIEW: Size = Size { max: PREVIEW_MAX, tag: "p", version: CACHE_VERSION, quality: 85 };
const ZOOM: Size = Size { max: ZOOM_MAX, tag: "z", version: ZOOM_VERSION, quality: 92 };

// --- analyzer hook ----------------------------------------------------------
// A tiny registry of callbacks invoked once, with the freshly decoded (full,
// oriented) image, every time a size is *generated* from an extraction/decode.
// This lets one decode feed many analyzers (H16 sharpness, H15a pHash) instead of
// each re-reading and re-decoding the file. Cache hits do not fire hooks — there is
// no decode to observe. Ships with a no-op default (empty registry).

/// An analyzer: given the decoded image and the file it came from, do its work
/// (compute a score/hash, stash it somewhere). Must be cheap-ish and must not panic —
/// it runs inside generation on worker threads. `Send + Sync + 'static` so it can be
/// stored in a `Vec` behind a mutex and cloned by `Arc` for lock-free dispatch.
pub type Analyzer = Arc<dyn Fn(&DynamicImage, &Path) + Send + Sync + 'static>;

fn analyzers() -> &'static Mutex<Vec<Analyzer>> {
    static REGISTRY: OnceLock<Mutex<Vec<Analyzer>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register an analyzer to be invoked with every freshly decoded image during
/// thumbnail/preview/zoom generation. Called once at startup by features that want to
/// piggyback on the decode (H16, H15a). No-op set by default.
pub fn register_analyzer(analyzer: Analyzer) {
    if let Ok(mut list) = analyzers().lock() {
        list.push(analyzer);
    }
}

/// Fire every registered analyzer once against a decoded image, but only when the
/// decoded size meets the minimum resolution required for accurate scoring.
///
/// `decoded_max` is the longest-edge cap used for this decode (i.e. `size.max` from
/// the `Size` that triggered the decode). Analyzers that depend on fine detail —
/// sharpness scoring, pHash — need at least `PREVIEW_MAX` (2048px) to see micro-blur;
/// firing them on a THUMB (512px) decode produces an inaccurate result that the
/// `sharpness IS NULL` guard then treats as canonical, preventing a later accurate score.
///
/// # Why we snapshot first
///
/// The global registry `Mutex` must **not** be held while executing callbacks:
/// callbacks can be CPU-heavy (sharpness scoring, pHash) and many decode workers run
/// in parallel — holding the lock during execution would serialize all of them on one
/// lock, defeating the parallelism (I7b review finding). Instead we snapshot the `Arc`
/// list under a brief lock and then call each analyzer with the lock released. The
/// `Arc` clones keep each callback alive for the duration; registration (the only
/// mutation) contends only with other registrations, not with ongoing callbacks.
fn run_analyzers(img: &DynamicImage, path: &Path, decoded_max: u32) {
    // Resolution gate: skip analyzers when the decoded size is below the preview tier.
    // A THUMB (512px) decode cannot show micro-blur; scoring on it produces a wrong
    // result that the IS NULL guard then permanently locks in, preventing an accurate
    // score from the batch indexer or a later preview/zoom decode.
    if decoded_max < PREVIEW_MAX {
        return;
    }
    // Snapshot: O(n) clone of Arc pointers, then release the lock immediately.
    let snapshot: Vec<Analyzer> = analyzers()
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    // Execute with the lock NOT held.
    for a in &snapshot {
        a(img, path);
    }
}

/// JPEG bytes for a small grid thumbnail (cached).
pub fn thumbnail_bytes(path: &Path) -> Result<Vec<u8>, String> {
    cached(path, THUMB)
}

/// Apply a photo's non-destructive user rotation on top of the EXIF-oriented tier: clockwise
/// by `degrees`; anything but 90/180/270 (after normalising) leaves the image as it is.
/// Rotation by a multiple of 90° resamples nothing (a pure pixel permutation).
pub fn rotate_image(img: DynamicImage, degrees: i64) -> DynamicImage {
    match ((degrees % 360) + 360) % 360 {
        90 => img.rotate90(),
        180 => img.rotate180(),
        270 => img.rotate270(),
        _ => img,
    }
}

/// The persistent thumbnail of a rotated photo (`media::render_image`): JPEG quality 90, the
/// same file the Tauri shell's byte path wrote before #165.
///
/// Stays on `image`'s encoder on purpose (#243): the file is byte-pinned to what the pre-#165
/// path wrote (`tests/media_render_image.rs`), and this is not a cold-preview stage.
pub(crate) fn encode_rotated_jpeg(img: &DynamicImage) -> Result<Vec<u8>, String> {
    let mut out = Cursor::new(Vec::new());
    img.write_with_encoder(JpegEncoder::new_with_quality(&mut out, 90))
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

// --- persistent offline thumbnails -------------------------------------------------------
// The normal disk cache is keyed by path+mtime+size, so it can't be found once the
// original is unreachable (e.g. a photo offloaded to a NAS that's now unmounted). This
// second store is keyed by the photo's identity instead, so a NAS-only photo stays browsable
// offline. It's written whenever a thumbnail is served while the original IS reachable, and
// proactively at offload time.
//
// The identity is the catalog's stable UUID plus the photo's UUID (#258), never a photo id:
// ids are per catalog, so an id-keyed file one catalog kept was shown by every other catalog
// with a photo of that id whose original was unreachable — another photo, indefinitely. Not
// `app::CatalogIdentity` either: that is a per-open handle id, so a file keyed by it would
// never be found after a restart. The pre-#258 id-keyed files ([`STALE_PERSIST_DIR`]) say
// nothing about which catalog wrote them; [`adopt_id_keyed_thumbs`] gives them, once, to the
// catalog opened at start-up, and then removes them.

/// The directory of the offline thumbnails, under the cache's `chairphoto` directory.
const PERSIST_DIR: &str = "persist-v2";

/// `persist`: the offline thumbnails before #258, `<photo id>.jpg`, shared by every catalog
/// with a photo of that id. Migrated by [`adopt_id_keyed_thumbs`], which removes it after.
const STALE_PERSIST_DIR: &str = "persist";

/// What [`adopt_id_keyed_thumbs`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Adopted {
    /// Files copied into the catalog's own store.
    pub copied: usize,
    /// Files whose photo already had a file of its own there (kept: it is at least as new).
    pub kept: usize,
    /// Files with no photo of that id in the catalog, or not regular files (a symlink): not
    /// adopted, and gone with the old directory.
    pub skipped: usize,
}

/// The one-time migration of the pre-#258 offline thumbnails (review fix258 M1), for the
/// catalog opened at start-up: each `persist/<id>.jpg` whose `id` is in `keys` is copied to
/// that photo's own file ([`persistent_thumb_path`]) unless one is there already, and only
/// when every copy has landed is `persist/` removed — so the migration runs once, and one
/// interrupted (a crash, a full disk) leaves `persist/` for the next start to resume. `Ok`
/// with nothing done when there is no `persist/`.
///
/// Which catalog wrote a file is not recorded anywhere. Adopting it into the start-up catalog
/// picks another catalog's photo only when that catalog, sharing this cache, last rendered the
/// id — exactly the tile the pre-#258 build showed in that case — and the next render of the
/// photo's reachable original overwrites it. Dropping the files instead would leave every
/// offloaded photo without a tile until its home volume is back.
///
/// Never through a symlink: a `persist` that is a symlink is refused (`Err`, left alone), an
/// entry that is not a regular file is skipped, and a target directory that is not a real
/// directory is refused. A copy is written to a temporary name beside its target and linked
/// into place without replacing anything (`hard_link`), so a file a render wrote meanwhile
/// is never overwritten and a reader never sees half a file.
///
/// Blocking disk I/O: call it off the UI thread.
pub fn adopt_id_keyed_thumbs(keys: &HashMap<i64, OfflineThumbKey>) -> std::io::Result<Adopted> {
    adopt_id_keyed_thumbs_in(&cache_dir().join("chairphoto"), keys)
}

/// The photo ids of the files in the pre-#258 store, if there is one (`Ok(None)` when not; an
/// `Err` for a symlink in its place). What the caller looks up keys for.
pub fn id_keyed_thumb_ids() -> std::io::Result<Option<Vec<i64>>> {
    let old = cache_dir().join("chairphoto").join(STALE_PERSIST_DIR);
    match std::fs::symlink_metadata(&old) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
        Ok(meta) if !meta.file_type().is_dir() => return Err(not_a_dir(&old)),
        Ok(_) => {}
    }
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(&old)? {
        if let Some(id) = id_keyed_name(&entry?.file_name()) {
            ids.push(id);
        }
    }
    Ok(Some(ids))
}

/// `<id>.jpg` → `id`.
fn id_keyed_name(name: &std::ffi::OsStr) -> Option<i64> {
    name.to_str()?.strip_suffix(".jpg")?.parse().ok()
}

fn not_a_dir(path: &Path) -> std::io::Error {
    std::io::Error::other(format!("{} is not a real directory", path.display()))
}

fn adopt_id_keyed_thumbs_in(root: &Path, keys: &HashMap<i64, OfflineThumbKey>) -> std::io::Result<Adopted> {
    let old = root.join(STALE_PERSIST_DIR);
    match std::fs::symlink_metadata(&old) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Adopted::default()),
        Err(e) => return Err(e),
        Ok(meta) if !meta.file_type().is_dir() => return Err(not_a_dir(&old)),
        Ok(_) => {}
    }
    let store = root.join(PERSIST_DIR);
    let mut done = Adopted::default();
    for entry in std::fs::read_dir(&old)? {
        let entry = entry?;
        let key = id_keyed_name(&entry.file_name()).and_then(|id| keys.get(&id));
        let regular = std::fs::symlink_metadata(entry.path()).is_ok_and(|m| m.file_type().is_file());
        let (Some(key), true) = (key, regular) else {
            done.skipped += 1;
            continue;
        };
        let dir = store.join(key.catalog.hyphenated().to_string());
        for d in [&store, &dir] {
            std::fs::create_dir_all(d)?;
            if !is_own_dir(d) {
                return Err(not_a_dir(d));
            }
        }
        let target = dir.join(format!("{}.jpg", key.photo.hyphenated()));
        if std::fs::symlink_metadata(&target).is_ok() {
            done.kept += 1;
            continue;
        }
        if link_new(&std::fs::read(entry.path())?, &target)? {
            done.copied += 1;
        } else {
            done.kept += 1;
        }
    }
    // Every file is in place: the old store has done its job. Its removal is what records
    // that the migration ran.
    std::fs::remove_dir_all(&old)?;
    Ok(done)
}

/// Write `bytes` to `target` unless something is there: through a temporary file beside it,
/// linked into place (`hard_link` never replaces). `Ok(false)` when `target` already existed.
fn link_new(bytes: &[u8], target: &Path) -> std::io::Result<bool> {
    use std::io::Write as _;
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("thumb");
    let tmp = target.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    let linked = written.and_then(|()| match std::fs::hard_link(&tmp, target) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    });
    let _ = std::fs::remove_file(&tmp);
    linked
}

/// Which photo, of which catalog, an offline thumbnail is kept for: the catalog's own UUID
/// (`catalog::CATALOG_UUID_KEY`, the one in `chairphoto:FaceId` markers) and the photo's
/// (`photos.uuid`). Both are UUIDs by construction ([`Self::new`] parses them), so the path
/// built from them stays inside the store.
///
/// Why the catalog's UUID and not the photo's alone: the kept file is the photo *as that
/// catalog shows it* — its user rotation applied — and catalogs that share a photo UUID (a
/// merge, a bundle import) can rotate it differently. Two catalogs that share both UUIDs are
/// copies of one catalog file (a restored backup, a sync): what one keeps is the other's too.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OfflineThumbKey {
    catalog: uuid::Uuid,
    photo: uuid::Uuid,
}

impl OfflineThumbKey {
    /// The key for `photo_uuid` in the catalog `catalog_uuid`; `None` unless both are UUIDs
    /// (a catalog with no identity minted, a legacy row): such a photo keeps no offline
    /// thumbnail rather than share one.
    pub fn new(catalog_uuid: &str, photo_uuid: &str) -> Option<Self> {
        let catalog = uuid::Uuid::parse_str(catalog_uuid).ok()?;
        let photo = uuid::Uuid::parse_str(photo_uuid).ok()?;
        Some(Self { catalog, photo })
    }
}

/// The directory holding every catalog's offline thumbnails, one directory per catalog UUID.
pub fn persistent_thumb_dir() -> PathBuf {
    cache_dir().join("chairphoto").join(PERSIST_DIR)
}

/// Path of a photo's persistent (identity-keyed) thumbnail.
pub fn persistent_thumb_path(key: &OfflineThumbKey) -> PathBuf {
    persistent_thumb_dir()
        .join(key.catalog.hyphenated().to_string())
        .join(format!("{}.jpg", key.photo.hyphenated()))
}

/// Save (or refresh) a photo's persistent thumbnail. Best-effort.
pub fn save_persistent_thumb(key: &OfflineThumbKey, bytes: &[u8]) {
    let p = persistent_thumb_path(key);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&p, bytes).ok();
}

/// A photo's persistent thumbnail bytes, if one was kept.
pub fn read_persistent_thumb(key: &OfflineThumbKey) -> Option<Vec<u8>> {
    std::fs::read(persistent_thumb_path(key)).ok()
}

/// One catalog's offline thumbnails as the store names them: the catalog's UUID and every
/// photo UUID it has ([`crate::catalog::Catalog::offline_thumb_owner`]).
#[derive(Clone, Debug)]
pub struct OfflineCatalog {
    catalog: uuid::Uuid,
    photos: std::collections::HashSet<uuid::Uuid>,
    /// Whether [`prune_offline_thumbs`] may remove this catalog's files for photos it does
    /// not have ([`Self::keep_orphans`]).
    prune_orphans: bool,
}

impl OfflineCatalog {
    /// `None` unless `catalog` is a UUID; photo values that are not are left out (they keep
    /// no offline thumbnail, [`OfflineThumbKey::new`]).
    pub fn new(catalog: &str, photos: impl IntoIterator<Item = String>) -> Option<Self> {
        let catalog = uuid::Uuid::parse_str(catalog).ok()?;
        let photos = photos.into_iter().filter_map(|p| uuid::Uuid::parse_str(&p).ok()).collect();
        Some(Self { catalog, photos, prune_orphans: true })
    }

    /// The catalog's UUID, as the store names its directory.
    pub fn catalog_uuid(&self) -> uuid::Uuid {
        self.catalog
    }

    /// Keep every file in this catalog's directory, whatever photos it has: another catalog
    /// may share its UUID (a copy of the file), and the photos this one dropped may be that
    /// one's — offline, its only tile (review of release/face-thumbs, LOW 2).
    pub fn keep_orphans(mut self) -> Self {
        self.prune_orphans = false;
        self
    }
}

/// What [`prune_offline_thumbs`] removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// The open catalog's files for photos it no longer has.
    pub files: usize,
    /// Other catalogs' directories, not opened for [`UNOPENED_CATALOG_AGE`].
    pub catalogs: usize,
}

/// How long the open catalog keeps the offline thumbnail of a photo it no longer has (removed,
/// or re-minted under a new UUID) before a prune removes it. A file written since the photo
/// list was read is never that old, so a photo imported meanwhile keeps its file; and a copy
/// of this catalog file (which shares its UUID) gets a month to render its own again.
pub const ORPHAN_AGE: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);

/// How long another catalog's offline thumbnails are kept after it was last opened (its
/// directory's [`OPENED_MARKER`], else its newest file): a year. Conservative on purpose: an
/// archive catalog opened once a year whose photos all sit on an unmounted NAS has nothing
/// else to show, and its files are rewritten on its next open only where an original is
/// reachable. What it frees is a deleted or trial catalog's directory.
pub const UNOPENED_CATALOG_AGE: std::time::Duration = std::time::Duration::from_secs(365 * 24 * 3600);

/// The file a prune writes in the open catalog's directory, whose mtime says when the catalog
/// was last opened.
const OPENED_MARKER: &str = ".opened";

/// Mark the catalog `catalog_uuid`'s offline thumbnails as in use now (its directory's
/// [`OPENED_MARKER`]), when it has any: a catalog opened by a switch, not at start-up, is not
/// pruned then, but must not look abandoned to the next start's [`prune_offline_thumbs`].
/// Best-effort.
pub fn mark_offline_catalog_opened(catalog_uuid: &str) {
    let Ok(catalog) = uuid::Uuid::parse_str(catalog_uuid) else { return };
    let dir = persistent_thumb_dir().join(catalog.hyphenated().to_string());
    if is_own_dir(&dir) {
        let _ = write_opened_marker(&dir);
    }
}

/// Write `dir`'s [`OPENED_MARKER`] afresh: whatever is at the name is removed first — a
/// symlink as the link itself, never its target — and the marker is made with `create_new`,
/// which does not follow a symlink planted meanwhile (review of release/face-thumbs, LOW 1:
/// a plain write followed one and truncated its target). Another writer marking it at the
/// same moment is as good (`AlreadyExists` is success).
fn write_opened_marker(dir: &Path) -> std::io::Result<()> {
    let marker = dir.join(OPENED_MARKER);
    match std::fs::remove_file(&marker) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&marker) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Clean up the offline thumbnail store (review of #258, N2) for the catalog `open`, opened
/// now: its files for photos it no longer has, older than [`ORPHAN_AGE`]; and the directories
/// of other catalogs not opened for [`UNOPENED_CATALOG_AGE`]. Marks `open`'s directory as
/// opened now. Never follows a symlink, and touches only names the store writes (UUID
/// directories, `<uuid>.jpg` files). **Blocking** (walks the store): off the UI thread.
pub fn prune_offline_thumbs(open: &OfflineCatalog) -> std::io::Result<Pruned> {
    prune_offline_thumbs_in(&persistent_thumb_dir(), open, std::time::SystemTime::now())
}

fn prune_offline_thumbs_in(store: &Path, open: &OfflineCatalog, now: std::time::SystemTime) -> std::io::Result<Pruned> {
    use std::time::SystemTime;
    let mut pruned = Pruned::default();
    if !is_own_dir(store) {
        return Ok(pruned);
    }
    let older_than = |at: SystemTime, age| now.duration_since(at).is_ok_and(|d| d > age);
    let own = store.join(open.catalog.hyphenated().to_string());
    if is_own_dir(&own) {
        write_opened_marker(&own)?;
    }
    if open.prune_orphans && is_own_dir(&own) {
        for entry in std::fs::read_dir(&own)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(photo) = name.to_str().and_then(|n| n.strip_suffix(".jpg")).and_then(|n| uuid::Uuid::parse_str(n).ok())
            else {
                continue;
            };
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else { continue };
            if meta.file_type().is_file()
                && !open.photos.contains(&photo)
                && meta.modified().is_ok_and(|m| older_than(m, ORPHAN_AGE))
                && std::fs::remove_file(entry.path()).is_ok()
            {
                pruned.files += 1;
            }
        }
    }
    for entry in std::fs::read_dir(store)? {
        let entry = entry?;
        let Some(catalog) = entry.file_name().to_str().and_then(|n| uuid::Uuid::parse_str(n).ok()) else { continue };
        let dir = entry.path();
        if catalog == open.catalog || !is_own_dir(&dir) {
            continue;
        }
        let modified = |p: &Path| std::fs::symlink_metadata(p).and_then(|m| m.modified()).ok();
        // When it was last opened: its marker, else (a directory from before markers) the
        // newest of the directory and its files.
        let last = match modified(&dir.join(OPENED_MARKER)) {
            Some(at) => Some(at),
            None => std::fs::read_dir(&dir)?
                .filter_map(|e| e.ok().and_then(|e| modified(&e.path())))
                .chain(modified(&dir))
                .max(),
        };
        // Read the marker again just before removing: a catalog switch may have marked it
        // since (`mark_offline_catalog_opened`), and an opened catalog keeps its files.
        let still_unopened = || modified(&dir.join(OPENED_MARKER)).is_none_or(|at| older_than(at, UNOPENED_CATALOG_AGE));
        if last.is_some_and(|at| older_than(at, UNOPENED_CATALOG_AGE))
            && still_unopened()
            && std::fs::remove_dir_all(&dir).is_ok()
        {
            pruned.catalogs += 1;
        }
    }
    Ok(pruned)
}

/// Generate a thumbnail from `path` and persist it under `key` — called at offload time so
/// the grid keeps an image after the original leaves local disk.
pub fn ensure_persistent_thumb(key: &OfflineThumbKey, path: &Path) -> Result<(), String> {
    let bytes = thumbnail_bytes(path)?;
    save_persistent_thumb(key, &bytes);
    Ok(())
}

/// JPEG bytes for a large loupe preview (cached).
pub fn preview_bytes(path: &Path) -> Result<Vec<u8>, String> {
    cached(path, PREVIEW)
}

/// The pixel size of `path`'s cached 2048 px preview ([`preview_bytes`], the oriented image the
/// faces indexer detects on), read from the cached file's header. `None` when it is not
/// cached — nothing is generated — or its header cannot be read. The face-region writer
/// cross-checks the recorded frame against it (#154).
///
/// Until the current preview is generated, the pre-#245 one stands in: its file in the old
/// `p2048v5` directory if that is still there, else the size [`cleanup_stale_caches`] kept of
/// it before removing the directory. The cross-check compares aspects only, and an upscaled
/// preview has its decode's aspect (to a pixel of rounding on a 2048 px edge), so the old
/// size says what the new one will; it is also the frame the faces indexed before #245 were
/// found on. Without it the version bump would skip the cross-check for every photo whose
/// preview had not been regenerated yet.
pub fn cached_preview_size(path: &Path) -> Option<(u32, u32)> {
    let cache_path = cache_path_for(path, PREVIEW).ok()?;
    if let Some(size) = image_size(&cache_path) {
        return Some(size);
    }
    let name = cache_path.file_name()?.to_str()?;
    let root = cache_dir().join("chairphoto");
    let old_dir = root.join(STALE_PREVIEW_DIR);
    // Same check as `cleanup_stale_caches`: a symlinked old directory is never followed,
    // here either — read its own real directory only, never through a link (review Nit-1).
    is_own_dir(&old_dir)
        .then(|| regular_file_size(&old_dir.join(name)))
        .flatten()
        .or_else(|| stale_preview_sizes(&root)?.get(name).copied())
}

/// The pixel size in an image file's header, or `None`.
fn image_size(file: &Path) -> Option<(u32, u32)> {
    ImageReader::open(file).ok()?.with_guessed_format().ok()?.into_dimensions().ok()
}

/// [`image_size`] of a regular file only — never through a symlink, never a FIFO — for the
/// old cache files ChairPhoto no longer writes.
fn regular_file_size(file: &Path) -> Option<(u32, u32)> {
    std::fs::symlink_metadata(file).ok().filter(|m| m.file_type().is_file())?;
    image_size(file)
}

/// Whether a decoded JPEG (e.g. a cached thumbnail) is effectively grayscale (B&W).
/// Samples a grid of pixels and reports grayscale when almost none show meaningful
/// colour — robust to JPEG chroma noise and a few stray coloured pixels. This is the
/// reliable monochrome signal (camera "B&W" flags lie on some bodies).
pub fn is_grayscale_jpeg(jpeg: &[u8]) -> bool {
    let Ok(img) = image::load_from_memory(jpeg) else {
        return false;
    };
    let rgb = img.to_rgb8();
    let (w, h) = rgb.dimensions();
    if w == 0 || h == 0 {
        return false;
    }
    // Sample ~64×64 points regardless of size.
    let step_x = (w / 64).max(1);
    let step_y = (h / 64).max(1);
    let mut sampled = 0u32;
    let mut coloured = 0u32;
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            let p = rgb.get_pixel(x, y).0;
            let chroma = p[0].max(p[1]).max(p[2]) - p[0].min(p[1]).min(p[2]);
            sampled += 1;
            if chroma > 18 {
                coloured += 1;
            }
            x += step_x;
        }
        y += step_y;
    }
    sampled > 0 && (coloured as f32 / sampled as f32) < 0.01
}

/// JPEG bytes for full-resolution zoom — the embedded preview at native size and
/// high quality, for pixel-peeping focus/sharpness in the loupe (cached).
pub fn zoom_bytes(path: &Path) -> Result<Vec<u8>, String> {
    cached(path, ZOOM)
}

/// Serve one cache size, generating it on a miss.
///
/// Grid-pool latency guard: a lone small-size request must not pay for a larger
/// decode. So on a miss we extract an embedded preview sized for exactly this size
/// (RAW: the smallest preview ≥ `size.max`) and decode once — no over-decode. But the
/// single decode is not wasted: because a larger tier's decode is a strict superset of
/// what the smaller tiers need, whenever we *do* decode for a size we opportunistically
/// derive and cache the smaller tiers too (see `generate_from_decode`). The next
/// smaller-size request is then a cache hit — free.
fn cached(path: &Path, size: Size) -> Result<Vec<u8>, String> {
    let cache_path = cache_path_for(path, size)?;
    if let Ok(bytes) = std::fs::read(&cache_path) {
        return Ok(bytes);
    }
    // Decode once for this size. The decode we just paid for is a strict superset of
    // every smaller tier, so derive and cache those too — a lone thumb request stays a
    // thumb decode (no over-decode), but a preview/zoom decode also fills the smaller
    // tiers so the next grid request is a free cache hit.
    let probe = probe_colour_space_beside(path);
    let img = extract_and_decode(path, size.max)?;
    if let Some(probe) = probe {
        let _ = probe.join();
    }
    run_analyzers(&img, path, size.max);
    let bytes = generate_from_decode(path, &img, size)?;
    for smaller in smaller_sizes(size.max) {
        let cp = cache_path_for(path, smaller)?;
        if !cp.exists() {
            // Best-effort: an opportunistic derive must never fail the request it rode in on.
            let _ = generate_from_decode(path, &img, smaller);
        }
    }
    Ok(bytes)
}

/// The cache sizes strictly smaller than `max`, largest first — the tiers a decode for
/// `max` can derive for free.
fn smaller_sizes(max: u32) -> Vec<Size> {
    [ZOOM, PREVIEW, THUMB]
        .into_iter()
        .filter(|s| s.max < max)
        .collect()
}

/// Generate every cache size for a photo in a single extraction + decode: extract the
/// largest embedded preview once, decode once, run analyzers once, then downscale that
/// one image into every requested tier (largest → smallest). This is the bulk path
/// (cache warming, import) where all sizes are wanted — it reads the RAW off the NAS
/// once instead of once per size. Missing tiers are (re)generated; existing ones are
/// left as-is. Returns nothing; the sizes land in the on-disk cache.
pub fn warm_all_sizes(path: &Path) -> Result<(), String> {
    let sizes = [ZOOM, PREVIEW, THUMB];
    // If every size is already cached, there is nothing to decode (and no hook to fire).
    let mut missing = Vec::new();
    for s in sizes {
        let cp = cache_path_for(path, s)?;
        if !cp.exists() {
            missing.push(s);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    // Decode once at the largest needed size; the smaller tiers downscale from it.
    let largest = missing.iter().map(|s| s.max).max().unwrap();
    let probe = probe_colour_space_beside(path);
    let img = extract_and_decode(path, largest)?;
    if let Some(probe) = probe {
        let _ = probe.join();
    }
    run_analyzers(&img, path, largest);
    for s in missing {
        generate_from_decode(path, &img, s)?;
    }
    Ok(())
}

/// Extract an embedded preview / poster frame / decoded raster sized for `max`, decode
/// it, and apply orientation — producing the full oriented image (no downscale to the
/// target yet). This is the one expensive step (network read + decode) we want to do
/// once per photo when generating multiple sizes.
fn extract_and_decode(path: &Path, max: u32) -> Result<DynamicImage, String> {
    // RAW: extract an embedded preview sized for the target; its pixels are in
    // sensor (unrotated) orientation, so apply the RAW file's own EXIF orientation.
    // Raster: decode the file and use the orientation the decoder reports.
    let plain_raster = !is_raw(path) && !is_video(path) && !is_heic(path);
    let (source, raw_orientation) = if is_raw(path) {
        (extract_raw_preview(path, max)?, Some(exif_orientation(path)))
    } else if is_video(path) {
        // Video: grab a poster frame with ffmpeg; the rest of the pipeline resizes it.
        (extract_video_frame(path)?, None)
    } else if is_heic(path) {
        // HEIF/HEIC (iPhone): the `image` crate can't decode it, so convert to an upright
        // JPEG via ImageMagick's libheif delegate. -auto-orient bakes EXIF orientation into
        // the pixels, so downstream treats it as already-oriented (NoTransforms).
        (decode_via_magick(path, max)?, Some(Orientation::NoTransforms))
    } else {
        (std::fs::read(path).map_err(|e| e.to_string())?, None)
    };

    match decode_oriented(&source, raw_orientation) {
        Ok(img) => Ok(img),
        // The `image` crate gave up — a format it doesn't know (PSD, …) or a damaged
        // file (e.g. a JPEG with a corrupt SOF header). ImageMagick is both more
        // format-complete and more damage-tolerant, so give it one shot before failing.
        // Plain rasters only: RAW/video/HEIC sources already came from an external tool.
        Err(e) if plain_raster => {
            let rescued = decode_via_magick(path, max)
                .map_err(|magick_err| format!("{e}; magick fallback: {magick_err}"))?;
            decode_oriented(&rescued, Some(Orientation::NoTransforms))
        }
        Err(e) => Err(e),
    }
}

/// Downscale one already-decoded image into cache size `size`, encode it, and write the
/// cache file. Returns the encoded JPEG bytes. Shared by the single-size path and the
/// warm-all chain so both derive identically from one decode.
fn generate_from_decode(path: &Path, img: &DynamicImage, size: Size) -> Result<Vec<u8>, String> {
    let bytes = encode_size(path, img, size)?;
    let cache_path = cache_path_for(path, size)?;
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&cache_path, &bytes).ok();
    Ok(bytes)
}

/// Downscale a decoded image to `size` and JPEG-encode it (with Adobe-RGB→sRGB when the
/// source file is Adobe RGB). Pure — no disk writes.
fn encode_size(path: &Path, img: &DynamicImage, size: Size) -> Result<Vec<u8>, String> {
    // A tier only ever shrinks: an image that already fits is encoded at its own size. (image's
    // `thumbnail` fits the image to the box both ways, so the 10 000 px zoom tier used to
    // blow a 6000 px decode up to 10 000 px — a 67 MP JPEG that took seconds to encode and a
    // 267 MB texture, for no more detail, #168.) `downscale::thumbnail` is image's
    // `thumbnail`, byte for byte, without its per-pixel overhead.
    let fits = img.width() <= size.max && img.height() <= size.max;
    let resized = if fits { std::borrow::Cow::Borrowed(img) } else { std::borrow::Cow::Owned(downscale::thumbnail(img, size.max)) };
    let mut out = Cursor::new(Vec::new());
    // The webview shows untagged JPEGs as sRGB. Sony shoots Adobe RGB (wider gamut), so
    // an Adobe RGB preview displayed as-is looks dull/desaturated. Convert it to sRGB for
    // display. The original RAW is untouched; the edited export also renders in sRGB
    // (LibRaw `output_color = 1`), so the editor's proxy and the export share a colour
    // space — a prerequisite for the tone match (K1).
    if is_adobe_rgb(path) {
        let mut rgb = resized.to_rgb8();
        adobe_rgb_to_srgb(&mut rgb);
        DynamicImage::ImageRgb8(rgb)
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, size.quality))
            .map_err(|e| e.to_string())?;
    } else {
        resized
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, size.quality))
            .map_err(|e| e.to_string())?;
    }
    Ok(out.into_inner())
}

/// Decode encoded image bytes and apply orientation: the override when given (source
/// pixels whose orientation the decoder can't know — RAW previews, magick output),
/// otherwise whatever the decoder reports.
fn decode_oriented(
    source: &[u8],
    orientation_override: Option<Orientation>,
) -> Result<DynamicImage, String> {
    let mut decoder = ImageReader::new(Cursor::new(source))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_decoder()
        .map_err(|e| e.to_string())?;
    let orientation = match orientation_override {
        Some(o) => o,
        None => decoder.orientation().unwrap_or(Orientation::NoTransforms),
    };
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// Start [`is_adobe_rgb`] for `path` on its own thread, for a caller about to decode: the
/// probe is an `exiftool` run (~75 ms, a quarter of a cold 24 MP preview) that the encode
/// after the decode needs, and the two need not wait for each other. Join it before encoding;
/// its answer is in `is_adobe_rgb`'s memo by then (#168). A probe that cannot start leaves
/// the encode to run it, as before.
fn probe_colour_space_beside(path: &Path) -> Option<std::thread::JoinHandle<()>> {
    let path = path.to_path_buf();
    std::thread::Builder::new()
        .name("colour-space-probe".into())
        .spawn(move || {
            is_adobe_rgb(&path);
        })
        .ok()
}

/// Whether a file's color space is Adobe RGB (Sony tags this as ColorSpace=Uncalibrated
/// + InteroperabilityIndex R03). Read in-process from EXIF (exiftool only as a fallback, #243)
/// and memoized per path, since
/// `generate` runs up to 3× per image (thumb/preview/zoom).
fn is_adobe_rgb(path: &Path) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock() {
        if let Some(&v) = map.get(path) {
            return v;
        }
    }
    let detected = detect_adobe_rgb(path);
    if let Ok(mut map) = cache.lock() {
        map.insert(path.to_path_buf(), detected);
    }
    detected
}

/// How much of a file's head holds the EXIF a colour-space read needs. A JPEG keeps it in
/// an APP1 segment within the first 64 KiB; a TIFF-based RAW (ARW, DNG, NEF, …) keeps its
/// IFDs near the front, ahead of the strips. A head too short for the IFDs it points to is
/// an error from the parser, which sends [`detect_adobe_rgb`] to exiftool, so the read can
/// only miss a tag, never invent one (#243).
const COLOUR_SPACE_HEAD: u64 = 4 << 20;

fn detect_adobe_rgb(path: &Path) -> bool {
    match adobe_rgb_in_process(path) {
        Some(v) => v,
        None => detect_adobe_rgb_exiftool(path),
    }
}

/// Read ColorSpace and InteroperabilityIndex from the file's EXIF in-process (#243), the same
/// two tags `exiftool -ColorSpace -InteropIndex` reported: Adobe RGB is `ColorSpace = 2` or
/// an interoperability index of `R03` (Sony tags `ColorSpace = Uncalibrated` + `R03`).
/// `None` when the file has no readable EXIF container (the caller asks exiftool, which also
/// knows formats the parser does not).
fn adobe_rgb_in_process(path: &Path) -> Option<bool> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut head = Vec::new();
    file.take(COLOUR_SPACE_HEAD).read_to_end(&mut head).ok()?;
    let exif = exif::Reader::new().read_from_container(&mut Cursor::new(&head)).ok()?;
    Some(adobe_rgb_from_exif(&exif))
}

fn adobe_rgb_from_exif(exif: &exif::Exif) -> bool {
    let colour_space = exif
        .get_field(exif::Tag::ColorSpace, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0));
    let interop = exif.get_field(exif::Tag::InteroperabilityIndex, exif::In::PRIMARY).map(|f| match &f.value {
        exif::Value::Ascii(v) => v.iter().flatten().map(|&b| b as char).collect::<String>(),
        _ => String::new(),
    });
    colour_space == Some(2) || interop.is_some_and(|i| i.contains("R03"))
}

fn detect_adobe_rgb_exiftool(path: &Path) -> bool {
    let output = Command::new("exiftool")
        .args(["-s3", "-ColorSpace", "-InteropIndex"])
        .arg(path)
        .output();
    if let Ok(out) = output {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            return s.contains("Adobe RGB") || s.contains("R03");
        }
    }
    false
}

/// Convert an Adobe RGB (1998) image to sRGB in place. Linearize with Adobe's gamma,
/// apply the Adobe-RGB→sRGB linear matrix, then re-apply the sRGB transfer curve.
fn adobe_rgb_to_srgb(img: &mut RgbImage) {
    const ADOBE_GAMMA: f32 = 2.199_218_8; // 2 + 51/256
    let srgb_encode = |c: f32| -> f32 {
        let c = c.clamp(0.0, 1.0);
        if c <= 0.003_130_8 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    for px in img.pixels_mut() {
        let ar = (px[0] as f32 / 255.0).powf(ADOBE_GAMMA);
        let ag = (px[1] as f32 / 255.0).powf(ADOBE_GAMMA);
        let ab = (px[2] as f32 / 255.0).powf(ADOBE_GAMMA);
        // Adobe RGB linear → sRGB linear (D65). Off-diagonals are ~0 for R←R, G←G.
        let sr = 1.398_25 * ar - 0.398_25 * ag;
        let sg = ag;
        let sb = -0.042_93 * ag + 1.042_93 * ab;
        px[0] = (srgb_encode(sr) * 255.0).round() as u8;
        px[1] = (srgb_encode(sg) * 255.0).round() as u8;
        px[2] = (srgb_encode(sb) * 255.0).round() as u8;
    }
}

/// Extract an embedded preview JPEG from a RAW file.
///
/// Fast path: exiv2 (excellent for Sony ARW — returns JPEG previews including the
/// full-resolution one). Adobe DNG, however, stores its previews as JPEG-compressed
/// TIFF that the image crate can't decode, so when exiv2 returns non-JPEG bytes we
/// fall back to exiftool, which yields clean JPEG for both formats (just slower).
/// Extract a poster frame (JPEG bytes) from a video with ffmpeg. Tries ~1s in (skips black
/// intros); falls back to the first frame for very short clips.
fn extract_video_frame(path: &Path) -> Result<Vec<u8>, String> {
    let grab = |seek: &str| -> Option<Vec<u8>> {
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-ss", seek, "-i"])
            .arg(path)
            .args(["-frames:v", "1", "-an", "-f", "mjpeg", "pipe:1"])
            .output()
            .ok()?;
        if out.status.success() && !out.stdout.is_empty() {
            Some(out.stdout)
        } else {
            None
        }
    };
    grab("1")
        .or_else(|| grab("0"))
        .ok_or_else(|| format!("ffmpeg could not extract a frame from {}", path.display()))
}

/// HEIF/HEIC container (iPhone photos). The `image` crate can't decode these; we route
/// them through ImageMagick's libheif delegate instead (see `decode_via_magick`), which turns
/// them by their container's `irot`/`imir` — the rule the face-region frame follows for the
/// same files (`metadata::heif`, #154), so it is one rule.
fn is_heic(path: &Path) -> bool {
    crate::metadata::heif::is_heif(path)
}

/// Decode an image file to JPEG bytes via ImageMagick (`magick`), upright (EXIF
/// orientation baked in) and downscaled to `max` on the long edge, converted to sRGB for
/// correct on-screen colour (iPhone HEIC is usually Display P3). The primary route for
/// HEIC/HEIF (needs ImageMagick built with the libheif delegate) and the rescue route for
/// rasters the `image` crate rejects — formats it doesn't know (PSD) or damaged files
/// magick's decoders tolerate. `[0]` limits multi-layer formats to their first frame
/// (a PSD's flattened composite) so layered files don't emit one JPEG per layer.
fn decode_via_magick(path: &Path, max: u32) -> Result<Vec<u8>, String> {
    let mut input = path.as_os_str().to_owned();
    input.push("[0]");
    let out = Command::new("magick")
        .arg(input)
        .arg("-auto-orient")
        .args(["-resize", &format!("{max}x{max}>")])
        .args(["-colorspace", "sRGB"])
        .args(["-quality", "92"])
        .arg("jpg:-")
        .output()
        .map_err(|e| format!("decode needs ImageMagick (`magick`): {e}"))?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(format!(
            "magick failed to decode {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(out.stdout)
}

fn extract_raw_preview(path: &Path, target_max: u32) -> Result<Vec<u8>, String> {
    if let Ok(bytes) = extract_via_exiv2(path, target_max) {
        if is_jpeg(&bytes) {
            return Ok(bytes);
        }
    }
    extract_via_exiftool(path, target_max)
}

/// exiv2 extraction: choose the smallest preview whose longest edge is >=
/// `target_max` (else the largest), extract it into a unique temp dir, read it back.
/// exiv2's exit status is NOT trusted — on some DNGs it prints a non-fatal maker-
/// note warning and exits non-zero yet still writes the file — so we rely on the
/// presence of an output file instead.
fn extract_via_exiv2(path: &Path, target_max: u32) -> Result<Vec<u8>, String> {
    let index = choose_preview_index(path, target_max)?;
    let tmp = unique_tmp_dir(path);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;

    let result = (|| {
        let output = Command::new("exiv2")
            .arg(format!("-ep{index}"))
            .arg("-l")
            .arg(&tmp)
            .arg(path)
            .output()
            .map_err(|e| format!("exiv2 not available: {e}"))?;
        read_only_image_in(&tmp).map_err(|e| {
            let stderr = String::from_utf8_lossy(&output.stderr);
            format!("{e} (exiv2: {})", stderr.trim())
        })
    })();

    std::fs::remove_dir_all(&tmp).ok();
    result
}

/// exiftool fallback: pull a JPEG preview straight to stdout. Picks the full-size
/// `JpgFromRaw` for large targets and the smaller `PreviewImage` otherwise.
fn extract_via_exiftool(path: &Path, target_max: u32) -> Result<Vec<u8>, String> {
    let tags: &[&str] = if target_max > 1024 {
        &["-JpgFromRaw", "-PreviewImage", "-ThumbnailImage"]
    } else {
        &["-PreviewImage", "-JpgFromRaw", "-ThumbnailImage"]
    };
    for tag in tags {
        let output = Command::new("exiftool")
            .args(["-b", tag])
            .arg(path)
            .output()
            .map_err(|e| format!("exiftool not available: {e}"))?;
        if output.status.success() && is_jpeg(&output.stdout) {
            return Ok(output.stdout);
        }
    }
    Err(format!("No decodable preview found in {}", path.display()))
}

/// JPEG files start with the SOI marker 0xFFD8.
fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == 0xff && bytes[1] == 0xd8
}

/// Parse `exiv2 -pp` and pick the preview index. Returns the smallest preview with
/// a long edge >= `target_max`, else the largest available.
fn choose_preview_index(path: &Path, target_max: u32) -> Result<u32, String> {
    let output = Command::new("exiv2")
        .arg("-pp")
        .arg(path)
        .output()
        .map_err(|e| format!("exiv2 not available: {e}"))?;
    let listing = String::from_utf8_lossy(&output.stdout);

    // Collect (index, long_edge) for each "Preview N: ..., WxH pixels, ..." line.
    let mut previews: Vec<(u32, u32)> = Vec::new();
    for line in listing.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("Preview ") else {
            continue;
        };
        let Some((idx_str, after)) = rest.split_once(':') else {
            continue;
        };
        let Ok(index) = idx_str.trim().parse::<u32>() else {
            continue;
        };
        if let Some(dims) = after.split_once(" pixels").and_then(|(d, _)| {
            d.rsplit(',').next().map(str::trim)
        }) {
            if let Some((w, h)) = dims.split_once('x') {
                if let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>()) {
                    previews.push((index, w.max(h)));
                }
            }
        }
    }

    if previews.is_empty() {
        return Err(format!("No embedded preview found in {}", path.display()));
    }
    previews.sort_by_key(|&(_, edge)| edge);
    let chosen = previews
        .iter()
        .find(|&&(_, edge)| edge >= target_max)
        .or_else(|| previews.last())
        .unwrap();
    Ok(chosen.0)
}

/// Read the single preview file exiv2 wrote into `dir`. Since we extract exactly
/// one preview index, there is at most one file; we read it regardless of its
/// extension (it may be .jpg or .tif depending on the RAW format).
fn read_only_image_in(dir: &Path) -> Result<Vec<u8>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_file() {
            return std::fs::read(&p).map_err(|e| e.to_string());
        }
    }
    Err("exiv2 produced no preview file".into())
}

/// Read the EXIF Orientation (1–8) of a file via exiv2, mapped to the image
/// crate's `Orientation`. Defaults to no transform when unavailable.
pub(crate) fn exif_orientation(path: &Path) -> Orientation {
    let output = Command::new("exiv2")
        .args(["-g", "Exif.Image.Orientation", "-Pv"])
        .arg(path)
        .output();
    if let Ok(out) = output {
        if out.status.success() {
            if let Ok(n) = String::from_utf8_lossy(&out.stdout).trim().parse::<u8>() {
                if let Some(o) = Orientation::from_exif(n) {
                    return o;
                }
            }
        }
    }
    Orientation::NoTransforms
}

/// Cache file path: <cache_dir>/chairphoto/<tag><max>v<version>/<hash>.jpg, where
/// the hash covers path + mtime + size so edits invalidate the cache, and `version` is
/// `size`'s own cache-directory version (shared [`CACHE_VERSION`] for thumb/preview,
/// [`ZOOM_VERSION`] for zoom).
fn cache_path_for(path: &Path, size: Size) -> Result<PathBuf, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let key = format!("{}|{}|{}", path.display(), mtime, meta.len());
    let hash = fnv1a(&key);

    let base = cache_dir()
        .join("chairphoto")
        .join(format!("{}{}v{}", size.tag, size.max, size.version));
    Ok(base.join(format!("{hash:016x}.jpg")))
}

/// Cache directories older builds wrote and nothing reads any more, by their fixed names under
/// `<cache_dir>/chairphoto`, kept only so [`cleanup_stale_caches`] can find them.
///
/// `z10000v5`: zoom before `ZOOM_VERSION` split off from `CACHE_VERSION` (#168), upscaled to
/// 10 000 px for any original smaller than that.
const STALE_ZOOM_DIR: &str = "z10000v5";
/// `t512v5`: thumbnails before `CACHE_VERSION` 6 (#245), upscaled for originals under 512 px.
const STALE_THUMB_DIR: &str = "t512v5";
/// `p2048v5`: previews before `CACHE_VERSION` 6 (#245), upscaled for originals under 2048 px.
const STALE_PREVIEW_DIR: &str = "p2048v5";
/// `cover512v1`: cover thumbnails (`plugins::edit::cover`) before its `COVER_FORMAT` 2 (#245),
/// rendered from those upscaled previews, so enlarged for originals under 512 px.
const STALE_COVER_DIR: &str = "cover512v1";
/// What [`cleanup_stale_caches`] keeps of [`STALE_PREVIEW_DIR`] for [`cached_preview_size`]:
/// one `<file name> <width> <height>` line per old preview.
const STALE_PREVIEW_SIZES: &str = "p2048v5.sizes";

/// One-time, best-effort removal of the cache directories older builds left behind:
/// [`STALE_ZOOM_DIR`] (#168), [`STALE_THUMB_DIR`], [`STALE_PREVIEW_DIR`] and
/// [`STALE_COVER_DIR`] (#245). (Not the pre-#258 offline thumbnails, [`STALE_PERSIST_DIR`]:
/// those are migrated first, by [`adopt_id_keyed_thumbs`].) Nothing reads their images any
/// more, so removing them only reclaims disk space — except that the face-region writer's cross-check still wants the old
/// previews' pixel sizes until each photo's preview is regenerated ([`cached_preview_size`]).
/// Those are written first, to [`STALE_PREVIEW_SIZES`] (through a temporary file renamed into
/// place), and the preview directory is removed only once they have landed; if they cannot be
/// kept the directory stays for the next start.
///
/// Safe by construction: every path is this process's own `cache_dir()` joined with a fixed
/// literal, never anything caller-supplied, and a directory is removed only if that exact
/// path is a real directory — in particular never a symlink (checked with
/// [`std::fs::symlink_metadata`], which does not follow it), so a symlink planted at that
/// name is left untouched rather than followed. `fs::remove_dir_all` itself does not follow
/// symlinks it finds inside the tree either, and the size pass reads only regular files
/// named as cache files, so no entry under a directory can redirect anything elsewhere. A
/// missing directory or any I/O error is silently a no-op: this never panics or reports a
/// failure.
///
/// Call off the UI thread — it is disk I/O (a header read per old preview) and nothing waits
/// on it (`app::boot_with` spawns it on its own thread).
pub fn cleanup_stale_caches() {
    let root = cache_dir().join("chairphoto");
    remove_own_dir(&root.join(STALE_ZOOM_DIR));
    remove_own_dir(&root.join(STALE_THUMB_DIR));
    remove_own_dir(&root.join(STALE_COVER_DIR));
    let previews = root.join(STALE_PREVIEW_DIR);
    if !is_own_dir(&previews) {
        return;
    }
    // Two processes sharing this cache (two data dirs, one cache: `single_instance` does not
    // keep them apart) take turns here, the turn held until the old directory is gone: else
    // one could read the sizes file before the other renamed its own over it, list the old
    // directory while the other removes it, and rename a smaller set over the full one (review
    // fix245b, INFO). With no turn to be had, nothing is done: the old directory stays for
    // the next start.
    let Ok(_turn) = preview_sizes_turn(&root) else { return };
    if is_own_dir(&previews) && keep_stale_preview_sizes(&root, &previews).is_ok() {
        remove_own_dir(&previews);
    }
}

/// The lock file whose `flock` [`cleanup_stale_caches`]'s preview-size pass holds.
const STALE_PREVIEW_SIZES_LOCK: &str = "p2048v5.sizes.lock";

/// Wait for, then hold, the preview-size pass's turn: an exclusive `flock` on
/// [`STALE_PREVIEW_SIZES_LOCK`] under `root`, released when the file is dropped. `Err` when
/// the lock file is not a regular file of ours (a symlink is never opened) or cannot be locked.
fn preview_sizes_turn(root: &Path) -> std::io::Result<std::fs::File> {
    let path = root.join(STALE_PREVIEW_SIZES_LOCK);
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        if !meta.file_type().is_file() {
            return Err(std::io::Error::other("the preview-size lock is not a regular file"));
        }
    }
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
    file.lock()?;
    Ok(file)
}

/// Whether `dir` is a real directory, not a symlink to one.
fn is_own_dir(dir: &Path) -> bool {
    matches!(std::fs::symlink_metadata(dir), Ok(meta) if meta.file_type().is_dir())
}

/// Remove `dir` if it [`is_own_dir`]; errors are ignored.
fn remove_own_dir(dir: &Path) {
    if is_own_dir(dir) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Whether `name` is a cache file's ([`cache_path_for`]): 16 lowercase hex digits and `.jpg`.
fn is_cache_file_name(name: &str) -> bool {
    name.strip_suffix(".jpg")
        .is_some_and(|hash| hash.len() == 16 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Write the pixel size of every preview in `previews` to [`STALE_PREVIEW_SIZES`] under
/// `root`, merged with what an earlier, interrupted pass kept. A preview whose header cannot
/// be read is left out, as [`cached_preview_size`] could not read it either. `Err` when the
/// sizes did not land.
fn keep_stale_preview_sizes(root: &Path, previews: &Path) -> std::io::Result<()> {
    use std::io::Write;
    sweep_stale_tmp_sizes_files(root);
    let mut sizes = read_stale_preview_sizes(&root.join(STALE_PREVIEW_SIZES));
    for entry in std::fs::read_dir(previews)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().filter(|n| is_cache_file_name(n)).map(str::to_owned) else {
            continue;
        };
        if let Some(size) = regular_file_size(&entry.path()) {
            sizes.insert(name, size);
        }
    }
    let mut lines: Vec<String> = sizes.iter().map(|(name, (w, h))| format!("{name} {w} {h}\n")).collect();
    lines.sort();
    // A name unique to this attempt — this process's id plus a per-process nonce — so two
    // processes sharing one cache dir (`single_instance` is keyed per app *data* dir, not
    // cache dir, so two XDG_DATA_HOMEs with one default cache can run this at once, #245
    // review LOW-2) never share a tmp name and so can never interleave through it:
    // `create_new` claims a name nothing else has, and only this attempt writes to or renames
    // it. Since the pass holds its turn (`preview_sizes_turn`), only a build from before the
    // turn can sweep it mid-write (`sweep_stale_tmp_sizes_files`); then this rename fails,
    // this attempt returns `Err`, and `p2048v5` is kept for the next start — fail-safe,
    // never a partial sizes file.
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let tmp = root.join(format!("{STALE_PREVIEW_SIZES}.{}.{nonce}.tmp", std::process::id()));
    // Only a name this attempt created is ever removed: a `create_new` that fails left
    // whatever holds the name alone.
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let write = (|| -> std::io::Result<()> {
        file.write_all(lines.concat().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, root.join(STALE_PREVIEW_SIZES))
    })();
    drop(file);
    if write.is_err() {
        // Our own attempt's name: remove what we created rather than leave it for the next
        // sweep, best-effort (a failure here changes nothing — it is still only ever read
        // back as a `.tmp`-suffixed name no code treats as [`STALE_PREVIEW_SIZES`]).
        let _ = std::fs::remove_file(&tmp);
    }
    write
}

/// Best-effort removal of a previous attempt's leftover temporary sizes file under `root`:
/// the fixed name a pre-LOW-2 build used, or one of today's unique `<pid>.<nonce>` ones,
/// left behind by a process that crashed or was killed before its own rename landed.
/// Harmless either way — nothing ever reads a `.tmp`-suffixed name back as
/// [`STALE_PREVIEW_SIZES`] — this only keeps them from accumulating. It matches any
/// `<STALE_PREVIEW_SIZES>.*.tmp` name, not only the `<pid>.<nonce>` shape: it only ever
/// looks inside ChairPhoto's own cache directory, where nothing else writes such names. Never a directory: a
/// name it cannot remove (one planted there instead) is left alone, not traversed into or
/// removed recursively. Never a symlink's target: `remove_file` unlinks the name itself,
/// whatever it points to, never the pointed-to file's content.
fn sweep_stale_tmp_sizes_files(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    let prefix = format!("{STALE_PREVIEW_SIZES}.");
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
        if !(name.starts_with(&prefix) && name.ends_with(".tmp")) {
            continue;
        }
        if matches!(std::fs::symlink_metadata(entry.path()), Ok(meta) if meta.file_type().is_dir()) {
            continue;
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Parse [`STALE_PREVIEW_SIZES`]. Lines that are not `<cache file name> <w> <h>` are skipped.
/// The write is tmp + fsync + rename, so a complete file always ends in `\n`; one that
/// doesn't was truncated mid-write (external damage — disk full, a killed process without
/// our own tmp+rename, a copy cut short) and its last line may be only part of a write, so
/// that line is dropped rather than parsed — fail safe to no size (the cross-check is
/// skipped for that photo) rather than a wrong one. A missing or unreadable file, or one
/// that is not a regular file, is empty.
fn read_stale_preview_sizes(file: &Path) -> HashMap<String, (u32, u32)> {
    let regular = matches!(std::fs::symlink_metadata(file), Ok(meta) if meta.file_type().is_file());
    let text = if regular { std::fs::read_to_string(file).unwrap_or_default() } else { String::new() };
    let complete_lines = if text.is_empty() || text.ends_with('\n') {
        text.as_str()
    } else {
        text.rsplit_once('\n').map_or("", |(head, _)| head)
    };
    complete_lines
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_ascii_whitespace();
            let name = parts.next().filter(|n| is_cache_file_name(n))?;
            let w = parts.next()?.parse().ok()?;
            let h = parts.next()?.parse().ok()?;
            parts.next().is_none().then(|| (name.to_owned(), (w, h)))
        })
        .collect()
}

/// [`STALE_PREVIEW_SIZES`] under `root`, parsed once per version of the file: the faces
/// indexer asks for one photo after another, and the cleanup may write the file after the
/// first ask, so the memo is keyed by the file's path, length and mtime. `None` when there is
/// no such file.
fn stale_preview_sizes(root: &Path) -> Option<Arc<HashMap<String, (u32, u32)>>> {
    type Key = (PathBuf, u64, Option<std::time::SystemTime>);
    static MEMO: Mutex<Option<(Key, Arc<HashMap<String, (u32, u32)>>)>> = Mutex::new(None);
    let file = root.join(STALE_PREVIEW_SIZES);
    let meta = std::fs::symlink_metadata(&file).ok().filter(|m| m.file_type().is_file())?;
    let key = (file, meta.len(), meta.modified().ok());
    let mut memo = MEMO.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((memo_key, sizes)) = memo.as_ref() {
        if *memo_key == key {
            return Some(sizes.clone());
        }
    }
    let sizes = Arc::new(read_stale_preview_sizes(&key.0));
    *memo = Some((key, sizes.clone()));
    Some(sizes)
}

/// A unique temp dir for one extraction, avoiding collisions between concurrent
/// extractions and files that share a stem in different folders.
fn unique_tmp_dir(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let hash = fnv1a(&format!("{}|{n}", path.display()));
    std::env::temp_dir()
        .join("chairphoto-extract")
        .join(format!("{hash:016x}"))
}

/// Resolve the user cache dir (XDG_CACHE_HOME or ~/.cache), with a temp fallback.
pub(crate) fn cache_dir() -> PathBuf {
    // This crate's unit tests: isolated before the first resolution, whichever test it is.
    #[cfg(test)]
    crate::test_home::isolate();
    let dir = cache_dir_from(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"));
    // Tests never use the real one (`test_home`).
    #[cfg(any(test, feature = "test-hooks"))]
    crate::test_home::check(&dir.join("chairphoto"), ".cache", "XDG_CACHE_HOME");
    dir
}

/// [`cache_dir`] from the two variables. An empty or relative `XDG_CACHE_HOME` is ignored,
/// as the XDG Base Directory spec says ("All paths set in these environment variables must
/// be absolute. If an implementation encounters a relative path in any of these variables it
/// should consider the path invalid and ignore it"): it would put the cache wherever the
/// process happened to be started (#245 review, same class as d73791e's library root).
fn cache_dir_from(xdg: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(xdg) = xdg.map(PathBuf::from).filter(|p| p.is_absolute()) {
        return xdg;
    }
    if let Some(home) = home.filter(|h| !h.is_empty()) {
        return PathBuf::from(home).join(".cache");
    }
    std::env::temp_dir()
}

/// Small, dependency-free 64-bit hash for cache file names (not cryptographic).
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in s.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

mod downscale;

#[cfg(test)]
mod bench;
#[cfg(test)]
mod colour_space_tests;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn adobe_to_srgb_keeps_neutral_gray() {
        // A neutral gray must stay neutral (and roughly the same value).
        let mut img = RgbImage::from_pixel(2, 2, image::Rgb([128, 128, 128]));
        adobe_rgb_to_srgb(&mut img);
        let p = img.get_pixel(0, 0).0;
        assert_eq!(p[0], p[1], "stays neutral (R=G)");
        assert_eq!(p[1], p[2], "stays neutral (G=B)");
        assert!((p[0] as i32 - 128).abs() <= 4, "gray ~unchanged, got {}", p[0]);
    }

    #[test]
    fn adobe_to_srgb_boosts_saturated_red() {
        // An Adobe RGB red maps to a higher sRGB red value (sRGB needs a bigger number to
        // express the same colour) — i.e. it looks more saturated than the dull as-is view.
        let mut img = RgbImage::from_pixel(2, 2, image::Rgb([200, 100, 100]));
        adobe_rgb_to_srgb(&mut img);
        let p = img.get_pixel(0, 0).0;
        assert!(p[0] > 200, "red channel should increase, got {}", p[0]);
    }

    // --- tiers never upscale (#168) ------------------------------------------

    /// An image smaller than a tier is encoded at its own size — the zoom tier is the
    /// decode's native resolution — while a larger one still shrinks to fit.
    #[test]
    fn a_tier_never_upscales() {
        let path = Path::new("/nonexistent/upscale-check.jpg");
        let dims = |img: &DynamicImage, size: Size| {
            let bytes = encode_size(path, img, size).unwrap();
            let decoded = image::load_from_memory(&bytes).unwrap();
            (decoded.width(), decoded.height())
        };
        let small = DynamicImage::ImageRgb8(RgbImage::from_pixel(600, 400, image::Rgb([90, 120, 150])));
        assert_eq!(dims(&small, ZOOM), (600, 400), "zoom: native");
        assert_eq!(dims(&small, PREVIEW), (600, 400), "preview of a small image: native");
        assert_eq!(dims(&small, THUMB), (512, 341), "thumb: shrunk to fit");
        let tall = DynamicImage::ImageRgb8(RgbImage::from_pixel(300, 3000, image::Rgb([9, 9, 9])));
        assert_eq!(dims(&tall, PREVIEW), (205, 2048), "a long edge over the box still shrinks");
    }

    /// Each tier's cache directory: zoom under its own [`ZOOM_VERSION`], thumb and preview
    /// under the shared [`CACHE_VERSION`] — and none of them a directory an older build wrote
    /// upscaled files into (`z10000v5` before #168, `t512v5`/`p2048v5` before #245), which
    /// must never be read back as current.
    #[test]
    fn each_tier_caches_under_a_directory_no_upscaling_build_wrote() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("zoom-version");
        let tmp = tmp_dir.path().to_path_buf();
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        let img = write_test_jpeg(&tmp, "zoomversion.jpg", 64, 64);

        let zoom_dir = cache_path_for(&img, ZOOM).unwrap().parent().unwrap().file_name().unwrap().to_owned();
        let preview_dir = cache_path_for(&img, PREVIEW).unwrap().parent().unwrap().file_name().unwrap().to_owned();
        let thumb_dir = cache_path_for(&img, THUMB).unwrap().parent().unwrap().file_name().unwrap().to_owned();

        assert_eq!(zoom_dir, format!("z{ZOOM_MAX}v{ZOOM_VERSION}").as_str());
        assert_eq!(preview_dir, format!("p{PREVIEW_MAX}v{CACHE_VERSION}").as_str());
        assert_eq!(thumb_dir, format!("t{THUMB_MAX}v{CACHE_VERSION}").as_str());
        assert_eq!(
            (zoom_dir.to_str().unwrap(), preview_dir.to_str().unwrap(), thumb_dir.to_str().unwrap()),
            ("z10000v6", "p2048v6", "t512v6")
        );
        for stale in [STALE_ZOOM_DIR, STALE_PREVIEW_DIR, STALE_THUMB_DIR] {
            assert!(![&zoom_dir, &preview_dir, &thumb_dir].contains(&&std::ffi::OsString::from(stale)), "{stale}");
        }
    }

    // --- pre-#245 thumbnails and previews regenerate at native size (#245) ---------------
    // Before 2497fa2 a tier fitted every decode to its box both ways, so a small original's
    // thumbnail and preview were enlarged. Those files sit in `t512v5`/`p2048v5` under the
    // same file names the current tiers use (the name hashes path, mtime and length only).

    /// A solid JPEG of `w`×`h` as an old build cached it, at `dir/name`.
    fn plant_old_tier(dir: &Path, name: &std::ffi::OsStr, w: u32, h: u32) {
        std::fs::create_dir_all(dir).unwrap();
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(RgbImage::from_pixel(w, h, image::Rgb([200, 10, 10])))
            .write_with_encoder(JpegEncoder::new_with_quality(&mut bytes, 80))
            .unwrap();
        std::fs::write(dir.join(name), bytes.into_inner()).unwrap();
    }

    /// [`plant_old_tier`] at `img`'s own preview cache name, under [`STALE_PREVIEW_DIR`] in
    /// `cache` (a `XDG_CACHE_HOME`) — for tests outside this module that exercise the real
    /// [`cached_preview_size`] path against a pre-#245 upscaled preview (review Nit-2).
    /// Its only caller today is behind `faces` (`plugins::faces::regions`), which `plugins`
    /// itself compiles out without that feature (`#[cfg(feature = "faces")] pub mod faces;`),
    /// so this is gated the same way rather than warning as dead code without it.
    #[cfg(feature = "faces")]
    pub(crate) fn plant_old_preview_tier(cache: &Path, img: &Path, w: u32, h: u32) {
        let name = cache_path_for(img, PREVIEW).unwrap().file_name().unwrap().to_owned();
        plant_old_tier(&cache.join("chairphoto").join(STALE_PREVIEW_DIR), &name, w, h);
    }

    /// The owner's decision on #245: a small original whose upscaled thumbnail and preview an
    /// old build cached gets them regenerated at its own size — the old files are never served.
    #[test]
    fn an_old_upscaled_thumbnail_and_preview_are_not_served() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("old-upscaled");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        let img = write_test_jpeg(tmp_dir.path(), "small.jpg", 300, 200);
        let name = cache_path_for(&img, PREVIEW).unwrap().file_name().unwrap().to_owned();
        assert_eq!(name, cache_path_for(&img, THUMB).unwrap().file_name().unwrap());
        // What the build before 2497fa2 cached for this 300x200 original.
        plant_old_tier(&cache.join("chairphoto").join(STALE_THUMB_DIR), &name, 512, 341);
        plant_old_tier(&cache.join("chairphoto").join(STALE_PREVIEW_DIR), &name, 2048, 1365);

        let dims = |bytes: Vec<u8>| image::load_from_memory(&bytes).map(|i| (i.width(), i.height())).unwrap();
        assert_eq!(dims(thumbnail_bytes(&img).unwrap()), (300, 200), "thumbnail");
        assert_eq!(dims(preview_bytes(&img).unwrap()), (300, 200), "preview");
    }

    /// The face-region writer's cross-check (#154) keeps the old preview's size until the
    /// photo's preview is regenerated: from the old file while it is there, then from what
    /// the cleanup kept of it, and once regenerated from the new preview — the same aspect
    /// throughout, so the cross-check answers as it did before the bump.
    #[test]
    fn the_preview_size_outlives_the_old_preview_directory() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("old-preview-size");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        let img = write_test_jpeg(tmp_dir.path(), "faces.jpg", 1200, 800);
        let other = write_test_jpeg(tmp_dir.path(), "other.jpg", 600, 400);
        let old_dir = cache.join("chairphoto").join(STALE_PREVIEW_DIR);
        let name = |p: &Path| cache_path_for(p, PREVIEW).unwrap().file_name().unwrap().to_owned();
        plant_old_tier(&old_dir, &name(&img), 2048, 1365);

        assert_eq!(cached_preview_size(&img), Some((2048, 1365)), "from the old file");
        assert_eq!(cached_preview_size(&other), None, "never cached is still unknown");

        cleanup_stale_caches();
        assert!(!old_dir.exists(), "the old preview directory is removed");
        assert_eq!(cached_preview_size(&img), Some((2048, 1365)), "from the kept sizes");
        assert_eq!(cached_preview_size(&other), None);
        assert!(!cache_path_for(&img, PREVIEW).unwrap().exists(), "asking generated nothing");

        preview_bytes(&img).unwrap();
        assert_eq!(cached_preview_size(&img), Some((1200, 800)), "the regenerated preview wins");
    }

    /// An interrupted cleanup (sizes kept, directory partly removed) loses nothing on the next
    /// start: the sizes already kept are merged with the files still there.
    #[test]
    fn a_second_cleanup_keeps_the_sizes_the_first_kept() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("old-preview-merge");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        let a = write_test_jpeg(tmp_dir.path(), "a.jpg", 64, 48);
        let b = write_test_jpeg(tmp_dir.path(), "b.jpg", 64, 48);
        let old_dir = cache.join("chairphoto").join(STALE_PREVIEW_DIR);
        let name = |p: &Path| cache_path_for(p, PREVIEW).unwrap().file_name().unwrap().to_owned();
        plant_old_tier(&old_dir, &name(&a), 2048, 1536);
        cleanup_stale_caches();
        plant_old_tier(&old_dir, &name(&b), 1536, 2048);
        cleanup_stale_caches();
        assert_eq!(cached_preview_size(&a), Some((2048, 1536)));
        assert_eq!(cached_preview_size(&b), Some((1536, 2048)));
    }

    // --- review fix245b: truncated sizes file, racing writers, symlinked old dir ---------

    /// LOW-1: a truncated last line — written by something other than
    /// [`keep_stale_preview_sizes`]'s own tmp+fsync+rename, which always ends the file in
    /// `\n` — parses as no entry for that line, not a wrong one. Dropping the whole line
    /// (not just letting its own parse fail) is what tells apart "the name is cut short
    /// too" (still three whitespace-separated tokens; the digits after a truncated name
    /// would mis-parse as a plausible but wrong size) from a line that is simply absent.
    #[test]
    fn read_stale_preview_sizes_drops_an_unterminated_last_line() {
        let dir = TestTmpDir::new("stale-sizes-truncated");
        let file = dir.path().join("p2048v5.sizes");

        // A complete file (trailing '\n'): every line is trusted.
        std::fs::write(&file, b"0123456789abcdef.jpg 2048 1365\n").unwrap();
        assert_eq!(
            read_stale_preview_sizes(&file),
            HashMap::from([("0123456789abcdef.jpg".to_string(), (2048, 1365))]),
        );

        // The review's own example: one line, truncated, no trailing newline at all.
        std::fs::write(&file, b"0123456789abcdef.jpg 2048 13").unwrap();
        assert_eq!(read_stale_preview_sizes(&file), HashMap::new(), "an unterminated line is never trusted");

        // A complete line followed by a truncated one: the undamaged line still lands,
        // only the damaged tail is dropped.
        std::fs::write(&file, b"0123456789abcdef.jpg 2048 1365\nfedcba9876543210.jpg 2048 13").unwrap();
        assert_eq!(
            read_stale_preview_sizes(&file),
            HashMap::from([("0123456789abcdef.jpg".to_string(), (2048, 1365))]),
            "only the undamaged line is kept"
        );
    }

    /// LOW-2: a leftover temporary sizes file — the fixed name a pre-fix build used, or one
    /// of this fix's own `<pid>.<nonce>` ones — left behind by a process that crashed or was
    /// killed before its rename landed is swept on the next cleanup. Harmless even without
    /// the sweep (nothing ever reads a `.tmp`-suffixed name back as the real sizes file),
    /// but a directory planted at such a name is left alone rather than removed, and a
    /// normal cleanup leaves no `.tmp` file behind at all.
    #[test]
    fn stray_tmp_sizes_files_are_swept_or_left_harmless() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("stale-sizes-stray-tmp");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        let img = write_test_jpeg(tmp_dir.path(), "stray.jpg", 64, 48);
        let root = cache.join("chairphoto");
        let old_dir = root.join(STALE_PREVIEW_DIR);
        let name = cache_path_for(&img, PREVIEW).unwrap().file_name().unwrap().to_owned();
        plant_old_tier(&old_dir, &name, 2048, 1536);

        std::fs::create_dir_all(&root).unwrap();
        // A pre-fix build's fixed name, and a stray unique one from an earlier crashed attempt.
        std::fs::write(root.join(format!("{STALE_PREVIEW_SIZES}.tmp")), b"leftover").unwrap();
        std::fs::write(root.join(format!("{STALE_PREVIEW_SIZES}.4242.7.tmp")), b"leftover too").unwrap();
        // A directory squatting a plausible tmp name: left alone, not traversed into.
        let dir_at_tmp_name = root.join(format!("{STALE_PREVIEW_SIZES}.9999.0.tmp"));
        std::fs::create_dir_all(&dir_at_tmp_name).unwrap();
        std::fs::write(dir_at_tmp_name.join("keep.txt"), b"not ours to remove").unwrap();

        cleanup_stale_caches();

        assert!(!root.join(format!("{STALE_PREVIEW_SIZES}.tmp")).exists(), "the pre-fix fixed name is swept");
        assert!(!root.join(format!("{STALE_PREVIEW_SIZES}.4242.7.tmp")).exists(), "a stray unique temp is swept");
        assert!(dir_at_tmp_name.join("keep.txt").exists(), "a directory at a tmp-shaped name is left alone");
        assert_eq!(cached_preview_size(&img), Some((2048, 1536)), "the real write still landed");
        let any_tmp_file_left = std::fs::read_dir(&root).unwrap().flatten().any(|e| {
            e.file_name().to_str().is_some_and(|n| n.ends_with(".tmp"))
                && !matches!(std::fs::symlink_metadata(e.path()), Ok(meta) if meta.file_type().is_dir())
        });
        assert!(!any_tmp_file_left, "no .tmp regular file remains after a successful cleanup");
    }

    /// #245 sizes-file review (INFO, the two-writer lost update): the preview-size pass waits
    /// for its turn — a `flock` another process sharing the cache holds through its own pass —
    /// so it never reads the sizes file or the old directory while that pass rewrites or
    /// removes them. Here the turn is held by the test; the cleanup on another thread waits,
    /// and runs once it is released. (Timing: "waits" is observed over 300 ms.)
    #[test]
    fn the_preview_size_pass_waits_for_its_turn() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("stale-sizes-turn");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        let img = write_test_jpeg(tmp_dir.path(), "turn.jpg", 64, 48);
        let root = cache.join("chairphoto");
        let old_dir = root.join(STALE_PREVIEW_DIR);
        let name = cache_path_for(&img, PREVIEW).unwrap().file_name().unwrap().to_owned();
        plant_old_tier(&old_dir, &name, 2048, 1536);

        let held = preview_sizes_turn(&root).unwrap();
        let cleanup = std::thread::spawn(cleanup_stale_caches);
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(old_dir.is_dir() && !root.join(STALE_PREVIEW_SIZES).exists(), "waits while the turn is held");
        drop(held);
        cleanup.join().unwrap();
        assert!(!old_dir.exists(), "then runs");
        assert_eq!(cached_preview_size(&img), Some((2048, 1536)));

        // A lock file that is not a regular file (a symlink) is never opened: the pass is
        // skipped and the old directory kept for the next start.
        plant_old_tier(&old_dir, &name, 2048, 1536);
        std::fs::remove_file(root.join(STALE_PREVIEW_SIZES_LOCK)).unwrap();
        std::os::unix::fs::symlink(tmp_dir.path().join("elsewhere"), root.join(STALE_PREVIEW_SIZES_LOCK)).unwrap();
        cleanup_stale_caches();
        assert!(old_dir.is_dir(), "kept");
        assert!(!tmp_dir.path().join("elsewhere").exists(), "nothing created through the link");
    }

    /// Nit-1: [`cached_preview_size`]'s fallback refuses a symlinked old preview directory
    /// just as [`cleanup_stale_caches`] does — never following it to read a header.
    #[test]
    #[cfg(unix)]
    fn cached_preview_size_never_follows_a_symlinked_old_directory() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("stale-preview-symlink-read");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        let img = write_test_jpeg(tmp_dir.path(), "linked.jpg", 1200, 800);

        let elsewhere = tmp_dir.path().join("elsewhere");
        let name = cache_path_for(&img, PREVIEW).unwrap().file_name().unwrap().to_owned();
        plant_old_tier(&elsewhere, &name, 2048, 1365);

        let root = cache.join("chairphoto");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(STALE_PREVIEW_DIR)).unwrap();

        assert_eq!(cached_preview_size(&img), None, "a symlinked old directory is never read");
    }

    // --- decode-once chain + analyzer hook -----------------------------------
    // These tests touch the global on-disk cache (via XDG_CACHE_HOME) and the global
    // analyzer registry, both process-wide. Serialize them behind one mutex so the env
    // var and the registry can't be mutated by two tests at once.
    use std::sync::atomic::AtomicUsize;

    /// A test's own temp directory, removed on drop.
    ///
    /// The name carries the process id. Keying only on a constant — as these tests did —
    /// gives every `cargo test` process on the machine the same path, and each test's
    /// cleanup then deletes a directory another process is still writing into, which
    /// surfaces as `No such file or directory` from `write_test_jpeg` rather than as
    /// anything to do with thumbnails. Two worktrees, or a targeted run beside a full one,
    /// is enough to trigger it.
    ///
    /// Dropping rather than calling `remove_dir_all` at the end of each test also means a
    /// panicking test cleans up after itself; the old placement left the directory behind,
    /// which is how 7.9 GB of fixtures accumulated in `/tmp`.
    pub(crate) struct TestTmpDir(PathBuf);

    impl TestTmpDir {
        pub(crate) fn new(name: &str) -> Self {
            crate::test_home::isolate();
            let dir = std::env::temp_dir()
                .join(format!("cp-thumb-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub(crate) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestTmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Clear the analyzer registry, then install a single counter that increments once per
    /// decode. Returns the counter. The registry is process-global, so callers hold
    /// `test_lock()` for the duration.
    fn install_decode_counter() -> std::sync::Arc<AtomicUsize> {
        if let Ok(mut list) = analyzers().lock() {
            list.clear();
        }
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        register_analyzer(Arc::new(move |_img, _path| {
            c.fetch_add(1, Ordering::Relaxed);
        }));
        counter
    }

    /// Write a small solid-colour JPEG to a unique path under `dir` so each test has its
    /// own cache key (the key is path+mtime+size). A plain raster decodes the *same*
    /// source file for every size, so chain-derived and independently-generated caches
    /// downscale from an identical decode — giving byte-identical results.
    pub(crate) fn write_test_jpeg(dir: &Path, name: &str, w: u32, h: u32) -> PathBuf {
        let mut img = RgbImage::new(w, h);
        // A non-uniform gradient so downscaling is a real resample (not a trivial fill).
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let path = dir.join(name);
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_with_encoder(JpegEncoder::new_with_quality(&mut bytes, 90))
            .unwrap();
        std::fs::write(&path, bytes.into_inner()).unwrap();
        path
    }

    // --- stale cache cleanup (#168 review, #245) ------------------------------
    // Shares the env-var lock with the tests above: XDG_CACHE_HOME is process-global.

    #[test]
    fn cleanup_stale_caches_removes_only_the_old_directories() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("stale-dirs");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        let root = cache.join("chairphoto");

        for stale in [STALE_ZOOM_DIR, STALE_THUMB_DIR, STALE_PREVIEW_DIR, STALE_COVER_DIR] {
            std::fs::create_dir_all(root.join(stale)).unwrap();
            std::fs::write(root.join(stale).join("deadbeefdeadbeef.jpg"), b"old upscaled tier").unwrap();
        }
        // The current tiers and both offline thumbnail stores must survive untouched: the
        // pre-#258 `persist` is removed only by its migration (review fix258 M1).
        let keep = ["z10000v6", "p2048v6", "t512v6", "cover512v2", PERSIST_DIR, STALE_PERSIST_DIR];
        for dir in keep {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("keep.jpg"), b"current").unwrap();
        }

        cleanup_stale_caches();

        for stale in [STALE_ZOOM_DIR, STALE_THUMB_DIR, STALE_PREVIEW_DIR, STALE_COVER_DIR] {
            assert!(!root.join(stale).exists(), "{stale} should be gone");
        }
        for dir in keep {
            assert!(root.join(dir).join("keep.jpg").exists(), "{dir} must be untouched");
        }
        // The unreadable old "preview" kept no size, but the sizes file was still written.
        assert!(root.join(STALE_PREVIEW_SIZES).is_file());
    }

    // --- the pre-#258 store's migration (review fix258 M1) -------------------------------

    /// A cache root with `persist/<id>.jpg` holding `content` for each id, and the keys of a
    /// catalog that has photos 1, 2 and 3.
    fn old_store(tag: &str, ids: &[i64]) -> (TestTmpDir, PathBuf, HashMap<i64, OfflineThumbKey>) {
        let tmp = TestTmpDir::new(tag);
        let root = tmp.path().join("chairphoto");
        std::fs::create_dir_all(root.join(STALE_PERSIST_DIR)).unwrap();
        for id in ids {
            std::fs::write(root.join(STALE_PERSIST_DIR).join(format!("{id}.jpg")), format!("old {id}")).unwrap();
        }
        let catalog = uuid::Uuid::new_v4().to_string();
        let keys = (1..=3)
            .map(|id| (id, OfflineThumbKey::new(&catalog, &uuid::Uuid::new_v4().to_string()).unwrap()))
            .collect();
        (tmp, root, keys)
    }

    fn new_file(root: &Path, key: &OfflineThumbKey) -> PathBuf {
        root.join(PERSIST_DIR).join(key.catalog.hyphenated().to_string()).join(format!("{}.jpg", key.photo.hyphenated()))
    }

    /// Each old file of a photo the catalog has goes to that photo's own file; one a render
    /// already wrote is kept (it is at least as new); files of ids the catalog lacks are not
    /// adopted. Then, and only then, the old store is removed: the migration runs once.
    #[test]
    fn the_old_store_is_adopted_by_the_catalog_then_removed() {
        let (_tmp, root, keys) = old_store("adopt", &[1, 2, 9]);
        std::fs::create_dir_all(new_file(&root, &keys[&2]).parent().unwrap()).unwrap();
        std::fs::write(new_file(&root, &keys[&2]), b"rendered since").unwrap();

        let done = adopt_id_keyed_thumbs_in(&root, &keys).unwrap();

        assert_eq!(done, Adopted { copied: 1, kept: 1, skipped: 1 });
        assert_eq!(std::fs::read(new_file(&root, &keys[&1])).unwrap(), b"old 1");
        assert_eq!(std::fs::read(new_file(&root, &keys[&2])).unwrap(), b"rendered since", "never replaced");
        assert!(!root.join(STALE_PERSIST_DIR).exists(), "removed once everything landed");
        let leftovers: Vec<_> = std::fs::read_dir(new_file(&root, &keys[&1]).parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temporary files left: {leftovers:?}");
        assert_eq!(adopt_id_keyed_thumbs_in(&root, &keys).unwrap(), Adopted::default(), "a second start does nothing");
    }

    /// A migration that fails part-way (here: the catalog's directory cannot be made) keeps the
    /// old store — nothing is lost — and the next start resumes: what landed is kept, the
    /// rest is copied, and only then is the old store removed.
    #[test]
    fn an_interrupted_migration_keeps_the_old_store_and_resumes() {
        let (_tmp, root, keys) = old_store("adopt-resume", &[1, 2, 3]);
        // A first run already moved photo 1 before it died.
        std::fs::create_dir_all(new_file(&root, &keys[&1]).parent().unwrap()).unwrap();
        std::fs::write(new_file(&root, &keys[&1]), b"old 1").unwrap();
        // Something in the way of the catalog's directory: the copy cannot land.
        let dir = new_file(&root, &keys[&1]).parent().unwrap().to_path_buf();
        std::fs::rename(&dir, dir.with_extension("aside")).unwrap();
        std::fs::write(&dir, b"not a directory").unwrap();

        assert!(adopt_id_keyed_thumbs_in(&root, &keys).is_err());
        assert!(root.join(STALE_PERSIST_DIR).join("2.jpg").is_file(), "the old store is kept after a failure");

        std::fs::remove_file(&dir).unwrap();
        std::fs::rename(dir.with_extension("aside"), &dir).unwrap();
        let done = adopt_id_keyed_thumbs_in(&root, &keys).unwrap();
        assert_eq!(done, Adopted { copied: 2, kept: 1, skipped: 0 }, "resumed");
        for id in [1, 2, 3] {
            assert_eq!(std::fs::read(new_file(&root, &keys[&id])).unwrap(), format!("old {id}").into_bytes());
        }
        assert!(!root.join(STALE_PERSIST_DIR).exists());
    }

    /// Never through a symlink: a `persist` that is a symlink is refused and left alone (its
    /// target untouched, nothing adopted); an entry that is a symlink is not followed.
    #[test]
    fn the_migration_never_follows_a_symlink() {
        let (tmp, root, keys) = old_store("adopt-symlink", &[]);
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("1.jpg"), b"not ours").unwrap();
        std::fs::remove_dir(root.join(STALE_PERSIST_DIR)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(STALE_PERSIST_DIR)).unwrap();

        assert!(adopt_id_keyed_thumbs_in(&root, &keys).is_err(), "a symlinked store is refused");
        assert!(std::fs::symlink_metadata(root.join(STALE_PERSIST_DIR)).unwrap().file_type().is_symlink());
        assert!(elsewhere.join("1.jpg").is_file(), "its target is untouched");
        assert!(!new_file(&root, &keys[&1]).exists(), "nothing adopted through it");

        // A real store holding a symlinked entry: the entry is skipped, its target untouched.
        std::fs::remove_file(root.join(STALE_PERSIST_DIR)).unwrap();
        std::fs::create_dir_all(root.join(STALE_PERSIST_DIR)).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("1.jpg"), root.join(STALE_PERSIST_DIR).join("1.jpg")).unwrap();
        let done = adopt_id_keyed_thumbs_in(&root, &keys).unwrap();
        assert_eq!(done, Adopted { copied: 0, kept: 0, skipped: 1 });
        assert!(!new_file(&root, &keys[&1]).exists());
        assert_eq!(std::fs::read(elsewhere.join("1.jpg")).unwrap(), b"not ours");
    }

    // --- the offline store's cleanup (review of #258, N2) ----------------------------------

    /// Set `path`'s mtime `age` before `now`.
    fn aged(path: &Path, now: std::time::SystemTime, age: std::time::Duration) {
        let file = std::fs::File::options().read(true).open(path).unwrap();
        file.set_modified(now - age).unwrap();
    }

    /// A prune removes the open catalog's files for photos it no longer has once they are
    /// older than `ORPHAN_AGE` — never a photo's it has, nor a fresh one (a photo imported
    /// since the list was read); another catalog's directory only once it has not been opened
    /// for `UNOPENED_CATALOG_AGE`; and nothing that is not the store's own (a non-UUID name, a
    /// symlink). It marks the open catalog's directory as opened.
    #[test]
    fn a_prune_removes_only_gone_photos_and_long_unopened_catalogs() {
        let tmp = TestTmpDir::new("offline-prune");
        let store = tmp.path().join(PERSIST_DIR);
        let now = std::time::SystemTime::now();
        let day = std::time::Duration::from_secs(24 * 3600);
        let (open_cat, recent_cat, stale_cat, marked_cat) =
            (uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let (kept, gone_old, gone_new) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let file = |cat: uuid::Uuid, photo: uuid::Uuid, age: std::time::Duration| {
            let dir = store.join(cat.hyphenated().to_string());
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join(format!("{}.jpg", photo.hyphenated()));
            std::fs::write(&p, b"jpeg").unwrap();
            aged(&p, now, age);
            p
        };
        let kept_old = file(open_cat, kept, 400 * day);
        let gone_old = file(open_cat, gone_old, ORPHAN_AGE + day);
        let gone_new = file(open_cat, gone_new, day);
        let foreign_name = store.join(open_cat.hyphenated().to_string()).join("notes.jpg");
        std::fs::write(&foreign_name, b"x").unwrap();
        aged(&foreign_name, now, 400 * day);
        // Another catalog browsed last month; one untouched for over a year; one whose files
        // are old but which was opened lately (its marker).
        let recent = file(recent_cat, uuid::Uuid::new_v4(), 30 * day);
        let stale = file(stale_cat, uuid::Uuid::new_v4(), UNOPENED_CATALOG_AGE + day);
        aged(stale.parent().unwrap(), now, UNOPENED_CATALOG_AGE + day);
        let marked = file(marked_cat, uuid::Uuid::new_v4(), UNOPENED_CATALOG_AGE + day);
        std::fs::write(marked.parent().unwrap().join(OPENED_MARKER), b"").unwrap();
        aged(marked.parent().unwrap(), now, UNOPENED_CATALOG_AGE + day);
        // Not the store's: a non-UUID directory, and a symlinked "catalog" pointing elsewhere.
        let other = store.join("not-a-catalog");
        std::fs::create_dir_all(&other).unwrap();
        aged(&other, now, UNOPENED_CATALOG_AGE + day);
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("keep.jpg"), b"x").unwrap();
        aged(&elsewhere.join("keep.jpg"), now, UNOPENED_CATALOG_AGE + day);
        let link = store.join(uuid::Uuid::new_v4().hyphenated().to_string());
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();

        let open = OfflineCatalog::new(&open_cat.to_string(), [kept.to_string()]).unwrap();
        let done = prune_offline_thumbs_in(&store, &open, now).unwrap();

        assert_eq!(done, Pruned { files: 1, catalogs: 1 });
        assert!(kept_old.is_file(), "a photo the catalog has keeps its file, however old");
        assert!(!gone_old.exists(), "a gone photo's old file is removed");
        assert!(gone_new.is_file(), "a fresh file is kept: it may be a photo imported meanwhile");
        assert!(foreign_name.is_file(), "a name the store never writes is left");
        assert!(recent.is_file(), "a catalog opened lately keeps its files");
        assert!(!stale.parent().unwrap().exists(), "one not opened for a year is removed");
        assert!(marked.is_file(), "its marker says when it was opened, not its files' age");
        assert!(other.is_dir() && link.exists() && elsewhere.join("keep.jpg").is_file(), "nothing not the store's");
        assert!(store.join(open_cat.hyphenated().to_string()).join(OPENED_MARKER).is_file(), "the open catalog is marked");
        // A second prune finds nothing more.
        assert_eq!(prune_offline_thumbs_in(&store, &open, now).unwrap(), Pruned::default());
    }

    /// Review of release/face-thumbs, LOW 1: a symlink planted at the `.opened` name is
    /// replaced by a marker of the store's own; its target is never written through.
    #[test]
    fn a_symlinked_opened_marker_never_touches_its_target() {
        let tmp = TestTmpDir::new("offline-marker-symlink");
        let store = tmp.path().join(PERSIST_DIR);
        let cat = uuid::Uuid::new_v4();
        let dir = store.join(cat.hyphenated().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let target = tmp.path().join("precious.txt");
        std::fs::write(&target, b"keep me").unwrap();
        std::os::unix::fs::symlink(&target, dir.join(OPENED_MARKER)).unwrap();
        let open = OfflineCatalog::new(&cat.to_string(), []).unwrap();
        prune_offline_thumbs_in(&store, &open, std::time::SystemTime::now()).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me", "the prune's marker");
        let meta = std::fs::symlink_metadata(dir.join(OPENED_MARKER)).unwrap();
        assert!(meta.file_type().is_file(), "a marker of its own now");
        std::fs::remove_file(dir.join(OPENED_MARKER)).unwrap();
        std::os::unix::fs::symlink(&target, dir.join(OPENED_MARKER)).unwrap();
        write_opened_marker(&dir).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"keep me", "a switch's marker");
        assert!(std::fs::symlink_metadata(dir.join(OPENED_MARKER)).unwrap().file_type().is_file());
    }

    /// Review of release/face-thumbs, LOW 2: a catalog whose UUID another known catalog shares
    /// (`keep_orphans`) keeps the files of photos it does not have, however old.
    #[test]
    fn a_catalog_that_keeps_orphans_prunes_none() {
        let tmp = TestTmpDir::new("offline-keep-orphans");
        let store = tmp.path().join(PERSIST_DIR);
        let now = std::time::SystemTime::now();
        let cat = uuid::Uuid::new_v4();
        let dir = store.join(cat.hyphenated().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let orphan = dir.join(format!("{}.jpg", uuid::Uuid::new_v4().hyphenated()));
        std::fs::write(&orphan, b"jpeg").unwrap();
        aged(&orphan, now, ORPHAN_AGE * 2);
        let open = OfflineCatalog::new(&cat.to_string(), []).unwrap().keep_orphans();
        assert_eq!(prune_offline_thumbs_in(&store, &open, now).unwrap(), Pruned::default());
        assert!(orphan.is_file());
        let open = OfflineCatalog::new(&cat.to_string(), []).unwrap();
        assert_eq!(prune_offline_thumbs_in(&store, &open, now).unwrap().files, 1, "without it, pruned");
    }

    /// The offline thumbnail's key (#258) takes UUIDs only, so nothing else ever reaches its
    /// path; and a photo of one catalog never shares a file with its namesake in another.
    #[test]
    fn an_offline_thumbnail_key_is_two_uuids_and_names_its_catalog() {
        let (cat_a, cat_b, photo) = (uuid::Uuid::new_v4().to_string(), uuid::Uuid::new_v4().to_string(), uuid::Uuid::new_v4().to_string());
        assert!(OfflineThumbKey::new(&cat_a, "../../etc/passwd").is_none());
        assert!(OfflineThumbKey::new("", &photo).is_none());
        assert!(OfflineThumbKey::new(&cat_a, "42").is_none(), "a photo id is not a key");
        let a = OfflineThumbKey::new(&cat_a, &photo).unwrap();
        let b = OfflineThumbKey::new(&cat_b, &photo).unwrap();
        assert_ne!(persistent_thumb_path(&a), persistent_thumb_path(&b));
        assert!(persistent_thumb_path(&a).starts_with(persistent_thumb_dir()));
        assert_eq!(OfflineThumbKey::new(&cat_a.to_uppercase(), &photo), Some(a), "one spelling per UUID");
    }

    /// #245 review: an empty or relative `XDG_CACHE_HOME` is ignored (the XDG spec), never a
    /// cache under whatever directory the process was started in.
    #[test]
    fn a_relative_xdg_cache_home_is_ignored() {
        let os = |s: &str| Some(std::ffi::OsString::from(s));
        assert_eq!(cache_dir_from(os("/x/cache"), os("/home/u")), PathBuf::from("/x/cache"));
        assert_eq!(cache_dir_from(os("rel/cache"), os("/home/u")), PathBuf::from("/home/u/.cache"));
        assert_eq!(cache_dir_from(os("./cache"), os("/home/u")), PathBuf::from("/home/u/.cache"));
        assert_eq!(cache_dir_from(os(""), os("/home/u")), PathBuf::from("/home/u/.cache"));
        assert_eq!(cache_dir_from(None, os("/home/u")), PathBuf::from("/home/u/.cache"));
        assert_eq!(cache_dir_from(os("rel"), None), std::env::temp_dir());
        assert_eq!(cache_dir_from(None, os("")), std::env::temp_dir(), "an empty HOME is no home");
    }

    #[test]
    fn cleanup_stale_caches_is_a_silent_no_op_when_absent() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("stale-absent");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        // Nothing to remove; must not panic, and makes nothing.
        cleanup_stale_caches();
        assert!(!cache.exists());
    }

    /// A symlink planted at an exact stale name is never followed: neither it nor whatever
    /// it points at is touched. `symlink_metadata` sees the link, not a directory, so the
    /// removal refuses outright. A symlink inside the old preview directory is not read for
    /// a size, and one at the sizes file's temporary name is replaced, not written through.
    #[test]
    #[cfg(unix)]
    fn cleanup_stale_caches_never_follows_a_symlink() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("stale-symlink");
        let cache = tmp_dir.path().join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);

        let elsewhere = tmp_dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("precious.txt"), b"not ours to delete").unwrap();
        plant_old_tier(&elsewhere, std::ffi::OsStr::new("0123456789abcdef.jpg"), 40, 30);

        let root = cache.join("chairphoto");
        std::fs::create_dir_all(&root).unwrap();
        for stale in [STALE_ZOOM_DIR, STALE_THUMB_DIR, STALE_PREVIEW_DIR, STALE_COVER_DIR] {
            std::os::unix::fs::symlink(&elsewhere, root.join(stale)).unwrap();
        }

        cleanup_stale_caches();

        for stale in [STALE_ZOOM_DIR, STALE_THUMB_DIR, STALE_PREVIEW_DIR, STALE_COVER_DIR] {
            let meta = std::fs::symlink_metadata(root.join(stale)).expect("the symlink itself must still exist");
            assert!(meta.file_type().is_symlink(), "{stale} must remain a symlink, not be removed or replaced");
        }
        assert!(elsewhere.join("precious.txt").exists(), "the symlinks' target must be untouched");
        assert!(!root.join(STALE_PREVIEW_SIZES).exists(), "a symlinked preview directory is not read");

        // A real old preview directory holding a symlinked "preview", and a symlink at the
        // sizes file's temporary name.
        std::fs::remove_file(root.join(STALE_PREVIEW_DIR)).unwrap();
        std::fs::create_dir_all(root.join(STALE_PREVIEW_DIR)).unwrap();
        let inner = root.join(STALE_PREVIEW_DIR).join("0123456789abcdef.jpg");
        std::os::unix::fs::symlink(elsewhere.join("0123456789abcdef.jpg"), inner).unwrap();
        let target = elsewhere.join("tmp-target");
        std::fs::write(&target, b"not ours to write").unwrap();
        std::os::unix::fs::symlink(&target, root.join(format!("{STALE_PREVIEW_SIZES}.tmp"))).unwrap();

        cleanup_stale_caches();

        assert!(!root.join(STALE_PREVIEW_DIR).exists(), "the real directory is removed");
        assert_eq!(std::fs::read(&target).unwrap(), b"not ours to write", "the temporary name's target is untouched");
        assert!(elsewhere.join("0123456789abcdef.jpg").exists(), "a symlinked preview's target is untouched");
        let sizes = std::fs::read_to_string(root.join(STALE_PREVIEW_SIZES)).unwrap();
        assert_eq!(sizes, "", "a symlinked preview kept no size");
        assert!(std::fs::symlink_metadata(root.join(STALE_PREVIEW_SIZES)).unwrap().file_type().is_file());
    }

    #[test]
    fn analyzer_hook_fires_once_per_decode() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("hook");
        let tmp = tmp_dir.path().to_path_buf();
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        let counter = install_decode_counter();

        // Use an image large enough that its longest edge exceeds PREVIEW_MAX (2048px), so
        // a preview-size decode doesn't down-sample it to a size where the hook is gated off.
        let img = write_test_jpeg(&tmp, "hook.jpg", 2200, 1800);

        // A thumbnail request must NOT fire the hook — THUMB (512px) is below the resolution
        // gate (PREVIEW_MAX = 2048). Scoring micro-blur at 512px is inaccurate.
        thumbnail_bytes(&img).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 0, "thumb decode must not fire the hook");

        // A preview request (PREVIEW_MAX = 2048px) exceeds the gate → hook fires once.
        preview_bytes(&img).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 1, "preview decode fires the hook once");

        // Second preview request is a cache hit: no decode, no hook.
        preview_bytes(&img).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 1, "cache hit fires no hook");

        // Cleanup: drop the test analyzer so it doesn't leak into other tests.
        if let Ok(mut list) = analyzers().lock() {
            list.clear();
        }
    }

    /// #154: the face-region writer's preview size comes from the cached preview's header —
    /// nothing until the preview is cached (and asking generates nothing), then its size.
    #[test]
    fn cached_preview_size_reads_only_the_cache() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("cachedsize");
        let tmp = tmp_dir.path().to_path_buf();
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        let img = write_test_jpeg(&tmp, "cachedsize.jpg", 1200, 900);

        assert_eq!(cached_preview_size(&img), None, "not cached yet");
        assert!(!cache_path_for(&img, PREVIEW).unwrap().exists(), "asking generated it");
        let preview = image::load_from_memory(&preview_bytes(&img).unwrap()).unwrap();
        assert_eq!(cached_preview_size(&img), Some((preview.width(), preview.height())));
    }

    #[test]
    fn single_small_request_does_not_over_decode() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("nodecode");
        let tmp = tmp_dir.path().to_path_buf();
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache"));
        let counter = install_decode_counter();

        let img = write_test_jpeg(&tmp, "nodecode.jpg", 1200, 900);

        // A lone thumbnail request must decode exactly once — never a preview/zoom-size
        // decode on the grid pool's critical path. The THUMB decode (512px) is below the
        // resolution gate, so the hook must NOT fire.
        thumbnail_bytes(&img).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 0, "lone thumb → no hook (below resolution gate)");

        // A preview request now needs its own (larger) decode: the thumb tier was derived
        // *down* from the thumb decode and can't serve a preview. The preview decode is at
        // PREVIEW_MAX (2048px) which meets the resolution gate → hook fires once.
        preview_bytes(&img).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 1, "preview decode fires the hook once");

        // …but the preview decode opportunistically re-derived every smaller tier, so a
        // second thumbnail request is a pure cache hit — no further decode, no further hook.
        let n = counter.load(Ordering::Relaxed);
        thumbnail_bytes(&img).unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), n, "smaller tiers ride the larger decode");

        if let Ok(mut list) = analyzers().lock() {
            list.clear();
        }
    }

    #[test]
    fn chain_matches_independent_generation() {
        let _guard = test_lock();
        let tmp_dir = TestTmpDir::new("chain");
        let tmp = tmp_dir.path().to_path_buf();

        // Two identical rasters at distinct paths → distinct cache keys, no cross-talk.
        let chain_img = write_test_jpeg(&tmp, "chain.jpg", 2400, 1600);
        let indep_img = write_test_jpeg(&tmp, "indep.jpg", 2400, 1600);

        // (a) The chain: one decode fills every size.
        std::env::set_var("XDG_CACHE_HOME", tmp.join("cache-chain"));
        let counter = install_decode_counter();
        warm_all_sizes(&chain_img).unwrap();
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "warm_all_sizes decodes exactly once for all three sizes"
        );
        let chain_thumb = thumbnail_bytes(&chain_img).unwrap();
        let chain_preview = preview_bytes(&chain_img).unwrap();
        let chain_zoom = zoom_bytes(&chain_img).unwrap();
        // All served from cache: still just the one decode.
        assert_eq!(counter.load(Ordering::Relaxed), 1, "sizes served from cache");

        // (b) Independent generation: encode each size directly from a fresh full decode,
        // the way the code did before I7b (extract at the size's own max, downscale, encode).
        let full = extract_and_decode(&indep_img, ZOOM.max).unwrap();
        let indep_thumb = encode_size(&indep_img, &full, THUMB).unwrap();
        let indep_preview = encode_size(&indep_img, &full, PREVIEW).unwrap();
        let indep_zoom = encode_size(&indep_img, &full, ZOOM).unwrap();

        // For a plain raster, both routes downscale from the same decode → byte-identical.
        assert_eq!(chain_thumb, indep_thumb, "thumb: chain == independent");
        assert_eq!(chain_preview, indep_preview, "preview: chain == independent");
        assert_eq!(chain_zoom, indep_zoom, "zoom: chain == independent");

        if let Ok(mut list) = analyzers().lock() {
            list.clear();
        }
    }
}
