//! One-way export (see docs/storage-and-import.md, "Export"). Resolves each photo's
//! best available original via the catalog resolver and writes it to a destination
//! folder according to a preset. Never touches the catalog or the source files.
//!
//! Presets implemented here (G1):
//! - **Hand-off**: the RAW/original + its existing XMP sidecar (interchange for
//!   darktable/Lightroom — metadata rides in the sidecar).
//! - **Show off**: a JPEG. With no edited version selected it's the embedded full-size
//!   preview; for the selected version it renders the crop + tone from the
//!   full-resolution source — a true RAW decode (LibRaw) when the `raw` feature is on,
//!   else the embedded preview. Keywords + rating/label + any filled IPTC fields are then
//!   embedded into the JPEG (XMP + legacy IPTC, via exiftool) and mirrored into a paired
//!   `.xmp` sidecar. (Note: rendering runs sequentially; a large Show-off selection is
//!   slow but stays off the UI thread — see the command.)
//!
//! The *Full bundle* preset (RAW + previews + catalog metadata as a `.chairphoto`
//! bundle) is the catalog-merge format and lands with F1.
//!
//! Work is split so file I/O never holds the catalog lock or blocks the UI thread:
//! [`resolve_originals`] runs under the lock (DB + path resolution); [`write_exports`]
//! is pure filesystem work and is called from `spawn_blocking`.

use crate::catalog::{Catalog, ExportKeywords, IptcFields};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExportPreset {
    /// RAW/original + its XMP sidecar.
    HandOff,
    /// A JPEG: the selected version rendered from the full-resolution source, or the
    /// embedded full-size preview when nothing is edited.
    ShowOff,
    /// Like Show-off, but resized to Instagram's feed width (1080px) — the platform
    /// downscales anything larger, so this matches what Instagram actually displays
    /// (1080×1080 square, 1080×1350 portrait, …) and avoids a double recompression.
    Instagram,
}

/// Instagram's maximum feed image width in pixels. Every supported aspect (square,
/// 4:5 portrait, 1.91:1 landscape, 9:16 story) is 1080 wide, so resizing to this width
/// (preserving aspect, never upscaling) yields the platform-correct pixel size.
const INSTAGRAM_WIDTH: u32 = 1080;

#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    pub exported: usize,
    /// Photos whose original couldn't be resolved (offline NAS / missing).
    pub skipped_offline: usize,
    pub errors: usize,
}

/// One resolved photo: its best available original plus the keywords to emit (G2). For
/// a Show-off export it also carries the selected version's edit (crop + tone) and name,
/// so the JPEG is rendered from the full-resolution source and filed per-version.
pub struct ResolvedItem {
    pub original: PathBuf,
    pub keywords: ExportKeywords,
    /// The selected version's edit record (`None` = export the unedited original).
    pub edit_json: Option<String>,
    /// The selected version's name, used to disambiguate the output filename.
    pub version_name: Option<String>,
    /// Star rating (0–5); embedded into a Show-off JPEG when > 0.
    pub rating: i64,
    /// Color label (e.g. "Green"); embedded when non-empty.
    pub label: String,
    /// Authored IPTC fields (title/caption/creator/…); only filled fields are embedded.
    pub iptc: IptcFields,
}

/// What `resolve_originals` produced: the reachable originals to write, plus counts of
/// the ones we couldn't (so the caller reports rather than silently truncating).
pub struct ResolvedExport {
    pub items: Vec<ResolvedItem>,
    pub skipped_offline: usize,
    pub resolve_errors: usize,
}

/// Resolve each photo's best available original and assemble its export keywords (in
/// `languages`; empty = canonical + neutral synonyms). Unreachable originals are
/// counted as `skipped_offline`; resolver errors are counted (not fatal) so one bad
/// row doesn't abort the export. Run this under the catalog lock, then release it.
/// `selected_version_id` is the version active in the UI (Show-off renders it from the
/// full-res source); it applies only to *its* photo — other photos in the selection
/// export unedited.
pub fn resolve_originals(
    catalog: &Catalog,
    photo_ids: &[i64],
    languages: &[String],
    selected_version_id: Option<i64>,
) -> ResolvedExport {
    let mut out = ResolvedExport {
        items: Vec::new(),
        skipped_offline: 0,
        resolve_errors: 0,
    };
    // Fetch the selected version once; attach it to the matching photo below.
    let selected = selected_version_id.and_then(|vid| catalog.get_version(vid).ok().flatten());
    for &id in photo_ids {
        match catalog.resolve_photo_path(id) {
            Ok(Some(original)) => {
                let keywords = catalog
                    .assemble_export_keywords(id, languages)
                    .unwrap_or_default();
                let (edit_json, version_name) = match &selected {
                    Some(v) if v.photo_id == id => {
                        (Some(v.edit_json.clone()), Some(v.name.clone()))
                    }
                    _ => (None, None),
                };
                // Rating/label/IPTC for the metadata stamp (Show-off). Best-effort — a
                // missing photo row or empty IPTC just means fewer fields embedded.
                let (rating, label) = catalog
                    .get_photo(id)
                    .map(|p| (p.rating, p.label))
                    .unwrap_or((0, String::new()));
                let iptc = catalog.get_iptc(id).unwrap_or_default();
                out.items.push(ResolvedItem {
                    original,
                    keywords,
                    edit_json,
                    version_name,
                    rating,
                    label,
                    iptc,
                });
            }
            Ok(None) => out.skipped_offline += 1,
            Err(_) => out.resolve_errors += 1,
        }
    }
    out
}

