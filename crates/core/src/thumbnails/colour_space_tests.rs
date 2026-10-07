//! #243: the in-process colour-space read agrees with exiftool.

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
    let plain = encode_rotated_jpeg(&DynamicImage::ImageRgb8(RgbImage::from_pixel(16, 16, image::Rgb([90, 120, 200])))).unwrap();
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
