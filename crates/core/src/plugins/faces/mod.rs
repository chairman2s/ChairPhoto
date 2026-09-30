//! Face-tagging backend — the faces plugin's Rust side. Compiled only with the `faces`
//! Cargo feature (a build without it carries no ONNX/face code). See docs/face-tagging.md.
//!
//! Sub-modules:
//!
//! - [`models`] — the model manager: download YuNet + AuraFace into `<app_data_dir>/models/
//!   faces/` with pinned SHA-256 verification, and report a clean "models missing" state
//!   instead of panicking.
//! - [`engine`] — detect faces (YuNet) and embed them (AuraFace) — the detect/embed core.
//! - [`store`]  — `faces__faces` table + `faces__queue` (resumable indexing queue). Lazy
//!   `ensure_schema` following the `plugins/ai` and `plugins/map` patterns. Embedding
//!   BLOB codec (Vec<f32> ↔ f32-LE BLOB). No model dependency — testable without ONNX.
//! - [`indexer`] — async-compatible blocking indexing job: populate queue, drain with
//!   bounded parallelism, emit `faces:progress` events, abort-safe. Detection/embedding
//!   injected via closure so the test suite runs without models.
//! - [`matcher`] — the seed / match / cluster engine (H13c): auto-seed 1-face+1-person
//!   photos, per-person centroids, Hungarian assignment for N-faces/M-tags photos,
//!   nearest-centroid open matching, incremental greedy clustering for the rest, plus the
//!   accept/reject/ignore/assign/name-cluster mutations. Pure over embeddings + SQLite,
//!   with a hand-rolled Kuhn–Munkres — fully testable on synthetic embeddings, no models.
//! - [`regions`] — MWG Regions sidecar write/read wiring (H13f): export confirmed faces as
//!   merge-safe `mwg-rs:Regions` (via `crate::xmp`) on confirm/unconfirm, and import existing
//!   foreign regions as confirmed faces (IoU-match to detections, `source='xmp'`). Pure over
//!   SQLite — no model dependency.

pub mod engine;
pub mod indexer;
pub mod matcher;
pub mod models;
pub mod regions;
pub mod store;

pub use engine::{cosine, detect_faces, detect_faces_rgb, embed_face, DetectedFace, EngineError};
pub use indexer::{load_force_cpu, FacesProgress, IndexedFace, FORCE_CPU_SETTING};
pub use matcher::{
    hungarian_min_cost, run_matching, MatchOutcome, MatchSettings, PEOPLE_ROOT_DEFAULT,
    PEOPLE_ROOT_SETTING, THRESHOLD_DEFAULT, THRESHOLD_SETTING,
};
pub use models::{ModelReport, ModelStatus};
pub use store::{blob_to_embedding, embedding_to_blob};
