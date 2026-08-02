//! The `.chairphoto` **bundle** format — the transport unit for the laptop ⇄ desktop
//! additive merge (Epic F). A bundle packages **one import batch** so the desktop can
//! absorb a trip's photos, ratings, edits, and any new taxonomy without ever deleting
//! or overwriting anything it already has. See `docs/storage-and-import.md`
//! ("Catalog topology & merge" / "Export").
//!
//! This module (F1a) is the **contract only**: the on-disk layout constants plus the
//! serde manifest model that the writer (F1b), merge engine (F1c), and importer (F1d)
//! all build on. There are no commands, no file IO, and no catalog access here — just
//! plain data + JSON (de)serialization, with round-trip tests to freeze the shape.
//!
//! F1b: [`writer`] — gather catalog data and serialize a batch to a bundle zip.
//!
//! ## On-disk layout (a zip archive)
//!
//! A bundle is a single zip archive (conventionally named `<something>.chairphoto`):
//!
//! ```text
//! bundle.chairphoto  (zip)
//! ├── manifest.json              # the [`BundleManifest`] below (the catalog metadata)
//! ├── originals/                 # the RAW/JPEG originals, keyed by their logical path
//! │   ├── 2026/06/28/DSC01234.ARW
//! │   ├── 2026/06/28/DSC01234.ARW.xmp   # each original's XMP sidecar sits beside it
//! │   └── …
//! └── previews/                  # cached JPEG previews, so the catalog is instantly
//!     ├── <uuid>.jpg             # browsable on import even before originals land
//!     └── …
//! ```
//!
//! - **`originals/`** mirrors each photo's catalog-root-relative logical path
//!   ([`BundlePhoto::relative_path`]) so the importer can drop them under the desktop
//!   root reusing the ingest collision rules. Sidecars travel **beside** their original
//!   (the `<original>.xmp` convention), carrying the photo UUID for the by-UUID match.
//! - **`previews/`** is keyed by photo UUID (stable, path-independent) and is optional:
//!   a metadata-only bundle may omit it, and a reader must tolerate a missing preview.
//!
//! Identity is always the **UUID**, never a path: photos are keyed by `photos.uuid`,
//! tags by `tags.uuid`, the batch by `import_batches.uuid`. Paths are only hints for
//! where to place bytes; the merge matches on UUID.

pub mod importer;
pub mod writer;

use serde::{Deserialize, Serialize};

use crate::catalog::{IptcFields, PickState};

/// The extension for a bundle archive (`foo.chairphoto`). Note the *catalog* file uses
/// the same extension for a different thing; a bundle is distinguished by being a zip
/// that contains [`MANIFEST_FILENAME`].
pub const BUNDLE_EXTENSION: &str = "chairphoto";

/// The manifest entry name inside the zip archive.
pub const MANIFEST_FILENAME: &str = "manifest.json";

/// The archive directory holding the originals (+ their `.xmp` sidecars), mirroring
/// each photo's catalog-root-relative logical path.
pub const ORIGINALS_DIR: &str = "originals";

/// The archive directory holding cached JPEG previews, keyed by photo UUID.
pub const PREVIEWS_DIR: &str = "previews";

/// The bundle format version. Bump when the manifest shape changes incompatibly; a
/// reader must refuse a `format_version` it does not understand. `1` = the initial
/// additive-merge format.
pub const BUNDLE_FORMAT_VERSION: u32 = 1;

/// The top-level bundle manifest (`manifest.json`). One bundle carries exactly one
/// import batch, its photos, and the slice of the tag taxonomy those photos reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleManifest {
    /// The on-disk contract version. See [`BUNDLE_FORMAT_VERSION`].
    pub format_version: u32,
    /// The chairphoto version that produced the bundle (informational; e.g. `"0.3.1"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    /// Unix seconds when the bundle was written (distinct from the batch's own
    /// `created_at`, which is the original ingest time).
    pub created_at: i64,
    /// The single import batch this bundle represents.
    pub batch: BundleBatch,
    /// The photos in the batch, keyed by [`BundlePhoto::uuid`].
    #[serde(default)]
    pub photos: Vec<BundlePhoto>,
    /// The subtree of the tag taxonomy referenced by any photo's assignments, so the
    /// importer can create missing tags/terms before unioning assignments.
    #[serde(default)]
    pub taxonomy: Vec<BundleTag>,
}

