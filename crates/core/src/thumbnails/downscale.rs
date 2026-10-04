//! The cache tiers' downscale (#168): `DynamicImage::thumbnail`'s output, computed fast.
//!
//! `image`'s `thumbnail` averages each output pixel's block of source pixels through the
//! generic `GenericImageView::get_pixel`, one bounds-checked pixel at a time: 160 ms to
//! shrink a 24 MP decode to the 2048 px preview, more than the decode itself, on every cold
//! loupe preview and every cold grid thumbnail. For the 8-bit images every JPEG decodes to,
//! [`thumbnail`] runs the same algorithm over the raw buffer instead — the same block bounds
//! (computed in the same `f32` arithmetic), the same integer sums and rounding — so its
//! output is **byte-identical** to `image`'s (the tests compare them), and the cached tiers do
//! not change. Anything else (16-bit, float, a size that is not a shrink) is `image`'s own.

use image::{DynamicImage, ImageBuffer, Pixel};

/// `img.thumbnail(max, max)`: the largest size that fits in `max` × `max` with the aspect
/// kept, block-averaged.
pub(super) fn thumbnail(img: &DynamicImage, max: u32) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    let (nw, nh) = fit_dimensions(w, h, max);
    if nw > w || nh > h || nw == 0 || nh == 0 {
        return img.thumbnail(max, max);
    }
    match img {
        DynamicImage::ImageRgb8(b) => DynamicImage::ImageRgb8(shrink(b, nw, nh)),
        DynamicImage::ImageRgba8(b) => DynamicImage::ImageRgba8(shrink(b, nw, nh)),
        DynamicImage::ImageLuma8(b) => DynamicImage::ImageLuma8(shrink(b, nw, nh)),
        DynamicImage::ImageLumaA8(b) => DynamicImage::ImageLumaA8(shrink(b, nw, nh)),
        _ => img.thumbnail(max, max),
    }
}

/// `image`'s `resize_dimensions(w, h, max, max, false)`: scale by the smaller ratio, rounded,
/// at least 1 px.
fn fit_dimensions(w: u32, h: u32, max: u32) -> (u32, u32) {
    let ratio = f64::min(f64::from(max) / f64::from(w), f64::from(max) / f64::from(h));
    let nw = ((f64::from(w) * ratio).round() as u64).max(1);
    let nh = ((f64::from(h) * ratio).round() as u64).max(1);
    (nw.min(u64::from(u32::MAX)) as u32, nh.min(u64::from(u32::MAX)) as u32)
}

/// Each output index's half-open source span, as `imageops::thumbnail` computes it for a
/// shrink (`ratio` ≥ 1, so every span holds at least one source index).
fn spans(len: u32, new_len: u32) -> Vec<(usize, usize)> {
    let ratio = len as f32 / new_len as f32;
    (0..new_len)
        .map(|o| {
            let lo_f = o as f32 * ratio;
            let hi_f = lo_f + ratio;
            let lo = (lo_f.ceil() as u32).clamp(0, len - 1);
            let hi = (hi_f.ceil() as u32).clamp(lo, len);
            (lo as usize, hi as usize)
        })
        .collect()
}

/// The block average of `src` at `nw` × `nh` (both no larger than `src`'s).
fn shrink<P: Pixel<Subpixel = u8>>(src: &ImageBuffer<P, Vec<u8>>, nw: u32, nh: u32) -> ImageBuffer<P, Vec<u8>> {
    let c = P::CHANNEL_COUNT as usize;
    let (w, h) = src.dimensions();
    let (cols, rows) = (spans(w, nw), spans(h, nh));
    let stride = w as usize * c;
    let data = src.as_raw();
    let mut out = vec![0u8; nw as usize * nh as usize * c];
    // One output row at a time: its source rows summed column-wise first (a straight,
    // vectorisable pass over the bytes), then each block's columns. Integer sums in another
    // order are the same sums.
    let mut acc = vec![0u32; stride];
    for (oy, &(top, bottom)) in rows.iter().enumerate() {
        acc.fill(0);
        for y in top..bottom {
            for (a, &v) in acc.iter_mut().zip(&data[y * stride..(y + 1) * stride]) {
                *a += u32::from(v);
            }
        }
        let line = &mut out[oy * nw as usize * c..(oy + 1) * nw as usize * c];
        let rows = (bottom - top) as u32;
        match c {
            1 => blocks::<1>(&acc, &cols, rows, line),
            2 => blocks::<2>(&acc, &cols, rows, line),
            3 => blocks::<3>(&acc, &cols, rows, line),
            _ => blocks::<4>(&acc, &cols, rows, line),
        }
    }
    ImageBuffer::from_raw(nw, nh, out).expect("the buffer is nw × nh × channels")
}

/// One output row from its column sums `acc`: each block's `C` channels summed over its
/// columns and averaged over `rows` × its width, rounded as `image` rounds.
fn blocks<const C: usize>(acc: &[u32], cols: &[(usize, usize)], rows: u32, line: &mut [u8]) {
    for (&(left, right), out) in cols.iter().zip(line.chunks_exact_mut(C)) {
        let mut sum = [0u32; C];
        for px in acc[left * C..right * C].chunks_exact(C) {
            for (s, &v) in sum.iter_mut().zip(px) {
                *s += v;
            }
        }
        let n = (right - left) as u32 * rows;
        let round = n / 2;
        let div = Divide::by(n);
        for (o, s) in out.iter_mut().zip(sum) {
            *o = div.of(s + round).min(255) as u8;
        }
    }
}

