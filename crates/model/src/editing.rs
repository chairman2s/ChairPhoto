//! The edit record a photo version stores as its `edit_json`, and the basic helpers around
//! it — a port of `src/modules/editing.ts` (see docs/editing.md).
//!
//! # Why a mirror, not core's type
//!
//! The render engine's record (`crates/core/src/plugins/edit/mod.rs`, `EditRecord`) is
//! private, deserialize-only, and *defaulted*: an absent `tone.ev` reads as `0.0`, an absent
//! `engine` as `1`. The UI needs the opposite. It must tell an absent key from a zero one
//! (a sparse tone merges key by key; `isEngine1Version` asks whether the record holds
//! anything at all) and write back exactly what it read. So [`VersionEdit`] mirrors the
//! TypeScript `VersionEdit` deliberately: every field optional, `f64` for every TS
//! `number`, and unknown keys kept in an `extra` map at every level (the record and each
//! nested object) and written back, as the TS object spreads carried them.
//!
//! # JSON compatibility
//!
//! - **Reading.** Every record the TS app wrote parses. Inside a present object, a field
//!   core defaults is defaulted here with core's value (`crop.w` → 1, `grain.size` → 1,
//!   `bw` → enabled Rec.601 weights, …) and a field core requires is required here
//!   (`perspective` corners, `lut.file`), so the model reads what the renderer renders.
//!   A key whose value has the wrong type is dropped rather than failing the whole record
//!   ([`parse_edit`]); TS kept such a value verbatim, but core could not render it either.
//!   An explicit `null` is kept as [`Field::Null`], distinct from a missing key, and
//!   written back: TS kept it, and `{"bw":null}` is not `{}` to `isEngine1Version`. Every
//!   optional key (the record's, the tone's, the white balance's, `crop.aspect`,
//!   `perspective.aspect`) is a [`Field`]. Not matched: a `null` inside a fixed-shape
//!   object's number or flag (`crop.x`, `bw.r`, a `zones` entry) fails that key like any
//!   wrong type, so the key is dropped — core cannot render such a record either.
//! - **Writing.** [`VersionEdit::to_json`] omits absent fields (as `JSON.stringify` omits
//!   `undefined`) and spells integral numbers as integers — core reads `engine` and
//!   `grain.seed` as integers and rejects `2.0`. See [`crate::js_compat::to_json_string`].

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::js_compat;

/// A record key in one of its three JSON states. TS kept an explicit `null` as a value
/// distinct from a missing key: `JSON.stringify({ bw: null })` is `{"bw":null}`, not `{}`,
/// and `isEngine1Version` reads that difference. Every optional record field is a `Field`,
/// so `null` round-trips and counts as holding something, while every `?? x` read treats
/// it like absence ([`Field::get`], [`Field::value`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Field<T> {
    /// The key is not there (TS `undefined`); omitted when written.
    Absent,
    /// The key is there with `null`.
    Null,
    /// The key has a value.
    Set(T),
}

impl<T> Default for Field<T> {
    fn default() -> Self {
        Field::Absent
    }
}

impl<T> Field<T> {
    /// The value, if set — `null` and absent both read as none (`?? x`).
    pub fn value(&self) -> Option<&T> {
        match self {
            Field::Set(v) => Some(v),
            Field::Absent | Field::Null => None,
        }
    }

    pub fn into_value(self) -> Option<T> {
        match self {
            Field::Set(v) => Some(v),
            Field::Absent | Field::Null => None,
        }
    }

    pub fn is_absent(&self) -> bool {
        matches!(self, Field::Absent)
    }

    pub fn is_set(&self) -> bool {
        matches!(self, Field::Set(_))
    }

    /// `{ ...{k: self}, ...{k: over} }`: `over` wins when its key is there, `null` included.
    pub fn overlaid(&self, over: &Field<T>) -> Field<T>
    where
        T: Clone,
    {
        if over.is_absent() {
            self.clone()
        } else {
            over.clone()
        }
    }
}

impl<T: Copy> Field<T> {
    /// The value, if set (`?? x` reads `null` as missing).
    pub fn get(&self) -> Option<T> {
        self.value().copied()
    }
}

impl Field<String> {
    pub fn as_deref(&self) -> Option<&str> {
        self.value().map(String::as_str)
    }
}

impl<T> From<Option<T>> for Field<T> {
    /// `None` is absent (TS `undefined`), not `null`.
    fn from(o: Option<T>) -> Self {
        o.map_or(Field::Absent, Field::Set)
    }
}

impl<T: Serialize> Serialize for Field<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Field::Set(v) => v.serialize(s),
            // Absent fields are skipped by `skip_serializing_if`; were one written anyway,
            // `null` is the only JSON for it.
            Field::Absent | Field::Null => s.serialize_none(),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    /// Called only for a key that is there (a missing key takes `#[serde(default)]`).
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Option::<T>::deserialize(d)?.map_or(Field::Null, Field::Set))
    }
}

/// Crop rectangle as fractions (0–1) of the source — resolution-independent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Crop {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default = "one")]
    pub w: f64,
    #[serde(default = "one")]
    pub h: f64,
    /// The chosen aspect preset label (UI bookkeeping; the engine locks "A:B" ratios).
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub aspect: Field<String>,
    /// Keys this build does not know, kept and written back as the TS spreads did.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Crop {
    /// A crop without an aspect label.
    pub fn rect(x: f64, y: f64, w: f64, h: f64) -> Self {
        Crop { x, y, w, h, aspect: Field::Absent, extra: Map::new() }
    }
}

/// One corner of a [`Perspective`] quad, as fractions (0–1) of the source.
pub type QuadPoint = [f64; 2];

