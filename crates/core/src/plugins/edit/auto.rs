//! Classical auto-tone (docs/plans/darkroom): a percentile analysis of a luma histogram
//! → a conservative starting fragment (ev / contrast / highlights / shadows) for the
//! proof sheet's Auto cells. Deliberately classical and boring — a learned model can
//! replace the command's internals later without the surface changing (00-status.md).

use image::RgbImage;

pub struct AutoTone {
    pub ev: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
}

/// 256-bin Rec. 709 luma histogram, subsampled to at most ~1M samples (the same
/// sampling zones.rs and export::luma_histogram use).
fn luma_histogram(img: &RgbImage) -> [u64; 256] {
    let (w, h) = img.dimensions();
    let mut hist = [0u64; 256];
    if w == 0 || h == 0 {
        return hist;
    }
    let step = (((w as u64 * h as u64) as f64 / 1_000_000.0).sqrt().ceil() as u32).max(1);
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            let p = img.get_pixel(x, y).0;
            let luma = (2126 * p[0] as u32 + 7152 * p[1] as u32 + 722 * p[2] as u32) / 10000;
            hist[luma as usize] += 1;
            x += step;
        }
        y += step;
    }
    hist
}

/// Smallest luma (0..1) whose CDF reaches fraction `q`. 0.5 for an empty histogram.
fn percentile(hist: &[u64; 256], q: f64) -> f32 {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return 0.5;
    }
    let target = ((total as f64 * q) as u64).max(1);
    let mut acc = 0u64;
    for (i, &c) in hist.iter().enumerate() {
        acc += c;
        if acc >= target {
            return i as f32 / 255.0;
        }
    }
    1.0
}

/// The suggestion, from a histogram. Gentle on purpose: Auto is a starting point the
/// proof sheet builds looks on, never a finished edit.
pub fn suggest_auto_tone(hist: &[u64; 256]) -> AutoTone {
    let p05 = percentile(hist, 0.05);
    let median = percentile(hist, 0.50);
    let p95 = percentile(hist, 0.95);
    // Push the median toward a slightly-bright mid-grey at 60% strength, ±1.2 EV cap.
    let ev = ((0.42f32 / median.clamp(0.02, 0.98)).log2() * 0.6).clamp(-1.2, 1.2);
    // A thin tonal spread earns contrast; a wide one earns none.
    let spread = (p95 - p05).max(0.0);
    let contrast = ((0.55 - spread) * 0.8).clamp(0.0, 0.3);
    // A bright tail near clipping → recover; a crushed toe → lift.
    let highlights = (-(p95 - 0.90).max(0.0) * 4.0).clamp(-0.5, 0.0);
    let shadows = ((0.06 - p05).max(0.0) * 5.0).clamp(0.0, 0.4);
    AutoTone { ev, contrast, highlights, shadows }
}

/// Histogram + suggestion in one call — the `suggest_auto_tone` command's worker.
pub fn auto_tone_for(img: &RgbImage) -> AutoTone {
    suggest_auto_tone(&luma_histogram(img))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A histogram with `share` of the mass at `at` and the rest spread mid (64..192).
    fn hist_with(at: usize, share: f64) -> [u64; 256] {
        let mut h = [0u64; 256];
        let total = 100_000u64;
        h[at] = (total as f64 * share) as u64;
        let rest = total - h[at];
        for i in 64..192 {
            h[i] += rest / 128;
        }
        h
    }

    #[test]
    fn auto_tone_brightens_dark_histogram() {
        // Mass piled in the deep shadows.
        let mut h = [0u64; 256];
        for i in 20..60 {
            h[i] = 1000;
        }
        let a = suggest_auto_tone(&h);
        assert!(a.ev > 0.3, "dark scene should brighten, got ev {}", a.ev);
        assert!(a.highlights == 0.0, "nothing bright to recover");
    }

    #[test]
    fn auto_tone_leaves_good_exposure_alone() {
        // A mid-heavy bell: median near 0.5, no clipped tails.
        let mut h = [0u64; 256];
        for i in 64..192 {
            h[i] = 1000;
        }
        let a = suggest_auto_tone(&h);
        assert!(a.ev.abs() < 0.2, "balanced scene barely moves, got ev {}", a.ev);
        assert!(a.contrast < 0.15, "got contrast {}", a.contrast);
        assert_eq!(a.highlights, 0.0);
        assert_eq!(a.shadows, 0.0);
    }

    #[test]
    fn auto_tone_recovers_clipped_highlights() {
        let a = suggest_auto_tone(&hist_with(252, 0.2));
        assert!(a.highlights < -0.1, "clipped tail should recover, got {}", a.highlights);
    }

    #[test]
    fn auto_tone_for_runs_on_an_image() {
        let img = RgbImage::from_fn(80, 60, |x, _| image::Rgb([(x * 3) as u8; 3]));
        let a = auto_tone_for(&img);
        assert!(a.ev.is_finite() && a.contrast.is_finite());
    }
}
