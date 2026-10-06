//! `media::render_image` — the GPUI app's decode path (#101) — against the cached tiers'
//! JPEGs, on real files through the real disk caches. (Until #165 these tests also held it to
//! the Tauri protocols' byte path, `media::render_bytes`, which went with the shell.)
//!
//! Its own test binary on purpose: the thumbnail cache lives under `XDG_CACHE_HOME`, a
//! process-wide variable that no other test binary's modules can move under these tests.
//! Within this binary each test holds [`Fixture`]'s lock for its whole run and points the
//! variable at its own directory, removed when it ends.

mod common;

use chairphoto_core::app::AppState;
use chairphoto_core::catalog::Catalog;
use chairphoto_core::image_pool::{ImageKind, JobKey};
use chairphoto_core::media::{render_image, video_tile};
use common::TestTmpDir;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, RgbImage};
use std::path::{Path, PathBuf};
use std::ops::Deref;
use std::sync::{Mutex, MutexGuard};

/// A test's directory, with `XDG_CACHE_HOME` pointing into it for as long as the fixture
/// lives. Holds the process-wide lock, so tests here run one at a time.
struct Fixture {
    dir: TestTmpDir,
    _lock: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        static LOCK: Mutex<()> = Mutex::new(());
        let lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = TestTmpDir::new(tag);
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        Fixture { dir, _lock: lock }
    }
}

impl Deref for Fixture {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.dir
    }
}

/// A JPEG whose pixels differ everywhere, so a rotation or a wrong tier shows.
fn write_jpeg(dir: &Path, name: &str, w: u32, h: u32) -> PathBuf {
    let img = RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([(x * 255 / w) as u8, (y * 255 / h) as u8, ((x + y) % 256) as u8])
    });
    let mut out = std::io::Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(img)
        .write_with_encoder(JpegEncoder::new_with_quality(&mut out, 90))
        .unwrap();
    let path = dir.join(name);
    std::fs::write(&path, out.into_inner()).unwrap();
    path
}

/// An `AppState` with a catalog rooted at `dir/photos` holding `files` (created by `make`).
fn catalog_with(dir: &Path, files: &[&str], make: impl Fn(&Path, &str)) -> (AppState, Vec<i64>) {
    let root = dir.join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let catalog = Catalog::open(&dir.join("t.chairphoto"), &root).unwrap();
    let mut ids = Vec::new();
    for name in files {
        make(&root, name);
        let abs = root.join(name);
        let size = std::fs::metadata(&abs).unwrap().len() as i64;
        ids.push(catalog.upsert_photo(&abs, None, 1, size).unwrap().id);
    }
    let state = AppState::default();
    *state.catalog.lock().unwrap() = Some(catalog);
    (state, ids)
}

fn decode(bytes: &[u8]) -> image::RgbImage {
    image::load_from_memory(bytes).unwrap().to_rgb8()
}

/// Where the open catalog keeps photo `id`'s offline thumbnail (#258).
fn kept_path(state: &AppState, id: i64) -> PathBuf {
    let guard = state.catalog.lock().unwrap();
    let key = guard.as_ref().unwrap().offline_thumb_key(id).unwrap().expect("a photo row has a key");
    chairphoto_core::thumbnails::persistent_thumb_path(&key)
}

/// The cached JPEG of one tier of the original at `abs`.
fn cached_tier(abs: &Path, kind: ImageKind) -> Vec<u8> {
    use chairphoto_core::thumbnails::{preview_bytes, thumbnail_bytes, zoom_bytes};
    match kind {
        ImageKind::Thumb => thumbnail_bytes(abs),
        ImageKind::Preview => preview_bytes(abs),
        ImageKind::Zoom => zoom_bytes(abs),
    }
    .unwrap()
}