impl BundleManifest {
    /// A fresh manifest for `batch`, stamped with the current format version and
    /// `created_at`, and no photos/taxonomy yet.
    pub fn new(batch: BundleBatch, created_at: i64) -> Self {
        Self {
            format_version: BUNDLE_FORMAT_VERSION,
            app_version: None,
            created_at,
            batch,
            photos: Vec::new(),
            taxonomy: Vec::new(),
        }
    }

    /// Serialize the manifest to pretty JSON (what lands as `manifest.json`).
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Parse a manifest from JSON bytes/text.
    pub fn from_json(s: &str) -> serde_json::Result<Self> {
        serde_json::from_str(s)
    }

    /// Find a photo entry by its UUID.
    pub fn photo(&self, uuid: &str) -> Option<&BundlePhoto> {
        self.photos.iter().find(|p| p.uuid == uuid)
    }

    /// Find a taxonomy entry by its tag UUID.
    pub fn tag(&self, uuid: &str) -> Option<&BundleTag> {
        self.taxonomy.iter().find(|t| t.uuid == uuid)
    }
}

/// The import batch record (`import_batches`) carried by the bundle. Identity is the
/// `uuid`; the merge inserts it idempotently (a re-import of the same batch is a no-op).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleBatch {
    /// `import_batches.uuid` — the permanent batch identity ("negative film roll").
    pub uuid: String,
    /// Human hint about the ingest (e.g. the source folder / import name).
    #[serde(default)]
    pub source_label: String,
    /// Free-text note on the batch.
    #[serde(default)]
    pub note: String,
    /// Unix seconds of the original ingest.
    pub created_at: i64,
}

/// One photo in the bundle, keyed by its `photos.uuid`. Carries the non-destructive
/// state that merges additively (matched by UUID; existing rows are never touched).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundlePhoto {
    /// `photos.uuid` — the stable identity used for the by-UUID merge match.
    pub uuid: String,
    /// Catalog-root-relative logical path. Also the path under `originals/` in the
    /// archive (the sidecar, if any, sits at `<relative_path>.xmp`).
    pub relative_path: String,
    /// 0–5 star rating.
    #[serde(default)]
    pub rating: i64,
    /// Color label string (empty = none).
    #[serde(default)]
    pub label: String,
    /// Culling pick state.
    #[serde(default = "default_pick_state")]
    pub pick_state: PickState,
    /// User-authored IPTC Core fields (only applied to brand-new photos on merge).
    #[serde(default)]
    pub iptc: IptcFields,
    /// The photo-level opaque edit record (`photo_edits.edit_json`), if any. Opaque to
    /// core — the editing module owns its meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_record: Option<String>,
    /// Named non-destructive versions (`photo_versions`), each with its opaque
    /// `edit_json`. Ordered by `position`.
    #[serde(default)]
    pub versions: Vec<BundleVersion>,
    /// Tag assignments **by tag UUID** (references entries in [`BundleManifest::taxonomy`]).
    /// Using UUIDs (not paths) keeps assignments stable across taxonomy renames.
    #[serde(default)]
    pub tag_uuids: Vec<String>,
}

/// A named non-destructive version of a photo (`photo_versions`). No catalog-local `id`
/// travels — identity within a photo is `(name, position)`; `edit_json` is opaque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleVersion {
    pub name: String,
    pub edit_json: String,
    #[serde(default)]
    pub position: i64,
}