/// Four-corner perspective (keystone) correction: the source quadrilateral that becomes the
/// output rectangle, as fractions of the source. Corners are named by their position on
/// the *subject*, so one quad carries rotation and keystone together.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Perspective {
    pub tl: QuadPoint,
    pub tr: QuadPoint,
    pub br: QuadPoint,
    pub bl: QuadPoint,
    /// Output aspect (width / height). Absent = the engine derives it from the quad.
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub aspect: Field<f64>,
    /// Keys this build does not know, kept and written back as the TS spreads did.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A corner of the perspective quad.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuadCorner {
    Tl,
    Tr,
    Br,
    Bl,
}

impl QuadCorner {
    /// The record key (`"tl"` …).
    pub fn key(self) -> &'static str {
        match self {
            QuadCorner::Tl => "tl",
            QuadCorner::Tr => "tr",
            QuadCorner::Br => "br",
            QuadCorner::Bl => "bl",
        }
    }
}

/// The four corners in drawing order, so the overlay polygon and the handles agree.
pub const QUAD_CORNERS: [QuadCorner; 4] = [QuadCorner::Tl, QuadCorner::Tr, QuadCorner::Br, QuadCorner::Bl];

impl Perspective {
    pub fn corner(&self, c: QuadCorner) -> QuadPoint {
        match c {
            QuadCorner::Tl => self.tl,
            QuadCorner::Tr => self.tr,
            QuadCorner::Br => self.br,
            QuadCorner::Bl => self.bl,
        }
    }

    pub fn corner_mut(&mut self, c: QuadCorner) -> &mut QuadPoint {
        match c {
            QuadCorner::Tl => &mut self.tl,
            QuadCorner::Tr => &mut self.tr,
            QuadCorner::Br => &mut self.br,
            QuadCorner::Bl => &mut self.bl,
        }
    }
}

/// `DEFAULT_QUAD`: the quad a fresh perspective edit starts from — the whole frame, inset far enough that
/// all four handles are visible and grabbable.
pub fn default_quad() -> Perspective {
    Perspective {
        tl: [0.06, 0.06],
        tr: [0.94, 0.06],
        br: [0.94, 0.94],
        bl: [0.06, 0.94],
        aspect: Field::Absent,
        extra: Map::new(),
    }
}

/// White balance. Relative (the default): `temp`/`tint` −1..1 around as-shot. Kelvin
/// (engine 2): `mode: "kelvin"`, the scene's light in `kelvin`, `tint` in Kelvin units.
/// Every field is optional because TS reads each with `?? 0` and records are sparse;
/// `mode` stays a string so an unknown mode round-trips instead of failing the record.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Wb {
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub temp: Field<f64>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub tint: Field<f64>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub mode: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub kelvin: Field<f64>,
    /// Keys this build does not know, kept and written back as the TS spreads did.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Wb {
    /// A relative white balance `{ temp, tint }`.
    pub fn relative(temp: f64, tint: f64) -> Self {
        Wb { temp: Field::Set(temp), tint: Field::Set(tint), mode: Field::Absent, kelvin: Field::Absent, extra: Map::new() }
    }

    pub fn is_kelvin(&self) -> bool {
        self.mode.as_deref() == Some("kelvin")
    }
}

/// Tone. Sparse on disk — `{"tone":{"ev":0.5}}` is a normal record, and presets list only
/// the keys they touch — so every field is optional; absent reads as 0 everywhere.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tone {
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub ev: Field<f64>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub contrast: Field<f64>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub highlights: Field<f64>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub shadows: Field<f64>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub whites: Field<f64>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub blacks: Field<f64>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub vibrance: Field<f64>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub saturation: Field<f64>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub wb: Field<Wb>,
    /// Keys this build does not know, kept and written back as the TS spreads did.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Tone {
    /// `ZERO_TONE`: every slider at 0, white balance `{ temp: 0, tint: 0 }`.
    pub fn zero() -> Self {
        Tone {
            ev: Field::Set(0.0),
            contrast: Field::Set(0.0),
            highlights: Field::Set(0.0),
            shadows: Field::Set(0.0),
            whites: Field::Set(0.0),
            blacks: Field::Set(0.0),
            vibrance: Field::Set(0.0),
            saturation: Field::Set(0.0),
            wb: Field::Set(Wb::relative(0.0, 0.0)),
            extra: Map::new(),
        }
    }

    /// `{ ...self, ...over }`: `over`'s present keys win (white balance as a whole).
    pub fn merged(&self, over: &Tone) -> Tone {
        Tone {
            ev: self.ev.overlaid(&over.ev),
            contrast: self.contrast.overlaid(&over.contrast),
            highlights: self.highlights.overlaid(&over.highlights),
            shadows: self.shadows.overlaid(&over.shadows),
            whites: self.whites.overlaid(&over.whites),
            blacks: self.blacks.overlaid(&over.blacks),
            vibrance: self.vibrance.overlaid(&over.vibrance),
            saturation: self.saturation.overlaid(&over.saturation),
            wb: self.wb.overlaid(&over.wb),
            extra: merged_extra(&self.extra, &over.extra),
        }
    }
}

/// Black & white conversion via a channel mixer (weights normalized by the engine).
/// Absent fields read as core's default: an enabled neutral Rec.601 mix.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bw {
    pub enabled: bool,
    pub r: f64,
    pub g: f64,
    pub b: f64,
    /// Keys this build does not know, kept and written back as the TS spreads did.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Bw {
    fn default() -> Self {
        Bw::mix(0.299, 0.587, 0.114)
    }
}

/// Split toning: hues in degrees (0–360), sats 0–1, balance −1..1.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Split {
    pub shadow_hue: f64,
    pub shadow_sat: f64,
    pub highlight_hue: f64,
    pub highlight_sat: f64,
    pub balance: f64,
    /// Keys this build does not know, kept and written back as the TS spreads did.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Deterministic film grain — the seed is part of the record so exports reproduce. Core
