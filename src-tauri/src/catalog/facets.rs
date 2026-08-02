//! Derived facets: internal, computed-from-EXIF view filters (has-GPS, shot-on-mobile,
//! drone, soft). Unlike tags/auto-tags, facets are **never exported** — they only narrow
//! the current view (see docs/taxonomy.md, "Facets vs auto-tags"). Membership is computed
//! on the fly from `photos` columns (no stored state → always consistent), so a facet
//! is just a boolean SQL predicate ANDed into `list_photos`.
//!
//! The make/model matches are deliberately simple heuristics; extend the table as new
//! devices show up.
//!
//! The `soft` facet is dynamic: its threshold is read from the `sharpness.soft_threshold`
//! setting at predicate-construction time. The default is conservative (H16 design doc):
//! false negatives are fine; a false positive erodes trust in the signal. Default: 15.0
//! (a reasonably low bar that only flags clearly unsharp frames).

use super::{Catalog, CatalogError, Result};

/// Settings key for the soft-photo sharpness threshold (stored as a decimal string).
/// The `soft` facet fires when `photos.sharpness < threshold AND sharpness IS NOT NULL`.
/// Conservative default: `15.0`. See `docs/sharpness-culling.md`.
pub const SOFT_THRESHOLD_KEY: &str = "sharpness.soft_threshold";

/// Conservative default threshold for the `soft` facet. False negatives are acceptable;
/// false positives erode trust. 15.0 flags only clearly unsharp frames on the tiled scorer.
pub const SOFT_THRESHOLD_DEFAULT: f64 = 15.0;

/// A facet the UI can offer as a filter chip. `key` is stable; `label` is for display.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Facet {
    pub key: String,
    pub label: String,
}

/// `(key, label, SQL predicate over the `photos` alias `p`)`. The predicate is a
/// boolean expression ANDed into `list_photos` when the facet is active.
/// Only the static (non-threshold) facets go here; `soft` is handled dynamically.
const FACETS: &[(&str, &str, &str)] = &[
    (
        "has-gps",
        "Has GPS",
        "p.gps_latitude IS NOT NULL AND p.gps_longitude IS NOT NULL",
    ),
    (
        "mobile",
        "Shot on mobile",
        "(LOWER(p.camera_make) IN \
           ('apple','samsung','google','xiaomi','oneplus','huawei','oppo','vivo','motorola') \
          OR p.camera_model LIKE 'iPhone%' OR p.camera_model LIKE 'iPad%' \
          OR p.camera_model LIKE 'Pixel%')",
    ),
    (
        // Screenshots (and saved graphics) carry no camera EXIF and are PNG — real photos
        // never are. Confirmed against the iPhone/Android camera-roll dump: PNG-with-no-camera
        // files cluster on exact phone screen resolutions (1242x2688, 1290x2796, 750x1334…).
        "screenshot",
        "Screenshot",
        "((p.camera_make IS NULL OR p.camera_make = '') \
          AND (p.camera_model IS NULL OR p.camera_model = '') \
          AND LOWER(p.extension) = 'png')",
    ),
    (
        "drone",
        "Drone",
        // GLOB is case-sensitive in SQLite, so UPPER() the model first (DJI camera
        // model codes look like FC3411). LIKE arms are ASCII so case-fold already.
        "(LOWER(p.camera_make) LIKE '%dji%' OR LOWER(p.camera_make) LIKE '%parrot%' \
          OR LOWER(p.camera_make) LIKE '%autel%' OR UPPER(p.camera_model) GLOB 'FC[0-9]*')",
    ),
];

impl Catalog {
    /// The facets the UI can offer (for rendering filter chips). The static EXIF-derived
    /// facets are always present; the `soft` sharpness facet is included when at least one
    /// scored photo exists; the `soft-in-burst` and `sharpest-of-burst` facets are included
    /// when at least one burst analysis has been run; one "Published: <platform>" facet is
    /// appended per platform that has at least one publication (keyed `published:<platform>`,
    /// handled specially in `list_photos`).
    pub fn available_facets(&self) -> Vec<Facet> {
        let mut out: Vec<Facet> = FACETS
            .iter()
            .map(|(key, label, _)| Facet {
                key: (*key).to_string(),
                label: (*label).to_string(),
            })
            .collect();

        // `soft` facet: only show when at least one photo has been scored. This avoids
        // cluttering the filter bar before any sharpness indexing has run.
        let has_scored: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM photos WHERE sharpness IS NOT NULL LIMIT 1)",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if has_scored {
            out.push(Facet {
                key: "soft".to_string(),
                label: "Soft (out of focus)".to_string(),
            });
        }

        // `soft-in-burst` and `sharpest-of-burst` facets: only show when at least one
        // burst analysis has been run. Both facets are predicated on `burst_flag`.
        let has_burst: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM photos WHERE burst_flag IS NOT NULL LIMIT 1)",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if has_burst {
            out.push(Facet {
                key: "soft-in-burst".to_string(),
                label: "Soft in burst".to_string(),
            });
            out.push(Facet {
                key: "sharpest-of-burst".to_string(),
                label: "Sharpest of burst".to_string(),
            });
        }

        if let Ok(platforms) = self.published_platforms() {
            for p in platforms {
                out.push(Facet {
                    key: format!("published:{p}"),
                    label: format!("Published: {}", titlecase(&p)),
                });
            }
        }
        out
    }

    /// Read the soft-threshold from settings, falling back to the conservative default.
    pub fn soft_threshold(&self) -> f64 {
        self.get_setting(SOFT_THRESHOLD_KEY)
            .ok()
            .flatten()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|&v| v.is_finite() && v > 0.0)
            .unwrap_or(SOFT_THRESHOLD_DEFAULT)
    }

    /// The SQL predicate for a facet key, or an error for an unknown key.
    /// Returns a heap-allocated `String` for the dynamic `soft` predicate;
    /// callers must push it into the WHERE clause rather than embedding a `&'static str`.
    pub(crate) fn facet_predicate_owned(&self, key: &str) -> Result<String> {
        if key == "soft" {
            let t = self.soft_threshold();
            return Ok(format!(
                "p.sharpness IS NOT NULL AND p.sharpness < {t}"
            ));
        }
        if key == "soft-in-burst" {
            return Ok("p.burst_flag = 'soft-in-burst'".to_string());
        }
        if key == "sharpest-of-burst" {
            return Ok("p.burst_flag = 'sharpest-of-burst'".to_string());
        }
        FACETS
            .iter()
            .find(|(k, _, _)| *k == key)
            .map(|(_, _, pred)| (*pred).to_string())
            .ok_or_else(|| CatalogError::Validation(format!("unknown facet: {key}")))
    }
}

/// Capitalize the first letter for a display label ("instagram" → "Instagram").
fn titlecase(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
