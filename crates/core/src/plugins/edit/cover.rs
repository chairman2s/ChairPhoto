//! Cover thumbnails (docs/editing.md § Cover): when a version is a photo's cover, the
//! Library grid shows that version's look. The `thumb://` handler asks here for a
//! 512 px render of the cover's settings — settings only; the original and its preview
//! are read, never written — and serves it instead of the camera's embedded preview.
//!
//! Renders are cached on disk, keyed by the original (path, mtime, size) and the exact
//! settings, so a thumbnail is rendered once per look: a changed cover or a changed file is
//! a new key. Engine-1 versions render from the 2048 px preview proxy (tens of ms);
//! engine-2 versions render from the RAW working image — the `.rawf` decode cache when it
//! holds the file, else a LibRaw decode (under its crash marker) that also fills that
//! cache. Any failure is an `Err`, and the caller falls back to the plain thumbnail.

use std::path::{Path, PathBuf};

/// Long edge of a cover thumbnail — the grid's thumbnail tier.
pub const COVER_EDGE: u32 = 512;
/// Bumped when the cover render changes, so old cached covers are never served.
///
/// v2 (#245): an engine-1 cover renders from the 2048 px preview, which before 2497fa2 was
/// upscaled for an original under 2048 px — so a v1 cover of an original under 512 px is
/// enlarged too. `thumbnails::cleanup_stale_caches` removes the old `cover512v1` directory.
const COVER_FORMAT: u32 = 2;

fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Where the cover render for (`path` as it is now, `edit_json`) lives.
fn cache_path(root: &Path, path: &Path, edit_json: &str) -> Result<PathBuf, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let key = format!("{}|{mtime}|{}|{edit_json}", path.display(), meta.len());
    Ok(root.join(format!("cover{COVER_EDGE}v{COVER_FORMAT}")).join(format!("{:016x}.jpg", fnv1a(&key))))
}

/// The cover thumbnail JPEG for `path` rendered with `edit_json`: from the disk cache, or
/// rendered and cached. Not rotated — the caller applies the user rotation like it does
/// for the plain thumbnail. `photo_id` is `catalog`'s (#259: an engine-2 cover reuses only
/// that catalog's resident decode).
pub fn cover_thumb(
    path: &Path,
    catalog: crate::app::CatalogIdentity,
    photo_id: i64,
    edit_json: &str,
) -> Result<Vec<u8>, String> {
    cover_thumb_in(&crate::thumbnails::cache_dir().join("chairphoto"), path, photo_id, edit_json, |p, id, json| {
        render(p, catalog, id, json)
    })
}

fn cover_thumb_in(
    root: &Path,
    path: &Path,
    photo_id: i64,
    edit_json: &str,
    render: impl FnOnce(&Path, i64, &str) -> Result<Vec<u8>, String>,
) -> Result<Vec<u8>, String> {
    let file = cache_path(root, path, edit_json)?;
    if let Ok(bytes) = std::fs::read(&file) {
        if bytes.len() > 4 && bytes.starts_with(&[0xFF, 0xD8]) {
            return Ok(bytes);
        }
    }
    let bytes = render(path, photo_id, edit_json)?;
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
        let tmp = file.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &file);
        }
    }
    Ok(bytes)
}

fn render(path: &Path, catalog: crate::app::CatalogIdentity, photo_id: i64, edit_json: &str) -> Result<Vec<u8>, String> {
    if super::record_engine(edit_json) == 2 {
        return render_engine2(path, catalog, photo_id, edit_json);
    }
    let preview = crate::thumbnails::preview_bytes(path)?;
    super::render_jpeg(&preview, edit_json, COVER_EDGE)
}

#[cfg(feature = "raw")]
fn render_engine2(path: &Path, catalog: crate::app::CatalogIdentity, photo_id: i64, edit_json: &str) -> Result<Vec<u8>, String> {
    use super::{render_image_opts, RenderOpts, RenderSource};
    let budget = crate::develop::cache::DEFAULT_BUDGET_GB * 1024 * 1024 * 1024;
    // The session's image when it holds this catalog's photo, else one bounded offline load.
    let (token, image) = crate::develop::offline::working_image_for(catalog, photo_id, path, budget)?;
    let out = render_image_opts(RenderSource::Working { token, image }, edit_json, COVER_EDGE, RenderOpts::default())?;
    super::encode_jpeg(&out, 85)
}

#[cfg(not(feature = "raw"))]
fn render_engine2(_path: &Path, _catalog: crate::app::CatalogIdentity, _photo_id: i64, _edit_json: &str) -> Result<Vec<u8>, String> {
    Err("this cover was developed on the RAW engine, which this build lacks".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("chairphoto-cover-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Covers cache under `cover512v2`: `cover512v1` holds covers rendered from the upscaled
    /// pre-#245 previews, which must never be served (and which the stale-cache cleanup removes).
    #[test]
    fn covers_cache_under_the_post_245_directory() {
        let dir = tmp("dir-name");
        let original = dir.join("a.jpg");
        std::fs::write(&original, b"x").unwrap();
        let file = cache_path(&dir, &original, "{}").unwrap();
        assert_eq!(file.parent().unwrap().file_name().unwrap(), "cover512v2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cover renders once per (file, settings); a new look or a changed file renders
    /// again; a damaged cache file is re-rendered rather than served.
    #[test]
    fn a_cover_renders_once_per_look_and_file() {
        let d = tmp("once");
        let root = d.join("cache");
        let file = d.join("a.ARW");
        std::fs::write(&file, b"original").unwrap();
        let calls = std::cell::Cell::new(0);
        let fake = |_: &Path, _: i64, json: &str| -> Result<Vec<u8>, String> {
            calls.set(calls.get() + 1);
            Ok([&[0xFF, 0xD8][..], json.as_bytes()].concat())
        };
        let a = cover_thumb_in(&root, &file, 1, r#"{"tone":{"ev":1}}"#, fake).unwrap();
        let again = cover_thumb_in(&root, &file, 1, r#"{"tone":{"ev":1}}"#, fake).unwrap();
        assert_eq!(a, again);
        assert_eq!(calls.get(), 1, "the second request is a cache hit");

        cover_thumb_in(&root, &file, 1, r#"{"tone":{"ev":2}}"#, fake).unwrap();
        assert_eq!(calls.get(), 2, "a new look renders");

        std::fs::write(&file, b"original, replaced").unwrap();
        cover_thumb_in(&root, &file, 1, r#"{"tone":{"ev":1}}"#, fake).unwrap();
        assert_eq!(calls.get(), 3, "a changed file renders");

        let cached = cache_path(&root, &file, r#"{"tone":{"ev":1}}"#).unwrap();
        std::fs::write(&cached, b"garbage").unwrap();
        cover_thumb_in(&root, &file, 1, r#"{"tone":{"ev":1}}"#, fake).unwrap();
        assert_eq!(calls.get(), 4, "a damaged cache file is rendered again, not served");
        std::fs::remove_dir_all(&d).ok();
    }

    /// A render failure is an error (the caller falls back to the plain thumbnail) and
    /// leaves nothing in the cache.
    #[test]
    fn a_failed_render_is_an_error_and_caches_nothing() {
        let d = tmp("fail");
        let root = d.join("cache");
        let file = d.join("a.ARW");
        std::fs::write(&file, b"x").unwrap();
        let err = cover_thumb_in(&root, &file, 1, "{}", |_, _, _| Err("boom".into())).unwrap_err();
        assert_eq!(err, "boom");
        assert!(!cache_path(&root, &file, "{}").unwrap().exists());
        std::fs::remove_dir_all(&d).ok();
    }
}
