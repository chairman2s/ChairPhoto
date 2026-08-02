//! The basic editor's **render engine** (behind the `edit` Cargo feature): applies a
//! version's non-destructive edit record — a normalized crop + tone adjustments — to a
//! source JPEG and returns a new JPEG. It never touches the original file; the caller
//! feeds it the cached preview proxy (live editing/loupe) or, later, a full RAW decode
//! (export). See docs/editing.md. The edit record shape is owned here but stays opaque
//! to the catalog core.

pub mod cube;
mod look;

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, GenericImageView, Rgb, RgbImage};
use look::{Bw, Grain, Split};
use serde::Deserialize;

/// A version's edit record. All fields optional/defaulted so a partial/empty record
/// renders as a no-op (an unedited copy).
#[derive(Deserialize, Default)]
struct EditRecord {
    #[serde(default)]
    crop: Option<Crop>,
    #[serde(default)]
    tone: Tone,
    /// Straighten angle in degrees (rotate the image about its centre to level it). The
    /// UI pairs this with an inscribed crop so the rotation's empty corners stay out of
    /// frame. Positive matches a line's screen-space tilt (y-down), so `-tilt` levels it.
    #[serde(default)]
    straighten: f32,
    /// Black & white conversion (channel mixer). `None` = colour.
    #[serde(default)]
    bw: Option<Bw>,
    /// Split toning (sepia, selenium, teal/orange…). `None` = no toning.
    #[serde(default)]
    split: Option<Split>,
    /// Film grain (deterministic; see [`look::Grain`]). `None` = no grain.
    #[serde(default)]
    grain: Option<Grain>,
    /// Lifted matte blacks, 0..1.
    #[serde(default)]
    fade: f32,
    /// Corner shading, -1..1 (negative darkens).
    #[serde(default)]
    vignette: f32,
    /// Optional user-supplied .cube LUT applied to the developed image.
    #[serde(default)]
    lut: Option<LutRef>,
}

/// Reference to a `.cube` LUT by bare filename, resolved against the app-data `luts/`
/// folder — never a path, so edit records stay portable across machines. A missing or
/// corrupt file renders as if no LUT were set (non-fatal).
#[derive(Deserialize)]
struct LutRef {
    file: String,
    /// Blend between the un-LUT-ed and LUT-ed image, 0..1.
    #[serde(default = "one")]
    amount: f32,
}

/// Crop rectangle as fractions (0–1) of the source, so one record works on any size.
#[derive(Deserialize)]
struct Crop {
    #[serde(default)]
    x: f32,
    #[serde(default)]
    y: f32,
    #[serde(default = "one")]
    w: f32,
    #[serde(default = "one")]
    h: f32,
    /// Optional locked aspect ratio (e.g. "1:1", "4:5"). When set, the output is forced
    /// to *exactly* this ratio — independent rounding of the w/h fractions across source
    /// sizes would otherwise leave a "1:1" crop off-square by a pixel.
    #[serde(default)]
    aspect: Option<String>,
}

/// Parse an "A:B" aspect string into the ratio A/B (width over height). Returns `None`
/// for "Free"/"Original"/unparseable values.
fn parse_aspect(aspect: &Option<String>) -> Option<f32> {
    let s = aspect.as_deref()?.trim();
    let (a, b) = s.split_once(':')?;
    let a: f32 = a.trim().parse().ok()?;
    let b: f32 = b.trim().parse().ok()?;
    if a > 0.0 && b > 0.0 {
        Some(a / b)
    } else {
        None
    }
}