/// Write the resolved originals to `dest_dir` using `preset`, plus an optional reach
/// `hashtags` bundle as `hashtags.txt` (G3 — distribution labels, kept separate from
/// the per-photo content keywords). Pure filesystem work — safe to call from
/// `spawn_blocking`. Creates `dest_dir` if needed.
pub fn write_exports(
    resolved: &ResolvedExport,
    preset: ExportPreset,
    dest_dir: &Path,
    hashtags: &[String],
) -> Result<ExportResult, String> {
    std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
    let mut result = ExportResult {
        exported: 0,
        skipped_offline: resolved.skipped_offline,
        errors: resolved.resolve_errors,
    };
    for item in &resolved.items {
        let outcome = match preset {
            ExportPreset::HandOff => export_handoff(item, dest_dir),
            ExportPreset::ShowOff => export_jpeg(item, dest_dir, None),
            ExportPreset::Instagram => export_jpeg(item, dest_dir, Some(INSTAGRAM_WIDTH)),
        };
        match outcome {
            Ok(()) => result.exported += 1,
            Err(_) => result.errors += 1,
        }
    }
    if !hashtags.is_empty() {
        // Best-effort, like the keyword write: photos are already exported, so a failed
        // hashtags.txt write must not discard the whole ExportResult.
        if let Err(e) = std::fs::write(dest_dir.join("hashtags.txt"), hashtags.join(" ")) {
            eprintln!("export: hashtags.txt write failed: {e}");
            result.errors += 1;
        }
    }
    Ok(result)
}

/// Copy the original next to its XMP sidecar (if one exists), never overwriting an
/// existing file in the destination (duplicate names — possible across volumes — get a
/// " (n)" suffix; the sidecar is renamed to stay paired with its original). Then emit
/// the assembled keywords into the destination sidecar (merge-safe, G2).
fn export_handoff(item: &ResolvedItem, dest_dir: &Path) -> Result<(), String> {
    let original = &item.original;
    let name = original
        .file_name()
        .ok_or_else(|| "original has no file name".to_string())?;
    let target = unique_path(&dest_dir.join(name));
    std::fs::copy(original, &target).map_err(|e| e.to_string())?;

    let sidecar = crate::xmp::sidecar_path(original);
    if sidecar.is_file() {
        // Pair the sidecar with the (possibly disambiguated) target via the same
        // convention the keyword writer uses.
        std::fs::copy(&sidecar, crate::xmp::sidecar_path(&target)).map_err(|e| e.to_string())?;
    }
    // Emit keywords into the destination sidecar (merges into the copied one, or
    // creates it). Best-effort: the RAW + sidecar are already exported, so a malformed
    // foreign sidecar that can't be parsed must not fail the whole item — log and move on.
    if let Err(e) = crate::xmp::write_keywords(&target, &item.keywords.flat, &item.keywords.hierarchical)
    {
        eprintln!("export: keyword write failed for {}: {e}", target.display());
    }
    // Rating/label + any filled IPTC into the same destination sidecar (keywords above
    // handled them). Best-effort — the RAW is already exported.
    if let Err(e) = embed_sidecar_metadata(&crate::xmp::sidecar_path(&target), item) {
        eprintln!("export: hand-off metadata write failed for {}: {e}", target.display());
    }
    Ok(())
}

/// Write rating/label + filled IPTC fields into a hand-off destination sidecar (the `.xmp`
/// next to the copied RAW) via exiftool — merge-safe (only the named XMP tags change;
/// keywords and foreign develop data are preserved). No-op when nothing is set.
fn embed_sidecar_metadata(sidecar: &Path, item: &ResolvedItem) -> Result<(), String> {
    if !sidecar.is_file() {
        return Ok(());
    }
    let mut args: Vec<String> = Vec::new();
    if item.rating > 0 {
        args.push(format!("-XMP:Rating={}", item.rating));
    }
    push_if(&mut args, "XMP:Label", &item.label);
    let f = &item.iptc;
    push_if(&mut args, "XMP-dc:Description", &f.description);
    push_if(&mut args, "XMP-photoshop:Headline", &f.headline);
    push_if(&mut args, "XMP-dc:Title", &f.title);
    push_if(&mut args, "XMP-dc:Creator", &f.creator);
    push_if(&mut args, "XMP-dc:Rights", &f.copyright);
    push_if(&mut args, "XMP-photoshop:Credit", &f.credit);
    push_if(&mut args, "XMP-photoshop:Source", &f.source);
    push_if(&mut args, "XMP-photoshop:City", &f.city);
    push_if(&mut args, "XMP-photoshop:State", &f.state);
    push_if(&mut args, "XMP-photoshop:Country", &f.country);
    push_if(&mut args, "XMP-iptcCore:CountryCode", &f.country_code);
    if args.is_empty() {
        return Ok(());
    }
    let mut cmd = Command::new("exiftool");
    cmd.arg("-q").arg("-overwrite_original").args(&args).arg(sidecar);
    run_exiftool(cmd)
}

