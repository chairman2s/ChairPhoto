//! A face's avatar crop, rendered on the image-pool worker (#223 F1).
//!
//! People-view avatars for a cover photo used to ask for the whole `Preview` tier (up to
//! 2048 px, tens of MB) just to show a 72 px circle — claiming one per visible covered
//! avatar (plus overscan) could exceed [`ImageStore`](../../../../app/image_store.rs)'s
//! budget, which evicts without regard to claims, so the evicted ones were asked for again
//! every frame and the view never settled (reviewed in `agent-notes/reviews/claude-old-images.log`,
//! F1). This renders only the avatar's own pixels instead: a small square cut from the face,
//! the same size the view ever draws.

use crate::app::AppState;
use crate::image_pool::{AvatarJob, ImageKind, JobKey};
use image::{imageops::FilterType, DynamicImage};

/// [`crate::media::render_image`]'s `JobKey::Avatar` arm: crop `job`'s face out of its
/// photo's `Preview` tier — always the original's frame, never a cover render (#152;
/// `media::decode_tier`'s `Preview` arm never sets `DecodedImage::cover`, so a photo's cover
/// never needs checking here) — and scale it to a `job.size`×`job.size` square with the face
/// filling it. [`crop_avatar`] is the same geometry
/// `modules::faces::logic::avatar_placement` places client-side (the app crate), done in
/// pixels instead so nothing bigger than the avatar itself is ever decoded or held for it.
pub fn render_avatar(state: &AppState, job: &AvatarJob) -> Result<DynamicImage, String> {
    let decoded = crate::media::render_image(state, JobKey::photo(job.photo_id, ImageKind::Preview))?;
    Ok(crop_avatar(&decoded.image, job.bbox(), job.size))
}

/// A `size`×`size` square cut from `image`, the normalized `bbox` (`(x, y, w, h)`, top-left
/// corner, in `image`'s own frame) centred and filling it: the square whose side is the
/// face's longer side (in `image`'s pixels) scales exactly onto `size`, clamped to `image`'s
/// bounds for a face near an edge. Mirrors `avatar_placement`'s face/scale math
/// (`crates/app/src/modules/faces/logic.rs`) — duplicated, not shared, because this crate
/// does not depend on the app crate.
pub fn crop_avatar(image: &DynamicImage, bbox: (f32, f32, f32, f32), size: u32) -> DynamicImage {
    let (iw, ih) = (image.width().max(1), image.height().max(1));
    let (nw, nh) = (iw as f32, ih as f32);
    let (x, y, w, h) = bbox;
    let face = (w.max(0.01) * nw).max(h.max(0.01) * nh).max(1.);
    let (fx, fy) = ((x + w / 2.) * nw, (y + h / 2.) * nh);
    let side = (face.round() as u32).clamp(1, iw.min(ih));
    let left = (fx - face / 2.).round().clamp(0., (iw - side) as f32) as u32;
    let top = (fy - face / 2.).round().clamp(0., (ih - side) as f32) as u32;
    let size = size.max(1);
    image.crop_imm(left, top, side, side).resize_exact(size, size, FilterType::Triangle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    fn solid(w: u32, h: u32) -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb([10, 20, 30])))
    }

    /// `bg` everywhere except a `2*half`-wide square centred on `mark`, which is `fg`: lets a
    /// test check *where* a crop landed, not just its size (a crop of the wrong square, if it
    /// happened to still be square, would pass a size-only check).
    fn marked(w: u32, h: u32, mark: (u32, u32), half: u32, bg: [u8; 3], fg: [u8; 3]) -> DynamicImage {
        let mut img = image::RgbImage::from_pixel(w, h, image::Rgb(bg));
        let (mx, my) = mark;
        let (x0, y0) = (mx.saturating_sub(half), my.saturating_sub(half));
        let (x1, y1) = ((mx + half).min(w), (my + half).min(h));
        for y in y0..y1 {
            for x in x0..x1 {
                img.put_pixel(x, y, image::Rgb(fg));
            }
        }
        DynamicImage::ImageRgb8(img)
    }

    fn rgb_at(image: &DynamicImage, x: u32, y: u32) -> [u8; 3] {
        let p = image.get_pixel(x, y).0;
        [p[0], p[1], p[2]]
    }

    /// A centred face crops to a square the requested size, centred on the box: a marker
    /// placed exactly at the face's centre lands at the output's centre too.
    #[test]
    fn a_centred_box_crops_to_a_centred_square() {
        let (bg, fg) = ([10, 20, 30], [250, 5, 5]);
        // bbox (0.4,0.4,0.2,0.2) on a 1000x1000 image: face centre (500,500), side 200.
        let img = marked(1000, 1000, (500, 500), 10, bg, fg);
        let out = crop_avatar(&img, (0.4, 0.4, 0.2, 0.2), 144);
        assert_eq!((out.width(), out.height()), (144, 144));
        assert_eq!(rgb_at(&out, 72, 72), fg, "the face's centre marker is not at the crop's centre");
    }

    /// A box near the edge clamps the crop into the image instead of panicking or going out
    /// of bounds: a marker at the image's corner lands at the clamped crop's corner, not
    /// where an unclamped (off-image) crop would have put it.
    #[test]
    fn a_box_at_the_edge_clamps_into_the_image() {
        let (bg, fg) = ([10, 20, 30], [250, 5, 5]);
        let img = marked(500, 300, (5, 5), 5, bg, fg);
        let out = crop_avatar(&img, (0.0, 0.0, 0.3, 0.3), 144);
        assert_eq!((out.width(), out.height()), (144, 144));
        assert_eq!(rgb_at(&out, 0, 0), fg, "the corner marker is not at the clamped crop's corner");

        // A box whose nominal centre falls outside the image altogether (0.9 + 0.3/2 > 1):
        // the clamped crop is the image's bottom-right 150×150 corner, square — (350,150) to
        // (500,300) — not a narrower, off-corner sliver (`crop_imm`'s own bounds clipping
        // would silently produce one, anchored at the unclamped — and here out-of-range —
        // (450,240), without this function's own clamp). A marker just inside the expected
        // square's corner, at (375,175), is outside that narrower sliver, so it only shows
        // up in the output if the clamp put the crop where it belongs.
        let img2 = marked(500, 300, (375, 175), 5, bg, fg);
        let out2 = crop_avatar(&img2, (0.9, 0.9, 0.3, 0.3), 144);
        assert_eq!((out2.width(), out2.height()), (144, 144));
        assert_eq!(rgb_at(&out2, 24, 24), fg, "the crop is not anchored at the image's actual bottom-right corner");
    }

    /// A wide (landscape) box's longer side — not a fixed axis — decides the crop's side.
    #[test]
    fn the_longer_side_of_the_box_sizes_the_square() {
        let wide = crop_avatar(&solid(1000, 1000), (0.1, 0.1, 0.5, 0.1), 144);
        let tall = crop_avatar(&solid(1000, 1000), (0.1, 0.1, 0.1, 0.5), 144);
        assert_eq!((wide.width(), wide.height()), (144, 144));
        assert_eq!((tall.width(), tall.height()), (144, 144));
    }

    /// A degenerate (zero-size) box still produces the requested square, not a panic.
    #[test]
    fn a_zero_size_box_does_not_panic() {
        let out = crop_avatar(&solid(200, 200), (0.5, 0.5, 0.0, 0.0), 144);
        assert_eq!((out.width(), out.height()), (144, 144));
    }
}
