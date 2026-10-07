//! Full RAW decode via the system **LibRaw** (FFI), behind the `raw` Cargo feature.
//!
//! Used for full-resolution edited export (docs/editing.md, Phase 3): the editor's
//! live preview and loupe render the embedded JPEG proxy, but a "Show off" export of an
//! edited version needs the original at native resolution so the crop is exact. This
//! decodes a RAW to an 8-bit sRGB [`DynamicImage`]; the edit record (crop + tone) is
//! then applied by `plugins::edit::render_image`.
//!
//! **Read-only / non-destructive:** LibRaw only ever opens the original for reading
//! (the binding invariant). The bindings are generated at build time from the installed
//! `libraw.h` (see `build.rs`) so the struct ABI matches the linked library exactly.

#[allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/libraw_bindings.rs"));
}

use image::{DynamicImage, RgbImage};
use std::ffi::{CStr, CString};
use std::path::Path;

// ── Crash protection ──────────────────────────────────────────────────────────────────
// LibRaw is C: a malformed or unusual file can segfault inside it, which kills the whole
// process and cannot be caught. Every call into it runs under a crash marker
// (`crate::crash_marker`); a file that has taken the process down twice is skipped, and
// the caller falls back to the embedded preview. The subject key carries the decoder
// version and the file's size and mtime, so a decoder upgrade or a replaced file gets a
// fresh chance without any user action. Probe and decode are separate kinds: a file whose
// unpack crashes still opens, and a probe surviving it must not reset the decode's count.

/// Crash-marker kind for the identify step (`libraw_open_file` only).
pub const KIND_PROBE: &str = "libraw-probe";
/// Crash-marker kind for a full decode (unpack + process), either bit depth.
pub const KIND_DECODE: &str = "libraw-decode";

/// The crash-marker subject for `path`: decoder version, path, size and mtime.
pub fn crash_subject(path: &Path) -> String {
    let (len, mtime) = std::fs::metadata(path)
        .map(|m| {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            (m.len(), mtime)
        })
        .unwrap_or((0, 0));
    format!("{}|{}|{len}|{mtime}", decoder_version(), path.display())
}

/// Why `path` is skipped, if it crashed the decoder too often under `kind`.
fn crash_block_reason(kind: &str, subject: &str) -> Option<String> {
    crate::crash_marker::blocked(kind, subject).map(|s| crash_reason_text(s.strikes))
}

fn crash_reason_text(strikes: u32) -> String {
    format!(
        "the RAW decoder crashed the app on this file {strikes} times, so it is skipped and \
         the camera preview is used instead; replacing the file or updating the app retries it"
    )
}

/// What the decoder makes of a file, from `libraw_open_file` alone — the identify step,
/// tens of milliseconds, no unpack. `Unsupported` is the honest state the Develop badge
/// shows for a camera newer than the pinned snapshot (docs/plans/raw-foundation).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(tag = "support", rename_all = "lowercase")]
pub enum RawSupport {
    Supported(RawIdentity),
    Unsupported {
        /// The camera as far as the file's own metadata names it, when LibRaw got that far.
        camera: Option<String>,
        reason: String,
    },
}

/// The identity LibRaw reports for a supported file.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct RawIdentity {
    pub make: String,
    pub model: String,
    /// Sensor-oriented dimensions of the *picture* — the camera's visible rectangle
    /// (`raw_inset_crops[0]`, the same trim the export applies), not the sensor's full
    /// readout with its masked borders. For a Sony ILCE-7RM6 that is 10016×6672 (67 MP)
    /// inside a ~73 MP readout.
    pub width: u32,
    pub height: u32,
}