/// Write a `<stem>[ - <version>].jpg` (no overwrite). An *edited* version renders from
/// the full-resolution source (a true RAW decode when available) so the crop is exact;
/// an unedited export uses the cached embedded full-size preview (fast, near-native).
/// `max_width`, when set, downscales to that width (Instagram preset) — never upscales.
fn export_jpeg(item: &ResolvedItem, dest_dir: &Path, max_width: Option<u32>) -> Result<(), String> {
    let original = &item.original;
    // A version whose record is empty / `{}` has no crop or tone — treat as unedited.
    let edit_json = item.edit_json.as_deref().unwrap_or("");
    let has_edit = {
        let t = edit_json.trim();
        !t.is_empty() && t != "{}"
    };
    let jpeg = render_export_jpeg(original, edit_json, has_edit, max_width)?;

    let stem = original
        .file_stem()
        .ok_or_else(|| "original has no file stem".to_string())?;
    let mut name = stem.to_string_lossy().into_owned();
    if let Some(version) = &item.version_name {
        name.push_str(" - ");
        name.push_str(version);
    }
    let mut base = PathBuf::from(dest_dir).join(name);
    base.set_extension("jpg");
    let target = unique_path(&base);
    std::fs::write(&target, jpeg).map_err(|e| e.to_string())?;

    // Stamp the keywords + rating/label + IPTC into the JPEG and a paired sidecar.
    // Best-effort: the pixels are already written, so a metadata failure (e.g. exiftool
    // missing) must not fail the export — log and move on, like the hand-off keyword write.
    if let Err(e) = embed_export_metadata(&target, item, has_edit) {
        eprintln!("export: metadata embed failed for {}: {e}", target.display());
    }
    Ok(())
}

/// Render a single resolved item to a JPEG at `out_path` (used by the Instagram poster to
/// produce the file it uploads). `max_width` resizes like the export presets; metadata is
/// embedded into the written JPEG. Pure filesystem/CPU work — call from `spawn_blocking`.
pub fn write_item_jpeg(
    item: &ResolvedItem,
    max_width: Option<u32>,
    out_path: &Path,
) -> Result<(), String> {
    let edit_json = item.edit_json.as_deref().unwrap_or("");
    let has_edit = {
        let t = edit_json.trim();
        !t.is_empty() && t != "{}"
    };
    let jpeg = render_export_jpeg(&item.original, edit_json, has_edit, max_width)?;
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(out_path, jpeg).map_err(|e| e.to_string())?;
    if let Err(e) = embed_export_metadata(out_path, item, has_edit) {
        eprintln!("export: metadata embed failed for {}: {e}", out_path.display());
    }
    Ok(())
}

/// Like [`write_item_jpeg`] but applies a per-platform **long-edge** limit instead of a
/// fixed width: the image is downscaled so the longer of its two dimensions does not
/// exceed `max_long_edge`. `None` or `Some(0)` = full resolution (no downscale). Used by
/// the Flickr/SmugMug publish path where the user configures a per-module limit. EXIF/GPS
/// copying and JPEG quality follow the same conventions as `write_item_jpeg`.
pub fn write_item_jpeg_with_long_edge(
    item: &ResolvedItem,
    max_long_edge: Option<u32>,
    out_path: &Path,
) -> Result<(), String> {
    let edit_json = item.edit_json.as_deref().unwrap_or("");
    let has_edit = {
        let t = edit_json.trim();
        !t.is_empty() && t != "{}"
    };
    // Fast path: unedited *and* no resize → ship the embedded preview as-is (avoids a
    // needless full RAW decode + re-encode, matching the `write_item_jpeg` / Instagram path).
    let limit = max_long_edge.unwrap_or(0);
    let jpeg = if !has_edit && limit == 0 {
        crate::thumbnails::zoom_bytes(&item.original)?
    } else {
        let mut img = decode_export_source(&item.original, edit_json, has_edit)?;
        if limit > 0 {
            img = downscale_to_long_edge(img, limit);
        }
        encode_jpeg(&img, 92)?
    };

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(out_path, &jpeg).map_err(|e| e.to_string())?;
    if let Err(e) = embed_export_metadata(out_path, item, has_edit) {
        eprintln!("export: metadata embed failed for {}: {e}", out_path.display());
    }
    Ok(())
}

