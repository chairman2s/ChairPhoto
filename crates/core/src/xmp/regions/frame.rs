//! The frames face boxes live in (#136): the display frame ChairPhoto stores, the stored
//! frame MWG measures, and which of them a `Regions`' `AppliedToDimensions` declares (#145).

use xmltree::Element;
use crate::xmp::ns::NS_STDIM;
use super::FaceRegion;
use super::mwg::{struct_body, struct_field};

/// What is known of a photo's frames, for converting its face boxes (#136).
///
/// Face boxes live in the **display** frame: the image as its preview shows it — turned by its
/// EXIF Orientation, or for a HEIF by its container's own transform (#154). MWG regions and
/// their `AppliedToDimensions` live in the **stored** frame: the pixels as the file holds them,
/// before the Orientation is applied (the MWG guidelines' rule for regions; see
/// `docs/face-tagging.md` on the citation). The non-destructive user rotation plays no part in
/// either; it is the catalog's alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegionFrame {
    /// The turn from the stored frame to the display frame, as an EXIF Orientation code (1-8):
    /// the original's EXIF Orientation, or a HEIF container's `irot`/`imir`
    /// ([`with_container`](Self::with_container)). `None` when unknown, which is never
    /// guessed: the boxes are written as they are and an existing `AppliedToDimensions` is
    /// left alone.
    pub orientation: Option<u8>,
    /// The stored image's pixel size (`photos.width`/`height`, EXIF `ExifImageWidth`/`Height`),
    /// `None` when unknown — then no `AppliedToDimensions` is written.
    pub stored_size: Option<(u32, u32)>,
    /// The pixel size of the decoded preview the faces are found and drawn on: the display
    /// frame, measured (#154). `None` when no preview is at hand. When it and the stored size
    /// are known, the preview must have the stored size's aspect turned by the orientation (no
    /// turn when the orientation is unknown); otherwise the recorded size or orientation is not
    /// this preview's, and no region is written or read ([`region_target`]).
    pub display_size: Option<(u32, u32)>,
    /// Why the orientation cannot be trusted though the file has one (#154). A doubted frame
    /// writes and reads no region.
    pub doubt: Option<FrameDoubt>,
}

/// Why a photo's turn from the stored frame to the display frame cannot be told (#154).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDoubt {
    /// A HEIF whose container transform — `irot`/`imir`, which its preview is turned by —
    /// could not be read.
    ContainerUnreadable,
    /// A HEIF whose container turns it as EXIF Orientation `container` would, while its EXIF
    /// Orientation says `exif`. The preview follows the container; a tool that goes by EXIF
    /// follows the other, so which frame is the stored one cannot be told.
    ContainerDisagrees { container: u8, exif: u8 },
}

impl std::fmt::Display for FrameDoubt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ContainerUnreadable => {
                f.write_str("the photo's HEIF container transform (irot/imir) could not be read")
            }
            Self::ContainerDisagrees { container, exif } => write!(
                f,
                "the photo's HEIF container turns it as EXIF Orientation {container} would, but \
                 its EXIF Orientation is {exif}, so which frame is the stored one cannot be told"
            ),
        }
    }
}

impl RegionFrame {
    /// This frame with a HEIF's own container transform folded in (#154). `container` is its
    /// `irot`/`imir` as an EXIF Orientation code (1 when the container turns nothing), `None`
    /// when the container could not be read.
    ///
    /// A HEIF's preview is turned by its container alone: libheif applies `irot`/`imir`, and
    /// ImageMagick then leaves the EXIF Orientation unapplied (observed with ImageMagick
    /// 7.1.2-31 and libheif 1.23.4, `docs/face-tagging.md`). So the container's turn is the
    /// display frame's. An EXIF Orientation that says the same, or none, agrees; any other is
    /// a [`FrameDoubt`], and so is an unreadable container.
    pub fn with_container(self, container: Option<u8>) -> Self {
        let Some(c) = container.filter(|c| (1..=8).contains(c)) else {
            return Self { doubt: Some(FrameDoubt::ContainerUnreadable), ..self };
        };
        match self.orientation {
            Some(exif) if exif != c => {
                Self { doubt: Some(FrameDoubt::ContainerDisagrees { container: c, exif }), ..self }
            }
            _ => Self { orientation: Some(c), ..self },
        }
    }
}