/// Exact `x / n` for the averages above, without a hardware divide per channel (which was
/// most of the shrink's time). For a block of fewer than 2^16 pixels, `x` (at most 256·n)
/// is below 2^24, so `x · fl(1/n)` is within 2^-28 of `x / n`, while a quotient that is
/// not whole is at least 1/n > 2^-16 from a whole number: adding 2^-20 and truncating
/// gives exactly `x / n`. Bigger blocks (a shrink to a few pixels) divide.
#[derive(Clone, Copy)]
struct Divide {
    n: u32,
    inv: f64,
}

impl Divide {
    fn by(n: u32) -> Self {
        Self { n, inv: 1.0 / f64::from(n) }
    }

    fn of(self, x: u32) -> u32 {
        if self.n < 1 << 16 {
            (f64::from(x) * self.inv + 1.0 / f64::from(1u32 << 20)) as u32
        } else {
            x / self.n
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic noise, so every block's average differs.
    fn noise(w: u32, h: u32, channels: u8, seed: u32) -> DynamicImage {
        let mut state = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 24) as u8
        };
        let raw: Vec<u8> = (0..w as usize * h as usize * channels as usize).map(|_| next()).collect();
        match channels {
            1 => DynamicImage::ImageLuma8(ImageBuffer::from_raw(w, h, raw).unwrap()),
            2 => DynamicImage::ImageLumaA8(ImageBuffer::from_raw(w, h, raw).unwrap()),
            3 => DynamicImage::ImageRgb8(ImageBuffer::from_raw(w, h, raw).unwrap()),
            _ => DynamicImage::ImageRgba8(ImageBuffer::from_raw(w, h, raw).unwrap()),
        }
    }

    /// Byte for byte what `image`'s own `thumbnail` gives, over awkward ratios (prime sizes,
    /// near-1 ratios, a 1 px edge), every 8-bit layout, and sizes that do not shrink.
    #[test]
    fn matches_image_thumbnail_byte_for_byte() {
        let sizes = [(1600, 1067), (1067, 1600), (997, 613), (701, 467), (700, 467), (3, 1), (1, 3), (513, 512), (300, 200)];
        // 700: exactly fits one size and grows another, which falls back.
        let maxes = [700, 512, 511, 1];
        for (i, &(w, h)) in sizes.iter().enumerate() {
            for channels in 1..=4u8 {
                let img = noise(w, h, channels, i as u32 * 31 + u32::from(channels));
                for &max in &maxes {
                    let ours = thumbnail(&img, max);
                    let theirs = img.thumbnail(max, max);
                    assert_eq!(
                        (ours.width(), ours.height()),
                        (theirs.width(), theirs.height()),
                        "{w}x{h} c{channels} max {max}: size"
                    );
                    assert!(ours.as_bytes() == theirs.as_bytes(), "{w}x{h} c{channels} max {max}: pixels differ");
                }
            }
        }
    }

    /// The divide-free quotient is exact over every numerator an average can have for blocks
    /// of up to 256 pixels, and around every multiple (where truncation could slip) for every
    /// block size up to 2^12 and at the edges of the range it is used for.
    #[test]
    fn the_reciprocal_quotient_is_exact() {
        let ns = (1..=1u32 << 12).chain([(1 << 16) - 2, (1 << 16) - 1, 1 << 16, (1 << 16) + 1, 1 << 20]);
        for n in ns {
            let div = Divide::by(n);
            let top = 256 * n;
            let xs: Box<dyn Iterator<Item = u32>> = if n <= 256 {
                Box::new(0..top)
            } else {
                Box::new((0..=256).flat_map(move |k| [k * n, (k * n).saturating_sub(1), k * n + 1, k * n + n / 2]))
            };
            for x in xs {
                assert_eq!(div.of(x), x / n, "{x} / {n}");
            }
        }
    }

    /// A 16-bit image is `image`'s own path, unchanged.
    #[test]
    fn other_layouts_fall_back_to_image() {
        let img = DynamicImage::ImageRgb16(ImageBuffer::from_pixel(40, 30, image::Rgb([1000u16, 2000, 3000])));
        assert!(thumbnail(&img, 16).as_bytes() == img.thumbnail(16, 16).as_bytes());
    }

    /// The point of it: a 24 MP shrink to the preview size is several times faster than
    /// `image`'s (see `thumbnails::bench`; the guard asks for 2×, room for a loaded machine).
    /// Release builds only: a debug build measures the optimiser's absence (run it with
    /// `cargo test --release -p chairphoto-core thumbnails::downscale`).
    #[test]
    fn shrinking_a_24_mp_decode_is_much_faster_than_images() {
        if cfg!(debug_assertions) {
            println!("SKIPPED: shrinking_a_24_mp_decode_is_much_faster_than_images — a debug build");
            return;
        }
        let img = noise(6000, 4000, 3, 7);
        let best = |f: &dyn Fn() -> DynamicImage| {
            (0..3)
                .map(|_| {
                    let t = std::time::Instant::now();
                    std::hint::black_box(f());
                    t.elapsed()
                })
                .min()
                .unwrap()
        };
        let ours = best(&|| thumbnail(&img, 2048));
        let theirs = best(&|| img.thumbnail(2048, 2048));
        assert!(ours * 2 < theirs, "ours {ours:?} vs image's {theirs:?}");
    }
}