/// Carry the original's camera EXIF + GPS into the rendered JPEG, then embed the item's
/// keywords, rating/label, and any filled IPTC fields (XMP + legacy IPTC) via exiftool, and
/// mirror the authored metadata into a `<name>.jpg.xmp` sidecar.
///
/// The re-encode (decode → resize → encode) strips all metadata, so without the EXIF copy
/// the export loses make/model/lens/exposure/ISO/date/GPS. The `image` crate doesn't rotate
/// pixels on decode, so the original Orientation stays valid for an unedited render; an
/// *edited* render is already baked to display orientation, so we reset Orientation to 1.
fn embed_export_metadata(jpeg: &Path, item: &ResolvedItem, has_edit: bool) -> Result<(), String> {
    let mut args: Vec<String> = Vec::new();

    // Copy camera EXIF + GPS from the original (first, so our authored tags below win).
    args.push("-tagsfromfile".into());
    args.push(item.original.to_string_lossy().into_owned());
    args.push("-EXIF:all".into());
    args.push("-GPS:all".into());
    if has_edit {
        // Edited pixels are already upright; don't let a copied Orientation re-rotate them.
        args.push("-Orientation#=1".into());
    }

    // Keywords: XMP dc:subject + legacy IPTC Keywords (broadest reader support), plus the
    // Lightroom-style hierarchical paths.
    let mut authored = false;
    for kw in &item.keywords.flat {
        args.push(format!("-XMP-dc:Subject={kw}"));
        args.push(format!("-IPTC:Keywords={kw}"));
        authored = true;
    }
    for path in &item.keywords.hierarchical {
        args.push(format!("-XMP-lr:HierarchicalSubject={path}"));
        authored = true;
    }

    if item.rating > 0 {
        args.push(format!("-XMP:Rating={}", item.rating));
        authored = true;
    }
    if push_if(&mut args, "XMP:Label", &item.label) {
        authored = true;
    }

    let f = &item.iptc;
    let iptc_pairs = [
        ("XMP-dc:Description", &f.description),
        ("IPTC:Caption-Abstract", &f.description),
        ("XMP-photoshop:Headline", &f.headline),
        ("XMP-dc:Title", &f.title),
        ("XMP-dc:Creator", &f.creator),
        ("XMP-dc:Rights", &f.copyright),
        ("XMP-photoshop:Credit", &f.credit),
        ("XMP-photoshop:Source", &f.source),
        ("XMP-photoshop:City", &f.city),
        ("XMP-photoshop:State", &f.state),
        ("XMP-photoshop:Country", &f.country),
        ("XMP-iptcCore:CountryCode", &f.country_code),
    ];
    for (tag, val) in iptc_pairs {
        if push_if(&mut args, tag, val) {
            authored = true;
        }
    }

    // 1) Embed into the JPEG (always — the EXIF copy applies even to an untagged photo).
    let mut embed = Command::new("exiftool");
    embed.arg("-q").arg("-overwrite_original").args(&args).arg(jpeg);
    run_exiftool(embed)?;

    // 2) Mirror authored metadata into a paired sidecar (skip for an untagged photo). The
    // `-o` form refuses to overwrite, so clear any existing sidecar first.
    if authored {
        let sidecar = crate::xmp::sidecar_path(jpeg);
        let _ = std::fs::remove_file(&sidecar);
        let mut side = Command::new("exiftool");
        side.arg("-q")
            .arg("-tagsfromfile")
            .arg(jpeg)
            .arg("-o")
            .arg(&sidecar);
        run_exiftool(side)?;
    }
    Ok(())
}

/// Append `-<tag>=<val>` when `val` is non-empty (trimmed). Returns whether it appended.
fn push_if(args: &mut Vec<String>, tag: &str, val: &str) -> bool {
    let v = val.trim();
    if v.is_empty() {
        return false;
    }
    args.push(format!("-{tag}={v}"));
    true
}