/// A node of the tag taxonomy referenced by the bundle. The merge unions by `uuid`
/// first, then by normalized `full_path`, creating missing tags/terms as needed. Only
/// the identity + interop labels travel — catalog-local ids never do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleTag {
    /// `tags.uuid` — the stable tag identity referenced by [`BundlePhoto::tag_uuids`].
    pub uuid: String,
    /// Language-neutral hierarchical path (e.g. `"Birds/Owls"`) — the fallback match key.
    pub full_path: String,
    /// The tag-level `tags.exportable` flag. `false` marks an **organizational** tag
    /// (e.g. a darktable `_`-prefixed grouping) that must never be emitted as an export
    /// keyword or hierarchical-path segment. Distinct from the per-term
    /// [`BundleTagTerm::export`]: this gates the whole tag, that gates one label. Carried
    /// so the additive merge (F1c) preserves it instead of falling back to the schema
    /// default (`1`/true), which would silently turn an organizational tag exportable.
    /// Defaults to `true` to match the DB default for bundles written before this field.
    #[serde(default = "default_true")]
    pub exportable: bool,
    /// Terms layered on the tag: translations (primary) and synonyms (non-primary),
    /// each with its own `export` flag. Covers terms/synonyms.
    #[serde(default)]
    pub terms: Vec<BundleTagTerm>,
}

/// A term (translation or synonym) on a [`BundleTag`], mirroring `tag_terms`. A
/// `is_primary` term is the canonical name for its language (a translation); a
/// non-primary term is a synonym. `export` = whether the term is emitted on export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleTagTerm {
    pub text: String,
    /// BCP-47-ish language code, or `None` for language-neutral.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default)]
    pub is_primary: bool,
    #[serde(default)]
    pub export: bool,
}

fn default_pick_state() -> PickState {
    PickState::None
}

