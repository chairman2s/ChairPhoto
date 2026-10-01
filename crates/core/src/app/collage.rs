//! Collage bodies shared by the Tauri commands and the GPUI Collage module (docs/collage.md):
//! the justified auto-arrange, the freeform render to a folder, and the freeform render saved
//! into the library.
//!
//! All three are blocking (catalog lock, preview decodes, compositing): run them on a worker.
//! Each takes `expected`: the catalog the photo ids were read from — `Some` from a front end
//! that captured it, `None` for "the open one". Ids are resolved under that catalog's lock
//! and, for a library save, the new photo is indexed only while that catalog is still open
//! (`with_catalog_as`); a switch in between fails closed with [`CATALOG_CHANGED`] and the
//! rendered file is removed rather than left unindexed in the old library.

use super::{AppState, CatalogIdentity, CATALOG_CHANGED};
use crate::catalog::Catalog;
use crate::collage::{CollageOptions, Fit, Placement};
use image::{DynamicImage, RgbaImage};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Mosaic options as the dialogs send them (serde camelCase): `aspect` is `"free"`/`None` or
/// a `"W:H"` ratio and `background` a CSS-ish colour, parsed into the engine's typed
/// [`CollageOptions`] by [`CollageOptionsDto::to_engine`].
#[derive(serde::Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CollageOptionsDto {
    pub width: u32,
    /// `null`/`"free"` = grow with rows; else a ratio like `"1:1"`, `"4:5"`, `"9:16"`.
    pub aspect: Option<String>,
    pub row_height: u32,
    pub gap: u32,
    /// Mat color as `#rgb`/`#rrggbb`/`#rrggbbaa` or `rgba(r,g,b,a)`.
    pub background: String,
    /// `"contain"` (whole photos) or `"cover"` (uniform, lightly cropped tiles).
    pub fit: String,
    pub border_width: u32,
    pub corner_radius: u32,
}

impl CollageOptionsDto {
    pub fn to_engine(&self) -> CollageOptions {
        CollageOptions {
            width: self.width,
            aspect: parse_aspect(self.aspect.as_deref()),
            row_height: self.row_height,
            gap: self.gap,
            background: parse_color(&self.background),
            fit: if self.fit.eq_ignore_ascii_case("cover") { Fit::Cover } else { Fit::Contain },
            border_width: self.border_width,
            corner_radius: self.corner_radius,
        }
    }
}

fn half() -> f32 {
    0.5
}
fn one() -> f32 {
    1.0
}

/// One freeform tile from the canvas editor: normalized `x/y/w/h` (0–1 of the canvas), stacking
/// order `z`, and the cover-crop's focal offset `ox/oy` and `zoom`. Matches the TS `Placement`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PlacementDto {
    pub photo_id: i64,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub z: i64,
    /// Focal offset (0..1; default 0.5 centered) for the cover-crop — pan within the frame.
    #[serde(default = "half")]
    pub ox: f32,
    #[serde(default = "half")]
    pub oy: f32,
    /// Zoom factor (≥1; default 1) for the cover-crop.
    #[serde(default = "one")]
    pub zoom: f32,
}

/// Freeform render options: the canvas pixel size + mat/border/corner styling.
#[derive(serde::Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FreeformOptionsDto {
    pub width: u32,
    pub height: u32,
    pub background: String,
    pub border_width: u32,
    pub corner_radius: u32,
}

/// Loads one photo's upright preview for compositing. The production one is
/// [`cached_preview`]; tests hand in one that needs no thumbnail cache.
pub type PreviewLoader = Arc<dyn Fn(&Path) -> Result<DynamicImage, String> + Send + Sync>;

/// The production loader: the photo's cached embedded preview ([`crate::thumbnails::zoom_bytes`],
/// never a full RAW decode, which keeps compositing memory-bounded), upright.
pub fn cached_preview() -> PreviewLoader {
    Arc::new(|path| decode_upright(&crate::thumbnails::zoom_bytes(path)?))
}

