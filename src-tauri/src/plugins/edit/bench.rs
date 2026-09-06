//! Ignored stage bench for the interactive render path (increment 1 of the GPU-smoothness
//! work, docs/plans/darkroom/00-status.md; run line in docs/performance-harness.md). It
//! times each stage a Darkroom drag frame pays — cached-proxy clone, downscale, RGB copy,
//! the look loop, JPEG/PNG encode, base64 — plus the end-to-end `render_image`, at the
//! 720 px fast tier and the 1400 px settled tier, and the 1024 px masses pass. Medians of
//! N runs, one JSON line per edge, in whichever profile it was compiled for. Run it both
//! ways: `tauri dev` ships the debug profile, the release build is what users install.
//!
//! Source: `CHAIRPHOTO_EDIT_BENCH_JPEG=/path/to/proxy.jpg` (a real 2048 px preview) or a
//! synthetic 2048×1365 gradient-plus-noise proxy. `CHAIRPHOTO_EDIT_BENCH_LUT=/path.cube`
//! adds a 3D LUT to the look. `CHAIRPHOTO_EDIT_BENCH_N` sets the run count (default 10).
use super::*;
use std::time::Instant;

/// The full record a settled Darkroom frame can carry: every tone field, zones, split
/// toning, fade, vignette, grain — so the loop takes every branch.
const FULL_RECORD: &str = r#"{
  "tone": {"ev": 0.3, "contrast": 0.15, "highlights": -0.2, "shadows": 0.2, "whites": 0.1,
           "blacks": -0.1, "vibrance": 0.2, "saturation": 0.1, "wb": {"temp": 0.2, "tint": -0.1}},
  "zones": [0.1, 0.0, 0.2, 0.0, 0.0, 0.1, 0.0, -0.1],
  "split": {"shadow_hue": 35, "shadow_sat": 0.2, "highlight_hue": 45, "highlight_sat": 0.1, "balance": 0},
  "fade": 0.2, "vignette": -0.3,
  "grain": {"amount": 0.5, "size": 1.2, "seed": 7}
}"#;

fn synthetic_proxy() -> Vec<u8> {
    let (w, h) = (2048u32, 1365u32);
    let img = RgbImage::from_fn(w, h, |x, y| {
        // A luminance ramp with colour bands and a little hash noise: JPEG-realistic
        // entropy so decode/encode are not measuring a flat field.
        let l = (x * 255 / (w - 1)) as i32;
        let n = ((x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77)) >> 24) as i32 - 128;
        let l = (l + n / 6).clamp(0, 255) as u8;
        match y * 3 / h {
            0 => Rgb([l, l / 2, l / 4]),
            1 => Rgb([l / 4, l, l / 2]),
            _ => Rgb([l / 2, l / 4, l]),
        }
    });
    encode_jpeg(&DynamicImage::ImageRgb8(img), 90).unwrap()
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let out = f();
    (out, t.elapsed().as_secs_f64() * 1000.0)
}

fn encode_png_fast(img: &DynamicImage) -> Vec<u8> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_with_encoder(PngEncoder::new_with_quality(
        &mut out,
        CompressionType::Fast,
        FilterType::NoFilter,
    ))
    .unwrap();
    out.into_inner()
}

#[test]
#[ignore = "edit render stage bench; see docs/performance-harness.md"]
fn render_stage_timings() {
    let jpeg = match std::env::var("CHAIRPHOTO_EDIT_BENCH_JPEG") {
        Ok(p) => std::fs::read(&p).unwrap_or_else(|e| panic!("read {p}: {e}")),
        Err(_) => synthetic_proxy(),
    };
    let lut = std::env::var("CHAIRPHOTO_EDIT_BENCH_LUT").ok().map(|p| {
        let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {p}: {e}"));
        cube::CubeLut::parse(&text).unwrap_or_else(|e| panic!("parse {p}: {e}"))
    });
    let n: usize = std::env::var("CHAIRPHOTO_EDIT_BENCH_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let edit: EditRecord = serde_json::from_str(FULL_RECORD).unwrap();

    // Cold decode, then warm the one-slot cache so the per-frame path measures a hit.
    let (_, decode_miss) = timed(|| image::load_from_memory(&jpeg).unwrap());
    let proxy = decode_proxy_cached(&jpeg).unwrap();
    let (pw, ph) = proxy.dimensions();
    println!(
        "{{\"bench\":\"render_stage_timings\",\"profile\":\"{}\",\"source\":\"{}x{}\",\"lut\":{},\"n\":{},\"decode_miss_ms\":{:.2}}}",
        timing::PROFILE,
        pw,
        ph,
        lut.is_some(),
        n,
        decode_miss
    );

    for edge in [720u32, 1400u32] {
        let mut rows: Vec<(&str, f64)> = Vec::new();
        let mut col = |name: &'static str, xs: Vec<f64>| rows.push((name, median(xs)));
        let mut clone_ms = Vec::new();
        let mut down_ms = Vec::new();
        let mut rgb_ms = Vec::new();
        let mut look_ms = Vec::new();
        let mut jpeg_ms = Vec::new();
        let mut png_ms = Vec::new();
        let mut b64_ms = Vec::new();
        let mut total_ms = Vec::new();
        let mut out_dims = (0u32, 0u32);
        let mut jpeg_len = 0usize;
        for _ in 0..n {
            let (img, t) = timed(|| decode_proxy_cached(&jpeg).unwrap());
            clone_ms.push(t);
            let (small, t) = timed(|| img.thumbnail(edge, edge));
            down_ms.push(t);
            let (mut rgb, t) = timed(|| small.to_rgb8());
            rgb_ms.push(t);
            out_dims = rgb.dimensions();
            let (_, t) = timed(|| look::apply_look(&mut rgb, &edit, lut.as_ref()));
            look_ms.push(t);
            let out = DynamicImage::ImageRgb8(rgb);
            let (bytes, t) = timed(|| encode_jpeg(&out, 90).unwrap());
            jpeg_ms.push(t);
            jpeg_len = bytes.len();
            let (_, t) = timed(|| encode_png_fast(&out));
            png_ms.push(t);
            let (_, t) = timed(|| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes));
            b64_ms.push(t);
            let (_, t) = timed(|| render_image(decode_proxy_cached(&jpeg).unwrap(), FULL_RECORD, edge).unwrap());
            total_ms.push(t);
        }
        col("decode_cache_clone", clone_ms);
        col("downscale", down_ms);
        col("to_rgb8", rgb_ms);
        col("look", look_ms);
        col("encode_jpeg_90", jpeg_ms);
        col("encode_png_fast", png_ms);
        col("base64", b64_ms);
        col("render_image_total", total_ms);
        let mut line = format!(
            "{{\"edge\":{edge},\"out\":\"{}x{}\",\"jpeg_bytes\":{jpeg_len}",
            out_dims.0, out_dims.1
        );
        for (name, v) in &rows {
            line.push_str(&format!(",\"{name}_ms\":{v:.2}"));
        }
        line.push('}');
        println!("{line}");
    }

    // The settle also pays a 1024 px re-render for the tone strip's masses.
    let mut masses_ms = Vec::new();
    for _ in 0..n {
        let (_, t) = timed(|| {
            let out = render_image(decode_proxy_cached(&jpeg).unwrap(), FULL_RECORD, 1024).unwrap();
            zones::zone_masses(&out.to_rgb8())
        });
        masses_ms.push(t);
    }
    println!("{{\"edge\":1024,\"masses_pass_ms\":{:.2}}}", median(masses_ms));
}