/// The orientation that turns an image as `first` does and then as `second` does — each, and
/// the result, an EXIF Orientation code (#154: a HEIF item's `irot` and `imir` in the order
/// the item lists them).
pub(crate) fn compose_orientations(first: u8, second: u8) -> u8 {
    // The eight orientations send this point to eight different places, and its coordinates
    // are exact in binary, so `1 - u` loses nothing.
    let probe = (0.125, 0.25);
    let both = stored_to_display_point(second, stored_to_display_point(first, probe));
    (1..=8)
        .find(|&o| stored_to_display_point(o, probe) == both)
        .expect("the eight orientations are closed under composition")
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
    /// The stored frame of a photo with this EXIF Orientation, as MWG measures regions.
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
///
/// Before any of that (#154), a frame that is doubted ([`FrameDoubt`]), or whose preview is
/// not the stored size turned by the orientation ([`preview_disagrees`]), is `Err` whatever
/// the sidecar declares: the boxes' own frame is not known, so no frame can be put to them.
pub(super) fn region_target(
    frame: RegionFrame,
    declared: Option<Result<(f64, f64), String>>,
) -> Result<RegionTarget, String> {
    if let Some(doubt) = frame.doubt {
        return Err(doubt.to_string());
    }
    if let Some(why) = preview_disagrees(frame) {
        return Err(why);
    }
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

/// Why the preview the faces were found on is not the recorded stored size turned by the
/// orientation (#154, review L2), or `None` when it is — or when either size is unknown.
///
/// The stored size comes from metadata (`ExifImageWidth`/`Height`), and a writer that
/// records it in the display frame defeats [`region_target`]'s swapped-dimensions rule: the
/// boxes would land in the wrong frame with no error. The preview is the display frame as
/// the faces were measured on it, so the two must agree: the stored aspect, swapped for a
/// quarter turn (5-8). With no known orientation the boxes go as they are against the
/// stored size, so the preview must then have the stored aspect itself; a preview turned a
/// quarter from it means a turn nothing recorded.
fn preview_disagrees(frame: RegionFrame) -> Option<String> {
    let ((sw, sh), (pw, ph)) = (frame.stored_size?, frame.display_size?);
    let quarter = frame.orientation.is_some_and(|o| (5..=8).contains(&o));
    let (ew, eh) = if quarter { (sh, sw) } else { (sw, sh) };
    let ratio = (f64::from(pw) / f64::from(ph)) / (f64::from(ew) / f64::from(eh));
    if (ratio - 1.0).abs() <= PREVIEW_ASPECT_TOLERANCE {
        return None;
    }
    let turn = match frame.orientation {
        Some(o) => format!("EXIF Orientation {o}"),
        None => "no known orientation".to_string(),
    };
    Some(format!(
        "the preview the faces were found on is {pw}x{ph}, not the recorded {sw}x{sh} image \
         under {turn}, so the faces' frame is not known"
    ))
}

/// How far the preview's aspect may stray from the stored size's and still be that image's:
/// wider than [`ASPECT_TOLERANCE`], because a RAW's preview is its embedded JPEG, which a
/// camera may size a few pixels off the sensor's aspect. (On 371 cached previews of Sony
/// ILCE-7RM6 RAWs with a known orientation the largest difference was 0.02%.) A quarter
/// turn changes the aspect of a 5:4 image by 56% and of a 3:2 one by 125%; a square image's
/// turn cannot be told by its aspect at all.
const PREVIEW_ASPECT_TOLERANCE: f64 = 0.02;
