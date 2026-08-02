//! Serde models returned to the frontend. Field names are camelCase to match
//! the TypeScript interfaces in `src/modules/registry.ts`.

use serde::{Deserialize, Serialize};

/// Pick state for culling. Stored in the DB as the lowercase string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PickState {
    None,
    Pick,
    Reject,
}

impl PickState {
    pub fn as_db_str(self) -> &'static str {
        match self {
            PickState::None => "none",
            PickState::Pick => "pick",
            PickState::Reject => "reject",
        }
    }

    pub fn from_db_str(value: &str) -> Self {
        match value {
            "pick" => PickState::Pick,
            "reject" => PickState::Reject,
            _ => PickState::None,
        }
    }
}

/// A photo row as seen by the frontend. `path` is relative to the catalog root.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Photo {
    pub id: i64,
    pub uuid: String,
    pub path: String,
    pub rating: i64,
    pub label: String,
    pub pick_state: PickState,
    pub capture_time: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub camera_model: Option<String>,
    pub lens: Option<String>,
    pub aperture: Option<f64>,
    pub shutter_speed: Option<String>,
    pub iso: Option<i64>,
    /// Comma-joined editor names detected from sidecars (e.g. "darktable").
    pub external_editors: String,
    pub thumbnail_path: Option<String>,
    /// Number of photos stacked under this one (e.g. a camera JPEG under its RAW). 0 = none.
    pub stack_count: i64,
    /// The master this photo is stacked under (e.g. a derivative's original), or None.
    pub stack_parent_id: Option<i64>,
    /// Phase A/B boundary for two-phase live scan (I6a): 1 = metadata ready for display;
    /// 0 = awaiting metadata extraction (EXIF/IPTC/XMP, thumbnails).
    pub metadata_ready: i64,
    /// Tiled Laplacian-variance sharpness score from the ~1024–2048px preview (H16b).
    /// `None` = not yet scored by the background indexer. Higher = sharper somewhere.
    pub sharpness: Option<f64>,
    /// How the sharpness score was computed: `"tile"` / `"face"` / `"afpoint"` (H16c).
    /// `None` when `sharpness` is `None`.
    pub sharpness_method: Option<String>,
    /// Burst-relative sharpness flag (H16e): `None` = not yet analysed (or single-photo
    /// cluster), `"soft-in-burst"` = below ~60% of cluster median, `"sharpest-of-burst"` =
    /// sharpest frame in its cluster. Set only by `analyze_burst_sharpness`.
    pub burst_flag: Option<String>,
}

/// Lightweight row used by the burst analysis engine to avoid fetching full Photo rows.
/// Not serialized — used internally between `burst_inputs` and the analysis command.
#[derive(Debug, Clone)]
pub struct BurstInput {
    pub id: i64,
    pub capture_time: Option<String>,
    pub phash: Option<u64>,
    pub rating: i64,
    pub sharpness: Option<f64>,
}

/// A node in the hierarchical tag tree.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tag {
    pub id: i64,
    /// Stable identity (merge taxonomies by uuid, not name). Populated for all tags.
    pub uuid: String,
    pub name: String,
    pub full_path: String,
    pub parent_id: Option<i64>,
    /// Internal description of the tag's meaning (not exported to image sidecars).
    pub description: String,
    /// Non-null = an auto-tag (system-managed membership by this rule, e.g. "monochrome").
    pub auto_rule: Option<String>,
    /// Private/sensitive: withheld from external/cloud AI providers (people's names etc.);
    /// the local model still receives it. Does not affect export or display.
    pub private: bool,
}

/// A user-defined tag group (a named set of tags for fast tagging).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagGroup {
    pub id: i64,
    pub name: String,
}

/// A deferred storage operation in the reconcile queue (E4): runs when the NAS is
/// reachable. `kind` is "backup" | "offload" | "restore"; `status` is "pending" |
/// "failed" (failed keeps the last `error`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingOperation {
    pub id: i64,
    pub kind: String,
    pub photo_id: i64,
    pub status: String,
    pub error: String,
    pub created_at: i64,
}

/// The keyword set to emit for a photo on export (see docs/taxonomy.md). `flat` is
/// the de-duplicated list of export labels for every assigned tag **and its ancestors**
/// (the "contained keywords" expansion), in the selected languages; `hierarchical` is
/// each assigned tag's translated path joined with `|` (Lightroom `lr:hierarchicalSubject`).
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportKeywords {
    pub flat: Vec<String>,
    pub hierarchical: Vec<String>,
}

/// An import batch ("negative film roll"): every ingest creates one, auto and
/// immutable. `photo_count` is the number of (non-missing) photos that belong to it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportBatch {
    pub id: i64,
    pub uuid: String,
    pub source_label: String,
    pub note: String,
    pub created_at: i64,
    pub photo_count: i64,
}

/// A manual album: a user-curated, ordered collection of photos. `photo_count` is
/// the number of (non-missing) photos currently in it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub note: String,
    pub photo_count: i64,
}

/// A smart album: a saved, named RULE that resolves to a photo set (the dynamic
/// counterpart to a manual `Album`). `rule_json` is the opaque Rule-JSON contract
/// (see docs/smart-albums.md), interpreted by `rule_to_sql`. `photo_count` is computed
/// LIVE — there is no membership table — by running the translated query as a COUNT.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmartAlbum {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub rule_json: String,
    pub photo_count: i64,
}

/// A named, non-destructive version of a photo (a crop/exposure variant). `edit_json`
/// is opaque to core — the editing module interprets it. See docs/editing.md.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhotoVersion {
    pub id: i64,
    pub photo_id: i64,
    pub name: String,
    pub edit_json: String,
    pub position: i64,
}