fn one() -> f32 {
    1.0
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Tone {
    ev: f32,         // exposure in stops
    contrast: f32,   // -1..1
    highlights: f32, // -1..1 (negative recovers, positive boosts)
    shadows: f32,    // -1..1 (positive lifts)
    whites: f32,     // -1..1 (positive clips brights, negative pulls top end down)
    blacks: f32,     // -1..1 (negative crushes darks, positive lifts black point)
    vibrance: f32,   // -1..1 (saturation boost weighted toward low-sat pixels)
    saturation: f32, // -1..1 (-1 = greyscale)
    wb: Wb,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Wb {
    temp: f32, // -1..1 relative (warm/cool)
    tint: f32, // -1..1 relative (green/magenta)
}

/// Whether an edit record renders black & white — an enabled B&W mixer or full
/// desaturation. Drives the monochrome auto-tag (H6); the renderer doesn't use it.
/// Unparseable/empty records are simply "not B&W".
pub fn is_bw(edit_json: &str) -> bool {
    let trimmed = edit_json.trim();
    if trimmed.is_empty() {
        return false;
    }
    match serde_json::from_str::<EditRecord>(trimmed) {
        Ok(edit) => {
            edit.bw.as_ref().is_some_and(|b| b.enabled) || edit.tone.saturation <= -0.999
        }
        Err(_) => false,
    }
}

/// Render `jpeg` with the edits in `edit_json`. `max_edge` (when > 0) downscales the
/// result's longest edge — used to keep live preview fast; pass 0 for full size.
pub fn render_jpeg(jpeg: &[u8], edit_json: &str, max_edge: u32) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(jpeg).map_err(|e| e.to_string())?;
    let out = render_image(img, edit_json, max_edge)?;
    encode_jpeg(&out, 90)
}

/// Encode an image to JPEG bytes at `quality` (1–100).
pub fn encode_jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_with_encoder(JpegEncoder::new_with_quality(&mut out, quality))
        .map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

/// Apply the edits in `edit_json` (normalized crop + tone) to an already-decoded image.
/// `max_edge` (when > 0) downscales the result's longest edge. Used for both the JPEG
/// proxy path ([`render_jpeg`]) and full-resolution edited export (RAW decode → here).
pub fn render_image(
    mut img: DynamicImage,
    edit_json: &str,
    max_edge: u32,
) -> Result<DynamicImage, String> {
    let trimmed = edit_json.trim();
    let edit: EditRecord = serde_json::from_str(if trimmed.is_empty() { "{}" } else { trimmed })
        .map_err(|e| format!("invalid edit record: {e}"))?;

    // 0) Straighten: rotate about the centre. Corners exposed by the rotation are left
    //    black; the UI's inscribed crop keeps them out of the final frame.
    if edit.straighten.abs() > 0.01 {
        img = rotate_about_center(&img, edit.straighten);
    }

    // 1) Crop (normalized → pixels), clamped to the image bounds.
    if let Some(c) = &edit.crop {
        let (w, h) = img.dimensions();
        let x = (c.x.clamp(0.0, 1.0) * w as f32).round() as u32;
        let y = (c.y.clamp(0.0, 1.0) * h as f32).round() as u32;
        let mut cw = ((c.w.clamp(0.0, 1.0) * w as f32).round() as u32)
            .clamp(1, w.saturating_sub(x).max(1));
        let mut ch = ((c.h.clamp(0.0, 1.0) * h as f32).round() as u32)
            .clamp(1, h.saturating_sub(y).max(1));
        // Enforce the locked aspect exactly (only ever shrinking, so we stay in bounds),
        // so a "1:1" crop is pixel-perfect square regardless of the source resolution.
        if let Some(ratio) = parse_aspect(&c.aspect) {
            if (cw as f32 / ch as f32) > ratio {
                cw = ((ch as f32 * ratio).round() as u32).clamp(1, cw);
            } else {
                ch = ((cw as f32 / ratio).round() as u32).clamp(1, ch);
            }
        }
        img = img.crop_imm(x, y, cw, ch);
    }

    // 2) Optional downscale (preview speed).
    if max_edge > 0 {
        let (w, h) = img.dimensions();
        if w.max(h) > max_edge {
            img = img.thumbnail(max_edge, max_edge);
        }
    }

    // 3) The look — tone, B&W mix, LUT, toning, fade, vignette — in the RGB domain.
    // The LUT (if referenced) is resolved once per render through cube's mtime cache;
    // a missing/corrupt file is non-fatal and the render proceeds without it.
    let lut = edit.lut.as_ref().and_then(|l| {
        crate::commands::luts_dir()
            .ok()
            .and_then(|dir| cube::load(&dir, &l.file))
    });
    let mut rgb = img.to_rgb8();
    look::apply_look(&mut rgb, &edit, lut.as_deref());

    Ok(DynamicImage::ImageRgb8(rgb))
}

/// Rotate an image about its centre by `degrees`, keeping the same canvas size. Pixels
/// pulled from outside the source (the exposed corners) are black; bilinear sampling
/// keeps edges smooth. `degrees` follows screen space (y-down), matching the UI's tilt.
fn rotate_about_center(img: &DynamicImage, degrees: f32) -> DynamicImage {
    let src = img.to_rgb8();
    let (w, h) = src.dimensions();
    let (sin, cos) = degrees.to_radians().sin_cos();
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let mut out = RgbImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            // Inverse rotation (output → source): rotate the offset by -degrees.
            let sx = cos * dx + sin * dy + cx - 0.5;
            let sy = -sin * dx + cos * dy + cy - 0.5;
            out.put_pixel(x, y, sample_bilinear(&src, sx, sy));
        }
    }
    DynamicImage::ImageRgb8(out)
}