/// Unrotated, every tier's pixels are exactly the decode of that tier's cached JPEG: no
/// second renderer, no re-encode.
#[test]
fn unrotated_tiers_are_the_cached_jpegs_decoded() {
    let dir = Fixture::new("media-plain");
    let (state, ids) = catalog_with(&dir, &["a.jpg"], |root, n| {
        write_jpeg(root, n, 2600, 1700);
    });
    let abs = dir.join("photos").join("a.jpg");
    for kind in [ImageKind::Thumb, ImageKind::Preview, ImageKind::Zoom] {
        let decoded = render_image(&state, JobKey::photo(ids[0], kind)).unwrap();
        assert!(!decoded.video_tile);
        assert_eq!(decoded.image.to_rgb8(), decode(&cached_tier(&abs, kind)), "{kind:?}");
    }
    // An unrotated thumbnail keeps the cached JPEG itself as the persistent copy.
    let kept = std::fs::read(kept_path(&state, ids[0])).unwrap();
    assert_eq!(kept, cached_tier(&abs, ImageKind::Thumb));
}

/// A user rotation: `render_image` rotates the cached pixels (no re-encode), so it equals the
/// rotated decode of the cached JPEG exactly, and keeps a persistent thumbnail that is the
/// rotated pixels as a quality-90 JPEG — the file the Tauri shell's byte path wrote before
/// #165, so a thumbnail kept then and one kept now are the same file.
#[test]
fn a_rotated_photo_rotates_pixels_and_keeps_a_rotated_persistent_thumb() {
    let dir = Fixture::new("media-rot");
    let (state, ids) = catalog_with(&dir, &["r.jpg"], |root, n| {
        write_jpeg(root, n, 1200, 800);
    });
    let id = ids[0];
    state.catalog.lock().unwrap().as_ref().unwrap().set_photo_rotation(id, 90).unwrap();

    let decoded = render_image(&state, JobKey::photo(id, ImageKind::Thumb)).unwrap().image;
    let abs = dir.join("photos").join("r.jpg");
    let expected = image::load_from_memory(&cached_tier(&abs, ImageKind::Thumb)).unwrap().rotate90();
    assert_eq!(decoded.to_rgb8(), expected.to_rgb8());

    let kept = std::fs::read(kept_path(&state, id)).unwrap();
    let mut q90 = std::io::Cursor::new(Vec::new());
    expected.write_with_encoder(JpegEncoder::new_with_quality(&mut q90, 90)).unwrap();
    assert_eq!(kept, q90.into_inner(), "the rotated pixels, re-encoded at quality 90");
}

/// Every rotation on the preview and zoom tiers: `render_image` is the cached tier's JPEG,
/// decoded and rotated as pixels.
#[test]
fn rotated_preview_and_zoom_rotate_the_cached_pixels() {
    let dir = Fixture::new("media-rot-tiers");
    let (state, ids) = catalog_with(&dir, &["t.jpg"], |root, n| {
        write_jpeg(root, n, 2600, 1300);
    });
    let id = ids[0];
    let abs = dir.join("photos").join("t.jpg");
    for degrees in [90, 180, 270] {
        state.catalog.lock().unwrap().as_ref().unwrap().set_photo_rotation(id, degrees).unwrap();
        for kind in [ImageKind::Preview, ImageKind::Zoom] {
            let plain = image::load_from_memory(&cached_tier(&abs, kind)).unwrap();
            let expected = match degrees {
                90 => plain.rotate90(),
                180 => plain.rotate180(),
                _ => plain.rotate270(),
            }
            .to_rgb8();
            let decoded = render_image(&state, JobKey::photo(id, kind)).unwrap().image.to_rgb8();
            assert_eq!(decoded, expected, "{kind:?} at {degrees}°");
        }
    }
}