/// Run an exiftool command, surfacing a non-zero exit (with stderr) as an error.
fn run_exiftool(mut cmd: Command) -> Result<(), String> {
    let out = cmd
        .output()
        .map_err(|e| format!("exiftool not runnable: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(())
}

/// JPEG bytes for a JPEG export (Show-off / Instagram). Unedited and full-size → the
/// cached embedded preview as-is (fast). Otherwise decode the full-resolution source,
/// apply the crop + tone, optionally downscale to `max_width`, and encode.
fn render_export_jpeg(
    original: &Path,
    edit_json: &str,
    has_edit: bool,
    max_width: Option<u32>,
) -> Result<Vec<u8>, String> {
    // Fast path: unedited *and* no resize → ship the embedded full-size preview (already
    // cached for the loupe). Avoids a needless RAW decode/re-encode.
    if !has_edit && max_width.is_none() {
        return crate::thumbnails::zoom_bytes(original);
    }

    // Otherwise we need pixels in hand (to crop and/or resize).
    let mut img = decode_export_source(original, edit_json, has_edit)?;
    if let Some(w) = max_width {
        img = downscale_to_width(img, w);
    }
    encode_jpeg(&img, 92)
}

/// The image to export: the edited render of the full-res source when there's an edit,
/// else the decoded embedded preview. Falls back to the embedded preview if the `edit`
/// engine is compiled out.
fn decode_export_source(
    original: &Path,
    edit_json: &str,
    has_edit: bool,
) -> Result<image::DynamicImage, String> {
    #[cfg(feature = "edit")]
    if has_edit {
        // The full-res source is already cropped to the camera's visible area and rotated
        // to display orientation (see `full_res_source` → `raw::decode_to_image`), so it
        // matches the frame the crop fractions were drawn on.
        let source = full_res_source(original)?;
        return crate::plugins::edit::render_image(source, edit_json, 0);
    }
    let _ = (edit_json, has_edit);
    let jpeg = crate::thumbnails::zoom_bytes(original)?;
    image::load_from_memory(&jpeg).map_err(|e| e.to_string())
}

/// Downscale `img` to `target_width`, preserving aspect; never upscales.
fn downscale_to_width(img: image::DynamicImage, target_width: u32) -> image::DynamicImage {
    use image::GenericImageView;
    let (w, h) = img.dimensions();
    if target_width == 0 || w <= target_width {
        return img;
    }
    let target_height = ((target_width as u64 * h as u64) / w as u64).max(1) as u32;
    img.resize_exact(target_width, target_height, image::imageops::FilterType::Lanczos3)
}

/// Downscale `img` so its long edge (the larger of width and height) does not exceed
/// `limit`, preserving aspect ratio. Never upscales. `limit = 0` is a no-op (full
/// resolution). This is the per-platform upload-size control for Flickr/SmugMug:
/// setting 0 / empty = full resolution (default), otherwise the image is resized down.
pub fn downscale_to_long_edge(img: image::DynamicImage, limit: u32) -> image::DynamicImage {
    use image::GenericImageView;
    if limit == 0 {
        return img;
    }
    let (w, h) = img.dimensions();
    let long_edge = w.max(h);
    if long_edge <= limit {
        // Already within the limit — no resize needed (never upscale).
        return img;
    }
    // Scale both dimensions so the long edge hits `limit` exactly.
    let (tw, th) = if w >= h {
        let th = ((limit as u64 * h as u64) / w as u64).max(1) as u32;
        (limit, th)
    } else {
        let tw = ((limit as u64 * w as u64) / h as u64).max(1) as u32;
        (tw, limit)
    };
    img.resize_exact(tw, th, image::imageops::FilterType::Lanczos3)
}

/// Encode an image to JPEG bytes at `quality` (1–100). Self-contained so JPEG export
/// works even when the `edit` render engine is compiled out.
fn encode_jpeg(img: &image::DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    use image::codecs::jpeg::JpegEncoder;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_with_encoder(JpegEncoder::new_with_quality(&mut out, quality))
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

/// Decode an original at full resolution for edited export. RAW originals go through
/// LibRaw when the `raw` feature is on; otherwise (or for non-RAW originals) we use the
/// best available decode — the embedded full-size preview for RAW, the file itself for
/// JPEG/PNG/etc.
#[cfg(feature = "edit")]
fn full_res_source(original: &Path) -> Result<image::DynamicImage, String> {
    if crate::scanner::is_raw(original) {
        #[cfg(feature = "raw")]
        {
            return crate::raw::decode_to_image(original)
                .map(|img| tone_match_to_preview(img, original));
        }
        #[cfg(not(feature = "raw"))]
        {
            // No RAW decoder: the embedded full-size preview is the best we have. Lower
            // resolution than a true decode, but the crop is still applied correctly.
            let jpeg = crate::thumbnails::zoom_bytes(original)?;
            return image::load_from_memory(&jpeg).map_err(|e| e.to_string());
        }
    }
    image::open(original).map_err(|e| e.to_string())
}

/// Close the tone gap between a plain LibRaw decode and the camera's embedded preview —
/// the proxy every edit was judged on. The camera bakes its picture-style tone curve
/// (S-curve + midtone lift) into the preview; a plain decode has only the sRGB transfer
/// curve, so exported pixels come out flatter and ~0.5–1 EV darker than what the editor
/// showed. Recover the camera's curve as a luma histogram-matching LUT (decode CDF →
/// preview CDF over the same full frame) and apply it per channel, like any tone curve —
/// the decode keeps its full-resolution real pixels, only the tonal mapping is aligned.
/// Best-effort: if the preview can't be read, return the decode unchanged (a darker
/// export beats a failed one).
#[cfg(all(feature = "edit", feature = "raw"))]
fn tone_match_to_preview(img: image::DynamicImage, original: &Path) -> image::DynamicImage {
    let preview = match crate::thumbnails::preview_bytes(original)
        .and_then(|b| image::load_from_memory(&b).map_err(|e| e.to_string()))
    {
        Ok(p) => p,
        Err(_) => return img,
    };
    let lut = tone_match_lut(&luma_histogram(&img), &luma_histogram(&preview));
    let mut rgb = img.into_rgb8();
    for p in rgb.pixels_mut() {
        p.0 = [
            lut[p.0[0] as usize],
            lut[p.0[1] as usize],
            lut[p.0[2] as usize],
        ];
    }
    image::DynamicImage::ImageRgb8(rgb)
}

/// 256-bin histogram of Rec. 709 luma over (gamma-encoded) pixels, subsampled to at most
/// ~1M samples — a tone curve derived from a histogram doesn't need every pixel of a
/// 60 MP frame.
#[cfg(all(feature = "edit", feature = "raw"))]
fn luma_histogram(img: &image::DynamicImage) -> [u64; 256] {
    use image::GenericImageView;
    let (w, h) = img.dimensions();
    let step = (((w as u64 * h as u64) as f64 / 1_000_000.0).sqrt().ceil() as u32).max(1);
    let mut hist = [0u64; 256];
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            let [r, g, b, _] = img.get_pixel(x, y).0;
            let luma = (2126 * r as u32 + 7152 * g as u32 + 722 * b as u32) / 10000;
            hist[luma as usize] += 1;
            x += step;
        }
        y += step;
    }
    hist
}

/// Classic monotonic histogram-matching LUT: map each source level to the smallest
/// target level whose CDF is at least the source level's CDF. Comparisons are done as
/// cross-multiplications so the two histograms may hold different sample counts.
/// Degenerate inputs (either histogram empty) yield the identity.
///
/// Source levels with **no samples** are interpolated between their populated
/// neighbours (and toward 0/255 past the ends) rather than snapped to the previous
/// level: the LUT is applied per RGB channel, and a coloured pixel's channel value can
/// sit at a luma level the histogram never saw — snapping those would band.
#[cfg(all(feature = "edit", feature = "raw"))]
fn tone_match_lut(src: &[u64; 256], dst: &[u64; 256]) -> [u8; 256] {
    let total_src: u64 = src.iter().sum();
    let total_dst: u64 = dst.iter().sum();
    let mut lut = [0u8; 256];
    if total_src == 0 || total_dst == 0 {
        for (i, v) in lut.iter_mut().enumerate() {
            *v = i as u8;
        }
        return lut;
    }
    let mut cum_src = 0u64;
    let mut cum_dst = dst[0];
    let mut j = 0usize;
    for (i, v) in lut.iter_mut().enumerate() {
        cum_src += src[i];
        while j < 255 && cum_dst * total_src < cum_src * total_dst {
            j += 1;
            cum_dst += dst[j];
        }
        *v = j as u8;
    }
    // Interpolate the unpopulated source levels between populated anchors.
    let mut prev: Option<usize> = None;
    for i in 0..256 {
        if src[i] == 0 {
            continue;
        }
        match prev {
            Some(p) => {
                let (a, b) = (lut[p] as i32, lut[i] as i32);
                for k in p + 1..i {
                    lut[k] = (a + (b - a) * (k - p) as i32 / (i - p) as i32) as u8;
                }
            }
            None => {
                // Head: a straight line from 0 up to the first anchor's value.
                let b = lut[i] as i32;
                for k in 0..i {
                    lut[k] = (b * k as i32 / i as i32) as u8;
                }
            }
        }
        prev = Some(i);
    }
    if let Some(p) = prev {
        // Tail: a straight line from the last anchor's value up to 255.
        let a = lut[p] as i32;
        for k in p + 1..256 {
            lut[k] = (a + (255 - a) * (k - p) as i32 / (255 - p) as i32) as u8;
        }
    }
    lut
}

/// A destination path that doesn't already exist: returns `path` if free, else
/// `stem (2).ext`, `stem (3).ext`, … This prevents a re-export or a duplicate source
/// filename from silently clobbering an already-written file.
fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|s| s.to_str());
    for n in 2..10_000 {
        let mut name = format!("{stem} ({n})");
        if let Some(ext) = ext {
            name.push('.');
            name.push_str(ext);
        }
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    path.to_path_buf() // pathological fallback (10k collisions)
}

