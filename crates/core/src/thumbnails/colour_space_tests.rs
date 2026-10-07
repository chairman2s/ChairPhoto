//! #243: the in-process colour-space read agrees with exiftool, and the `jpeg-encoder` tier
//! encode matches `image`'s encoder to within lossy-encode noise.

use super::*;
use exif::experimental::Writer;
use exif::{Field, In, Tag, Value};

/// A tiny JPEG whose APP1 carries `fields` (SOI + APP1 + the encoder's own stream).
fn jpeg_with_exif(dir: &Path, name: &str, fields: &[Field]) -> PathBuf {
    let mut writer = Writer::new();
    for f in fields {
        writer.push_field(f);
    }
    let mut tiff = Cursor::new(Vec::new());
    writer.write(&mut tiff, false).unwrap();
    let plain = encode_jpeg(&DynamicImage::ImageRgb8(RgbImage::from_pixel(16, 16, image::Rgb([90, 120, 200]))), 90).unwrap();
    let mut seg = b"Exif\0\0".to_vec();
    seg.extend(tiff.into_inner());
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend(((seg.len() + 2) as u16).to_be_bytes());
    out.extend(seg);
    out.extend(&plain[2..]);
    let p = dir.join(name);
    std::fs::write(&p, out).unwrap();
    p
}

fn colour_space(v: u16) -> Field {
    Field { tag: Tag::ColorSpace, ifd_num: In::PRIMARY, value: Value::Short(vec![v]) }
}

fn interop(v: &str) -> Field {
    Field { tag: Tag::InteroperabilityIndex, ifd_num: In::PRIMARY, value: Value::Ascii(vec![v.as_bytes().to_vec()]) }
}

// --- in-process colour-space detection ---------------------------------------------------

#[test]
fn in_process_read_recognises_adobe_rgb_tags() {
    let tmp = tests::TestTmpDir::new("cs-tags");
    let cases = [
        ("srgb.jpg", vec![colour_space(1)], false),
        ("adobe-cs.jpg", vec![colour_space(2)], true),
        ("sony.jpg", vec![colour_space(0xFFFF), interop("R03")], true),
        ("r98.jpg", vec![colour_space(1), interop("R98")], false),
        ("none.jpg", vec![Field { tag: Tag::Orientation, ifd_num: In::PRIMARY, value: Value::Short(vec![1]) }], false),
    ];
    for (name, fields, want) in cases {
        let p = jpeg_with_exif(tmp.path(), name, &fields);
        assert_eq!(adobe_rgb_in_process(&p), Some(want), "{name}");
    }
}

#[test]
fn file_without_exif_container_has_no_in_process_answer() {
    let tmp = tests::TestTmpDir::new("cs-none");
    let p = tests::write_test_jpeg(tmp.path(), "plain.jpg", 8, 8);
    assert_eq!(adobe_rgb_in_process(&p), None, "no EXIF: the caller falls back to exiftool");
}

/// Real files: the in-process answer equals exiftool's on the Sony ARWs the machine has.
#[test]
fn in_process_read_matches_exiftool_on_real_raws() {
    let exiftool_ok = Command::new("exiftool").arg("-ver").output().map(|o| o.status.success()).unwrap_or(false);
    let dir = std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Pictures/Raw")).unwrap_or_default();
    let raws: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("arw")))
        .take(8)
        .collect();
    if !exiftool_ok || raws.is_empty() {
        eprintln!("SKIPPED: in_process_read_matches_exiftool_on_real_raws — needs exiftool and ARWs under ~/Pictures/Raw");
        return;
    }
    for p in raws {
        assert_eq!(adobe_rgb_in_process(&p), Some(detect_adobe_rgb_exiftool(&p)), "{}", p.display());
    }
}

// --- JPEG tier encoder -------------------------------------------------------------------

fn psnr(a: &RgbImage, b: &RgbImage) -> f64 {
    assert_eq!(a.dimensions(), b.dimensions());
    let se: f64 = a.as_raw().iter().zip(b.as_raw()).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum();
    let mse = se / a.as_raw().len() as f64;
    if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

#[test]
fn tier_encode_matches_image_encoder_quality() {
    let img = DynamicImage::ImageRgb8(RgbImage::from_fn(640, 427, |x, y| {
        image::Rgb([(x / 3) as u8, (y / 2) as u8, ((x * y) / 97) as u8])
    }));
    for quality in [80u8, 85, 92] {
        let ours = encode_jpeg(&img, quality).unwrap();
        let mut theirs = Cursor::new(Vec::new());
        img.write_with_encoder(JpegEncoder::new_with_quality(&mut theirs, quality)).unwrap();
        let ours_px = image::load_from_memory(&ours).unwrap().to_rgb8();
        let theirs_px = image::load_from_memory(&theirs.into_inner()).unwrap().to_rgb8();
        let (a, b) = (psnr(&img.to_rgb8(), &ours_px), psnr(&img.to_rgb8(), &theirs_px));
        assert!(a > 30.0 && a > b - 1.5, "q{quality}: ours {a:.1} dB, image's {b:.1} dB");
    }
}

#[test]
fn tier_encode_handles_grey_and_alpha() {
    let grey = DynamicImage::ImageLuma8(image::GrayImage::from_pixel(9, 7, image::Luma([77])));
    let g = image::load_from_memory(&encode_jpeg(&grey, 85).unwrap()).unwrap();
    assert_eq!((g.width(), g.height()), (9, 7));
    let rgba = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(9, 7, image::Rgba([10, 200, 30, 5])));
    let a = image::load_from_memory(&encode_jpeg(&rgba, 85).unwrap()).unwrap().to_rgb8();
    assert!((a.get_pixel(4, 3)[1] as i32 - 200).abs() < 8);
    let wide = DynamicImage::ImageRgb8(RgbImage::new(70_000, 1));
    assert!(encode_jpeg(&wide, 85).is_err(), "beyond JPEG's 65535 px limit is an error, not a wrap");
}
