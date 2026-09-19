//! The Develop session's working images (docs/plans/raw-foundation): the full-resolution
//! linear decode of the RAW the Darkroom has open, held in memory for the session and
//! owned by the `develop` job family — a photo switch, a Develop exit, or a catalog switch
//! trips the claim and the images are released. Only compiled with both the decoder
//! (`raw`) and the engine (`edit`); without either, Develop keeps rendering the preview.

pub mod session;

pub use session::working_image_from;

use crate::plugins::edit::{SourceToken, WorkingImage};
use std::sync::{Arc, Mutex};

/// Resident working images: the current photo (and, in a later slice, its neighbours),
/// bounded by bytes. Inserting beyond the budget is refused rather than evicting the
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

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb32FImage;

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

    fn img(w: u32, h: u32) -> Arc<WorkingImage> {
        Arc::new(WorkingImage {
            width: w,
            height: h,
            linear: Rgb32FImage::new(w, h),
            cam_mul: [1.0; 4],
            rgb_cam: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            decoder: "test",
        })
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