#[cfg(test)]
mod tests {
    use super::downscale_to_long_edge;
    use image::{DynamicImage, GenericImageView, RgbImage};

    fn make_img(w: u32, h: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::new(w, h))
    }

    // limit = 0 → no resize at all (full resolution).
    #[test]
    fn zero_limit_is_noop() {
        let img = make_img(4000, 3000);
        let out = downscale_to_long_edge(img, 0);
        assert_eq!(out.dimensions(), (4000, 3000));
    }

    // Image already within the limit → returned unchanged (never upscale).
    #[test]
    fn below_limit_untouched() {
        let img = make_img(800, 600);
        let out = downscale_to_long_edge(img, 1080);
        assert_eq!(out.dimensions(), (800, 600));
    }

    // Image exactly at the limit → returned unchanged.
    #[test]
    fn at_limit_untouched() {
        let img = make_img(1080, 720);
        let out = downscale_to_long_edge(img, 1080);
        assert_eq!(out.dimensions(), (1080, 720));
    }

    // Landscape (width > height): long edge is width, scaled to limit.
    #[test]
    fn landscape_scaled_to_limit() {
        let img = make_img(4000, 3000);
        let out = downscale_to_long_edge(img, 2000);
        let (w, h) = out.dimensions();
        assert_eq!(w, 2000, "width should be the limit");
        assert_eq!(h, 1500, "height should be scaled proportionally");
    }

    // Portrait (height > width): long edge is height, scaled to limit.
    #[test]
    fn portrait_scaled_to_limit() {
        let img = make_img(3000, 4000);
        let out = downscale_to_long_edge(img, 2000);
        let (w, h) = out.dimensions();
        assert_eq!(h, 2000, "height should be the limit");
        assert_eq!(w, 1500, "width should be scaled proportionally");
    }

    // Square: both dimensions equal, scaled to limit.
    #[test]
    fn square_scaled_to_limit() {
        let img = make_img(2000, 2000);
        let out = downscale_to_long_edge(img, 1000);
        assert_eq!(out.dimensions(), (1000, 1000));
    }

    // After resize the long edge must not exceed the limit.
    #[test]
    fn long_edge_never_exceeds_limit() {
        for (w, h, limit) in [(5472, 3648, 1080), (3648, 5472, 1080), (1920, 1080, 1080)] {
            let img = make_img(w, h);
            let out = downscale_to_long_edge(img, limit);
            let (ow, oh) = out.dimensions();
            assert!(ow.max(oh) <= limit, "long edge {ow}x{oh} exceeds limit {limit}");
        }
    }

    // Never upscales a small image even if limit is larger.
    #[test]
    fn never_upscales() {
        let img = make_img(400, 300);
        let out = downscale_to_long_edge(img, 2000);
        assert_eq!(out.dimensions(), (400, 300), "should not upscale");
    }
}