/// reads `seed` as `u32`; it is written as a JSON integer (see the module docs).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Grain {
    pub amount: f64,
    pub size: f64,
    pub seed: f64,
    /// Keys this build does not know, kept and written back as the TS spreads did.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Grain {
    fn default() -> Self {
        Grain { amount: 0.0, size: 1.0, seed: 0.0, extra: Map::new() }
    }
}

/// A `.cube` LUT by bare filename in the app's luts folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LutRef {
    pub file: String,
    #[serde(default = "one")]
    pub amount: f64,
    /// Keys this build does not know, kept and written back as the TS spreads did.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Engine 2's lens corrections from the camera's own tables.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Lens {
    pub builtin: bool,
    /// Keys this build does not know, kept and written back as the TS spreads did.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Bw {
    /// An enabled mixer with these weights.
    pub fn mix(r: f64, g: f64, b: f64) -> Self {
        Bw { enabled: true, r, g, b, extra: Map::new() }
    }
}

/// `{ ...a, ...b }` over the unknown keys: `b`'s win.
fn merged_extra(a: &Map<String, Value>, b: &Map<String, Value>) -> Map<String, Value> {
    let mut out = a.clone();
    out.extend(b.iter().map(|(k, v)| (k.clone(), v.clone())));
    out
}

fn one() -> f64 {
    1.0
}

/// A version's edit record (TS `VersionEdit`). Every field is a [`Field`]; see the module
/// docs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VersionEdit {
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub crop: Field<Crop>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub tone: Field<Tone>,
    /// Four-corner perspective correction; absent = leave the geometry alone.
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub perspective: Field<Perspective>,
    /// Straighten angle in degrees.
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub straighten: Field<f64>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub bw: Field<Bw>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub split: Field<Split>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub grain: Field<Grain>,
    /// Lifted matte blacks, 0..1.
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub fade: Field<f64>,
    /// Corner shading, −1..1 (negative darkens).
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub vignette: Field<f64>,
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub lut: Field<LutRef>,
    /// Tone-strip zone offsets in EV, blacks→whites (8 entries). Absent = none.
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub zones: Field<Vec<f64>>,
    /// Which render engine the record was made for: absent or 1 = the gamma pipeline on
    /// the camera preview, 2 = the scene-linear pipeline on the RAW ([`ENGINE_LINEAR`]).
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub engine: Field<f64>,
    /// Engine 2's display transform ("srgb", "soft", "camera", "camera.2").
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub display: Field<String>,
    /// Engine 2's camera-match exposure offset, in EV. Absent = 0.
    #[serde(default, rename = "cameraEv", skip_serializing_if = "Field::is_absent")]
    pub camera_ev: Field<f64>,
    /// Engine 2 lens corrections. Belongs to the photo, not the look.
    #[serde(default, skip_serializing_if = "Field::is_absent")]
    pub lens: Field<Lens>,
    /// Top-level keys this build does not know, kept verbatim and written back — the TS
    /// app's object spreads carried them through every edit.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl VersionEdit {
    /// The record as `edit_json` (`JSON.stringify`): absent fields omitted, integral numbers
    /// as integers.
    pub fn to_json(&self) -> String {
        js_compat::to_json_string(self)
    }

    /// Whether the record holds nothing (`JSON.stringify(record) === "{}"`).
    pub fn is_empty(&self) -> bool {
        *self == VersionEdit::default()
    }

    /// `{ ...self, ...over }` for records read from JSON: every field `over` holds wins,
    /// the rest of `self` stays.
    pub fn overlaid(&self, over: &VersionEdit) -> VersionEdit {
        let mut out = self.clone();
        macro_rules! take {
            ($($f:ident),*) => { $( out.$f = out.$f.overlaid(&over.$f); )* };
        }
        take!(crop, tone, perspective, straighten, bw, split, grain, fade, vignette, lut, zones, engine, display, camera_ev, lens);
        for (k, v) in &over.extra {
            out.extra.insert(k.clone(), v.clone());
        }
        out
    }

    /// The record's look slice.
    pub fn look(&self) -> Look {
        Look {
            bw: self.bw.clone(),
            split: self.split.clone(),
            grain: self.grain.clone(),
            fade: self.fade.clone(),
            vignette: self.vignette.clone(),
            lut: self.lut.clone(),
        }
    }

    /// `{ ...self, ...lookFields(look) }`: all six look fields replaced, defaults dropped.
    /// (The TS spread writes every key, `undefined` included, so absent clears.)
    pub fn with_look(&self, look: &Look) -> VersionEdit {
        let l = look_fields(look);
        VersionEdit {
            bw: l.bw,
            split: l.split,
            grain: l.grain,
            fade: l.fade,
            vignette: l.vignette,
            lut: l.lut,
            ..self.clone()
        }
    }
}

/// The scene-linear engine that renders from the RAW working image.
pub const ENGINE_LINEAR: f64 = 2.0;

/// The display transform new engine-2 records start with.
pub const DEFAULT_LINEAR_DISPLAY: &str = "camera.2";

/// `record` as an engine-2 record. One already made for engine 2 is returned as is; one
/// becoming engine 2 now gets the default display transform (unless it names one) and
/// this frame's camera match — only when non-zero and only for the default transform.
pub fn as_linear_record(record: &VersionEdit, camera_ev: f64) -> VersionEdit {
    if is_linear(record) {
        return record.clone();
    }
    let mut out = VersionEdit {
        engine: Field::Set(ENGINE_LINEAR),
        // `record.display ?? DEFAULT`: a `null` display takes the default too.
        display: Field::Set(record.display.value().cloned().unwrap_or_else(|| DEFAULT_LINEAR_DISPLAY.into())),
        ..record.clone()
    };
    if camera_ev != 0.0 && out.display.as_deref() == Some(DEFAULT_LINEAR_DISPLAY) {
        out.camera_ev = Field::Set(camera_ev);
    }
    out
}

/// Whether a record belongs to the linear engine.
pub fn is_linear(e: &VersionEdit) -> bool {
    e.engine.get() == Some(ENGINE_LINEAR)
}