/// Open and identify `path` without unpacking pixels. Never panics on a bad file: LibRaw's
/// error text becomes the `reason`.
pub fn probe(path: &Path) -> RawSupport {
    let Ok(c_path) = CString::new(path.to_string_lossy().as_bytes()) else {
        return RawSupport::Unsupported { camera: None, reason: "path contains a NUL byte".into() };
    };
    let subject = crash_subject(path);
    // A file the full decode crashes on is reported here too, so Develop never starts a
    // session it knows will end the process.
    if let Some(reason) =
        crash_block_reason(KIND_PROBE, &subject).or_else(|| crash_block_reason(KIND_DECODE, &subject))
    {
        return RawSupport::Unsupported { camera: None, reason };
    }
    let _crash_guard = crate::crash_marker::enter(KIND_PROBE, &subject, &path.to_string_lossy());
    // SAFETY: the handle is created, used and closed in this block; every pointer read is
    // on the live handle; strings are NUL-terminated fixed arrays on the LibRaw struct.
    unsafe {
        let lr = ffi::libraw_init(0);
        if lr.is_null() {
            return RawSupport::Unsupported { camera: None, reason: "libraw_init returned null".into() };
        }
        let rc = ffi::libraw_open_file(lr, c_path.as_ptr());
        let result = if rc == 0 {
            let (width, height) = visible_size(&(*lr).sizes);
            RawSupport::Supported(RawIdentity {
                make: c_field(&(*lr).idata.make),
                model: c_field(&(*lr).idata.model),
                width,
                height,
            })
        } else {
            let reason = CStr::from_ptr(ffi::libraw_strerror(rc)).to_string_lossy().into_owned();
            let model = c_field(&(*lr).idata.model);
            RawSupport::Unsupported {
                camera: (!model.is_empty()).then_some(model),
                reason,
            }
        };
        ffi::libraw_recycle(lr);
        ffi::libraw_close(lr);
        result
    }
}

/// The compiled-in LibRaw's version string (e.g. `0.22.0-Devel202609`) — part of every
/// decode-cache key, so a decoder bump can never serve an older snapshot's pixels.
pub fn decoder_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| {
        // SAFETY: libraw_version returns a pointer to a static NUL-terminated string.
        unsafe { CStr::from_ptr(ffi::libraw_version()).to_string_lossy().into_owned() }
    })
}

/// The picture's size in sensor orientation: the camera's default visible rectangle when
/// the file declares one (`crop_to_inset` applies the same rule to the decode), else
/// LibRaw's own image size. Available from `libraw_open_file` alone.
fn visible_size(sizes: &ffi::libraw_image_sizes_t) -> (u32, u32) {
    let inset = sizes.raw_inset_crops[0];
    let (cw, ch) = (inset.cwidth as u32, inset.cheight as u32);
    if cw == 0 || ch == 0 || inset.cwidth == u16::MAX {
        (sizes.width as u32, sizes.height as u32)
    } else {
        (cw, ch)
    }
}

/// The engine's working-image decode: 16-bit **linear** (gamma 1.0), sRGB/Rec.709
/// primaries, as-shot white balance, no auto-brightening, highlights clipped at sensor
/// white (docs/plans/raw-foundation, decision 3), inset-cropped to the camera's visible
/// rectangle, in sensor orientation with the display orientation alongside.
#[derive(Debug)]
pub struct LinearDecode {
    pub width: u32,
    pub height: u32,
    /// Interleaved RGB, row-major, 0..=65535 with 65535 = sensor white.
    pub rgb16: Vec<u16>,
    pub orientation: image::metadata::Orientation,
    pub cam_mul: [f32; 4],
    /// LibRaw's daylight (D65) multipliers — the white `rgb_cam` is normalized to. With
    /// `cam_mul` they place the as-shot light, which Kelvin white balance renders around.
    pub pre_mul: [f32; 4],
    pub rgb_cam: [[f32; 3]; 3],
    /// The camera's own white-balance table where the file has one (LibRaw's
    /// `WBCT_Coeffs`): (colour temperature K, R, G, B multipliers) per row. Kelvin white
    /// balance calibrates to it, which is independent of how good the decoder's matrix is.
    pub wbct: Vec<[f32; 4]>,
    /// The camera's own lens-correction tables (`lens::embedded`), read from the original
    /// alongside the decode. Their radius runs over this image — the visible rectangle,
    /// which is the picture the camera measured them on (docs/plans/lens-corrections).
    pub lens: Option<crate::lens::LensCorrection>,
}

