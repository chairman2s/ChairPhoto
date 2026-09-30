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
        let mut proxy_hit_ms = Vec::new();
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
            let (_, t) = timed(|| encode_png_fast(&out).unwrap());
            png_ms.push(t);
            let (_, t) = timed(|| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes));
            b64_ms.push(t);
            let (_, t) = timed(|| render_image(decode_proxy_cached(&jpeg).unwrap(), FULL_RECORD, edge).unwrap());
            total_ms.push(t);
            // The drag path: same geometry as the previous frame → framed-base cache hit.
            let (_, t) = timed(|| render_proxy(RenderSource::PreviewJpeg(&jpeg), FULL_RECORD, edge, RenderOpts::default()).unwrap());
            proxy_hit_ms.push(t);
        }
        col("decode_cache_clone", clone_ms);
        col("downscale", down_ms);
        col("to_rgb8", rgb_ms);
        col("look", look_ms);
        col("encode_jpeg_90", jpeg_ms);
        col("encode_png_fast", png_ms);
        col("base64", b64_ms);
        col("render_image_total", total_ms);
        // The first render_proxy of an edge is the miss that fills the cache; hits only.
        proxy_hit_ms.remove(0);
        if proxy_hit_ms.is_empty() {
            proxy_hit_ms.push(f64::NAN);
        }
        col("render_proxy_cached", proxy_hit_ms);
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
            let out = render_proxy(RenderSource::PreviewJpeg(&jpeg), FULL_RECORD, 1024, RenderOpts::default()).unwrap();
            zones::zone_masses(&out.to_rgb8())
        });
        masses_ms.push(t);
    }
    println!("{{\"edge\":1024,\"masses_pass_ms\":{:.2}}}", median(masses_ms));
}

/// Ignored bench (docs/plans/lens-corrections, slice 3): what the camera's lens corrections
/// cost a framed-base miss on an A7R VI-sized working image (9984×6656) — against the
/// plain copy a miss pays without them: the vignetting-only pass and the full pass
/// (vignetting, distortion, lateral CA). Medians of `CHAIRPHOTO_EDIT_BENCH_N` runs
/// (default 5), one JSON line:
/// `cargo test [--release] --lib plugins::edit::bench::lens_stage_timings -- --ignored --nocapture`.
#[cfg(feature = "raw")]
#[test]
#[ignore = "lens-correction stage bench; prints timings"]
fn lens_stage_timings() {
    let n: usize = std::env::var("CHAIRPHOTO_EDIT_BENCH_N").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    let (w, h) = (9984u32, 6656u32);
    let src = Rgb32FImage::from_fn(w, h, |x, y| image::Rgb([x as f32 / w as f32, y as f32 / h as f32, 0.3]));
    let radial = |values: Vec<f32>| crate::lens::Radial { knots: (0..16).map(|i| i as f32 / 15.0).collect(), values };
    // Shaped like _DSC7742's tables (Sigma 24-70 at 25 mm): barrel to −4.75 %, CA ±0.02 %.
    let lens = crate::lens::LensCorrection {
        source: "bench".into(),
        vignetting: Some(radial((0..16).map(|i| 1.0 + i as f32 * 0.023).collect())),
        distortion: Some(radial((0..16).map(|i| 1.0 - i as f32 * 0.0032).collect())),
        chromatic: Some([radial(vec![1.0002; 16]), radial(vec![0.9999; 16])]),
    };
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let time = |f: &dyn Fn() -> Rgb32FImage| {
        median(
            (0..n)
                .map(|_| {
                    let t = Instant::now();
                    std::hint::black_box(f());
                    t.elapsed().as_secs_f64() * 1e3
                })
                .collect(),
        )
    };
    let copy = time(&|| src.clone());
    let vignetting = time(&|| radial_pass(&src, &lens, false, lens.vignetting.as_ref()));
    let full = time(&|| radial_pass(&src, &lens, true, lens.vignetting.as_ref()));
    println!(
        "{{\"bench\":\"lens_stages\",\"profile\":\"{}\",\"px\":\"{w}x{h}\",\"n\":{n},\"copy_ms\":{copy:.0},\"vignetting_ms\":{vignetting:.0},\"full_ms\":{full:.0}}}",
        timing::PROFILE
    );
}

/// Ignored bench: a whole framed-base miss (stage render at 720 px after a straighten
/// change) on an A7R VI-sized working image, with the lens correction off and on — the
/// cost a straighten drag frame or a photo's first render pays.
#[cfg(feature = "raw")]
#[test]
#[ignore = "lens-correction render bench; prints timings"]
fn lens_framed_miss_timings() {
    let n: usize = std::env::var("CHAIRPHOTO_EDIT_BENCH_N").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    let (w, h) = (9984u32, 6656u32);
    let radial = |values: Vec<f32>| crate::lens::Radial { knots: (0..16).map(|i| i as f32 / 15.0).collect(), values };
    let mut img = super::tests_support_working(w, h);
    img.lens = Some(crate::lens::LensCorrection {
        source: "bench".into(),
        vignetting: Some(radial((0..16).map(|i| 1.0 + i as f32 * 0.023).collect())),
        distortion: Some(radial((0..16).map(|i| 1.0 - i as f32 * 0.0032).collect())),
        chromatic: Some([radial(vec![1.0002; 16]), radial(vec![0.9999; 16])]),
    });
    let img = std::sync::Arc::new(img);
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let mut line = format!("{{\"bench\":\"lens_framed_miss\",\"profile\":\"{}\",\"px\":\"{w}x{h}\",\"n\":{n}", timing::PROFILE);
    for (name, lens) in [("off", ""), ("on", r#","lens":{"builtin":true}"#)] {
        let mut v = vec![];
        for i in 0..n {
            // A new straighten angle every run: always a miss, as during a drag.
            let json = format!(r#"{{"engine":2,"display":"camera.2","straighten":{}{lens}}}"#, 0.5 + i as f32 * 0.1);
            let token = SourceToken::Working { photo_id: 77, generation: 1 };
            let t = Instant::now();
            std::hint::black_box(render_proxy(RenderSource::Working { token, image: img.clone() }, &json, 720, RenderOpts::default()).unwrap());
            v.push(t.elapsed().as_secs_f64() * 1e3);
        }
        line.push_str(&format!(",\"straighten_{name}_ms\":{:.0}", median(v)));
        let mut v = vec![];
        for i in 0..n {
            // No geometry, a new photo token every run: the first render of an opened photo.
            let json = format!(r#"{{"engine":2,"display":"camera.2"{lens}}}"#);
            let token = SourceToken::Working { photo_id: 78, generation: 10 + i as u64 };
            let t = Instant::now();
            std::hint::black_box(render_proxy(RenderSource::Working { token, image: img.clone() }, &json, 720, RenderOpts::default()).unwrap());
            v.push(t.elapsed().as_secs_f64() * 1e3);
        }
        line.push_str(&format!(",\"open_{name}_ms\":{:.0}", median(v)));
    }
    println!("{line}}}");
}