/// Parse `"W:H"` (or `"free"`/empty) into the engine's optional aspect ratio. Anything
/// unparseable falls back to Free rather than erroring — the layout still produces a valid
/// (row-growing) canvas.
pub fn parse_aspect(s: Option<&str>) -> Option<(u32, u32)> {
    let s = s.unwrap_or("").trim();
    if s.is_empty() || s.eq_ignore_ascii_case("free") {
        return None;
    }
    let (w, h) = s.split_once(':')?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

/// Parse a `#rgb`/`#rrggbb`/`#rrggbbaa` hex or `rgba(r,g,b,a)` color into RGBA bytes.
/// Defaults to opaque white on anything unrecognized (the natural mat color).
pub fn parse_color(s: &str) -> [u8; 4] {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let parse2 = |i: usize| hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex.len() {
            // #rgb → expand each nibble (e.g. "f0a" → ff 00 aa).
            3 => {
                let nib = |c: char| c.to_digit(16).map(|v| (v * 17) as u8);
                let mut it = hex.chars();
                if let (Some(r), Some(g), Some(b)) =
                    (it.next().and_then(nib), it.next().and_then(nib), it.next().and_then(nib))
                {
                    return [r, g, b, 255];
                }
            }
            6 => {
                if let (Some(r), Some(g), Some(b)) = (parse2(0), parse2(2), parse2(4)) {
                    return [r, g, b, 255];
                }
            }
            8 => {
                if let (Some(r), Some(g), Some(b), Some(a)) = (parse2(0), parse2(2), parse2(4), parse2(6)) {
                    return [r, g, b, a];
                }
            }
            _ => {}
        }
    } else if let Some(inner) =
        s.strip_prefix("rgba(").or_else(|| s.strip_prefix("rgb(")).and_then(|t| t.strip_suffix(')'))
    {
        let mut parts = inner.split(',').map(str::trim);
        let r = parts.next().and_then(|v| v.parse::<u32>().ok());
        let g = parts.next().and_then(|v| v.parse::<u32>().ok());
        let b = parts.next().and_then(|v| v.parse::<u32>().ok());
        // Alpha is 0.0–1.0 in CSS rgba(); default opaque when absent.
        let a = parts
            .next()
            .and_then(|v| v.parse::<f32>().ok())
            .map(|f| (f.clamp(0.0, 1.0) * 255.0).round() as u8)
            .unwrap_or(255);
        if let (Some(r), Some(g), Some(b)) = (r, g, b) {
            return [r.min(255) as u8, g.min(255) as u8, b.min(255) as u8, a];
        }
    }
    [255, 255, 255, 255]
}

/// Decode JPEG/preview bytes and bake in EXIF orientation (so a portrait shot isn't sideways
/// in the collage — `image::load_from_memory` does not auto-rotate).
pub fn decode_upright(bytes: &[u8]) -> Result<DynamicImage, String> {
    use image::ImageDecoder;
    let mut decoder = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_decoder()
        .map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// Write a composed RGBA canvas to `dest`: PNG keeps alpha (a transparent mat survives); any
/// other format → JPEG q92 with alpha flattened onto `background` (JPEG has no alpha).
pub fn write_canvas(canvas: RgbaImage, dest: &Path, is_png: bool, background: [u8; 4]) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if is_png {
        return canvas.save_with_format(dest, image::ImageFormat::Png).map_err(|e| e.to_string());
    }
    let mut flat = image::RgbImage::new(canvas.width(), canvas.height());
    for (x, y, px) in canvas.enumerate_pixels() {
        let a = px[3] as f32 / 255.0;
        let blend = |c: usize| (px[c] as f32 * a + background[c] as f32 * (1.0 - a)).round() as u8;
        flat.put_pixel(x, y, image::Rgb([blend(0), blend(1), blend(2)]));
    }
    use image::codecs::jpeg::JpegEncoder;
    let file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut writer = std::io::BufWriter::new(file);
    DynamicImage::ImageRgb8(flat)
        .write_with_encoder(JpegEncoder::new_with_quality(&mut writer, 92))
        .map_err(|e| e.to_string())
}

