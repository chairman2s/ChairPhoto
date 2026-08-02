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
use std::ffi::CString;
use std::path::Path;

/// Decode a RAW file to a full-resolution 8-bit sRGB image, with the camera (as-shot)
/// white balance. Errors (unsupported camera, corrupt file, …) are returned so the
/// caller can fall back to the embedded preview rather than aborting an export.
pub fn decode_to_image(path: &Path) -> Result<DynamicImage, String> {
    let c_path = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| "path contains a NUL byte".to_string())?;

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