/// A rotated photo with a cover version: the thumbnail is the cover render, rotated — not
/// the plain thumbnail.
#[cfg(feature = "edit")]
#[test]
fn a_rotated_cover_thumbnail_is_the_cover_render_rotated() {
    let dir = Fixture::new("media-rot-cover");
    let (state, ids) = catalog_with(&dir, &["c.jpg"], |root, n| {
        write_jpeg(root, n, 1600, 1000);
    });
    let id = ids[0];
    let record = r#"{"tone": {"ev": 1.0, "contrast": 0.3}, "vignette": -0.5}"#;
    {
        let guard = state.catalog.lock().unwrap();
        let catalog = guard.as_ref().unwrap();
        let version = catalog.create_version(id, "Cover").unwrap();
        catalog.set_version_edit(version, record).unwrap();
        catalog.set_cover_version(id, Some(version)).unwrap();
        catalog.set_photo_rotation(id, 270).unwrap();
    }
    let abs = dir.join("photos").join("c.jpg");
    let json = state.catalog.lock().unwrap().as_ref().unwrap().cover_of(id).unwrap().unwrap().2;
    let cover = chairphoto_core::plugins::edit::cover::cover_thumb(&abs, id, &json).unwrap();
    let expected = image::load_from_memory(&cover).unwrap().rotate270().to_rgb8();

    let decoded = render_image(&state, JobKey::photo(id, ImageKind::Thumb)).unwrap().image.to_rgb8();
    assert_eq!(decoded, expected);
    let plain = image::load_from_memory(&chairphoto_core::thumbnails::thumbnail_bytes(&abs).unwrap())
        .unwrap()
        .rotate270()
        .to_rgb8();
    assert_ne!(decoded, plain, "the cover's look, not the plain thumbnail");
}

