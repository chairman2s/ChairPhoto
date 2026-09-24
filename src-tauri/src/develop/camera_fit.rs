//! Measure how the camera renders its own JPEG from the same sensor data, on a corpus of
//! the user's RAWs, and fit engine 2's `camera` display transform to it
//! (docs/plans/raw-foundation, decision 5). Ignored: it decodes every file in
//! `CHAIRPHOTO_RAW_CORPUS` (minutes in debug; run with `--release`).
//!
//! ```text
//! CHAIRPHOTO_RAW_CORPUS=~/…/library cargo test --release --lib \
//!   develop::camera_fit -- --ignored --nocapture
//! ```
//!
//! `CHAIRPHOTO_FIT_EXCLUDE=a.ARW,b.ARW` leaves files out of the fit (say why in the
//! commit); `CHAIRPHOTO_FIT_DUMP=<dir>` writes plain sRGB | shipped `camera` | camera
//! JPEG side by side per photo. The camera JPEG is the app's own preview, so an Adobe RGB
//! body's JPEG is compared after the thumbnail pipeline's conversion to sRGB. Point
//! `XDG_CACHE_HOME` somewhere disposable: the previews go through the thumbnail cache.
//!
//! For each photo: the working image and the embedded preview are brought to the same
//! small size and orientation, the centre 80 % is kept (the camera corrects vignetting and
//! distortion into its JPEG; the plain decode does not), and every channel of every pixel
//! is a pair (linear value × baseline lift, camera JPEG value). The pooled pairs give a
//! per-channel tone curve and a saturation factor; the printout compares the plain sRGB
//! transform with the fitted one per photo.

use crate::plugins::edit::linear::{self, DisplayTransform, BASELINE_EV};
use image::{imageops, Rgb32FImage, RgbImage};

const EDGE: u32 = 256;

fn corpus() -> Vec<std::path::PathBuf> {
    let Ok(dir) = std::env::var("CHAIRPHOTO_RAW_CORPUS") else { return vec![] };
    let mut out = vec![];
    let mut stack = vec![std::path::PathBuf::from(dir)];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if matches!(
                p.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase()).as_deref(),
                Some("arw" | "dng" | "orf" | "cr2" | "cr3" | "nef" | "raf")
            ) {
                out.push(p);
            }
        }
    }
    // Files to leave out, by name, with the reason stated where the variable is set: an
    // extended low ISO (below the body's base) is exposed brighter and pulled down in the
    // camera's own processing — an exposure offset, not the picture style being fitted.
    let skip: Vec<String> = std::env::var("CHAIRPHOTO_FIT_EXCLUDE")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    out.retain(|p| !skip.iter().any(|s| p.file_name().is_some_and(|n| n.to_string_lossy() == *s)));
    out.sort();
    out
}

fn luma(p: [u8; 3]) -> f64 {
    0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64
}

/// Mean |Δ| over the centre crop, 0..255.
fn mean_abs(a: &RgbImage, b: &RgbImage) -> f64 {
    let (w, h) = a.dimensions();
    let (x0, y0, x1, y1) = (w / 10, h / 10, w - w / 10, h - h / 10);
    let mut s = 0.0;
    let mut n = 0.0;
    for y in y0..y1 {
        for x in x0..x1 {
            let (p, q) = (a.get_pixel(x, y).0, b.get_pixel(x, y).0);
            for c in 0..3 {
                s += (p[c] as f64 - q[c] as f64).abs();
                n += 1.0;
            }
        }
    }
    s / n
}

fn mean_luma(a: &RgbImage) -> f64 {
    a.pixels().map(|p| luma(p.0)).sum::<f64>() / (a.width() * a.height()) as f64
}