/// Run `f` against the catalog `expected` names (or the open one), returning that catalog's
/// identity with the result, so a later write can be bound to the same catalog.
fn with_bound<T>(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    f: impl FnOnce(&Catalog) -> crate::catalog::Result<T>,
) -> Result<(CatalogIdentity, T), String> {
    match expected {
        Some(id) => super::with_catalog_as(state, id, f).map(|t| (id, t)),
        None => super::with_catalog_identified(state, f),
    }
}

/// Resolve every id to a reachable original, failing fast on an offline volume.
fn resolve_all(c: &Catalog, ids: impl IntoIterator<Item = i64>) -> crate::catalog::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for id in ids {
        match c.resolve_photo_path(id)? {
            Some(p) => out.push(p),
            None => {
                return Err(crate::catalog::CatalogError::NotFound(format!(
                    "photo {id} is not currently reachable (its volume may be offline)"
                )))
            }
        }
    }
    Ok(out)
}

/// Lay the photos out as the justified mosaic and return normalized placements (z = order),
/// seeding the freeform canvas. Aspects come from the upright preview so the layout matches
/// the rendered output.
pub fn auto_arrange(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    photo_ids: &[i64],
    opts: &CollageOptionsDto,
    load: &PreviewLoader,
) -> Result<Vec<PlacementDto>, String> {
    if photo_ids.is_empty() {
        return Ok(Vec::new());
    }
    let (_, paths) = with_bound(state, expected, |c| resolve_all(c, photo_ids.iter().copied()))?;
    let engine_opts = CollageOptions {
        width: opts.width.max(1),
        aspect: parse_aspect(opts.aspect.as_deref()),
        row_height: opts.row_height.max(1),
        gap: opts.gap,
        background: [0, 0, 0, 255],
        fit: Fit::Contain,
        border_width: 0,
        corner_radius: 0,
    };
    let mut aspects = Vec::with_capacity(paths.len());
    for path in &paths {
        let img = load(path)?;
        aspects.push((img.width() as f32 / img.height().max(1) as f32).max(0.01));
    }
    let (rects, height) = crate::collage::layout(&aspects, &engine_opts);
    let cw = engine_opts.width as f32;
    let ch = height.max(1) as f32;
    Ok(photo_ids
        .iter()
        .zip(rects.iter())
        .enumerate()
        .map(|(i, (&photo_id, r))| PlacementDto {
            photo_id,
            x: r.x as f32 / cw,
            y: r.y as f32 / ch,
            w: r.w as f32 / cw,
            h: r.h as f32 / ch,
            z: i as i64,
            ox: 0.5,
            oy: 0.5,
            zoom: 1.0,
        })
        .collect())
}

/// Composite the freeform collage of `paths` (one per placement) to `dest`.
fn render_freeform(
    paths: &[PathBuf],
    placements: &[PlacementDto],
    opts: &FreeformOptionsDto,
    is_png: bool,
    dest: &Path,
    load: &PreviewLoader,
) -> Result<(), String> {
    let bg = parse_color(&opts.background);
    let engine_opts = CollageOptions {
        width: opts.width.max(1),
        aspect: None,
        row_height: 1,
        gap: 0,
        background: bg,
        fit: Fit::Contain,
        border_width: opts.border_width,
        corner_radius: opts.corner_radius,
    };
    let mut items = Vec::with_capacity(paths.len());
    for (path, p) in paths.iter().zip(placements) {
        let rect = Placement { x: p.x, y: p.y, w: p.w, h: p.h, z: p.z, ox: p.ox, oy: p.oy, zoom: p.zoom };
        items.push((load(path)?, rect));
    }
    let canvas = crate::collage::compose_freeform(items, opts.width.max(1), opts.height.max(1), &engine_opts);
    write_canvas(canvas, dest, is_png, bg)
}

fn is_png(format: &str) -> bool {
    format.eq_ignore_ascii_case("png")
}