/// K1 — editor↔export tone match. The Develop preview (`plugins::edit::render_jpeg`,
/// applied to the cached proxy) and the edited export (`render_export_jpeg`, applied to
/// the full-res source) must produce the **same tone**. They share the look/tone pipeline
/// (`plugins::edit::render_image`); this test proves that a non-trivial edit rendered on
/// the *same source pixels* comes out matching through both public entrypoints, within a
/// small per-pixel tolerance (the only allowed drift is JPEG requantization + the 90-vs-92
/// encode quality difference — never a tonal offset).
#[cfg(all(test, feature = "edit"))]
mod tone_match_tests {
    use super::render_export_jpeg;
    use image::{GenericImageView, Rgb, RgbImage};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_IMAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempJpeg {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TempJpeg {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempJpeg {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn write_temp_jpeg(prefix: &str, jpeg: &[u8]) -> TempJpeg {
        let seq = TEST_IMAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "chairphoto-{prefix}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("synthetic.jpg");
        std::fs::write(&path, jpeg).unwrap();
        TempJpeg { dir, path }
    }

    /// A synthetic image with a full tonal range and colour so a WB/EV/contrast/B&W edit
    /// exercises the whole look pipeline (a flat patch would hide most divergences). A
    /// horizontal luminance ramp crossed with vertical R/G/B bands.
    fn synthetic() -> RgbImage {
        let (w, h) = (96u32, 64u32);
        RgbImage::from_fn(w, h, |x, y| {
            let l = (x * 255 / (w - 1)) as u8; // 0..255 ramp left→right
            match y * 3 / h {
                0 => Rgb([l, l / 2, l / 4]),   // warm band
                1 => Rgb([l / 4, l, l / 2]),   // green band
                _ => Rgb([l / 2, l / 4, l]),   // cool band
            }
        })
    }

    fn to_jpeg(img: &RgbImage) -> Vec<u8> {
        use image::codecs::jpeg::JpegEncoder;
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img.clone())
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, 95))
            .unwrap();
        out.into_inner()
    }

    /// Mean absolute per-channel difference between two same-size JPEGs, after decoding.
    fn mean_abs_diff(a: &[u8], b: &[u8]) -> f32 {
        let ia = image::load_from_memory(a).unwrap().to_rgb8();
        let ib = image::load_from_memory(b).unwrap().to_rgb8();
        assert_eq!(ia.dimensions(), ib.dimensions(), "paths must agree on geometry");
        let mut sum = 0f64;
        let mut n = 0u64;
        for (pa, pb) in ia.pixels().zip(ib.pixels()) {
            for c in 0..3 {
                sum += (pa[c] as f64 - pb[c] as f64).abs();
                n += 1;
            }
        }
        (sum / n as f64) as f32
    }

    /// Render `edit_json` on the same source through both public entrypoints and assert
    /// the outputs match tonally. The source is written as a **JPEG file** so the export's
    /// non-RAW decode (`image::open`) and the preview's `render_jpeg` (fed the same JPEG
    /// bytes) start from identical pixels — isolating the render/tone pipeline, which is
    /// the thing this test guards.
    fn assert_paths_match(edit_json: &str, tol: f32) {
        let src = synthetic();
        let jpeg = to_jpeg(&src);

        // Preview path: the shared engine applied to the proxy JPEG.
        let preview = crate::plugins::edit::render_jpeg(&jpeg, edit_json, 0).unwrap();

        // Export path: the real `render_export_jpeg` on a non-RAW source file. `image::open`
        // decodes the JPEG we just wrote, then the same `render_image` runs, so any tonal
        // divergence between the two callers would show up here.
        let source = write_temp_jpeg("tone-match", &jpeg);
        let export = render_export_jpeg(source.path(), edit_json, true, None).unwrap();

        let diff = mean_abs_diff(&preview, &export);
        assert!(
            diff <= tol,
            "editor↔export tone mismatch for edit {edit_json}: mean |Δ| = {diff} (tol {tol})"
        );
    }

    #[test]
    fn tone_ev_wb_contrast_matches() {
        // A realistic tonal edit: brighten, warm, add contrast, lift shadows.
        assert_paths_match(
            r#"{"tone":{"ev":0.5,"contrast":0.3,"shadows":0.2,"highlights":-0.15,
                       "wb":{"temp":0.4,"tint":-0.2}}}"#,
            2.0,
        );
    }

    #[test]
    fn film_look_matches() {
        // The full film-look stack (B&W mixer, split toning, fade, vignette, grain). Grain
        // is deterministic in normalized image space, so the same source size renders the
        // same pattern through both paths.
        assert_paths_match(
            r#"{"bw":{"enabled":true,"r":0.9,"g":0.15,"b":-0.05},
                "split":{"shadow_hue":35,"shadow_sat":0.25,"highlight_hue":45,
                         "highlight_sat":0.12},
                "fade":0.2,"vignette":-0.3,"grain":{"amount":0.4,"size":1.2,"seed":7}}"#,
            2.0,
        );
    }

    #[test]
    fn crop_plus_tone_matches() {
        // A crop changes geometry identically on both sides (same fractions), and the tone
        // then applies to the same cropped pixels.
        assert_paths_match(
            r#"{"crop":{"x":0.1,"y":0.1,"w":0.8,"h":0.7,"aspect":"1:1"},
                "tone":{"ev":-0.3,"saturation":0.4,"vibrance":0.2}}"#,
            2.0,
        );
    }

    #[test]
    fn geometry_is_identical_across_paths() {
        // Beyond tone, the two paths must agree on output dimensions for a locked-aspect
        // crop (a per-pixel diff is only meaningful if geometry lines up).
        let src = synthetic();
        let jpeg = to_jpeg(&src);
        let edit = r#"{"crop":{"x":0.0,"y":0.0,"w":0.5,"h":0.6,"aspect":"1:1"}}"#;
        let preview = crate::plugins::edit::render_jpeg(&jpeg, edit, 0).unwrap();
        let source = write_temp_jpeg("geometry-match", &jpeg);
        let export = render_export_jpeg(source.path(), edit, true, None).unwrap();
        let dp = image::load_from_memory(&preview).unwrap().dimensions();
        let de = image::load_from_memory(&export).unwrap().dimensions();
        assert_eq!(dp, de, "preview {dp:?} and export {de:?} geometry must match");
    }
}