/// Decode `path` for the working image. `abort` is polled between the expensive steps —
/// open, unpack, process, copy-out — so a superseded Develop session stops within one step.
pub fn decode_linear(path: &Path, abort: &std::sync::atomic::AtomicBool) -> Result<LinearDecode, String> {
    use std::sync::atomic::Ordering;
    let c_path = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| "path contains a NUL byte".to_string())?;
    let subject = crash_subject(path);
    if let Some(reason) = crash_block_reason(KIND_DECODE, &subject) {
        return Err(reason);
    }
    let _crash_guard = crate::crash_marker::enter(KIND_DECODE, &subject, &path.to_string_lossy());
    let check = |stage: &str| -> Result<(), String> {
        if abort.load(Ordering::Relaxed) {
            Err(format!("decode aborted before {stage}"))
        } else {
            Ok(())
        }
    };
    // SAFETY: as in `decode_to_image` — pointers checked before use, the handle recycled
    // and closed on every exit path.
    let decoded = unsafe {
        let lr = ffi::libraw_init(0);
        if lr.is_null() {
            return Err("libraw_init returned null".into());
        }
        let result = (|| {
            check("open")?;
            let rc = ffi::libraw_open_file(lr, c_path.as_ptr());
            if rc != 0 {
                return Err(format!("libraw_open_file failed ({})", CStr::from_ptr(ffi::libraw_strerror(rc)).to_string_lossy()));
            }
            check("unpack")?;
            let rc = ffi::libraw_unpack(lr);
            if rc != 0 {
                return Err(format!("libraw_unpack failed ({rc})"));
            }
            // The daylight multipliers, read before processing: dcraw's scale step writes
            // the multipliers it used (here the camera's) back into `pre_mul`.
            let pre_mul = (*lr).color.pre_mul;
            let wbct: Vec<[f32; 4]> = (*lr)
                .color
                .WBCT_Coeffs
                .iter()
                .filter(|r| r[0] > 0.0 && r[1] > 0.0 && r[2] > 0.0 && r[3] > 0.0)
                .map(|r| [r[0], r[1], r[2], r[3]])
                .collect();
            (*lr).params.use_camera_wb = 1;
            (*lr).params.output_bps = 16;
            (*lr).params.output_color = 1; // sRGB / Rec.709 primaries
            (*lr).params.gamm[0] = 1.0; // linear: no transfer curve
            (*lr).params.gamm[1] = 1.0;
            (*lr).params.no_auto_bright = 1;
            (*lr).params.highlight = 0; // clip at sensor white — decision 3
            (*lr).params.user_flip = 0; // sensor orientation; oriented by the caller
            check("process")?;
            let rc = ffi::libraw_dcraw_process(lr);
            if rc != 0 {
                return Err(format!("libraw_dcraw_process failed ({rc})"));
            }
            check("copy")?;
            let inset = (*lr).sizes.raw_inset_crops[0];
            let cam_mul = (*lr).color.cam_mul;
            let rc = &(*lr).color.rgb_cam;
            let rgb_cam = [
                [rc[0][0], rc[0][1], rc[0][2]],
                [rc[1][0], rc[1][1], rc[1][2]],
                [rc[2][0], rc[2][1], rc[2][2]],
            ];
            let mut errc: i32 = 0;
            let img = ffi::libraw_dcraw_make_mem_image(lr, &mut errc);
            if img.is_null() || errc != 0 {
                return Err(format!("libraw_dcraw_make_mem_image failed ({errc})"));
            }
            let copied = copy_processed_image16(img);
            ffi::libraw_dcraw_clear_mem(img);
            let (w, h, rgb16) = copied?;
            let (w, h, rgb16) = crop_rgb16_to_inset(w, h, rgb16, inset);
            Ok(LinearDecode {
                width: w,
                height: h,
                rgb16,
                orientation: image::metadata::Orientation::NoTransforms,
                cam_mul,
                pre_mul,
                rgb_cam,
                wbct,
                lens: None,
            })
        })();
        ffi::libraw_recycle(lr);
        ffi::libraw_close(lr);
        result
    }?;
    Ok(LinearDecode {
        orientation: crate::thumbnails::exif_orientation(path),
        lens: crate::lens::embedded::read(path),
        ..decoded
    })
}

/// Copy LibRaw's 16-bit, 3-channel processed buffer out as native-endian `u16`s.
unsafe fn copy_processed_image16(
    img: *mut ffi::libraw_processed_image_t,
) -> Result<(u32, u32, Vec<u16>), String> {
    let i = &*img;
    if i.colors != 3 || i.bits != 16 {
        return Err(format!("unexpected LibRaw output (colors={}, bits={})", i.colors, i.bits));
    }
    let (w, h) = (i.width as u32, i.height as u32);
    let len = i.data_size as usize;
    let n = w as usize * h as usize * 3;
    if len != n * 2 {
        return Err(format!("RGB16 buffer size mismatch ({len} bytes for {w}x{h})"));
    }
    // `data` is a flexible array member; LibRaw writes 16-bit samples in host order.
    let bytes = std::slice::from_raw_parts(i.data.as_ptr(), len);
    let mut out = Vec::with_capacity(n);
    for px in bytes.chunks_exact(2) {
        out.push(u16::from_ne_bytes([px[0], px[1]]));
    }
    Ok((w, h, out))
}

