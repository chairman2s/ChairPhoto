//! Tone-strip zones: per-EV-zone exposure offsets rendered as a per-luma gain LUT
//! (docs/plans/darkroom). Slice 1 (tracer): the LUT is the identity — the record field,
//! the wiring, and the contract (zeroed zones render byte-identically to no zones) land
//! first; the real feathered curve and the zone-mass histogram arrive in slice 2.

/// 256-entry per-luma gain LUT from 8 zone EV offsets (blacks→whites).
///
/// All-zero zones MUST produce the identity — old records with a zeroed strip have to
/// keep rendering bit-identically, and the test below locks that in for the slice-2
/// implementation too.
pub(super) fn zone_gain_lut(_zones: &[f32; 8]) -> [f32; 256] {
    [1.0; 256]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_jpeg() -> Vec<u8> {
        // A small gradient with all three channels in play.
        let img = image::RgbImage::from_fn(64, 48, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 5) as u8, ((x + y) * 2) as u8])
        });
        crate::plugins::edit::encode_jpeg(&image::DynamicImage::ImageRgb8(img), 95).unwrap()
    }

    #[test]
    fn zones_all_zero_is_identity() {
        assert!(zone_gain_lut(&[0.0; 8]).iter().all(|&g| g == 1.0));
    }

    #[test]
    fn zeroed_zones_render_byte_identical_to_no_zones() {
        let jpeg = synthetic_jpeg();
        let without = crate::plugins::edit::render_jpeg(&jpeg, r#"{"tone":{"ev":0.2}}"#, 0).unwrap();
        let with = crate::plugins::edit::render_jpeg(
            &jpeg,
            r#"{"tone":{"ev":0.2},"zones":[0,0,0,0,0,0,0,0]}"#,
            0,
        )
        .unwrap();
        assert_eq!(without, with, "a zeroed zone strip must not change a render");
    }

    #[test]
    fn v1_record_without_zones_parses_and_renders() {
        let jpeg = synthetic_jpeg();
        let v1 = r#"{"crop":{"x":0.1,"y":0.1,"w":0.5,"h":0.5},"tone":{"ev":0.5}}"#;
        assert!(crate::plugins::edit::render_jpeg(&jpeg, v1, 0).is_ok());
    }
}