/// Composite a freeform collage from explicit placements and write it to a fresh
/// `collage.<ext>` in `dest_dir` (`~` expanded). Returns the output path. Nothing in the
/// catalog changes.
pub fn make_freeform(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    placements: &[PlacementDto],
    opts: &FreeformOptionsDto,
    format: &str,
    dest_dir: &str,
    load: &PreviewLoader,
) -> Result<PathBuf, String> {
    if placements.is_empty() {
        return Err("No photos in the collage".into());
    }
    if dest_dir.trim().is_empty() {
        return Err("Choose an output folder.".into());
    }
    let (_, paths) = with_bound(state, expected, |c| resolve_all(c, placements.iter().map(|p| p.photo_id)))?;
    let png = is_png(format);
    let ext = if png { "png" } else { "jpg" };
    let dest = super::unique_path(&super::expand_home(dest_dir.trim()).join(format!("collage.{ext}")));
    render_freeform(&paths, placements, opts, png, &dest, load)?;
    Ok(dest)
}

/// Composite a freeform collage, save it into the library (`<root>/Collages/`), index it
/// (UUID + sidecar + metadata, no import batch) and tag it `Collage/<kind>` (both tags
/// non-exportable). Returns the new photo id.
///
/// The ids, the root and the index are all bound to one catalog: `expected`, or the one open
/// when this starts. If another catalog opens while the collage renders, the index fails
/// closed with [`CATALOG_CHANGED`] and the rendered file is removed.
pub fn save_to_catalog(
    state: &AppState,
    expected: Option<CatalogIdentity>,
    placements: &[PlacementDto],
    opts: &FreeformOptionsDto,
    format: &str,
    kind: &str,
    load: &PreviewLoader,
) -> Result<i64, String> {
    if placements.is_empty() {
        return Err("No photos in the collage".into());
    }
    let (identity, (paths, root)) = with_bound(state, expected, |c| {
        Ok((resolve_all(c, placements.iter().map(|p| p.photo_id))?, c.root().to_path_buf()))
    })?;
    let png = is_png(format);
    let ext = if png { "png" } else { "jpg" };
    let dest = super::unique_path(&root.join("Collages").join(format!("collage.{ext}")));
    render_freeform(&paths, placements, opts, png, &dest, load)?;

    let kind = kind.trim().to_string();
    // The identity check and the index share one catalog lock hold (`with_catalog_as`'s
    // contract, written out because `index_generated_file` answers a plain message).
    let indexed = (|| {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        if !identity.is(catalog) {
            return Err(CATALOG_CHANGED.to_string());
        }
        let photo_id = crate::scanner::index_generated_file(catalog, &dest)?;
        // Organizational tags: the parent AND the leaf are non-exportable, so they are never
        // emitted as keywords on export/publish.
        if !kind.is_empty() {
            if let Ok(parent) = catalog.create_tag("Collage") {
                let _ = catalog.set_tag_exportable(parent, false);
            }
            if let Ok(tag_id) = catalog.create_tag(&format!("Collage/{kind}")) {
                let _ = catalog.set_tag_exportable(tag_id, false);
                let _ = catalog.assign_tag(photo_id, tag_id);
            }
        }
        Ok(photo_id)
    })();
    if indexed.as_ref().is_err_and(|e| e == CATALOG_CHANGED) {
        // Our own output (the unique path did not exist before), never indexed anywhere.
        let _ = std::fs::remove_file(&dest);
    }
    indexed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestTmpDir;

    #[test]
    fn aspect_free_and_empty_are_none() {
        assert_eq!(parse_aspect(None), None);
        assert_eq!(parse_aspect(Some("")), None);
        assert_eq!(parse_aspect(Some("free")), None);
        assert_eq!(parse_aspect(Some("Free")), None);
    }

    #[test]
    fn aspect_ratios_parse() {
        assert_eq!(parse_aspect(Some("1:1")), Some((1, 1)));
        assert_eq!(parse_aspect(Some("4:5")), Some((4, 5)));
        assert_eq!(parse_aspect(Some(" 16 : 9 ")), Some((16, 9)));
    }

    #[test]
    fn aspect_garbage_falls_back_to_free() {
        assert_eq!(parse_aspect(Some("nonsense")), None);
        assert_eq!(parse_aspect(Some("1:0")), None);
        assert_eq!(parse_aspect(Some("0:1")), None);
    }

    #[test]
    fn color_hex_forms() {
        assert_eq!(parse_color("#ffffff"), [255, 255, 255, 255]);
        assert_eq!(parse_color("#000000"), [0, 0, 0, 255]);
        assert_eq!(parse_color("#f0a"), [255, 0, 170, 255]);
        assert_eq!(parse_color("#11223344"), [0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn color_rgba_and_fallback() {
        assert_eq!(parse_color("rgba(10, 20, 30, 0.5)"), [10, 20, 30, 128]);
        assert_eq!(parse_color("rgb(1,2,3)"), [1, 2, 3, 255]);
        // Unrecognized → opaque white (a sensible default mat).
        assert_eq!(parse_color("bogus"), [255, 255, 255, 255]);
        // A multi-byte character where a hex pair would be does not panic.
        assert_eq!(parse_color("#ééé"), [255, 255, 255, 255]);
    }

    /// A loader that decodes the file directly (no thumbnail cache).
    fn direct() -> PreviewLoader {
        Arc::new(|path| decode_upright(&std::fs::read(path).map_err(|e| e.to_string())?))
    }

    fn setup(tag: &str) -> (TestTmpDir, AppState, Vec<i64>) {
        let dir = TestTmpDir::new(&format!("collage-{tag}"));
        let root = dir.join("library");
        std::fs::create_dir_all(&root).unwrap();
        let c = Catalog::open(&dir.join("c.chairphoto"), &root).unwrap();
        let ids = [(40u32, 20u32, [255u8, 0, 0]), (20, 40, [0, 0, 255])]
            .iter()
            .enumerate()
            .map(|(i, (w, h, rgb))| {
                let p = root.join(format!("IMG_{i}.png"));
                image::RgbImage::from_pixel(*w, *h, image::Rgb(*rgb)).save(&p).unwrap();
                let len = std::fs::metadata(&p).unwrap().len() as i64;
                c.upsert_photo(&p, None, 0, len).unwrap().id
            })
            .collect();
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(c);
        (dir, state, ids)
    }

    fn mosaic() -> CollageOptionsDto {
        CollageOptionsDto {
            width: 2000,
            aspect: Some("1:1".into()),
            row_height: 460,
            gap: 10,
            background: "#ffffff".into(),
            fit: "contain".into(),
            border_width: 0,
            corner_radius: 0,
        }
    }

    fn halves(ids: &[i64]) -> Vec<PlacementDto> {
        ids.iter()
            .enumerate()
            .map(|(i, &photo_id)| PlacementDto {
                photo_id,
                x: i as f32 * 0.5,
                y: 0.0,
                w: 0.5,
                h: 1.0,
                z: i as i64,
                ox: 0.5,
                oy: 0.5,
                zoom: 1.0,
            })
            .collect()
    }

    fn freeform() -> FreeformOptionsDto {
        FreeformOptionsDto { width: 100, height: 50, background: "#00ff00".into(), border_width: 0, corner_radius: 0 }
    }

    fn switch(state: &AppState, dir: &Path) {
        crate::app::catalogs::detach_catalog_and_trip_jobs(state).unwrap();
        let b = Catalog::open(&dir.join("b.chairphoto"), &dir.join("b")).unwrap();
        crate::app::catalogs::publish_catalog_and_reset_jobs(state, b).unwrap();
    }

    #[test]
    fn auto_arrange_keeps_order_and_aspects() {
        let (_dir, state, ids) = setup("arrange");
        let p = auto_arrange(&state, None, &ids, &mosaic(), &direct()).unwrap();
        assert_eq!(p.iter().map(|p| p.photo_id).collect::<Vec<_>>(), ids);
        assert_eq!(p.iter().map(|p| p.z).collect::<Vec<_>>(), [0, 1]);
        // The landscape tile is wider than tall, the portrait taller than wide (in a 1:1 canvas).
        assert!(p[0].w > p[0].h && p[1].h > p[1].w, "{p:?}");
    }

    #[test]
    fn a_folder_render_writes_a_fresh_file_and_touches_neither_catalog_nor_originals() {
        let (dir, state, ids) = setup("folder");
        let out = dir.join("out");
        let before = std::fs::read(dir.join("library/IMG_0.png")).unwrap();
        let first = make_freeform(&state, None, &halves(&ids), &freeform(), "png", out.to_str().unwrap(), &direct()).unwrap();
        let second = make_freeform(&state, None, &halves(&ids), &freeform(), "jpeg", out.to_str().unwrap(), &direct()).unwrap();
        let third = make_freeform(&state, None, &halves(&ids), &freeform(), "png", out.to_str().unwrap(), &direct()).unwrap();
        assert_eq!(first, out.join("collage.png"));
        assert_eq!(second, out.join("collage.jpg"));
        assert_eq!(third, out.join("collage (2).png"), "never clobbers");
        let img = image::open(&first).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (100, 50));
        assert_eq!(img.get_pixel(25, 25).0[..3], [255, 0, 0], "left half is the first photo");
        assert_eq!(img.get_pixel(75, 25).0[..3], [0, 0, 255], "right half is the second");
        assert_eq!(std::fs::read(dir.join("library/IMG_0.png")).unwrap(), before, "original untouched");
        let count = state.catalog.lock().unwrap().as_ref().unwrap().count_photos(&Default::default()).unwrap();
        assert_eq!(count, 2, "a folder render indexes nothing");
    }

    #[test]
    fn a_library_save_indexes_and_tags_the_collage_non_exportable() {
        let (dir, state, ids) = setup("library");
        let id = save_to_catalog(&state, None, &halves(&ids), &freeform(), "jpeg", "Grid", &direct()).unwrap();
        let guard = state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        let path = c.require_photo_path(id).unwrap();
        assert_eq!(path, dir.join("library/Collages/collage.jpg"));
        let tags = c.get_photo_tags(id).unwrap();
        let leaf = tags.iter().find(|t| t.full_path == "Collage/Grid").expect("tagged Collage/Grid");
        let parent = leaf.parent_id.expect("under Collage");
        assert!(!c.tag_exportable(leaf.id).unwrap() && !c.tag_exportable(parent).unwrap());
    }

    /// The ids were read from catalog A; B is open by the time the dialog saves. Every entry
    /// point refuses, and B gains nothing.
    #[test]
    fn writes_bound_to_a_catalog_that_is_no_longer_open_fail_closed() {
        let (dir, state, ids) = setup("identity");
        let a = crate::app::catalog_identity(&state).unwrap();
        switch(&state, &dir);
        let p = halves(&ids);
        assert_eq!(auto_arrange(&state, Some(a), &ids, &mosaic(), &direct()).unwrap_err(), CATALOG_CHANGED);
        assert_eq!(
            make_freeform(&state, Some(a), &p, &freeform(), "png", dir.join("out").to_str().unwrap(), &direct()).unwrap_err(),
            CATALOG_CHANGED
        );
        assert_eq!(save_to_catalog(&state, Some(a), &p, &freeform(), "png", "Grid", &direct()).unwrap_err(), CATALOG_CHANGED);
        let count = state.catalog.lock().unwrap().as_ref().unwrap().count_photos(&Default::default()).unwrap();
        assert_eq!(count, 0, "the new catalog gained nothing");
        assert!(!dir.join("out").exists());
    }

    /// **Forced interleaving.** A switch lands while the library save renders (inside the
    /// preview loader): the index into the new catalog is refused and the rendered file is
    /// removed from the old library.
    #[test]
    fn a_switch_during_a_library_save_indexes_nothing_and_removes_the_file() {
        let (dir, state, ids) = setup("save-switch");
        let switched = std::sync::atomic::AtomicBool::new(false);
        let (s, d) = (state.clone(), dir.to_path_buf());
        let switching: PreviewLoader = Arc::new(move |path| {
            if !switched.swap(true, std::sync::atomic::Ordering::SeqCst) {
                switch(&s, &d);
            }
            decode_upright(&std::fs::read(path).map_err(|e| e.to_string())?)
        });
        let err = save_to_catalog(&state, None, &halves(&ids), &freeform(), "png", "Grid", &switching).unwrap_err();
        assert_eq!(err, CATALOG_CHANGED);
        assert!(!dir.join("library/Collages/collage.png").exists(), "the unindexed collage is removed");
        let count = state.catalog.lock().unwrap().as_ref().unwrap().count_photos(&Default::default()).unwrap();
        assert_eq!(count, 0, "the new catalog gained nothing");
    }
}
