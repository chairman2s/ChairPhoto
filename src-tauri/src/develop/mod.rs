//! The Develop session's working images (docs/plans/raw-foundation): the full-resolution
//! linear decode of the RAW the Darkroom has open, held in memory for the session and
//! owned by the `develop` job family — a photo switch, a Develop exit, or a catalog switch
//! trips the claim and the images are released. Only compiled with both the decoder
//! (`raw`) and the engine (`edit`); without either, Develop keeps rendering the preview.
//!
//! # Ownership
//!
//! The set is a process-global behind its own mutex, and that mutex is a **leaf** in the
//! lock order documented in `commands::jobs`: it is taken after any registry lock and never
//! held while taking another. Membership follows the `develop` claim: `session::open` and
//! `session::close` release on every ownership change, a catalog switch releases in its
//! detach phase (`DetachGuards::trip_and_clear_all`), and a superseded worker removes only
//! its own token. A stale token — one whose claim was tripped — therefore names nothing,
//! and `edit://` answers it with a 404 rather than other pixels.

pub mod cache;
pub mod offline;
pub mod session;
#[cfg(test)]
mod camera_fit;

pub use session::working_image_from;

use crate::plugins::edit::{SourceToken, WorkingImage};
use std::sync::{Arc, Mutex};

/// Resident working images: the current photo and its preloaded neighbours, bounded by
/// bytes. Inserting beyond the budget is refused rather than evicting the
/// current photo; a `clear` on every ownership change is the cleanup the product demands.
pub struct ResidentSet {
    budget_bytes: usize,
    images: Vec<(SourceToken, Arc<WorkingImage>)>,
}

/// One 67 MP float image is ~800 MB; four fit a 64 GB machine with room, and one fits a
/// 16 GB laptop.
pub const DEFAULT_BUDGET_BYTES: usize = 4 * 1024 * 1024 * 1024;

impl ResidentSet {
    pub const fn new(budget_bytes: usize) -> Self {
        ResidentSet { budget_bytes, images: Vec::new() }
    }

    pub fn get(&self, token: &SourceToken) -> Option<Arc<WorkingImage>> {
        self.images.iter().find(|(t, _)| t == token).map(|(_, i)| i.clone())
    }

    /// Any resident image of `photo_id`, with its token (a render outside the session
    /// reuses the session's decode rather than loading its own).
    pub fn find_photo(&self, photo_id: i64) -> Option<(SourceToken, Arc<WorkingImage>)> {
        self.images
            .iter()
            .find(|(t, _)| matches!(t, SourceToken::Working { photo_id: p, .. } if *p == photo_id))
            .map(|(t, i)| (t.clone(), i.clone()))
    }

    /// `false` when the image would exceed the budget (unless the set is empty: the current
    /// photo always fits — a budget below one image would otherwise mean no Develop at all).
    pub fn insert(&mut self, token: SourceToken, image: Arc<WorkingImage>) -> bool {
        let used: usize = self.images.iter().map(|(_, i)| i.bytes()).sum();
        if !self.images.is_empty() && used + image.bytes() > self.budget_bytes {
            return false;
        }
        self.images.retain(|(t, _)| *t != token);
        self.images.push((token, image));
        true
    }

    pub fn clear(&mut self) {
        self.images.clear();
    }

    /// Drop one token's image, if resident. A superseded worker's cleanup: it must not
    /// touch the image a newer claim has meanwhile published.
    pub fn remove(&mut self, token: &SourceToken) -> bool {
        let before = self.images.len();
        self.images.retain(|(t, _)| t != token);
        self.images.len() != before
    }

    /// The ownership change of a new claim: keep the images of `keep` (the newly opened
    /// photo and its neighbours) under the new `generation`, and drop everything else. The
    /// old tokens name nothing afterwards — a URL minted under the previous claim 404s,
    /// exactly as if the image had been released — while the pixels survive the step, which
    /// is what makes stepping to a preloaded neighbour instant. Returns the photo ids kept.
    pub fn retain_rekey(&mut self, keep: &[i64], generation: u64) -> Vec<i64> {
        self.images.retain(|(t, _)| matches!(t, SourceToken::Working { photo_id, .. } if keep.contains(photo_id)));
        let mut kept = Vec::new();
        for (t, _) in self.images.iter_mut() {
            if let SourceToken::Working { photo_id, .. } = *t {
                *t = SourceToken::Working { photo_id, generation };
                kept.push(photo_id);
            }
        }
        kept
    }

    pub fn len(&self) -> usize {
        self.images.len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }
}

