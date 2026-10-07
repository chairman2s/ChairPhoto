//! A cold loupe preview's stages (#168): what `preview_bytes` pays on a cache miss — the
//! read (or a RAW's embedded-preview extraction), the decode, the colour-space probe, the
//! downscale + encode of the preview and of the thumbnail it derives on the way — and what
//! the app then pays to decode the cached preview. Ignored; run it in release:
//!
//! ```sh
//! cargo test --release -p chairphoto-core thumbnails::bench -- --ignored --nocapture
//! ```
//!
//! `CHAIRPHOTO_PREVIEW_BENCH_FILE` names an original to time (a RAW times the extraction
//! path); the default is a generated 6000×4000 JPEG. `CHAIRPHOTO_PREVIEW_BENCH_N` sets the
//! runs per stage (default 5). Medians and maxima, one line per stage.

use super::*;
use std::time::{Duration, Instant};

fn time<T>(n: usize, mut f: impl FnMut(usize) -> T) -> (Duration, Duration, T) {
    let mut v = Vec::with_capacity(n);
    let mut last = None;
    for i in 0..n {
        let t = Instant::now();
        last = Some(f(i));
        v.push(t.elapsed());
    }
    v.sort();
    (v[v.len() / 2], *v.last().unwrap(), last.unwrap())
}

fn line(stage: &str, (p50, max): (Duration, Duration)) {
    println!("BENCH {stage:<34} p50 {:>8.1} ms  max {:>8.1} ms", p50.as_secs_f64() * 1e3, max.as_secs_f64() * 1e3);
}

#[test]
#[ignore = "a measurement (#168): run in release with --ignored --nocapture"]
fn cold_preview_stage_timings() {
    let n: usize = std::env::var("CHAIRPHOTO_PREVIEW_BENCH_N").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    let tmp = tests::TestTmpDir::new("preview-bench");
    let path = match std::env::var_os("CHAIRPHOTO_PREVIEW_BENCH_FILE") {
        Some(p) => PathBuf::from(p),
        None => {
            let p = tmp.path().join("bench.jpg");
            image::RgbImage::from_fn(6000, 4000, |x, y| image::Rgb([(x / 24) as u8, (y / 16) as u8, ((x ^ y) & 0xff) as u8]))
                .save(&p)
                .unwrap();
            p
        }
    };
    println!(
        "BENCH file {} ({} bytes) · {}",
        path.display(),
        std::fs::metadata(&path).unwrap().len(),
        if cfg!(debug_assertions) { "debug (not release numbers)" } else { "release" }
    );

    let source = if is_raw(&path) {
        let (p50, max, _) = time(n, |_| choose_preview_index(&path, PREVIEW_MAX).unwrap());
        line("raw: exiv2 -pp (choose preview)", (p50, max));
        let (p50, max, bytes) = time(n, |_| extract_raw_preview(&path, PREVIEW_MAX).unwrap());
        line("raw: extract (exiv2 -pp + -ep)", (p50, max));
        let (p50, max, _) = time(n, |_| exif_orientation(&path));
        line("raw: exif_orientation (exiv2)", (p50, max));
        bytes
    } else {
        let (p50, max, bytes) = time(n, |_| std::fs::read(&path).unwrap());
        line("read", (p50, max));
        bytes
    };
    let (p50, max, img) = time(n, |_| decode_oriented(&source, None).unwrap());
    line(&format!("decode {}x{}", img.width(), img.height()), (p50, max));
    let (p50, max, adobe) = time(n, |_| detect_adobe_rgb(&path));
    line(&format!("colour-space probe (adobe={adobe})"), (p50, max));
    let (p50, max, _) = time(n, |_| img.thumbnail(PREVIEW_MAX, PREVIEW_MAX));
    line("preview: downscale (image's)", (p50, max));
    let (p50, max, _) = time(n, |_| downscale::thumbnail(&img, PREVIEW_MAX));
    line("preview: downscale (ours)", (p50, max));
    let (p50, max, preview) = time(n, |_| encode_size(&path, &img, PREVIEW).unwrap());
    line("preview: downscale + encode", (p50, max));
    let small = downscale::thumbnail(&img, PREVIEW_MAX);
    let (p50, max, _) = time(n, |_| {
        let mut out = Cursor::new(Vec::new());
        small.write_with_encoder(JpegEncoder::new_with_quality(&mut out, PREVIEW.quality)).unwrap();
        out.into_inner()
    });
    line("preview: encode only (image's, pre-#243)", (p50, max));
    let (p50, max, _) = time(n, |_| encode_jpeg(&small, PREVIEW.quality).unwrap());
    line("preview: encode only (jpeg-encoder)", (p50, max));
    let (p50, max, _) = time(n, |_| encode_size(&path, &img, THUMB).unwrap());
    line("thumb (derived): downscale + encode", (p50, max));
    let (p50, max, _) = time(n, |_| image::load_from_memory(&preview).unwrap());
    line("app: decode the cached preview", (p50, max));
    // End to end: `preview_bytes` on a cache miss, each run on a fresh copy of the file (a
    // new cache key, and a colour-space probe not yet memoised), as a cold loupe step pays it.
    let _env = tests::test_lock();
    let copies: Vec<PathBuf> = (0..n)
        .map(|i| {
            let copy = tmp.path().join(format!("e2e-{i}.{}", path.extension().and_then(|e| e.to_str()).unwrap_or("jpg")));
            std::fs::copy(&path, &copy).unwrap();
            copy
        })
        .collect();
    let (p50, max, _) = time(n, |i| preview_bytes(&copies[i]).unwrap());
    line("end to end: preview_bytes, cold", (p50, max));
    let (p50, max, zoom) = time(1, |_| encode_size(&path, &img, ZOOM).unwrap());
    let zoom = image::load_from_memory(&zoom).unwrap();
    line(&format!("zoom: encode ({}x{})", zoom.width(), zoom.height()), (p50, max));
}