/// [`crop_to_inset`] for the 16-bit buffer.
fn crop_rgb16_to_inset(w: u32, h: u32, rgb16: Vec<u16>, inset: ffi::libraw_raw_inset_crop_t) -> (u32, u32, Vec<u16>) {
    let (cl, ct, cw, ch) = (inset.cleft as u32, inset.ctop as u32, inset.cwidth as u32, inset.cheight as u32);
    if cw == 0 || ch == 0 || inset.cwidth == u16::MAX || cl >= w || ct >= h {
        return (w, h, rgb16);
    }
    let cw = cw.min(w - cl);
    let ch = ch.min(h - ct);
    let mut out = Vec::with_capacity(cw as usize * ch as usize * 3);
    for y in ct..ct + ch {
        let row = y as usize * w as usize * 3;
        out.extend_from_slice(&rgb16[row + cl as usize * 3..row + (cl + cw) as usize * 3]);
    }
    (cw, ch, out)
}

/// A LibRaw fixed-size char field as a String (trimmed at the first NUL).
fn c_field(field: &[std::os::raw::c_char]) -> String {
    let bytes: Vec<u8> = field.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// Decode a RAW file to a full-resolution 8-bit sRGB image, with the camera (as-shot)
/// white balance. Errors (unsupported camera, corrupt file, …) are returned so the
/// caller can fall back to the embedded preview rather than aborting an export.
pub fn decode_to_image(path: &Path) -> Result<DynamicImage, String> {
    let c_path = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| "path contains a NUL byte".to_string())?;
    let subject = crash_subject(path);
    if let Some(reason) = crash_block_reason(KIND_DECODE, &subject) {
        return Err(reason);
    }
    let _crash_guard = crate::crash_marker::enter(KIND_DECODE, &subject, &path.to_string_lossy());

    // SAFETY: every pointer is checked before use, and the LibRaw handle is always
    // recycled + closed on every exit path (the inner closure isolates the fallible
    // work so cleanup runs even on early error).
    let mut img = unsafe {
        let lr = ffi::libraw_init(0);
        if lr.is_null() {
            return Err("libraw_init returned null".into());
        }
        let result = decode_with_handle(lr, &c_path);
        ffi::libraw_recycle(lr);
        ffi::libraw_close(lr);
        result
    }?;

    // Orient to display using the SAME EXIF orientation the thumbnail/editor preview use,
    // so the export matches what was on screen. (We can't trust LibRaw's `sizes.flip`:
    // forcing `user_flip = 0` for the sensor-space inset crop also zeroes it.)
    img.apply_orientation(crate::thumbnails::exif_orientation(path));
    Ok(img)
}

