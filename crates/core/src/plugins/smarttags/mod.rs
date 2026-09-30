//! Smart Tagging backend — local CLIP image embeddings → kNN tag suggestions (docs/ai-tagging.md,
//! Smart Tagging section; ROADMAP H7). Compiled only with the `smarttags` Cargo feature, so a
//! build without it carries no ONNX/CLIP code (`--no-default-features` stays green).
//!
//! Distinct from the `ai` (VLM) plugin: different technology (embeddings vs a vision LLM),
//! independently optional, and it owns its own `smarttags__*` tables and settings.
//!
//! Sub-modules:
//!
//! - [`models`] — the CLIP model manager: resolve the effective model path (`smarttags.model_path`
//!   override, else the pinned default under `<app_data_dir>/models/smarttags/`), download the
//!   default on first use with SHA-256 verification, and report a clean "model missing" state
//!   instead of panicking. Mirrors `plugins/faces/models.rs`. (H7a)
//! - [`embed`] — [`embed::encode_jpeg`]: JPEG bytes → resize 224×224 → CLIP-normalize → ONNX
//!   session (shared `ort` runtime, session pool + CUDA-with-CPU-fallback, both reused from the
//!   faces engine patterns) → L2-normalized embedding. A missing model degrades gracefully to a
//!   typed error; the pure preprocessing/normalize helpers are model-free and unit-tested. (H7a)
//! - [`store`] — `smarttags__embeddings` table (photo_id PK, vector BLOB, indexed_at): lazy
//!   `ensure_schema`, implicit queue via LEFT JOIN, f32-LE BLOB codec, upsert helper. (H7b)
//! - [`indexer`] — resumable background embedding-index job: Phase-B pattern (resolve → parallel
//!   preview-load+embed → sequential upsert), `smarttags:progress {done,total}` events,
//!   abort-safe. (H7b)
//! - [`suggest`] — brute-force cosine kNN scan over `smarttags__embeddings` → ranked tag
//!   suggestions into `smarttags__suggestions` (mirroring `ai__suggestions`); accept/reject
//!   state machine; ancestor-tag dedup. (H7c)
//! - [`classifier`] — per-tag logistic-regression binary classifiers trained over the cached
//!   CLIP embeddings (hand-rolled gradient descent, no new crate); weights in
//!   `smarttags__classifiers`; scores blended with kNN (weight scales with sample count). (H7e)

pub mod classifier;
pub mod embed;
pub mod indexer;
pub mod models;
pub mod store;
pub mod suggest;

pub use classifier::{
    ensure_schema as ensure_classifiers_schema, load_classifier, score_blended, train_all,
    Classifier, TrainOutcome, MIN_TRAIN_SAMPLES_DEFAULT, MIN_TRAIN_SAMPLES_KEY,
};
pub use embed::{cosine, encode_jpeg, l2_normalize, EmbedError};
pub use models::{ModelError, ModelReport, ModelStatus, MODEL_PATH_SETTING};
pub use suggest::{
    ensure_suggestions_schema, load_pending_suggestions, set_suggestion_state, suggest_tags,
    StoredSuggestion,
};

/// Delete the entire Smart Tagging index: embeddings, suggestions, and classifiers.
/// Called when the user wants to clean slate (e.g. after changing the model).
/// Idempotent — safe to call when the tables don't exist.
pub fn delete_index(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    // These are plugin-owned tables, so DROP is safe. If they don't exist, the
    // IF EXISTS clause makes it a no-op.
    conn.execute("DROP TABLE IF EXISTS smarttags__embeddings", [])?;
    conn.execute("DROP TABLE IF EXISTS smarttags__suggestions", [])?;
    conn.execute("DROP TABLE IF EXISTS smarttags__classifiers", [])?;
    Ok(())
}