/// The RAW-decode → preview tone match (histogram-matching LUT) that closes the camera
/// picture-style gap: a plain LibRaw decode is flatter/darker than the embedded preview
/// the editor showed, so the export pipeline lifts the decode onto the preview's tone.
#[cfg(all(test, feature = "edit", feature = "raw"))]
mod preview_tone_match_tests {
    use super::{luma_histogram, tone_match_lut};
    use image::{DynamicImage, Rgb, RgbImage};

    /// A gradient image whose luma spans `lo..=hi` — a controllable histogram.
    fn gradient(lo: u8, hi: u8) -> DynamicImage {
        let (w, h) = (64u32, 64u32);
        let span = (hi - lo) as u32;
        DynamicImage::ImageRgb8(RgbImage::from_fn(w, h, |x, _| {
            let v = lo as u32 + x * span / (w - 1);
            Rgb([v as u8, v as u8, v as u8])
        }))
    }

    #[test]
    fn identical_histograms_give_identity() {
        let hist = luma_histogram(&gradient(0, 255));
        let lut = tone_match_lut(&hist, &hist);
        for (i, &v) in lut.iter().enumerate() {
            assert_eq!(v as usize, i, "identity expected at level {i}");
        }
    }

    #[test]
    fn empty_histogram_gives_identity() {
        let empty = [0u64; 256];
        let hist = luma_histogram(&gradient(0, 255));
        for lut in [tone_match_lut(&empty, &hist), tone_match_lut(&hist, &empty)] {
            for (i, &v) in lut.iter().enumerate() {
                assert_eq!(v as usize, i);
            }
        }
    }

    #[test]
    fn dark_source_lifts_toward_bright_target() {
        // Source occupies 0..=127, target the same shape shifted to 64..=191 (the
        // camera-preview situation: same scene, brighter tone curve). Levels the source
        // actually uses must map up by ~the shift.
        let src = luma_histogram(&gradient(0, 127));
        let dst = luma_histogram(&gradient(64, 191));
        let lut = tone_match_lut(&src, &dst);
        for level in [0usize, 32, 64, 96, 127] {
            let mapped = lut[level] as i32;
            let expected = level as i32 + 64;
            assert!(
                (mapped - expected).abs() <= 3,
                "level {level} mapped to {mapped}, expected ≈{expected}"
            );
        }
    }

    #[test]
    fn lut_is_monotonic() {
        // Monotonicity must hold even for disjoint, oddly-shaped histograms — a tone
        // curve that reverses ordering would posterize/solarize the export.
        let src = luma_histogram(&gradient(10, 240));
        let dst = luma_histogram(&gradient(100, 140));
        let lut = tone_match_lut(&src, &dst);
        for i in 1..256 {
            assert!(lut[i] >= lut[i - 1], "LUT reverses at level {i}");
        }
    }

    #[test]
    fn histogram_counts_all_pixels_of_small_images() {
        // Small images are not subsampled (step = 1): every pixel lands in a bin.
        let hist = luma_histogram(&gradient(0, 255));
        assert_eq!(hist.iter().sum::<u64>(), 64 * 64);
    }

    /// Manual end-to-end check against a real RAW + its embedded preview (needs LibRaw
    /// and a real file, so not part of the normal suite):
    ///   CHAIRPHOTO_TEST_RAW=/path/to/photo.ARW cargo test --lib real_raw -- --ignored --nocapture
    /// Asserts the tone-matched full decode lands within 5% mean luma of the preview —
    /// the WYSIWYG contract behind the "Instagram export is much darker" fix.
    #[test]
    #[ignore]
    fn real_raw_decode_tone_matches_preview() {
        let Ok(path) = std::env::var("CHAIRPHOTO_TEST_RAW") else {
            panic!("set CHAIRPHOTO_TEST_RAW to a RAW file path");
        };
        let path = std::path::PathBuf::from(path);
        let mean = |img: &DynamicImage| {
            let h = luma_histogram(img);
            let n: u64 = h.iter().sum();
            h.iter().enumerate().map(|(i, &c)| i as u64 * c).sum::<u64>() as f64
                / (n as f64 * 255.0)
        };
        let decode = super::full_res_source(&path).expect("full-res decode");
        let preview = image::load_from_memory(
            &crate::thumbnails::preview_bytes(&path).expect("embedded preview"),
        )
        .unwrap();
        let (md, mp) = (mean(&decode), mean(&preview));
        println!("tone-matched decode mean {md:.4}, preview mean {mp:.4}");
        assert!(
            (md - mp).abs() / mp < 0.05,
            "decode mean {md:.4} deviates >5% from preview mean {mp:.4}"
        );
    }
}