/// Default for [`BundleTag::exportable`] — mirrors the `tags.exportable` DB default (`1`).
fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> BundleManifest {
        let mut m = BundleManifest::new(
            BundleBatch {
                uuid: "batch-uuid-1".into(),
                source_label: "Trip 2026-06".into(),
                note: "island hopping".into(),
                created_at: 1_719_000_000,
            },
            1_719_500_000,
        );
        m.app_version = Some("0.3.1".into());
        m.photos.push(BundlePhoto {
            uuid: "photo-uuid-a".into(),
            relative_path: "2026/06/28/DSC01234.ARW".into(),
            rating: 4,
            label: "green".into(),
            pick_state: PickState::Pick,
            iptc: IptcFields {
                headline: "Sunset".into(),
                city: "Bergen".into(),
                ..Default::default()
            },
            edit_record: Some(r#"{"basic-editor":{"exposure":0.3}}"#.into()),
            versions: vec![BundleVersion {
                name: "Instagram crop".into(),
                edit_json: r#"{"crop":"1:1"}"#.into(),
                position: 0,
            }],
            tag_uuids: vec!["tag-uuid-owl".into()],
        });
        // A photo exercising the defaults (no edits, no versions, no tags, empty iptc).
        m.photos.push(BundlePhoto {
            uuid: "photo-uuid-b".into(),
            relative_path: "2026/06/29/DSC01300.ARW".into(),
            rating: 0,
            label: String::new(),
            pick_state: PickState::None,
            iptc: IptcFields::default(),
            edit_record: None,
            versions: Vec::new(),
            tag_uuids: Vec::new(),
        });
        m.taxonomy.push(BundleTag {
            uuid: "tag-uuid-owl".into(),
            full_path: "Birds/Owls".into(),
            exportable: true,
            terms: vec![
                BundleTagTerm {
                    text: "Ugle".into(),
                    language: Some("nb".into()),
                    is_primary: true,
                    export: true,
                },
                BundleTagTerm {
                    text: "Owl".into(),
                    language: Some("en".into()),
                    is_primary: true,
                    export: true,
                },
            ],
        });
        // An organizational (non-exportable) grouping tag — must travel as such so the
        // merge doesn't turn it exportable.
        m.taxonomy.push(BundleTag {
            uuid: "tag-uuid-org".into(),
            full_path: "_Workflow".into(),
            exportable: false,
            terms: Vec::new(),
        });
        m
    }

    #[test]
    fn manifest_round_trips_through_json() {
        let m = sample_manifest();
        let json = m.to_json().expect("serialize");
        let back = BundleManifest::from_json(&json).expect("deserialize");
        assert_eq!(m, back);
    }

    #[test]
    fn new_manifest_stamps_current_format_version() {
        let m = BundleManifest::new(
            BundleBatch {
                uuid: "b".into(),
                source_label: String::new(),
                note: String::new(),
                created_at: 0,
            },
            0,
        );
        assert_eq!(m.format_version, BUNDLE_FORMAT_VERSION);
        assert!(m.photos.is_empty());
        assert!(m.taxonomy.is_empty());
    }

    #[test]
    fn manifest_uses_camel_case_and_expected_keys() {
        let json = sample_manifest().to_json().unwrap();
        assert!(json.contains("\"formatVersion\""));
        assert!(json.contains("\"createdAt\""));
        assert!(json.contains("\"sourceLabel\""));
        assert!(json.contains("\"relativePath\""));
        assert!(json.contains("\"pickState\""));
        assert!(json.contains("\"tagUuids\""));
        assert!(json.contains("\"fullPath\""));
        assert!(json.contains("\"exportable\""));
        // pick state serializes lowercase (matches the catalog convention).
        assert!(json.contains("\"pick\""));
    }

    #[test]
    fn tag_exportable_flag_round_trips() {
        let m = sample_manifest();
        let back = BundleManifest::from_json(&m.to_json().unwrap()).unwrap();
        assert_eq!(back, m);
        // The exportable/organizational distinction survives the round trip.
        assert!(back.tag("tag-uuid-owl").unwrap().exportable);
        assert!(!back.tag("tag-uuid-org").unwrap().exportable);
    }

    #[test]
    fn tag_exportable_defaults_true_for_older_bundles() {
        // A bundle written before the `exportable` field must default to true (the DB
        // default), never silently non-exportable.
        let json = r#"{
            "formatVersion": 1,
            "createdAt": 123,
            "batch": { "uuid": "b", "createdAt": 1 },
            "taxonomy": [ { "uuid": "t", "fullPath": "Birds" } ]
        }"#;
        let m = BundleManifest::from_json(json).expect("deserialize");
        assert!(m.tag("t").unwrap().exportable);
    }

    #[test]
    fn omitted_optional_fields_parse_to_defaults() {
        // A minimal manifest: only the required fields present. Everything optional
        // must fall back to its default so older/smaller bundles keep loading.
        let json = r#"{
            "formatVersion": 1,
            "createdAt": 123,
            "batch": { "uuid": "b", "createdAt": 1 }
        }"#;
        let m = BundleManifest::from_json(json).expect("deserialize minimal");
        assert_eq!(m.format_version, 1);
        assert_eq!(m.app_version, None);
        assert_eq!(m.batch.source_label, "");
        assert!(m.photos.is_empty());
        assert!(m.taxonomy.is_empty());

        // A photo with only its identity + path: optional state uses defaults.
        let json = r#"{
            "formatVersion": 1,
            "createdAt": 123,
            "batch": { "uuid": "b", "createdAt": 1 },
            "photos": [ { "uuid": "p", "relativePath": "a/b.arw" } ]
        }"#;
        let m = BundleManifest::from_json(json).expect("deserialize");
        let p = m.photo("p").expect("photo present");
        assert_eq!(p.rating, 0);
        assert_eq!(p.pick_state, PickState::None);
        assert_eq!(p.edit_record, None);
        assert!(p.versions.is_empty());
        assert!(p.tag_uuids.is_empty());
        assert_eq!(p.iptc, IptcFields::default());
    }

    #[test]
    fn lookups_find_by_uuid() {
        let m = sample_manifest();
        assert_eq!(m.photo("photo-uuid-a").unwrap().rating, 4);
        assert!(m.photo("missing").is_none());
        assert_eq!(m.tag("tag-uuid-owl").unwrap().full_path, "Birds/Owls");
        assert!(m.tag("missing").is_none());
    }
}