unsafe fn decode_with_handle(
    lr: *mut ffi::libraw_data_t,
    c_path: &CString,
) -> Result<DynamicImage, String> {
    let rc = ffi::libraw_open_file(lr, c_path.as_ptr());
    if rc != 0 {
        return Err(format!("libraw_open_file failed ({rc})"));
    }
    let rc = ffi::libraw_unpack(lr);
    if rc != 0 {
        return Err(format!("libraw_unpack failed ({rc})"));
    }

    // Output params: 8-bit, sRGB, camera white balance. `user_flip = 0` keeps the output
    // in sensor orientation so the inset crop below uses the camera's sensor-space
    // coordinates directly; the display orientation is applied afterwards from EXIF.
    (*lr).params.use_camera_wb = 1;
    (*lr).params.output_bps = 8;
    (*lr).params.output_color = 1; // sRGB primaries
    (*lr).params.user_flip = 0;
    // True sRGB transfer curve (power 2.4, linear-toe slope 12.92). LibRaw's default is
    // BT.709 (2.222 / 4.5), which every consumer then mis-displays as sRGB — ~0.3 EV
    // darker in the midtones. Everything downstream (encode, upload targets) assumes
    // untagged 8-bit output is sRGB, so encode it as actual sRGB.
    (*lr).params.gamm[0] = 1.0 / 2.4;
    (*lr).params.gamm[1] = 12.92;
    // Disable LibRaw's per-image auto-brightness (a histogram stretch). The camera's
    // embedded preview — the proxy the editor and its crop/tone sliders render on — is
    // NOT auto-stretched, so leaving auto-bright on would decode the export a stop or so
    // off the tone the user judged the edit against (K1: editor↔export tone match). With
    // it off, the export's tonal baseline tracks the as-shot exposure the preview shows.
    // The remaining gap — the camera's picture-style tone curve baked into the preview
    // but absent from a plain decode — is closed by the caller via
    // `export::tone_match_to_preview` (histogram matching against the embedded preview),
    // and the shared look pipeline (`plugins::edit::render_image`) then applies the same
    // EV/tone on top of a matching base.
    (*lr).params.no_auto_bright = 1;

    let rc = ffi::libraw_dcraw_process(lr);
    if rc != 0 {
        return Err(format!("libraw_dcraw_process failed ({rc})"));
    }

    // The camera's "default crop" (`raw_inset_crops[0]`): the visible image area the
    // camera JPEG / embedded preview use. LibRaw's full output includes extra masked
    // border (non-centered) that we must trim, or the export would be larger than — and
    // mis-framed against — what the editor showed.
    let inset = (*lr).sizes.raw_inset_crops[0];

    let mut errc: i32 = 0;
    let img = ffi::libraw_dcraw_make_mem_image(lr, &mut errc);
    if img.is_null() || errc != 0 {
        return Err(format!("libraw_dcraw_make_mem_image failed ({errc})"));
    }
    let decoded = copy_processed_image(img);
    ffi::libraw_dcraw_clear_mem(img);

    // Trim to the camera's visible area (still sensor-oriented; the caller applies EXIF
    // orientation last).
    Ok(crop_to_inset(decoded?, inset))
}

/// Crop to the camera's default visible rectangle. `cwidth == 0` or the `0xffff`
/// sentinel means "no inset" → return the image unchanged. The rect is clamped to the
/// image so a surprising value can never panic.
fn crop_to_inset(img: DynamicImage, inset: ffi::libraw_raw_inset_crop_t) -> DynamicImage {
    use image::GenericImageView;
    let (cl, ct, cw, ch) = (
        inset.cleft as u32,
        inset.ctop as u32,
        inset.cwidth as u32,
        inset.cheight as u32,
    );
    let (w, h) = img.dimensions();
    if cw == 0 || ch == 0 || inset.cwidth == u16::MAX || cl >= w || ct >= h {
        return img;
    }
    let cw = cw.min(w - cl);
    let ch = ch.min(h - ct);
    img.crop_imm(cl, ct, cw, ch)
}