/// Bilinearly sample `img` at fractional `(x, y)`; black for the rotation's exposed
/// corners. Coordinates within half a pixel of the image are clamped to the edge (rather
/// than blackened), so a straightened image has no 1px black sliver at the binding edge.
fn sample_bilinear(img: &RgbImage, x: f32, y: f32) -> Rgb<u8> {
    let (w, h) = img.dimensions();
    if x < -0.5 || y < -0.5 || x > w as f32 - 0.5 || y > h as f32 - 0.5 {
        return Rgb([0, 0, 0]);
    }
    let x = x.clamp(0.0, (w - 1) as f32);
    let y = y.clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (x.floor() as u32, y.floor() as u32);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let p = |xx, yy| img.get_pixel(xx, yy).0;
    let (p00, p10, p01, p11) = (p(x0, y0), p(x1, y0), p(x0, y1), p(x1, y1));
    let mut out = [0u8; 3];
    for c in 0..3 {
        let top = p00[c] as f32 * (1.0 - fx) + p10[c] as f32 * fx;
        let bot = p01[c] as f32 * (1.0 - fx) + p11[c] as f32 * fx;
        out[c] = (top * (1.0 - fy) + bot * fy).round().clamp(0.0, 255.0) as u8;
    }
    Rgb(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_jpeg(r: u8, g: u8, b: u8) -> Vec<u8> {
        let img = RgbImage::from_pixel(16, 16, image::Rgb([r, g, b]));
        let mut out = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_with_encoder(JpegEncoder::new_with_quality(&mut out, 95))
            .unwrap();
        out.into_inner()
    }

    fn mean_rgb(jpeg: &[u8]) -> (f32, f32, f32) {
        let img = image::load_from_memory(jpeg).unwrap().to_rgb8();
        let (mut r, mut g, mut b) = (0f64, 0f64, 0f64);
        for p in img.pixels() {
            r += p[0] as f64;
            g += p[1] as f64;
            b += p[2] as f64;
        }
        let n = img.pixels().count() as f64;
        ((r / n) as f32, (g / n) as f32, (b / n) as f32)
    }

    #[test]
    fn empty_edit_is_a_noop() {
        let src = solid_jpeg(120, 120, 120);
        let out = render_jpeg(&src, "{}", 0).unwrap();
        let (r, _, _) = mean_rgb(&out);
        assert!((r - 120.0).abs() < 4.0, "near-unchanged, got {r}");
    }

    #[test]
    fn positive_ev_brightens() {
        let src = solid_jpeg(100, 100, 100);
        let out = render_jpeg(&src, r#"{"tone":{"ev":1.0}}"#, 0).unwrap();
        let (r, _, _) = mean_rgb(&out);
        assert!(r > 150.0, "1 stop should brighten 100 well past 150, got {r}");
    }

    #[test]
    fn warm_wb_shifts_red_above_blue() {
        let src = solid_jpeg(120, 120, 120);
        let out = render_jpeg(&src, r#"{"tone":{"wb":{"temp":1.0}}}"#, 0).unwrap();
        let (r, _, b) = mean_rgb(&out);
        assert!(r > b + 20.0, "warm should push red above blue, got r={r} b={b}");
    }

    #[test]
    fn crop_reduces_dimensions() {
        let src = solid_jpeg(120, 120, 120);
        let out = render_jpeg(&src, r#"{"crop":{"x":0.0,"y":0.0,"w":0.5,"h":0.5}}"#, 0).unwrap();
        let img = image::load_from_memory(&out).unwrap();
        assert_eq!(img.dimensions(), (8, 8), "half-crop of 16px = 8px");
    }

    #[test]
    fn straighten_keeps_size_and_blackens_corners() {
        // A solid-white image rotated keeps its dimensions; the corners (pulled from
        // outside the source) go black, while the centre stays white.
        let src = solid_jpeg(255, 255, 255);
        let out = render_jpeg(&src, r#"{"straighten":15}"#, 0).unwrap();
        let img = image::load_from_memory(&out).unwrap().to_rgb8();
        assert_eq!(img.dimensions(), (16, 16), "rotation preserves canvas size");
        assert!(img.get_pixel(8, 8).0[0] > 240, "centre stays ~white (JPEG-lossy)");
        let corner = img.get_pixel(0, 0).0;
        assert!(corner[0] < 128, "corner is darkened by the rotation, got {corner:?}");
    }

    #[test]
    fn zero_straighten_is_a_noop() {
        let src = solid_jpeg(200, 100, 50);
        let out = render_jpeg(&src, r#"{"straighten":0}"#, 0).unwrap();
        let img = image::load_from_memory(&out).unwrap().to_rgb8();
        assert_eq!(img.get_pixel(0, 0).0, [200, 100, 50], "no rotation, corner intact");
    }

    #[test]
    fn locked_aspect_forces_exact_ratio() {
        // w/h fractions that round to different pixel counts (8 vs 9 on a 16px source);
        // the "1:1" lock must pull them back to an exact square.
        let src = solid_jpeg(120, 120, 120);
        let out = render_jpeg(
            &src,
            r#"{"crop":{"x":0.0,"y":0.0,"w":0.5,"h":0.55,"aspect":"1:1"}}"#,
            0,
        )
        .unwrap();
        let (w, h) = image::load_from_memory(&out).unwrap().dimensions();
        assert_eq!(w, h, "1:1 lock must be pixel-perfect square, got {w}x{h}");
    }

    #[test]
    fn saturation_minus_one_gives_greyscale() {
        // saturation=-1 should collapse R,G,B to the same luma value.
        let src = solid_jpeg(200, 100, 50);
        let out = render_jpeg(&src, r#"{"tone":{"saturation":-1.0}}"#, 0).unwrap();
        let (r, g, b) = mean_rgb(&out);
        assert!(
            (r - g).abs() < 3.0 && (r - b).abs() < 3.0,
            "saturation=-1 must be greyscale, got r={r} g={g} b={b}"
        );
    }

    #[test]
    fn bw_red_filter_brightens_reds_and_darkens_blues() {
        // A red-filter B&W conversion must render a red patch brighter than a blue
        // patch, and both outputs must be grey.
        let filter = r#"{"bw":{"enabled":true,"r":0.9,"g":0.15,"b":-0.05}}"#;
        let red = render_jpeg(&solid_jpeg(200, 40, 40), filter, 0).unwrap();
        let blue = render_jpeg(&solid_jpeg(40, 40, 200), filter, 0).unwrap();
        let (rr, rg, rb) = mean_rgb(&red);
        let (br, _, _) = mean_rgb(&blue);
        assert!(
            (rr - rg).abs() < 3.0 && (rr - rb).abs() < 3.0,
            "B&W output must be grey, got r={rr} g={rg} b={rb}"
        );
        assert!(rr > br + 50.0, "red filter: red patch {rr} must beat blue patch {br}");
    }

    #[test]
    fn bw_disabled_keeps_colour() {
        let src = solid_jpeg(200, 100, 50);
        let out = render_jpeg(&src, r#"{"bw":{"enabled":false}}"#, 0).unwrap();
        let (r, _, b) = mean_rgb(&out);
        assert!(r > b + 100.0, "disabled bw must keep colour, got r={r} b={b}");
    }

    #[test]
    fn new_look_params_at_defaults_are_identity() {
        let src = solid_jpeg(128, 128, 128);
        let out = render_jpeg(
            &src,
            r#"{"split":{"shadow_sat":0,"highlight_sat":0},"fade":0,"vignette":0}"#,
            0,
        )
        .unwrap();
        let (r, _, _) = mean_rgb(&out);
        assert!((r - 128.0).abs() < 4.0, "all-default look must be identity, got r={r}");
    }

    #[test]
    fn sepia_split_tints_grey_warm() {
        // Sepia (orange hue on both ends) on a grey image must push red above blue.
        let src = solid_jpeg(128, 128, 128);
        let out = render_jpeg(
            &src,
            r#"{"split":{"shadow_hue":35,"shadow_sat":0.3,"highlight_hue":45,"highlight_sat":0.2}}"#,
            0,
        )
        .unwrap();
        let (r, _, b) = mean_rgb(&out);
        assert!(r > b + 8.0, "sepia must tint warm, got r={r} b={b}");
    }

    #[test]
    fn fade_lifts_blacks() {
        let src = solid_jpeg(0, 0, 0);
        let out = render_jpeg(&src, r#"{"fade":1.0}"#, 0).unwrap();
        let (r, _, _) = mean_rgb(&out);
        assert!(r > 20.0, "full fade must lift pure black well off 0, got {r}");
    }

    #[test]
    fn negative_vignette_darkens_corners_not_centre() {
        let src = solid_jpeg(200, 200, 200);
        let out = render_jpeg(&src, r#"{"vignette":-1.0}"#, 0).unwrap();
        let img = image::load_from_memory(&out).unwrap().to_rgb8();
        let centre = img.get_pixel(8, 8).0[0];
        let corner = img.get_pixel(0, 0).0[0];
        assert!(centre > 185, "centre must stay bright, got {centre}");
        assert!(corner < centre - 30, "corner must darken, got corner={corner} centre={centre}");
    }

    #[test]
    fn grain_is_deterministic_and_seed_dependent() {
        // Same record → identical bytes (export reproducibility); a different seed
        // must change the pattern; amount 0 must be a no-op.
        let src = solid_jpeg(128, 128, 128);
        let a1 = render_jpeg(&src, r#"{"grain":{"amount":0.6,"size":1.0,"seed":7}}"#, 0).unwrap();
        let a2 = render_jpeg(&src, r#"{"grain":{"amount":0.6,"size":1.0,"seed":7}}"#, 0).unwrap();
        let b = render_jpeg(&src, r#"{"grain":{"amount":0.6,"size":1.0,"seed":8}}"#, 0).unwrap();
        let zero = render_jpeg(&src, r#"{"grain":{"amount":0.0}}"#, 0).unwrap();
        assert_eq!(a1, a2, "same edit record must render identical bytes");
        assert_ne!(a1, b, "a different seed must change the grain pattern");
        assert_eq!(zero, render_jpeg(&src, "{}", 0).unwrap(), "amount 0 is a no-op");
    }

    #[test]
    fn grain_roughly_preserves_mean_brightness() {
        let src = solid_jpeg(128, 128, 128);
        let out = render_jpeg(&src, r#"{"grain":{"amount":1.0,"size":1.0,"seed":1}}"#, 0).unwrap();
        let (r, _, _) = mean_rgb(&out);
        assert!((r - 128.0).abs() < 8.0, "grain must be zero-mean-ish, got {r}");
    }

    #[test]
    fn full_ts_shaped_record_parses() {
        // Field-name cross-check with the frontend (src/modules/editing.ts): a record
        // using every field exactly as the TS side serializes it must parse and render.
        let json = r#"{
            "crop": {"x": 0.1, "y": 0.1, "w": 0.8, "h": 0.8, "aspect": "1:1"},
            "tone": {"ev": 0.2, "contrast": 0.1, "highlights": -0.2, "shadows": 0.1,
                     "whites": 0.05, "blacks": -0.05, "vibrance": 0.1, "saturation": -0.2,
                     "wb": {"temp": 0.1, "tint": -0.05}},
            "straighten": 1.5,
            "bw": {"enabled": true, "r": 0.9, "g": 0.15, "b": -0.05},
            "split": {"shadow_hue": 35, "shadow_sat": 0.25, "highlight_hue": 45,
                      "highlight_sat": 0.12, "balance": 0.1},
            "grain": {"amount": 0.5, "size": 1.2, "seed": 42},
            "fade": 0.2,
            "vignette": -0.3,
            "lut": {"file": "some-film.cube", "amount": 0.8}
        }"#;
        let src = solid_jpeg(150, 120, 90);
        render_jpeg(&src, json, 0).expect("full TS-shaped record must parse and render");
    }

    #[test]
    fn is_bw_detects_mixer_and_full_desaturation() {
        assert!(is_bw(r#"{"bw":{"enabled":true}}"#), "enabled mixer is B&W");
        assert!(is_bw(r#"{"tone":{"saturation":-1}}"#), "saturation -1 is B&W");
        assert!(!is_bw(r#"{"bw":{"enabled":false}}"#), "disabled mixer is not");
        assert!(!is_bw(r#"{"tone":{"saturation":-0.5}}"#), "partial desat is not");
        assert!(!is_bw(""), "empty record is not");
        assert!(!is_bw("not json"), "garbage is not");
    }

    #[test]
    fn missing_lut_is_nonfatal() {
        // A record referencing a LUT file that doesn't exist must still render, and
        // identically to the same record without the LUT.
        let src = solid_jpeg(128, 96, 64);
        let with = render_jpeg(&src, r#"{"lut":{"file":"nope-missing.cube"}}"#, 0).unwrap();
        let without = render_jpeg(&src, "{}", 0).unwrap();
        assert_eq!(with, without, "missing LUT must degrade to a no-op");
    }

    #[test]
    fn whites_blacks_vibrance_at_zero_are_identity() {
        // All three new parameters at 0 must not change a neutral image.
        let src = solid_jpeg(128, 128, 128);
        let out = render_jpeg(
            &src,
            r#"{"tone":{"whites":0.0,"blacks":0.0,"vibrance":0.0}}"#,
            0,
        )
        .unwrap();
        let (r, _, _) = mean_rgb(&out);
        assert!(
            (r - 128.0).abs() < 4.0,
            "zero whites/blacks/vibrance should be near-identity, got r={r}"
        );
    }
}
