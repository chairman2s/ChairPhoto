//! `media::render_image` — the GPUI app's decode path (#101) — against `media::render_bytes`,
//! the Tauri protocols' path, on real files through the real disk caches.
//!
//! Its own test binary on purpose: the thumbnail cache lives under `XDG_CACHE_HOME`, a
//! process-wide variable that no other test binary's modules can move under these tests.
//! Within this binary each test holds [`Fixture`]'s lock for its whole run and points the
//! variable at its own directory, removed when it ends.

mod common;

use chairphoto_core::app::AppState;
use chairphoto_core::catalog::Catalog;
use chairphoto_core::image_pool::{ImageKind, JobKey};
use chairphoto_core::media::{render_bytes, render_image, video_tile};
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

/// Unrotated, every tier's pixels are exactly the decode of the bytes the Tauri protocol
/// serves: `render_image` is `render_bytes` minus the encode, not a second renderer.
#[test]
fn unrotated_tiers_are_the_protocol_bytes_decoded() {
    let dir = Fixture::new("media-plain");
    let (state, ids) = catalog_with(&dir, &["a.jpg"], |root, n| {
        write_jpeg(root, n, 2600, 1700);
    });
    for kind in [ImageKind::Thumb, ImageKind::Preview, ImageKind::Zoom] {
        let key = JobKey::photo(ids[0], kind);
        let bytes = render_bytes(&state, key.clone()).unwrap();
        let decoded = render_image(&state, key).unwrap();
        assert!(!decoded.video_tile);
        assert_eq!(decoded.image.to_rgb8(), decode(&bytes), "{kind:?}");
    }
}

/// A user rotation: `render_image` rotates the cached pixels (no re-encode), so it equals the
/// rotated decode of the cached JPEG exactly, has the protocol's dimensions, and leaves the
/// same persistent thumbnail file `render_bytes` writes.
#[test]
fn a_rotated_photo_rotates_pixels_and_keeps_the_same_persistent_thumb() {
    let dir = Fixture::new("media-rot");
    let (state, ids) = catalog_with(&dir, &["r.jpg"], |root, n| {
        write_jpeg(root, n, 1200, 800);
    });
    let id = ids[0];
    state.catalog.lock().unwrap().as_ref().unwrap().set_photo_rotation(id, 90).unwrap();
    let key = JobKey::photo(id, ImageKind::Thumb);

    let bytes = render_bytes(&state, key.clone()).unwrap();
    let from_bytes = std::fs::read(chairphoto_core::thumbnails::persistent_thumb_path(id)).unwrap();
    std::fs::remove_file(chairphoto_core::thumbnails::persistent_thumb_path(id)).unwrap();

    let decoded = render_image(&state, key).unwrap().image;
    let from_image = std::fs::read(chairphoto_core::thumbnails::persistent_thumb_path(id)).unwrap();
    assert_eq!(from_bytes, from_image, "the persistent thumbnail must not depend on the front end");
    assert_eq!(from_bytes, bytes);

    let abs = dir.join("photos").join("r.jpg");
    let cached = chairphoto_core::thumbnails::thumbnail_bytes(&abs).unwrap();
    let expected = image::load_from_memory(&cached).unwrap().rotate90().to_rgb8();
    assert_eq!(decoded.to_rgb8(), expected);
    assert_eq!((decoded.width(), decoded.height()), image::load_from_memory(&bytes).unwrap().to_rgb8().dimensions());
}

/// Every rotation on the preview and zoom tiers: `render_image` is the cached tier's JPEG,
/// decoded and rotated as pixels, with the protocol's dimensions.
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
            let cached = match kind {
                ImageKind::Preview => chairphoto_core::thumbnails::preview_bytes(&abs).unwrap(),
                _ => chairphoto_core::thumbnails::zoom_bytes(&abs).unwrap(),
            };
            let plain = image::load_from_memory(&cached).unwrap();
            let expected = match degrees {
                90 => plain.rotate90(),
                180 => plain.rotate180(),
                _ => plain.rotate270(),
            }
            .to_rgb8();
            let decoded = render_image(&state, JobKey::photo(id, kind)).unwrap().image.to_rgb8();
            assert_eq!(decoded, expected, "{kind:?} at {degrees}°");
            let served = decode(&render_bytes(&state, JobKey::photo(id, kind)).unwrap());
            assert_eq!(decoded.dimensions(), served.dimensions(), "{kind:?} at {degrees}°");
        }
    }
}

/// A rotated photo with a cover version: the thumbnail is the cover render, rotated — not
/// the plain thumbnail — with the protocol's dimensions.
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
    let served = decode(&render_bytes(&state, JobKey::photo(id, ImageKind::Thumb)).unwrap());
    assert_eq!(decoded.dimensions(), served.dimensions());
}

/// Original gone: the thumbnail tier falls back to the persistent copy, as the protocol does;
/// preview has no fallback and errors.
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
    // The byte path is unchanged: it still reports the failure.
    assert!(render_bytes(&state, JobKey::photo(ids[0], ImageKind::Thumb)).is_err());
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

/// The Darkroom frame without the encode: for a base-only render (lossless PNG on the byte
/// path) the pixels are identical; for a full render the size matches and the JPEG is close.
#[cfg(feature = "edit")]
#[test]
fn an_edit_frame_is_the_protocol_render_without_the_encode() {
    use chairphoto_core::image_pool::EditJob;
    use chairphoto_core::media::{render_edit_bytes, render_edit_image};
    use chairphoto_core::plugins::edit::SourceToken;

    let dir = Fixture::new("media-edit");
    let (state, ids) = catalog_with(&dir, &["e.jpg"], |root, n| {
        write_jpeg(root, n, 2400, 1600);
    });
    let record = r#"{"straighten": 4, "tone": {"ev": 0.4, "contrast": 0.2}, "vignette": -0.3}"#;
    let job = |max_edge, base_only| EditJob {
        photo_id: ids[0],
        edit_json: record.into(),
        max_edge,
        hi_res: false,
        base_only,
        source: SourceToken::Preview,
        clip: false,
        catalog_epoch: 0,
    };

    let base = job(720, true);
    let png = render_edit_bytes(&state, &base).unwrap();
    let frame = render_edit_image(&state, &base).unwrap();
    assert_eq!(frame.to_rgb8(), decode(&png), "a base-only frame is the PNG's pixels");

    let full = job(1400, false);
    let jpeg = decode(&render_edit_bytes(&state, &full).unwrap());
    let frame = render_edit_image(&state, &full).unwrap().to_rgb8();
    assert_eq!(frame.dimensions(), jpeg.dimensions());
    assert!(frame.width().max(frame.height()) <= 1400);
    let mean_abs: f64 = frame
        .as_raw()
        .iter()
        .zip(jpeg.as_raw())
        .map(|(a, b)| (*a as f64 - *b as f64).abs())
        .sum::<f64>()
        / frame.as_raw().len() as f64;
    assert!(mean_abs < 3.0, "JPEG q90 of the same frame drifted by {mean_abs:.2} on average");

    // The same frame through the pool's key.
    let via_key = render_image(&state, JobKey::Edit(base)).unwrap();
    assert_eq!(via_key.image.to_rgb8(), decode(&png));
}