/// #252: with no cover pinned, the thumbnail is the render of the version changed last, and
/// follows the next change to another version. The original's own thumbnail is still kept as
/// the offline fallback, though the tile has only ever shown a face.
#[cfg(feature = "edit")]
#[test]
fn the_thumbnail_is_the_render_of_the_last_changed_version() {
    let dir = Fixture::new("media-auto-face");
    let (state, ids) = catalog_with(&dir, &["f.jpg"], |root, n| {
        write_jpeg(root, n, 1600, 1000);
    });
    let id = ids[0];
    let abs = dir.join("photos").join("f.jpg");
    let (bright, dark, mid) = (r#"{"tone": {"ev": 1.0}}"#, r#"{"tone": {"ev": -1.0}}"#, r#"{"tone": {"ev": 0.5}}"#);
    let with_catalog = |f: &dyn Fn(&Catalog)| f(state.catalog.lock().unwrap().as_ref().unwrap());
    with_catalog(&|c| {
        let v = c.create_version(id, "Bright").unwrap();
        c.set_version_edit(v, bright).unwrap();
        let w = c.create_version(id, "Dark").unwrap();
        c.set_version_edit(w, dark).unwrap();
    });
    with_catalog(&|c| assert_eq!(c.get_photo(id).unwrap().cover_pin, chairphoto_core::catalog::CoverPin::Auto));
    let look = |json: &str| decode(&chairphoto_core::plugins::edit::cover::cover_thumb(&abs, id, json).unwrap());
    let thumb = || render_image(&state, JobKey::photo(id, ImageKind::Thumb)).unwrap();

    let shown = thumb();
    assert!(shown.cover, "a version's render");
    assert_eq!(shown.image.to_rgb8(), look(dark), "Dark, changed last");
    assert_ne!(look(dark), look(bright));
    with_catalog(&|c| {
        let bright_id = c.list_versions(id).unwrap()[0].id;
        c.set_version_edit(bright_id, mid).unwrap();
    });
    assert_eq!(thumb().image.to_rgb8(), look(mid), "the first version changed again: its look");
    let kept = std::fs::read(kept_path(&state, id)).unwrap();
    assert_eq!(kept, cached_tier(&abs, ImageKind::Thumb), "the original's own thumbnail kept for offline");
}

/// #252 review L3: a photo showing its face keeps its offline fallback current — a user
/// rotation after the fallback was kept rewrites it, as the plain path would.
#[cfg(feature = "edit")]
#[test]
fn the_face_path_keeps_the_offline_fallback_current_after_a_rotation() {
    let dir = Fixture::new("media-face-rotated-fallback");
    let (state, ids) = catalog_with(&dir, &["g.jpg"], |root, n| {
        write_jpeg(root, n, 1600, 1000);
    });
    let id = ids[0];
    let abs = dir.join("photos").join("g.jpg");
    {
        let guard = state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        let v = c.create_version(id, "Bright").unwrap();
        c.set_version_edit(v, r#"{"tone": {"ev": 1.0}}"#).unwrap();
    }
    let kept = || std::fs::read(kept_path(&state, id)).unwrap();
    assert!(render_image(&state, JobKey::photo(id, ImageKind::Thumb)).unwrap().cover);
    assert_eq!(kept(), cached_tier(&abs, ImageKind::Thumb), "unrotated: the cached thumbnail itself");

    state.catalog.lock().unwrap().as_ref().unwrap().set_photo_rotation(id, 90).unwrap();
    assert!(render_image(&state, JobKey::photo(id, ImageKind::Thumb)).unwrap().cover);
    let rotated = image::load_from_memory(&cached_tier(&abs, ImageKind::Thumb)).unwrap().rotate90();
    let mut q90 = std::io::Cursor::new(Vec::new());
    rotated.write_with_encoder(JpegEncoder::new_with_quality(&mut q90, 90)).unwrap();
    assert_eq!(kept(), q90.into_inner(), "rewritten for the new rotation");
}

/// Original gone: the thumbnail tier falls back to the persistent copy; preview has no
/// fallback and errors.
#[test]
fn an_unreachable_original_falls_back_to_the_persistent_thumb() {
    let dir = Fixture::new("media-offline");
    let (state, ids) = catalog_with(&dir, &["o.jpg"], |root, n| {
        write_jpeg(root, n, 900, 600);
    });
    let key = JobKey::photo(ids[0], ImageKind::Thumb);
    let online = render_image(&state, key.clone()).unwrap().image.to_rgb8();
    std::fs::remove_file(dir.join("photos").join("o.jpg")).unwrap();
    let offline = render_image(&state, key).unwrap().image.to_rgb8();
    assert_eq!(online, offline);
    assert!(render_image(&state, JobKey::photo(ids[0], ImageKind::Preview)).is_err());
}

// --- The offline thumbnail's identity (#258) -------------------------------------------------

/// The offline thumbnail belongs to one catalog's photo. Catalog A keeps one for its photo 1;
/// catalog B's photo 1 — another photo, its original unreachable — has none of its own, so its
/// tile fails rather than show A's photo. Until #258 the file was keyed by photo id alone and B
/// showed A's pixels, indefinitely.
#[test]
fn another_catalogs_offline_thumbnail_is_never_shown_for_a_colliding_id() {
    let dir = Fixture::new("media-offline-collide");
    let (state, a_ids) = catalog_with(&dir.join("a"), &["a.jpg"], |root, n| {
        write_jpeg(root, n, 900, 600);
    });
    let (b_state, b_ids) = catalog_with(&dir.join("b"), &["b.jpg"], |root, n| {
        write_jpeg(root, n, 600, 900);
    });
    assert_eq!(a_ids, b_ids, "the two catalogs' photos collide on id");
    let id = a_ids[0];
    let thumb = JobKey::photo(id, ImageKind::Thumb);
    let a_tile = render_image(&state, thumb.clone()).unwrap().image.to_rgb8();
    let a_kept = kept_path(&state, id);
    assert!(a_kept.is_file(), "A kept its photo's offline thumbnail");

    // B opens; its photo 1's original is away and was never seen.
    std::fs::remove_file(dir.join("b").join("photos").join("b.jpg")).unwrap();
    let b_catalog = b_state.catalog.lock().unwrap().take();
    let a_catalog = std::mem::replace(&mut *state.catalog.lock().unwrap(), b_catalog);
    match render_image(&state, thumb.clone()) {
        Ok(shown) => panic!("B's photo {id} showed a {:?} tile; A's was {:?}", shown.image.to_rgb8().dimensions(), a_tile.dimensions()),
        Err(e) => assert!(e.contains("no reachable copy"), "{e}"),
    }
    assert_ne!(kept_path(&state, id), a_kept, "B's photo {id} has a key of its own");

    // A again, its original away too: its own thumbnail stands in.
    *state.catalog.lock().unwrap() = a_catalog;
    std::fs::remove_file(dir.join("a").join("photos").join("a.jpg")).unwrap();
    assert_eq!(render_image(&state, thumb).unwrap().image.to_rgb8(), a_tile);
}

/// The key survives a restart: it is the catalog's stable UUID, not the per-open
/// `CatalogIdentity`, so a catalog reopened with its original away still finds its photo's
/// thumbnail.
#[test]
fn the_offline_thumbnail_is_found_after_the_catalog_is_reopened() {
    let dir = Fixture::new("media-offline-reopen");
    let (state, ids) = catalog_with(&dir, &["r.jpg"], |root, n| {
        write_jpeg(root, n, 900, 600);
    });
    let thumb = JobKey::photo(ids[0], ImageKind::Thumb);
    let online = render_image(&state, thumb.clone()).unwrap().image.to_rgb8();
    let before = chairphoto_core::app::catalog_identity(&state).unwrap();
    drop(state.catalog.lock().unwrap().take());
    std::fs::remove_file(dir.join("photos").join("r.jpg")).unwrap();

    let reopened = Catalog::open(&dir.join("t.chairphoto"), &dir.join("photos")).unwrap();
    *state.catalog.lock().unwrap() = Some(reopened);
    assert_ne!(chairphoto_core::app::catalog_identity(&state).unwrap(), before, "a new open, a new identity");
    assert_eq!(render_image(&state, thumb).unwrap().image.to_rgb8(), online);
}

/// A video whose poster frame cannot be made (here: not a video at all, so `ffmpeg` fails or
/// is missing) is the generic tile, not an error — the grid shows a video, never a hole.
#[test]
fn a_video_without_a_poster_is_the_generic_tile() {
    let dir = Fixture::new("media-video");
    let (state, ids) = catalog_with(&dir, &["clip.mp4"], |root, n| {
        std::fs::write(root.join(n), b"not a video").unwrap();
    });
    for kind in [ImageKind::Thumb, ImageKind::Preview] {
        let decoded = render_image(&state, JobKey::photo(ids[0], kind)).unwrap();
        assert!(decoded.video_tile, "{kind:?}");
        assert_eq!(decoded.image.to_rgb8(), video_tile().to_rgb8());
    }
}

/// A photo that is not a video and cannot be decoded is still an error.
#[test]
fn an_undecodable_photo_is_an_error() {
    let dir = Fixture::new("media-bad");
    let (state, ids) = catalog_with(&dir, &["bad.jpg"], |root, n| {
        std::fs::write(root.join(n), b"not a jpeg").unwrap();
    });
    assert!(render_image(&state, JobKey::photo(ids[0], ImageKind::Thumb)).is_err());
}


/// The Darkroom frame is the edit engine's render of the photo's cached preview tier, with no
/// encode between them: a base-only frame and a full frame are each exactly what
/// `render_proxy` makes of that JPEG, and the pool's key renders the same frame. (Until #165
/// this compared against the Tauri `edit://` protocol's PNG/JPEG bytes.)
#[cfg(feature = "edit")]
#[test]
fn an_edit_frame_is_the_engine_render_of_the_preview_tier() {
    use chairphoto_core::image_pool::EditJob;
    use chairphoto_core::media::render_edit_image;
    use chairphoto_core::plugins::edit::{render_proxy, RenderOpts, RenderSource, SourceToken};

    let dir = Fixture::new("media-edit");
    let (state, ids) = catalog_with(&dir, &["e.jpg"], |root, n| {
        write_jpeg(root, n, 2400, 1600);
    });
    let abs = dir.join("photos").join("e.jpg");
    let record = r#"{"straighten": 4, "tone": {"ev": 0.4, "contrast": 0.2}, "vignette": -0.3}"#;
    let job = |max_edge, base_only| EditJob {
        photo_id: ids[0],
        edit_json: record.into(),
        max_edge,
        hi_res: false,
        base_only,
        source: SourceToken::Preview,
        clip: false,
        catalog: chairphoto_core::app::catalog_identity(&state).unwrap(),
    };
    let engine = |max_edge, skip_look| {
        let preview = cached_tier(&abs, ImageKind::Preview);
        render_proxy(RenderSource::PreviewJpeg(&preview), record, max_edge, RenderOpts { skip_look })
            .unwrap()
            .to_rgb8()
    };

    let base = job(720, true);
    let base_frame = render_edit_image(&state, &base).unwrap().to_rgb8();
    assert_eq!(base_frame, engine(720, true), "a base-only frame is the engine's, unencoded");

    let full = render_edit_image(&state, &job(1400, false)).unwrap().to_rgb8();
    assert!(full.width().max(full.height()) <= 1400);
    assert_eq!(full, engine(1400, false), "a full frame is the engine's, unencoded");

    // The same frame through the pool's key.
    let via_key = render_image(&state, JobKey::Edit(base)).unwrap();
    assert_eq!(via_key.image.to_rgb8(), base_frame);
}

// --- Catalog identity (#251) -----------------------------------------------------------------

/// An edit render belongs to the catalog its photo id was read from. A switch publishes the
/// new catalog before a front end hears of it, so a job asked for the old catalog's photo can
/// reach a worker while a catalog with a colliding id is open: it renders nothing and answers
/// `CATALOG_CHANGED`, never the other photo's pixels. A job asked for the new catalog renders
/// the new catalog's photo, and with no catalog open at all (between a switch's two phases) a
/// job is refused the same way.
#[cfg(feature = "edit")]
#[test]
fn an_edit_job_renders_only_in_the_catalog_it_was_asked_for() {
    use chairphoto_core::app::{catalog_identity, CATALOG_CHANGED};
    use chairphoto_core::image_pool::EditJob;
    use chairphoto_core::media::render_edit_image;
    use chairphoto_core::plugins::edit::{render_proxy, RenderOpts, RenderSource, SourceToken};

    let dir = Fixture::new("media-edit-identity");
    // Two catalogs whose first photos share an id but not their pixels (or their shape).
    let (state, a_ids) = catalog_with(&dir.join("a"), &["a.jpg"], |root, n| {
        write_jpeg(root, n, 2400, 1600);
    });
    let (b_state, b_ids) = catalog_with(&dir.join("b"), &["b.jpg"], |root, n| {
        write_jpeg(root, n, 1200, 1800);
    });
    assert_eq!(a_ids, b_ids, "the two catalogs' photos collide on id");
    let id = a_ids[0];
    let b_catalog = b_state.catalog.lock().unwrap().take().unwrap();
    let record = r#"{"tone": {"ev": 0.3}}"#;
    let job = |catalog| EditJob {
        photo_id: id,
        edit_json: record.into(),
        max_edge: 900,
        hi_res: false,
        base_only: false,
        source: SourceToken::Preview,
        clip: false,
        catalog,
    };
    let engine = |abs: PathBuf| {
        let preview = cached_tier(&abs, ImageKind::Preview);
        render_proxy(RenderSource::PreviewJpeg(&preview), record, 900, RenderOpts::default()).unwrap().to_rgb8()
    };
    let a_frame = engine(dir.join("a").join("photos").join("a.jpg"));
    let b_frame = engine(dir.join("b").join("photos").join("b.jpg"));
    assert_ne!(a_frame.dimensions(), b_frame.dimensions());

    let a = catalog_identity(&state).unwrap();
    let asked_in_a = job(a);
    assert_eq!(render_edit_image(&state, &asked_in_a).unwrap().to_rgb8(), a_frame, "A open: A's photo");

    // The switch publishes B; nothing has told the requester yet.
    *state.catalog.lock().unwrap() = Some(b_catalog);
    let b = catalog_identity(&state).unwrap();
    assert_ne!(a, b);
    let err = render_edit_image(&state, &asked_in_a).unwrap_err();
    assert_eq!(err, CATALOG_CHANGED, "asked in A, rendered nothing in B");
    let via_key = render_image(&state, JobKey::Edit(asked_in_a.clone())).map(|d| d.image.to_rgb8());
    assert_eq!(via_key, Err(CATALOG_CHANGED.to_string()), "the pool's runner answers the same");
    assert_ne!(JobKey::Edit(asked_in_a.clone()), JobKey::Edit(job(b)), "never one pool job");

    // Asked again for B — after `catalog:switched` — it is B's photo.
    assert_eq!(render_edit_image(&state, &job(b)).unwrap().to_rgb8(), b_frame, "asked in B: B's photo");

    // Between a switch's two phases no catalog is open: refused alike.
    let b_catalog = state.catalog.lock().unwrap().take();
    assert_eq!(render_edit_image(&state, &job(b)).unwrap_err(), CATALOG_CHANGED);
    drop(b_catalog);
}