/// A record that a photo (in a specific version) was published to a platform. `platform`
/// is the marker declared by the publishing module (e.g. "instagram"); `version_id` is
/// `None` for the Original, and `version_name` is a snapshot kept so the record survives
/// the version being deleted. See docs/publications.md.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Publication {
    pub id: i64,
    pub photo_id: i64,
    pub version_id: Option<i64>,
    pub version_name: Option<String>,
    pub platform: String,
    pub url: Option<String>,
    pub published_at: i64,
}

/// A tag plus the number of photos tagged with it (including descendants).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagWithCount {
    #[serde(flatten)]
    pub tag: Tag,
    pub photo_count: i64,
}

/// One metadata entry (EXIF/IPTC/XMP field) for the metadata panel.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataEntry {
    pub key: String,
    pub group_name: String,
    pub value: String,
}

/// User-authored IPTC Core fields (distinct from extracted EXIF — these survive
/// rescans and are written to the XMP sidecar). Field set and XMP mappings follow
/// the IPTC Photo Metadata standard (iptc.org). Empty string = unset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IptcFields {
    pub description: String,  // dc:description (Lang Alt)
    pub headline: String,     // photoshop:Headline
    pub title: String,        // dc:title (Lang Alt)
    pub creator: String,      // dc:creator (Seq ProperName)
    pub copyright: String,    // dc:rights (Lang Alt)
    pub credit: String,       // photoshop:Credit
    pub source: String,       // photoshop:Source
    pub city: String,         // photoshop:City
    pub state: String,        // photoshop:State
    pub country: String,      // photoshop:Country
    pub country_code: String, // Iptc4xmpCore:CountryCode
}

/// The hot, indexed metadata fields promoted to columns on `photos` for fast
/// filtering/sorting and smart albums. Everything else lives in `photo_metadata`.
#[derive(Debug, Clone, Default)]
pub struct PromotedMetadata {
    pub capture_time: Option<String>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub lens: Option<String>,
    pub focal_length: Option<f64>,
    pub aperture: Option<f64>,
    pub shutter_speed: Option<String>,
    pub iso: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub gps_latitude: Option<f64>,
    pub gps_longitude: Option<f64>,
}

/// A label attached to a tag for display, translation, and export.
///
/// - `language = None` means language-neutral.
/// - `is_primary = true` marks the canonical name *for that language* (a translation);
///   non-primary terms are synonyms.
/// - `export` controls whether this term is emitted when exporting tags.
///
/// The tag's own `name`/`full_path` remain the internal, language-neutral identity;
/// terms are the human/interop labels layered on top.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagTerm {
    pub id: i64,
    pub tag_id: i64,
    pub text: String,
    pub language: Option<String>,
    pub is_primary: bool,
    pub export: bool,
}

/// The role a physical copy plays for a photo. See `docs/storage-and-import.md`.
/// Read resolution prefers copies in `priority()` order (lowest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LocationRole {
    /// Fast local working copy — preferred for reads.
    LocalCache,
    /// The canonical copy (e.g. on the NAS, or the in-place original).
    Primary,
    /// A redundant safety copy.
    Backup,
    /// An outbound copy (laptop / hand-off). Never read from for normal use.
    Export,
}

impl LocationRole {
    pub fn as_db_str(self) -> &'static str {
        match self {
            LocationRole::LocalCache => "local_cache",
            LocationRole::Primary => "primary",
            LocationRole::Backup => "backup",
            LocationRole::Export => "export",
        }
    }

    pub fn from_db_str(value: &str) -> Self {
        match value {
            "local_cache" => LocationRole::LocalCache,
            "backup" => LocationRole::Backup,
            "export" => LocationRole::Export,
            _ => LocationRole::Primary,
        }
    }

    /// Read-preference order: lower is preferred. Fast local copy first.
    pub fn priority(self) -> u8 {
        match self {
            LocationRole::LocalCache => 0,
            LocationRole::Primary => 1,
            LocationRole::Backup => 2,
            LocationRole::Export => 3,
        }
    }
}

/// What a volume is: a fast local working disk, or a backup (NAS/remote) target.
/// See `docs/storage-and-import.md` (Volume kinds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VolumeKind {
    Local,
    Backup,
}

impl VolumeKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            VolumeKind::Local => "local",
            VolumeKind::Backup => "backup",
        }
    }

    pub fn from_db_str(value: &str) -> Self {
        match value {
            "backup" => VolumeKind::Backup,
            _ => VolumeKind::Local,
        }
    }
}

/// A photo's storage status, derived from where its copies live (its locations'
/// volume kinds + reachability). See `docs/storage-and-import.md` (per-photo status).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StorageStatus {
    /// Only copy is on a local volume — at risk, not backed up.
    LocalOnly,
    /// A local copy plus a backup copy exists (safe).
    BackedUp,
    /// Backup-only, the backup volume is reachable (browse via cached preview).
    Archived,
    /// Backup-only and the backup volume is currently unreachable.
    Offline,
    /// No copy recorded on any volume.
    Missing,
}

/// A named storage location with a per-machine base path. Photo paths are stored
/// relative to a volume, so remapping a NAS mount point is a one-line change and
/// catalogs stay portable between machines.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    pub id: i64,
    pub uuid: String,
    pub name: String,
    pub base_path: String,
    /// `local` (working disk) or `backup` (NAS/remote).
    pub kind: VolumeKind,
    /// Whether the volume's base path is currently present on this machine.
    pub reachable: bool,
}

/// One physical copy of a photo, on a volume, at a path relative to that volume.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhotoLocation {
    pub id: i64,
    pub photo_id: i64,
    pub volume_id: i64,
    pub relative_path: String,
    pub role: LocationRole,
}