/// Median chroma (max − min) over the centre crop.
fn chroma(a: &RgbImage) -> f64 {
    let mut v: Vec<f64> = a
        .pixels()
        .map(|p| {
            let p = p.0;
            (*p.iter().max().unwrap() as f64) - (*p.iter().min().unwrap() as f64)
        })
        .collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// The preview turned and scaled to match the working image (the decode is oriented;
/// the embedded JPEG is not): of the turns whose aspect matches, the closest in luma.
fn matched_preview(preview: &RgbImage, small: &Rgb32FImage) -> RgbImage {
    let (w, h) = small.dimensions();
    let plain = linear::to_display(small, DisplayTransform::Srgb, BASELINE_EV);
    let cands = [
        preview.clone(),
        imageops::rotate90(preview),
        imageops::rotate270(preview),
        imageops::rotate180(preview),
    ];
    cands
        .into_iter()
        .filter(|c| (c.width() > c.height()) == (w > h))
        .map(|c| imageops::resize(&c, w, h, imageops::FilterType::Triangle))
        .min_by(|a, b| mean_abs(a, &plain).partial_cmp(&mean_abs(b, &plain)).unwrap())
        .expect("one turn matches")
}

#[test]
#[ignore]
fn fit_camera_transform() {
    let files = corpus();
    if files.is_empty() {
        println!("SKIPPED: fit_camera_transform — set CHAIRPHOTO_RAW_CORPUS to a folder of RAWs");
        return;
    }
    // Log-spaced bins of the lifted linear value, 2^-12 .. 2^3.
    const BINS: usize = 60;
    let (lo, hi) = (-12.0f64, 3.0f64);
    let bin = |x: f64| -> Option<usize> {
        if x <= 0.0 {
            return None;
        }
        let t = (x.log2() - lo) / (hi - lo);
        (0.0..1.0).contains(&t).then(|| (t * BINS as f64) as usize)
    };
    let mut pooled: Vec<Vec<f32>> = vec![vec![]; BINS];
    let mut pairs = vec![];
    let lift = 2f64.powf(BASELINE_EV as f64);
    for f in &files {
        let d = match crate::raw::decode_linear(f, &std::sync::atomic::AtomicBool::new(false)) {
            Ok(d) => d,
            Err(e) => {
                println!("skip {}: {e}", f.display());
                continue;
            }
        };
        let image = crate::develop::working_image_from(d);
        let small = linear::downscale_linear(&image.linear, EDGE);
        let preview = image::load_from_memory(&crate::thumbnails::preview_bytes(f).unwrap()).unwrap().to_rgb8();
        let pv = matched_preview(&preview, &small);
        let (w, h) = small.dimensions();
        for y in h / 10..h - h / 10 {
            for x in w / 10..w - w / 10 {
                let (s, p) = (small.get_pixel(x, y).0, pv.get_pixel(x, y).0);
                for c in 0..3 {
                    if let Some(b) = bin(s[c] as f64 * lift) {
                        pooled[b].push(p[c] as f32 / 255.0);
                    }
                }
            }
        }
        pairs.push((f.clone(), small, pv));
    }

    // Per-bin medians, smoothed over neighbours, then made monotone.
    let centre = |b: usize| 2f64.powf(lo + (b as f64 + 0.5) / BINS as f64 * (hi - lo));
    let mut knots: Vec<(f64, f64, usize)> = vec![];
    for (b, v) in pooled.iter_mut().enumerate() {
        if v.len() < 200 {
            continue;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        knots.push((centre(b), v[v.len() / 2] as f64, v.len()));
    }
    let raw_y: Vec<f64> = knots.iter().map(|k| k.1).collect();
    let mut best = 0.0f64;
    for (i, k) in knots.iter_mut().enumerate() {
        let a = i.saturating_sub(2);
        let b = (i + 3).min(raw_y.len());
        let m = raw_y[a..b].iter().sum::<f64>() / (b - a) as f64;
        best = best.max(m);
        k.1 = best;
    }
    println!("\ncamera curve (lifted linear → encoded), {} files:", pairs.len());
    for (x, y, n) in &knots {
        println!("  x={x:9.5}  y={y:.4}  srgb={:.4}  n={n}", linear::srgb_oetf((*x as f32).min(1.0)));
    }
    println!("rust:");
    let pts: Vec<String> = knots.iter().map(|(x, y, _)| format!("({x:.6}, {y:.4})")).collect();
    println!("[{}]", pts.join(", "));

    // Evaluate: the plain transform against the fitted curve, then the saturation factor.
    let curve = |x: f64| -> f64 {
        if x <= knots[0].0 {
            return knots[0].1 * x / knots[0].0;
        }
        for w in knots.windows(2) {
            if x <= w[1].0 {
                let t = (x.log2() - w[0].0.log2()) / (w[1].0.log2() - w[0].0.log2());
                return w[0].1 + t * (w[1].1 - w[0].1);
            }
        }
        knots.last().unwrap().1
    };
    let apply = |small: &Rgb32FImage, sat: f64| -> RgbImage {
        RgbImage::from_fn(small.width(), small.height(), |x, y| {
            let s = small.get_pixel(x, y).0;
            let v: Vec<f64> = s.iter().map(|&c| curve(c as f64 * lift)).collect();
            let l = 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2];
            let o: Vec<u8> = v.iter().map(|&c| ((l + (c - l) * sat) * 255.0).round().clamp(0.0, 255.0) as u8).collect();
            image::Rgb([o[0], o[1], o[2]])
        })
    };
    let mut ratios = vec![];
    for (_, small, pv) in &pairs {
        let c = apply(small, 1.0);
        ratios.push(chroma(pv) / chroma(&c).max(1.0));
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let sat = ratios[ratios.len() / 2];
    println!("\nsaturation factor after the curve (median over photos) = {sat:.3}  per photo {ratios:.2?}");
    println!("\nper photo: mean |Δ| vs camera JPEG (0..255), mean luma, median chroma");
    let (mut t0, mut t1, mut t2, mut t3) = (0.0, 0.0, 0.0, 0.0);
    for (f, small, pv) in &pairs {
        let plain = linear::to_display(small, DisplayTransform::Srgb, BASELINE_EV);
        let c = apply(small, 1.0);
        let cs = apply(small, sat);
        let shipped = linear::to_display(small, DisplayTransform::Camera, BASELINE_EV);
        if let Ok(dir) = std::env::var("CHAIRPHOTO_FIT_DUMP") {
            // Plain sRGB | shipped camera transform | camera JPEG, side by side.
            let (w, h) = shipped.dimensions();
            let mut side = RgbImage::new(w * 3 + 8, h);
            imageops::replace(&mut side, &plain, 0, 0);
            imageops::replace(&mut side, &shipped, (w + 4) as i64, 0);
            imageops::replace(&mut side, pv, (2 * w + 8) as i64, 0);
            let name = f.file_stem().unwrap().to_string_lossy();
            side.save(std::path::Path::new(&dir).join(format!("{name}.png"))).unwrap();
            let m = |i: &RgbImage| {
                let n = (i.width() * i.height()) as f64;
                let mut a = [0f64; 3];
                for p in i.pixels() {
                    for c in 0..3 {
                        a[c] += p.0[c] as f64 / n;
                    }
                }
                a
            };
            println!("  {name}: mean RGB shipped {:.1?} camera {:.1?}", m(&shipped), m(pv));
        }
        let (a, b, d) = (mean_abs(&plain, pv), mean_abs(&c, pv), mean_abs(&cs, pv));
        let e = mean_abs(&shipped, pv);
        t0 += a;
        t1 += b;
        t2 += d;
        t3 += e;
        println!(
            "  {:<16} srgb {a:5.1}  curve {b:5.1}  curve+sat {d:5.1}  shipped {e:5.1} | luma cam {:5.1} srgb {:5.1} fit {:5.1} | chroma cam {:4.0} srgb {:4.0} fit {:4.0}",
            f.file_name().unwrap().to_string_lossy(),
            mean_luma(pv),
            mean_luma(&plain),
            mean_luma(&cs),
            chroma(pv),
            chroma(&plain),
            chroma(&cs),
        );
    }
    let n = pairs.len() as f64;
    println!("  mean             srgb {:5.1}  curve {:5.1}  curve+sat {:5.1}  shipped {:5.1}", t0 / n, t1 / n, t2 / n, t3 / n);
}