/// A fresh engine-2 record from an engine-1 one: geometry (crop, perspective, straighten)
/// copied, tone and look reset, the default display transform, and this frame's camera
/// match when non-zero. Lens corrections are not copied (as in TS). A `null` framing
/// field is copied as `null`, as `crop: e.crop` did.
pub fn for_linear_engine(e: &VersionEdit, camera_ev: f64) -> VersionEdit {
    VersionEdit {
        crop: e.crop.clone(),
        perspective: e.perspective.clone(),
        straighten: e.straighten.clone(),
        engine: Field::Set(ENGINE_LINEAR),
        display: Field::Set(DEFAULT_LINEAR_DISPLAY.into()),
        camera_ev: (camera_ev != 0.0).then_some(camera_ev).into(),
        ..VersionEdit::default()
    }
}

/// Whether a saved version belongs to engine 1: no engine-2 stamp, and it holds something
/// (`JSON.stringify(record) !== "{}"` — a key set to `null`, as in `{"bw":null}`, counts).
pub fn is_engine1_version(record: &VersionEdit) -> bool {
    !is_linear(record) && !record.is_empty()
}

/// The look-only slice of an edit — everything except framing and tone.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Look {
    pub bw: Field<Bw>,
    pub split: Field<Split>,
    pub grain: Field<Grain>,
    pub fade: Field<f64>,
    pub vignette: Field<f64>,
    pub lut: Field<LutRef>,
}

impl Look {
    /// `ZERO_LOOK`: an all-off look (fade and vignette explicitly 0).
    pub fn zero() -> Self {
        Look { fade: Field::Set(0.0), vignette: Field::Set(0.0), ..Look::default() }
    }

    /// `{ ...self, ...over }` for a `Partial<Look>` patch: `over`'s present fields win.
    pub fn merged(&self, over: &Look) -> Look {
        Look {
            bw: self.bw.overlaid(&over.bw),
            split: self.split.overlaid(&over.split),
            grain: self.grain.overlaid(&over.grain),
            fade: self.fade.overlaid(&over.fade),
            vignette: self.vignette.overlaid(&over.vignette),
            lut: self.lut.overlaid(&over.lut),
        }
    }
}

/// The look with all-default values dropped, so the stored record stays minimal: grain
/// only with a positive amount; fade and vignette only when truthy (`|| undefined` — so 0
/// and NaN are dropped, and so is `null`). `bw`, `split` and `lut` pass through as they are,
/// `null` included.
pub fn look_fields(l: &Look) -> Look {
    let truthy = |v: &Field<f64>| v.get().filter(|x| *x != 0.0 && !x.is_nan()).into();
    Look {
        bw: l.bw.clone(),
        split: l.split.clone(),
        grain: l.grain.value().filter(|g| g.amount > 0.0).cloned().into(),
        fade: truthy(&l.fade),
        vignette: truthy(&l.vignette),
        lut: l.lut.clone(),
    }
}

/// A B&W contrast-filter chip.
#[derive(Clone, Debug, PartialEq)]
pub struct BwFilter {
    pub label: &'static str,
    pub bw: Bw,
}

/// `BW_FILTERS`: B&W contrast-filter chips for the Effects section — channel-mixer weight
/// recipes.
pub fn bw_filters() -> [BwFilter; 4] {
    [
        BwFilter { label: "Neutral", bw: Bw::mix(0.299, 0.587, 0.114) },
        BwFilter { label: "Red", bw: Bw::mix(0.9, 0.15, -0.05) },
        BwFilter { label: "Yellow", bw: Bw::mix(0.55, 0.4, 0.05) },
        BwFilter { label: "Green", bw: Bw::mix(0.2, 0.7, 0.1) },
    ]
}

/// The supported straighten range, ± degrees.
pub const STRAIGHTEN_MAX: f64 = 45.0;

/// Clamp the straighten angle to ±[`STRAIGHTEN_MAX`] (NaN stays NaN, as in JS).
pub fn clamp_straighten(deg: f64) -> f64 {
    js_compat::clamp(deg, -STRAIGHTEN_MAX, STRAIGHTEN_MAX)
}

/// The *extra* rotation in degrees that levels a line drawn on the image — to horizontal
/// if within 45° of it, otherwise to vertical. Screen-space coordinates (y-down).
pub fn level_from_line(x1: f64, y1: f64, x2: f64, y2: f64) -> f64 {
    let deg = (y2 - y1).atan2(x2 - x1) * 180.0 / std::f64::consts::PI;
    let mut m = deg; // fold to (-90, 90] — a line has no direction
    while m > 90.0 {
        m -= 180.0;
    }
    while m <= -90.0 {
        m += 180.0;
    }
    if m.abs() <= 45.0 {
        return -m;
    }
    if m > 0.0 {
        90.0 - m
    } else {
        -90.0 - m
    }
}

/// The largest centred crop (same aspect as the image) inside the image after
/// straightening by `degrees`, pulled in 0.3% — keeps the rotation's corners out of frame.
/// The full frame at ~0° or for an unmeasured image.
pub fn inscribed_crop(img_w: f64, img_h: f64, degrees: f64) -> Crop {
    let a = degrees.abs() * std::f64::consts::PI / 180.0;
    if a < 1e-4 || img_w <= 0.0 || img_h <= 0.0 {
        return Crop { aspect: Field::Set("Original".into()), ..Crop::rect(0.0, 0.0, 1.0, 1.0) };
    }
    let (sin, cos) = a.sin_cos();
    let s = js_compat::min(img_w / (img_w * cos + img_h * sin), img_h / (img_w * sin + img_h * cos));
    let sc = js_compat::clamp(s * 0.997, 0.05, 1.0);
    Crop { aspect: Field::Set("Original".into()), ..Crop::rect((1.0 - sc) / 2.0, (1.0 - sc) / 2.0, sc, sc) }
}

