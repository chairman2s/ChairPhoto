//! The frames face boxes live in (#136): the display frame ChairPhoto stores, the stored
//! frame MWG measures, and which of them a `Regions`' `AppliedToDimensions` declares (#145).

use xmltree::Element;
use crate::xmp::ns::NS_STDIM;
use super::FaceRegion;
use super::mwg::{struct_body, struct_field};

/// What the catalog knows of a photo's frames, for converting its face boxes (#136).
///
/// Face boxes live in the **display** frame: the image as its EXIF Orientation turns it.
/// MWG regions and their `AppliedToDimensions` live in the **stored** frame: the pixels as the
/// file holds them, before the Orientation is applied (MWG 2.0 § 5.9). The non-destructive
/// user rotation plays no part in either; it is the catalog's alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegionFrame {
    /// The original's EXIF Orientation code (1-8), `None` when unknown. Unknown is never
    /// guessed: the boxes are written as they are and an existing `AppliedToDimensions` is
    /// left alone.
    pub orientation: Option<u8>,
    /// The stored image's pixel size (`photos.width`/`height`, EXIF `ExifImageWidth`/`Height`),
    /// `None` when unknown — then no `AppliedToDimensions` is written.
    pub stored_size: Option<(u32, u32)>,
}

/// Where a normalized point `(u, v)` of the stored image lands in the image displayed under
/// EXIF Orientation `o` — the turn the preview the faces are detected on went through.
fn stored_to_display_point(o: u8, (u, v): (f32, f32)) -> (f32, f32) {
    match o {
        2 => (1.0 - u, v),       // mirror horizontal
        3 => (1.0 - u, 1.0 - v), // rotate 180
        4 => (u, 1.0 - v),       // mirror vertical
        5 => (v, u),             // mirror horizontal and rotate 270 CW (transpose)
        6 => (1.0 - v, u),       // rotate 90 CW
        7 => (1.0 - v, 1.0 - u), // mirror horizontal and rotate 90 CW (transverse)
        8 => (v, 1.0 - u),       // rotate 270 CW
        _ => (u, v),
    }
}

/// The inverse of [`stored_to_display_point`]: 6 and 8 undo each other, every other
/// orientation undoes itself.
fn display_to_stored_point(o: u8, p: (f32, f32)) -> (f32, f32) {
    let inverse = match o {
        6 => 8,
        8 => 6,
        o => o,
    };
    stored_to_display_point(inverse, p)
}

/// A top-left bbox mapped point by point: both corners, then the box they span.
fn map_bbox(
    bbox: (f32, f32, f32, f32),
    point: impl Fn((f32, f32)) -> (f32, f32),
) -> (f32, f32, f32, f32) {
    let (x, y, w, h) = bbox;
    let (ax, ay) = point((x, y));
    let (bx, by) = point((x + w, y + h));
    (ax.min(bx), ay.min(by), (ax - bx).abs(), (ay - by).abs())
}

/// A display-frame box in the stored frame of a photo with EXIF Orientation `o`.
pub(super) fn display_to_stored(o: u8, bbox: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
    map_bbox(bbox, |p| display_to_stored_point(o, p))
}

/// A stored-frame box in the display frame of a photo with EXIF Orientation `o`.
pub(super) fn stored_to_display(o: u8, bbox: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
    map_bbox(bbox, |p| stored_to_display_point(o, p))
}

/// The frame ChairPhoto's boxes are written in, and read back from, for one `mwg-rs:Regions`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RegionTarget {
    /// The boxes as they are: the orientation is unknown, or the sidecar's own
    /// `AppliedToDimensions` declares the display frame.
    AsIs,
    /// The stored frame of a photo with this EXIF Orientation (MWG 2.0 § 5.9).
    Stored(u8),
}

impl RegionTarget {
    /// A display-frame face as it is written.
    pub(super) fn write(self, r: &FaceRegion) -> FaceRegion {
        match self {
            Self::AsIs => r.clone(),
            Self::Stored(o) => FaceRegion { bbox: display_to_stored(o, r.bbox), ..r.clone() },
        }
    }

    /// A region as read, in the display frame.
    pub(super) fn read(self, bbox: (f32, f32, f32, f32)) -> (f32, f32, f32, f32) {
        match self {
            Self::AsIs => bbox,
            Self::Stored(o) => stored_to_display(o, bbox),
        }
    }
}

/// The size an `AppliedToDimensions` declares, or why it declares none ChairPhoto can use.
pub(super) fn applied_dimensions(dims: &Element) -> Result<(f64, f64), String> {
    let body = struct_body(dims).ok_or("its AppliedToDimensions is not a struct")?;
    let side = |f| {
        struct_field(body, NS_STDIM, f)
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
    };
    match (side("w"), side("h")) {
        (Some(w), Some(h)) => Ok((w, h)),
        _ => Err("its AppliedToDimensions declares no usable width and height".into()),
    }
}

/// Which frame ChairPhoto's boxes go into a `Regions` whose `AppliedToDimensions` is
/// `declared` (`None`: it has none), never changing that declaration (#145):
///
/// - Unknown orientation: as they are (#136, never guessed).
/// - No declared size, or ChairPhoto's own old `1x1` stand-in, which declares no frame: the
///   stored frame, as MWG requires.
/// - A size with the stored image's aspect (the same image, perhaps resized): the stored frame.
/// - A size with the aspect swapped, on a photo whose orientation turns it a quarter (5-8):
///   the display frame that size describes — the regions already there are measured in it.
/// - Anything else — another aspect, a swapped one the orientation does not explain, or a
///   quarter-turned photo whose stored size is unknown, so which frame is declared cannot be
///   told — is `Err`: the write is refused rather than mixing frames.
pub(super) fn region_target(
    frame: RegionFrame,
    declared: Option<Result<(f64, f64), String>>,
) -> Result<RegionTarget, String> {
    let Some(o) = frame.orientation else {
        return Ok(RegionTarget::AsIs);
    };
    let stored = RegionTarget::Stored(o);
    let quarter = (5..=8).contains(&o);
    let (dw, dh) = match declared {
        None => return Ok(stored),
        Some(Ok((w, h))) if w == 1.0 && h == 1.0 => return Ok(stored),
        Some(Ok(size)) => size,
        Some(Err(why)) => return Err(why),
    };
    let Some((sw, sh)) = frame.stored_size else {
        return if quarter {
            Err(format!(
                "its AppliedToDimensions {dw}x{dh} may be either frame of a photo turned a quarter \
                 (EXIF Orientation {o}), and the photo's own size is not known"
            ))
        } else {
            Ok(stored)
        };
    };
    let (sw, sh) = (f64::from(sw), f64::from(sh));
    let same_aspect = |w: f64, h: f64| ((dw / dh) / (w / h) - 1.0).abs() <= ASPECT_TOLERANCE;
    if same_aspect(sw, sh) {
        Ok(stored)
    } else if quarter && same_aspect(sh, sw) {
        Ok(RegionTarget::AsIs)
    } else {
        Err(format!(
            "its AppliedToDimensions {dw}x{dh} is not a frame of this {sw}x{sh} image \
             (EXIF Orientation {o})"
        ))
    }
}

/// How far two aspect ratios may differ and still be one image's: a resize rounds each side
/// to a whole pixel, and a RAW's recorded size can differ from a converter's by a few pixels.
const ASPECT_TOLERANCE: f64 = 0.01;