static RESIDENT: Mutex<ResidentSet> = Mutex::new(ResidentSet::new(DEFAULT_BUDGET_BYTES));

/// The working image a token names, if it is resident right now.
pub fn resident(token: &SourceToken) -> Option<Arc<WorkingImage>> {
    RESIDENT.lock().ok()?.get(token)
}

pub(crate) fn with_resident<T>(f: impl FnOnce(&mut ResidentSet) -> T) -> T {
    let mut guard = RESIDENT.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// Release every working image. The cleanup half of every ownership transition — the
/// catalog switch calls it from its detach phase, so a switch cannot leave a replaced
/// catalog's decode resident and reachable by a token that names the new catalog's ids.
pub(crate) fn release_all() {
    with_resident(|r| r.clear());
    offline::clear();
}

/// How many bytes the resident images hold right now — the number a "did it clean up"
/// check reads.
pub fn resident_bytes() -> usize {
    with_resident(|r| r.images.iter().map(|(_, i)| i.bytes()).sum())
}

/// The resident set is process-global, so tests that assert on it must not interleave:
/// each takes this lock for its whole body.
#[cfg(test)]
pub(crate) fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A small synthetic working image for ownership tests (`w`×`h`, all black).
#[cfg(test)]
pub(crate) fn test_image(w: u32, h: u32) -> Arc<WorkingImage> {
    Arc::new(WorkingImage {
        width: w,
        height: h,
        linear: image::Rgb32FImage::new(w, h),
        cam_mul: [1.0; 4],
        pre_mul: [1.0; 4],
        rgb_cam: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        wbct: Vec::new(),
        decoder: "test",
        camera_ev: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs only with a real RAW at `CHAIRPHOTO_RAW_FIXTURE`: the distance between the
    /// engine-2 as-shot render and the camera's own preview, as mean sRGB values — the
    /// number that sets `BASELINE_EV` and the display default (docs/plans/raw-foundation,
    /// slice 2). Prints every stage's mean so a brightness bug is localizable.
    #[test]
    fn fixture_working_image_renders_near_the_camera_preview() {
        let Ok(fixture) = std::env::var("CHAIRPHOTO_RAW_FIXTURE") else {
            println!("SKIPPED: fixture_working_image_renders_near_the_camera_preview — set CHAIRPHOTO_RAW_FIXTURE");
            return;
        };
        use crate::plugins::edit::{render_proxy, RenderOpts, RenderSource, SourceToken};
        let path = std::path::Path::new(&fixture);
        let d = crate::raw::decode_linear(path, &std::sync::atomic::AtomicBool::new(false)).unwrap();
        let mean16 = d.rgb16.iter().map(|&v| v as f64).sum::<f64>() / d.rgb16.len() as f64 / 65535.0;
        let image = Arc::new(working_image_from(d));
        let src = &image.linear;
        let mean_lin = src.as_raw().iter().map(|&v| v as f64).sum::<f64>() / src.as_raw().len() as f64;
        let max_lin = src.as_raw().iter().cloned().fold(0.0f32, f32::max);
        println!("linear: mean16={mean16:.4} mean_f32={mean_lin:.4} max_f32={max_lin:.3} {}x{}", image.width, image.height);
        // Stage by stage: the downscale, the display transform, and both together.
        {
            use crate::plugins::edit::linear::{to_display, DisplayTransform, BASELINE_EV};
            let small = crate::plugins::edit::linear::downscale_linear(src, 720);
            let m = |v: &[f32]| v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64;
            println!("stage: downscale_linear(720) mean_f32={:.4} max={:.3}", m(small.as_raw()), small.as_raw().iter().cloned().fold(0.0f32, f32::max));
            let mu = |v: &[u8]| v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64;
            for ev in [0.0f32, 0.5, 1.0, 1.5] {
                let d = to_display(&small, DisplayTransform::Srgb, ev);
                let soft = to_display(&small, DisplayTransform::Soft { shoulder: 0.8 }, ev);
                println!("stage: baseline {ev:+.1} EV → srgb mean={:.1}  soft mean={:.1}", mu(d.as_raw()), mu(soft.as_raw()));
            }
            let _ = BASELINE_EV;
        }
        let token = SourceToken::Working { photo_id: 1, generation: 1 };
        let out = render_proxy(RenderSource::Working { token, image: image.clone() }, r#"{"engine": 2}"#, 720, RenderOpts::default()).unwrap().to_rgb8();
        let mean_out = out.as_raw().iter().map(|&v| v as f64).sum::<f64>() / out.as_raw().len() as f64;
        let preview = crate::thumbnails::preview_bytes(path).unwrap();
        let pv = image::load_from_memory(&preview).unwrap().to_rgb8();
        let mean_pv = pv.as_raw().iter().map(|&v| v as f64).sum::<f64>() / pv.as_raw().len() as f64;
        println!("display: engine2 mean={mean_out:.1}/255  camera preview mean={mean_pv:.1}/255  ({}x{} vs {}x{})", out.width(), out.height(), pv.width(), pv.height());
        assert!(mean_lin < 0.9, "the linear working image is not nearly white");
    }

    /// With a real RAW (`CHAIRPHOTO_RAW_FIXTURE`): the clip overlay marks the sensor's
    /// clipped share at the stage size, and a −3 EV pull changes nothing about it.
    #[test]
    fn fixture_clip_overlay_marks_what_the_sensor_clipped() {
        let Ok(fixture) = std::env::var("CHAIRPHOTO_RAW_FIXTURE") else {
            println!("SKIPPED: fixture_clip_overlay_marks_what_the_sensor_clipped — set CHAIRPHOTO_RAW_FIXTURE");
            return;
        };
        use crate::plugins::edit::{clip_overlay_png, SourceToken};
        let d = crate::raw::decode_linear(std::path::Path::new(&fixture), &std::sync::atomic::AtomicBool::new(false)).unwrap();
        let image = Arc::new(working_image_from(d));
        let full = image.linear.as_raw().chunks_exact(3).filter(|p| p.iter().any(|&c| c >= 1.0)).count();
        let token = SourceToken::Working { photo_id: 1, generation: 77 };
        let marked = |json: &str| {
            let png = clip_overlay_png(token.clone(), image.clone(), json, 1400).unwrap();
            let m = image::load_from_memory(&png).unwrap().to_rgba8();
            (m.pixels().filter(|p| p.0[3] > 0).count(), m.width() * m.height())
        };
        let (a, n) = marked(r#"{"engine":2,"display":"camera.2"}"#);
        let (b, _) = marked(r#"{"engine":2,"display":"camera.2","tone":{"ev":-3}}"#);
        println!(
            "clip: full-res clipped {:.3}% ; overlay at 1400 marks {:.3}% ({a} of {n})",
            100.0 * full as f64 / (image.width as f64 * image.height as f64),
            100.0 * a as f64 / n as f64
        );
        assert_eq!(a, b, "no slider moves the sensor's clipping");
    }

    /// With a real RAW (`CHAIRPHOTO_RAW_FIXTURE`): how far an engine-2 export sits from the
    /// view at Fit — resampling order alone — for several records. Sets
    /// `parity::PARITY_TOLERANCE`.
    #[test]
    fn fixture_export_matches_the_view_at_fit() {
        let Ok(fixture) = std::env::var("CHAIRPHOTO_RAW_FIXTURE") else {
            println!("SKIPPED: fixture_export_matches_the_view_at_fit — set CHAIRPHOTO_RAW_FIXTURE");
            return;
        };
        use crate::plugins::edit::{parity, render_image_opts, RenderOpts, RenderSource, SourceToken};
        let d = crate::raw::decode_linear(std::path::Path::new(&fixture), &std::sync::atomic::AtomicBool::new(false)).unwrap();
        let image = Arc::new(working_image_from(d));
        for (i, json) in [
            r#"{"engine":2,"display":"camera.2"}"#,
            r#"{"engine":2,"display":"camera.2","tone":{"ev":0.7,"contrast":0.4,"shadows":0.3}}"#,
            r#"{"engine":2,"display":"camera.2","straighten":2,"crop":{"x":0.1,"y":0.1,"w":0.7,"h":0.8},"grain":{"amount":0.5,"size":1.2},"vignette":-0.4}"#,
        ]
        .iter()
        .enumerate()
        {
            let token = SourceToken::Working { photo_id: 1, generation: 500 + i as u64 };
            let out = render_image_opts(RenderSource::Working { token: token.clone(), image: image.clone() }, json, 0, RenderOpts::default()).unwrap();
            let d = parity::fit_difference(&token, &image, json, &out).unwrap();
            println!("parity: record {i} mean |Δ| at Fit {d:.2} (tolerance {})", parity::PARITY_TOLERANCE);
            assert!(d <= parity::PARITY_TOLERANCE, "resampling alone should stay inside the tolerance");
        }
    }

    /// With a real RAW (`CHAIRPHOTO_RAW_FIXTURE`): the as-shot Kelvin and tint the camera's
    /// multipliers place, and that rendering them changes nothing.
    #[test]
    fn fixture_as_shot_kelvin_is_plausible_and_renders_as_shot() {
        let Ok(fixture) = std::env::var("CHAIRPHOTO_RAW_FIXTURE") else {
            println!("SKIPPED: fixture_as_shot_kelvin_is_plausible_and_renders_as_shot — set CHAIRPHOTO_RAW_FIXTURE");
            return;
        };
        use crate::plugins::edit::linear;
        let d = crate::raw::decode_linear(std::path::Path::new(&fixture), &std::sync::atomic::AtomicBool::new(false)).unwrap();
        println!("kelvin: cam_mul {:?} pre_mul {:?}", d.cam_mul, d.pre_mul);
        let (k, t) = linear::as_shot_kelvin(&d.cam_mul, &d.pre_mul, &d.rgb_cam, &d.wbct).expect("an as-shot light");
        println!("kelvin: as shot {k:.0} K, tint {t:+.1}");
        // The camera's own labelled presets (Sony stores WB_RGBLevels<N>K), when exiftool
        // can read them: what the model makes of each.
        if let Ok(out) = std::process::Command::new("exiftool").args(["-s", "-WB_RGBLevels*"]).arg(&fixture).output() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let Some((name, vals)) = line.split_once(':') else { continue };
                let v: Vec<f32> = vals.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if v.len() != 3 {
                    continue;
                }
                let cm = [v[0], v[1], v[2], v[1]];
                if let Some((pk, pt)) = linear::as_shot_kelvin(&cm, &d.pre_mul, &d.rgb_cam, &d.wbct) {
                    println!("kelvin: preset {:<28} → {pk:6.0} K, tint {pt:+.1}", name.trim());
                }
            }
        }
        assert!((1667.0..25000.0).contains(&k));
        let m = linear::wb_matrix(&linear::WbSpec::Kelvin { kelvin: k, tint: t }, &d.cam_mul, &d.pre_mul, &d.rgb_cam, &d.wbct).unwrap();
        for (i, row) in m.iter().enumerate() {
            for (j, v) in row.iter().enumerate() {
                assert!((v - if i == j { 1.0 } else { 0.0 }).abs() < 1e-3, "{m:?}");
            }
        }
    }

    fn img(w: u32, h: u32) -> Arc<WorkingImage> {
        test_image(w, h)
    }

    #[test]
    fn remove_drops_only_the_named_token() {
        let mut set = ResidentSet::new(usize::MAX);
        let a = SourceToken::Working { photo_id: 1, generation: 1 };
        let b = SourceToken::Working { photo_id: 2, generation: 2 };
        assert!(set.insert(a.clone(), img(4, 4)));
        assert!(set.insert(b.clone(), img(4, 4)));
        assert!(set.remove(&a));
        assert!(!set.remove(&a), "already gone");
        assert!(set.get(&a).is_none());
        assert!(set.get(&b).is_some(), "the other image is untouched");
    }

    #[test]
    fn retain_rekey_keeps_only_the_named_photos_under_the_new_generation() {
        let mut set = ResidentSet::new(usize::MAX);
        for id in [1, 2, 3] {
            assert!(set.insert(SourceToken::Working { photo_id: id, generation: 7 }, img(4, 4)));
        }
        let mut kept = set.retain_rekey(&[2, 3, 9], 8);
        kept.sort();
        assert_eq!(kept, vec![2, 3]);
        assert_eq!(set.len(), 2);
        for id in [2, 3] {
            assert!(set.get(&SourceToken::Working { photo_id: id, generation: 8 }).is_some());
            assert!(set.get(&SourceToken::Working { photo_id: id, generation: 7 }).is_none(), "old tokens name nothing");
        }
        assert!(set.get(&SourceToken::Working { photo_id: 1, generation: 7 }).is_none());
        assert!(set.retain_rekey(&[], 9).is_empty());
        assert!(set.is_empty());
    }

    #[test]
    fn resident_set_never_evicts_the_current_photo() {
        let one = img(100, 100); // 120 000 bytes
        let mut set = ResidentSet::new(one.bytes() + 10);
        let cur = SourceToken::Working { photo_id: 1, generation: 1 };
        assert!(set.insert(cur.clone(), one.clone()));
        let neighbour = SourceToken::Working { photo_id: 2, generation: 1 };
        assert!(!set.insert(neighbour.clone(), img(100, 100)), "over budget: refused");
        assert!(set.get(&cur).is_some(), "…and the current photo stays");
        assert!(set.get(&neighbour).is_none());
        // A budget too small for even one image still admits the current photo.
        let mut tiny = ResidentSet::new(1);
        assert!(tiny.insert(cur.clone(), one));
        set.clear();
        assert!(set.is_empty());
    }
}