/// Copy LibRaw's processed-image buffer into an owned [`DynamicImage`]. Only the
/// 8-bit, 3-channel bitmap form is expected (the params above force it).
unsafe fn copy_processed_image(
    img: *mut ffi::libraw_processed_image_t,
) -> Result<DynamicImage, String> {
    let i = &*img;
    if i.colors != 3 || i.bits != 8 {
        return Err(format!(
            "unexpected LibRaw output (colors={}, bits={})",
            i.colors, i.bits
        ));
    }
    let (w, h) = (i.width as u32, i.height as u32);
    let len = i.data_size as usize;
    // `data` is a C flexible array member (`unsigned char data[1]`); the real length is
    // `data_size`. Copy it out before LibRaw frees the buffer.
    let bytes = std::slice::from_raw_parts(i.data.as_ptr(), len).to_vec();
    let rgb = RgbImage::from_raw(w, h, bytes)
        .ok_or_else(|| format!("RGB buffer size mismatch ({len} bytes for {w}x{h})"))?;
    Ok(DynamicImage::ImageRgb8(rgb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_version_names_the_vendored_snapshot() {
        let v = decoder_version();
        assert!(v.starts_with("0."), "unexpected LibRaw version string {v:?}");
    }

    #[test]
    fn probe_reports_unsupported_without_panicking() {
        let dir = std::env::temp_dir().join(format!("chairphoto-raw-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let not_raw = dir.join("plain.txt");
        std::fs::write(&not_raw, b"this is not a raw file at all").unwrap();
        match probe(&not_raw) {
            RawSupport::Unsupported { reason, .. } => assert!(!reason.is_empty()),
            other => panic!("a text file must not probe as supported: {other:?}"),
        }
        let missing = dir.join("missing.ARW");
        assert!(matches!(probe(&missing), RawSupport::Unsupported { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_crash_subject_changes_with_the_file_and_names_the_decoder() {
        let dir = std::env::temp_dir().join(format!("chairphoto-raw-subject-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.ARW");
        std::fs::write(&f, b"one").unwrap();
        let first = crash_subject(&f);
        assert!(first.starts_with(decoder_version()), "{first}");
        assert!(first.contains("a.ARW"));
        assert_eq!(first, crash_subject(&f), "stable while the file is unchanged");
        std::fs::write(&f, b"replaced, longer").unwrap();
        assert_ne!(first, crash_subject(&f), "a replaced file gets a fresh chance");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_crash_reason_says_what_happens_instead() {
        let r = crash_reason_text(2);
        assert!(r.contains("2 times") && r.contains("camera preview"), "{r}");
    }

    #[test]
    fn decode_linear_honours_abort() {
        let dir = std::env::temp_dir().join(format!("chairphoto-raw-abort-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.ARW");
        std::fs::write(&f, b"not a raw").unwrap();
        let tripped = std::sync::atomic::AtomicBool::new(true);
        let err = decode_linear(&f, &tripped).unwrap_err();
        assert!(err.contains("aborted"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Runs only with a real RAW at `CHAIRPHOTO_RAW_FIXTURE`: the linear decode is 16-bit,
    /// the picture size, never brighter than sensor white, and orientation-tagged.
    #[test]
    fn decode_linear_is_16bit_linear_and_never_brightens() {
        let Ok(fixture) = std::env::var("CHAIRPHOTO_RAW_FIXTURE") else {
            println!("SKIPPED: decode_linear_is_16bit_linear_and_never_brightens — set CHAIRPHOTO_RAW_FIXTURE");
            return;
        };
        let t = std::time::Instant::now();
        let d = decode_linear(Path::new(&fixture), &std::sync::atomic::AtomicBool::new(false)).unwrap();
        println!("decode_linear: {}x{} in {:.2?}, cam_mul {:?}", d.width, d.height, t.elapsed(), d.cam_mul);
        assert_eq!(d.rgb16.len(), d.width as usize * d.height as usize * 3);
        // Linear + no auto-bright: a mid-grey scene sits well below the top — the mean of
        // the whole frame must not be pushed up toward white by a histogram stretch.
        let mean = d.rgb16.iter().map(|&v| v as u64).sum::<u64>() / d.rgb16.len() as u64;
        println!("mean sample {mean} / 65535");
        assert!(mean < 40000, "a linear decode without auto-bright is not this bright");
        if let Ok(expect) = std::env::var("CHAIRPHOTO_RAW_FIXTURE_SIZE") {
            assert_eq!(format!("{}x{}", d.width, d.height), expect);
        }
    }

    /// Runs only with a real RAW at `CHAIRPHOTO_RAW_FIXTURE`; announces itself otherwise.
    #[test]
    fn probe_identifies_a_real_raw_fixture() {
        let Ok(fixture) = std::env::var("CHAIRPHOTO_RAW_FIXTURE") else {
            println!("SKIPPED: probe_identifies_a_real_raw_fixture — set CHAIRPHOTO_RAW_FIXTURE to a RAW file");
            return;
        };
        match probe(Path::new(&fixture)) {
            RawSupport::Supported(id) => {
                println!("probe: {} {} {}x{} ({:.2} MP)", id.make, id.model, id.width, id.height,
                    id.width as f64 * id.height as f64 / 1e6);
                assert!(!id.model.is_empty());
                assert!(id.width > 0 && id.height > 0);
                // The picture, not the readout: for a known fixture, pin the exact size.
                if let Ok(expect) = std::env::var("CHAIRPHOTO_RAW_FIXTURE_SIZE") {
                    assert_eq!(format!("{}x{}", id.width, id.height), expect, "visible size");
                }
            }
            RawSupport::Unsupported { reason, .. } => panic!("fixture not supported: {reason}"),
        }
    }
}