/// A crop preset. `ratio` = width/height; `None`: "Original" = no crop, "Free" = no lock.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AspectPreset {
    pub label: &'static str,
    pub ratio: Option<f64>,
    pub hint: Option<&'static str>,
}

/// Crop presets, social sizes flagged in the hint.
pub const ASPECTS: [AspectPreset; 11] = [
    AspectPreset { label: "Original", ratio: None, hint: None },
    AspectPreset { label: "Free", ratio: None, hint: Some("Crop with no fixed aspect") },
    AspectPreset { label: "1:1", ratio: Some(1.0), hint: Some("Instagram / Facebook square") },
    AspectPreset { label: "4:5", ratio: Some(4.0 / 5.0), hint: Some("Instagram portrait (max)") },
    AspectPreset { label: "1.91:1", ratio: Some(1.91), hint: Some("Instagram / Facebook link") },
    AspectPreset { label: "9:16", ratio: Some(9.0 / 16.0), hint: Some("Reels · Stories · TikTok · Snapchat · Shorts") },
    AspectPreset { label: "16:9", ratio: Some(16.0 / 9.0), hint: Some("Facebook · video") },
    AspectPreset { label: "3:2", ratio: Some(3.0 / 2.0), hint: None },
    AspectPreset { label: "2:3", ratio: Some(2.0 / 3.0), hint: None },
    AspectPreset { label: "4:3", ratio: Some(4.0 / 3.0), hint: None },
    AspectPreset { label: "3:4", ratio: Some(3.0 / 4.0), hint: None },
];

/// Composition overlay drawn inside the crop. Persisted as a preference by its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CropOverlay {
    None,
    Thirds,
    Phi,
    Golden,
}

impl CropOverlay {
    /// The persisted key (`"none"`, `"thirds"`, `"phi"`, `"golden"`).
    pub fn key(self) -> &'static str {
        match self {
            CropOverlay::None => "none",
            CropOverlay::Thirds => "thirds",
            CropOverlay::Phi => "phi",
            CropOverlay::Golden => "golden",
        }
    }

    /// The overlay for a persisted key.
    pub fn from_key(key: &str) -> Option<Self> {
        OVERLAYS.iter().map(|(o, _)| *o).find(|o| o.key() == key)
    }
}

/// The overlays in menu order, with their labels.
pub const OVERLAYS: [(CropOverlay, &str); 4] = [
    (CropOverlay::None, "None"),
    (CropOverlay::Thirds, "Thirds"),
    (CropOverlay::Phi, "Phi grid"),
    (CropOverlay::Golden, "Golden"),
];

/// Grid line offsets (as fractions) for the line-based overlays; empty for the others.
pub fn overlay_lines(o: CropOverlay) -> &'static [f64] {
    match o {
        CropOverlay::Thirds => &[1.0 / 3.0, 2.0 / 3.0],
        CropOverlay::Phi => &[0.382, 0.618],
        CropOverlay::None | CropOverlay::Golden => &[],
    }
}

/// A golden (Fibonacci) spiral as an SVG path in a 0–100 box, stretched to the crop:
/// chained quarter-arcs with radii in the 1/phi ratio, curling inward.
pub const GOLDEN_SPIRAL_PATH: &str = concat!(
    "M 0,61.8 A 61.8,61.8 0 0 1 61.8,0 A 38.2,38.2 0 0 1 100,38.2 ",
    "A 23.6,23.6 0 0 1 76.4,61.8 A 14.6,14.6 0 0 1 61.8,47.2 A 9,9 0 0 1 70.8,38.2"
);

/// Parse a version's stored `edit_json` (tolerant of empty/partial). Absent or empty
/// input, invalid JSON and a non-object (TS would return `null`/an array cast as a
/// record) give the empty record. A key whose value does not fit its type is dropped and
/// the rest kept (see the module docs).
pub fn parse_edit(edit_json: Option<&str>) -> VersionEdit {
    match edit_json {
        Some(s) if !s.is_empty() => serde_json::from_str::<Value>(s).map(edit_from_value).unwrap_or_default(),
        _ => VersionEdit::default(),
    }
}

/// A record from an already-parsed JSON value; see [`parse_edit`].
pub fn edit_from_value(v: Value) -> VersionEdit {
    let Value::Object(obj) = v else { return VersionEdit::default() };
    if let Ok(e) = serde_json::from_value::<VersionEdit>(Value::Object(obj.clone())) {
        return e;
    }
    // Salvage key by key: a bad `fade` must not cost the photo its crop.
    let mut out = VersionEdit::default();
    for (k, v) in obj {
        let mut one = Map::new();
        one.insert(k, v);
        if let Ok(e) = serde_json::from_value::<VersionEdit>(Value::Object(one)) {
            out = out.overlaid(&e);
        }
    }
    out
}

/// The largest crop of pixel aspect `ratio` (width/height) inside a W×H image, scaled by
/// `size` (0–1) and centred, as fractions. Degenerate inputs give the full frame.
pub fn fit_crop(img_w: f64, img_h: f64, ratio: f64, size: f64) -> Crop {
    if img_w <= 0.0 || img_h <= 0.0 || ratio <= 0.0 {
        return Crop::rect(0.0, 0.0, 1.0, 1.0);
    }
    let (mut cw, mut ch) = if img_w / img_h >= ratio { (ratio * img_h, img_h) } else { (img_w, img_w / ratio) };
    cw *= size;
    ch *= size;
    let w = js_compat::min(1.0, cw / img_w);
    let h = js_compat::min(1.0, ch / img_h);
    Crop::rect((1.0 - w) / 2.0, (1.0 - h) / 2.0, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(v: Value) -> VersionEdit {
        parse_edit(Some(&v.to_string()))
    }

    // --- src/modules/__tests__/editing.perspective.test.ts (5 cases) ---

    #[test]
    fn perspective_round_trips_through_the_stored_edit_json() {
        let perspective = Perspective { tl: [0.098, 0.171], tr: [0.853, 0.106], br: [0.878, 0.9], bl: [0.083, 0.921], aspect: Field::Absent, extra: Map::new() };
        let saved = VersionEdit { perspective: Field::Set(perspective.clone()), straighten: Field::Set(0.0), ..Default::default() }.to_json();
        assert_eq!(parse_edit(Some(&saved)).perspective, Field::Set(perspective));
    }

    #[test]
    fn perspective_is_absent_not_defaulted_when_a_version_has_none() {
        assert_eq!(rec(json!({"straighten": 2})).perspective, Field::Absent);
        assert_eq!(parse_edit(Some("{}")).perspective, Field::Absent);
        assert_eq!(parse_edit(None).perspective, Field::Absent);
    }

    #[test]
    fn perspective_survives_a_record_written_by_an_older_build() {
        let back = parse_edit(Some(r#"{"crop":{"x":0.1,"y":0,"w":0.8,"h":1},"tone":{"ev":0.5}}"#));
        assert_eq!(back.perspective, Field::Absent);
        assert_eq!(back.crop, Field::Set(Crop::rect(0.1, 0.0, 0.8, 1.0)));
    }

    #[test]
    fn quad_corners_are_in_drawing_order() {
        let keys: Vec<&str> = QUAD_CORNERS.iter().map(|c| c.key()).collect();
        assert_eq!(keys, ["tl", "tr", "br", "bl"]);
    }

    #[test]
    fn default_quad_is_an_inset_full_frame_with_every_corner_grabbable() {
        let xs: Vec<f64> = QUAD_CORNERS.iter().map(|c| default_quad().corner(*c)[0]).collect();
        let ys: Vec<f64> = QUAD_CORNERS.iter().map(|c| default_quad().corner(*c)[1]).collect();
        for v in xs.iter().chain(&ys) {
            assert!(*v > 0.0 && *v < 1.0);
        }
        let span = |v: &[f64]| v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min);
        assert!(span(&xs) > 0.8);
        assert!(span(&ys) > 0.8);
    }

    // --- src/modules/__tests__/editingEngine.test.ts (6 cases) ---

    #[test]
    fn engine_id_is_absent_on_existing_records_and_survives_a_round_trip() {
        assert!(!is_linear(&parse_edit(Some(r#"{"tone":{"ev":1}}"#))));
        assert!(is_linear(&parse_edit(Some(r#"{"engine":2}"#))));
        let back: Value = serde_json::from_str(&parse_edit(Some(r#"{"engine":2,"tone":{"ev":1}}"#)).to_json()).unwrap();
        assert_eq!(back["engine"], json!(2));
        // Written as an integer: core reads `engine` as u32 and would reject `2.0`.
        assert!(back["engine"].is_u64());
    }

    #[test]
    fn for_linear_engine_copies_geometry_resets_tone_and_look_and_stamps_engine_2() {
        let out = for_linear_engine(
            &rec(json!({
                "crop": {"x": 0.1, "y": 0.1, "w": 0.8, "h": 0.8, "aspect": "1:1"},
                "straighten": 2,
                "tone": {"ev": 1},
                "bw": {"enabled": true, "r": 1, "g": 0, "b": 0},
                "zones": [0, 1, 0, 0, 0, 0, 0, 0],
            })),
            0.0,
        );
        assert_eq!(out, rec(json!({"crop": {"x": 0.1, "y": 0.1, "w": 0.8, "h": 0.8, "aspect": "1:1"}, "straighten": 2, "engine": 2, "display": "camera.2"})));
    }

    #[test]
    fn a_record_becoming_engine_2_gets_the_camera_look_an_engine_2_record_keeps_its_own() {
        assert_eq!(as_linear_record(&rec(json!({"tone": {"ev": 0.5}})), 0.0), rec(json!({"tone": {"ev": 0.5}, "engine": 2, "display": "camera.2"})));
        assert_eq!(as_linear_record(&rec(json!({"engine": 1, "fade": 0.1})), 0.0), rec(json!({"engine": 2, "fade": 0.1, "display": "camera.2"})));
        // Saved before the default changed: no display means plain sRGB, forever.
        let old = parse_edit(Some(r#"{"engine":2,"fade":0.1}"#));
        assert_eq!(as_linear_record(&old, 0.0), old);
        assert_eq!(as_linear_record(&parse_edit(Some(r#"{"engine":2,"display":"soft"}"#)), 0.0).display.as_deref(), Some("soft"));
    }

    #[test]
    fn stamps_this_frames_camera_match_on_a_new_record_only_and_leaves_0_off() {
        assert_eq!(
            as_linear_record(&rec(json!({"fade": 0.1})), -1.6),
            rec(json!({"fade": 0.1, "engine": 2, "display": "camera.2", "cameraEv": -1.6}))
        );
        assert_eq!(as_linear_record(&rec(json!({"fade": 0.1})), 0.0).camera_ev, Field::Absent);
        let saved = parse_edit(Some(r#"{"engine":2,"display":"camera","cameraEv":-0.5}"#));
        assert_eq!(as_linear_record(&saved, -1.6), saved);
        assert_eq!(as_linear_record(&rec(json!({"display": "srgb"})), -1.6).camera_ev, Field::Absent);
    }

    #[test]
    fn a_saved_record_without_the_engine_2_stamp_that_holds_anything_is_engine_1() {
        assert!(is_engine1_version(&parse_edit(Some(r#"{"tone":{"ev":0.5}}"#))));
        assert!(is_engine1_version(&parse_edit(Some(r#"{"engine":1,"fade":0.2}"#))));
        assert!(!is_engine1_version(&parse_edit(Some(r#"{"engine":2,"display":"camera.2"}"#))));
        assert!(!is_engine1_version(&parse_edit(Some("{}"))));
    }

    #[test]
    fn the_fork_keeps_the_framing_resets_tone_and_look_and_takes_this_frames_match() {
        let old = parse_edit(Some(
            r#"{"crop":{"x":0.1,"y":0,"w":0.8,"h":1,"aspect":"4:5"},"straighten":1.2,"tone":{"ev":0.8},"fade":0.3,"lut":{"file":"a.cube","amount":1}}"#,
        ));
        assert_eq!(
            for_linear_engine(&old, -1.6),
            rec(json!({"crop": {"x": 0.1, "y": 0, "w": 0.8, "h": 1, "aspect": "4:5"}, "straighten": 1.2, "engine": 2, "display": "camera.2", "cameraEv": -1.6}))
        );
        assert_eq!(for_linear_engine(&old, 0.0).camera_ev, Field::Absent);
    }

    // --- JSON compatibility with records the TS app wrote (new; no TS counterpart) ---

    /// Every record shape the TS tests feed `parseEdit`, parsed and written back: the same
    /// JSON value (key order aside), so the Tauri app reads what the GPUI app writes.
    #[test]
    fn records_from_the_ts_tests_round_trip_value_for_value() {
        let records = [
            r#"{"perspective":{"tl":[0.098,0.171],"tr":[0.853,0.106],"br":[0.878,0.9],"bl":[0.083,0.921]},"straighten":0}"#,
            r#"{"crop":{"x":0.1,"y":0,"w":0.8,"h":1},"tone":{"ev":0.5}}"#,
            r#"{"engine":2,"tone":{"ev":1}}"#,
            r#"{"engine":2,"display":"camera","cameraEv":-0.5}"#,
            r#"{"crop":{"x":0.1,"y":0,"w":0.8,"h":1,"aspect":"4:5"},"straighten":1.2,"tone":{"ev":0.8},"fade":0.3,"lut":{"file":"a.cube","amount":1}}"#,
            r#"{"engine":2,"display":"camera","cameraEv":-1.6,"lens":{"builtin":true},"crop":{"x":0.1,"y":0.1,"w":0.8,"h":0.8,"aspect":"4:5"},"straighten":1.5,"perspective":{"tl":[0,0],"tr":[1,0],"br":[1,1],"bl":[0,1]},"tone":{"ev":0.5,"contrast":0.2},"zones":[0,0.1,0,0,0,0,0,0],"fade":0.2,"lut":{"file":"portra.cube","amount":0.8}}"#,
            r#"{"tone":{"wb":{"temp":0,"tint":6,"mode":"kelvin","kelvin":4800}}}"#,
            r#"{"bw":{"enabled":true,"r":0.9,"g":0.15,"b":-0.05},"split":{"shadow_hue":35,"shadow_sat":0.25,"highlight_hue":45,"highlight_sat":0.12,"balance":0},"grain":{"amount":0.5,"size":1.2,"seed":0},"vignette":-0.2}"#,
            "{}",
        ];
        for json in records {
            let written = parse_edit(Some(json)).to_json();
            let a: Value = serde_json::from_str(json).unwrap();
            let b: Value = serde_json::from_str(&written).unwrap();
            assert_eq!(a, b, "{json} → {written}");
        }
    }

    /// Review finding (Codex, #103): unknown keys inside nested objects were dropped, so
    /// saving an unrelated adjustment destroyed extension data the TS spreads carried.
    #[test]
    fn unknown_keys_survive_in_every_nested_object() {
        let json = r#"{
            "crop": {"x": 0, "y": 0, "w": 1, "h": 1, "f": 1},
            "tone": {"ev": 1, "future": 0.2, "wb": {"temp": 0.1, "tint": 0, "f": "w"}},
            "perspective": {"tl": [0, 0], "tr": [1, 0], "br": [1, 1], "bl": [0, 1], "f": [1]},
            "bw": {"enabled": true, "r": 1, "g": 0, "b": 0, "f": true},
            "split": {"shadow_hue": 1, "shadow_sat": 0, "highlight_hue": 2, "highlight_sat": 0, "balance": 0, "f": 2},
            "grain": {"amount": 0.2, "size": 1, "seed": 0, "f": {"x": 1}},
            "lut": {"file": "a.cube", "amount": 1, "f": "l"},
            "lens": {"builtin": true, "f": 3}
        }"#;
        let e = parse_edit(Some(json));
        let back: Value = serde_json::from_str(&e.to_json()).unwrap();
        assert_eq!(back, serde_json::from_str::<Value>(json).unwrap());
        // Saving an unrelated adjustment keeps them.
        let adjusted = e.with_look(&Look { fade: Field::Set(0.3), ..e.look() });
        let back: Value = serde_json::from_str(&adjusted.to_json()).unwrap();
        assert_eq!(back["tone"]["future"], json!(0.2));
        assert_eq!(back["tone"]["wb"]["f"], json!("w"));
        assert_eq!(back["crop"]["f"], json!(1));
        assert_eq!(back["fade"], json!(0.3));
        // A sparse tone merge keeps both sides' unknown keys, the patch's winning.
        let a = rec(json!({"tone": {"ev": 1, "p": 1, "q": 1}})).tone.into_value().unwrap();
        let b = rec(json!({"tone": {"contrast": 0.2, "q": 2}})).tone.into_value().unwrap();
        let m = serde_json::to_value(a.merged(&b)).unwrap();
        assert_eq!((m["p"].clone(), m["q"].clone()), (json!(1), json!(2)));
    }

    /// Review finding (Codex, #103): an explicit `null` was read as absence, so `{"bw":null}`
    /// saved as `{}` and `is_engine1_version` said false where TS said true — and
    /// DarkroomView keeps an engine-1 version on the camera preview by that answer.
    #[test]
    fn explicit_null_round_trips_and_counts_as_holding_something() {
        for json in [
            r#"{"bw":null}"#,
            r#"{"crop":null,"fade":null,"lut":null,"engine":null,"cameraEv":null}"#,
            r#"{"tone":{"ev":null,"wb":null},"straighten":null}"#,
            r#"{"tone":{"wb":{"temp":null,"tint":0,"mode":null,"kelvin":null}}}"#,
            r#"{"crop":{"x":0,"y":0,"w":1,"h":1,"aspect":null},"perspective":{"tl":[0,0],"tr":[1,0],"br":[1,1],"bl":[0,1],"aspect":null}}"#,
        ] {
            let e = parse_edit(Some(json));
            let (a, b): (Value, Value) = (serde_json::from_str(json).unwrap(), serde_json::from_str(&e.to_json()).unwrap());
            assert_eq!(a, b, "{json}");
        }
        let bw_null = parse_edit(Some(r#"{"bw":null}"#));
        assert_eq!(bw_null.bw, Field::Null);
        assert_eq!(bw_null.to_json(), r#"{"bw":null}"#);
        assert!(is_engine1_version(&bw_null));
        assert!(!bw_null.is_empty());
        assert!(!is_engine1_version(&parse_edit(Some(r#"{"engine":2,"bw":null}"#))));
    }

    /// Every TS read of a nullable key was `?? x` or a truthiness test, so `null` reads as
    /// missing — while the spreads carry it and the explicit `undefined`s drop it.
    #[test]
    fn explicit_null_reads_as_missing_and_travels_as_ts_spreads_carried_it() {
        // `record.display ?? DEFAULT`
        assert_eq!(as_linear_record(&rec(json!({"display": null})), 0.0).display.as_deref(), Some(DEFAULT_LINEAR_DISPLAY));
        // `{ ...record, engine: 2, … }` keeps the other nulls.
        assert_eq!(as_linear_record(&rec(json!({"bw": null})), 0.0).bw, Field::Null);
        // `crop: e.crop` copies a null crop; the look is not copied at all.
        let forked = for_linear_engine(&rec(json!({"crop": null, "fade": null})), 0.0);
        assert_eq!((forked.crop, forked.fade), (Field::Null, Field::Absent));
        // lookFields: `fade || undefined` drops a null fade, `bw: l.bw` keeps a null bw.
        let e = rec(json!({"fade": null, "bw": null, "grain": null}));
        let adjusted = e.with_look(&e.look());
        assert_eq!((adjusted.fade, adjusted.bw, adjusted.grain), (Field::Absent, Field::Null, Field::Absent));
        // `JSON.parse` of a null key and `{}` differ only in holding something.
        assert_eq!(rec(json!({"tone": null})).tone.value(), None);
    }

    #[test]
    fn integers_stay_integers_for_core() {
        let written = rec(json!({"engine": 2, "grain": {"amount": 0.2, "size": 1, "seed": 7}, "straighten": 2})).to_json();
        assert!(written.contains(r#""engine":2"#) && written.contains(r#""seed":7"#) && written.contains(r#""straighten":2"#), "{written}");
    }

    /// The render engine parses what this crate writes. `record_engine` falls back to 1
    /// when the whole record fails to parse, so reading back 2 proves core accepted every
    /// field — including the integer-typed `engine` and `grain.seed`. Needs core's render
    /// engine: `--features edit-parity` (see Cargo.toml for why it is opt-in).
    #[cfg(not(feature = "edit-parity"))]
    #[test]
    fn core_parses_every_record_this_crate_writes() {
        println!("SKIPPED: core_parses_every_record_this_crate_writes — needs `--features edit-parity`");
    }

    #[cfg(feature = "edit-parity")]
    #[test]
    fn core_parses_every_record_this_crate_writes() {
        use chairphoto_core::plugins::edit::{is_bw, record_engine};
        let full = rec(json!({
            "crop": {"x": 0.1, "y": 0.1, "w": 0.8, "h": 0.8, "aspect": "4:5"},
            "tone": {"ev": 0.5, "contrast": 0.2, "wb": {"temp": 0, "tint": 6, "mode": "kelvin", "kelvin": 4800}},
            "perspective": {"tl": [0, 0], "tr": [1, 0], "br": [1, 1], "bl": [0, 1], "aspect": 1.5},
            "straighten": 2,
            "bw": {"enabled": true, "r": 0.9, "g": 0.15, "b": -0.05},
            "split": {"shadow_hue": 35, "shadow_sat": 0.25, "highlight_hue": 45, "highlight_sat": 0.12, "balance": 0},
            "grain": {"amount": 0.5, "size": 1.2, "seed": 3},
            "fade": 0.1, "vignette": -0.2,
            "lut": {"file": "portra.cube", "amount": 0.8},
            "zones": [0, 0.1, 0, 0, 0, 0, 0, 0],
            "engine": 2, "display": "camera.2", "cameraEv": -1.6,
            "lens": {"builtin": true},
        }));
        let written = full.to_json();
        assert_eq!(record_engine(&written), 2, "{written}");
        assert!(is_bw(&written));
        let forked = for_linear_engine(&full, -1.6).to_json();
        assert_eq!(record_engine(&forked), 2, "{forked}");
    }

    #[test]
    fn unknown_keys_survive_and_bad_values_cost_only_their_key() {
        let e = parse_edit(Some(r#"{"future":{"a":[1,2]},"fade":"oops","crop":{"x":0,"y":0,"w":0.5,"h":0.5}}"#));
        assert_eq!(e.crop, Field::Set(Crop::rect(0.0, 0.0, 0.5, 0.5)));
        assert_eq!(e.fade, Field::Absent);
        let back: Value = serde_json::from_str(&e.to_json()).unwrap();
        assert_eq!(back["future"], json!({"a": [1, 2]}));
        assert_eq!(parse_edit(Some("not json")), VersionEdit::default());
        assert_eq!(parse_edit(Some("null")), VersionEdit::default());
        assert_eq!(parse_edit(Some("")), VersionEdit::default());
    }
}
