//! Per-tag logistic-regression binary classifiers for Smart Tagging (H7e).
//!
//! For each tag that has at least `smarttags.min_train_samples` (default 10) confirmed
//! photos with a CLIP embedding, this module trains a binary logistic-regression
//! classifier (positive class = photo carries the tag, negative class = everything else
//! that has an embedding). The learned weights are stored in `smarttags__classifiers` and
//! reloaded on demand.
//!
//! ## Algorithm
//!
//! Hand-rolled mini-batch gradient descent (no new crate):
//!
//! ```text
//! σ(x) = 1 / (1 + exp(-x))
//! p_i  = σ(w · x_i + b)
//! L    = -mean(y_i log p_i + (1-y_i) log(1-p_i)) + λ‖w‖²/2   (L2 regularization)
//! ∂L/∂w = (1/n) Σ (p_i - y_i) x_i + λ w
//! ∂L/∂b = (1/n) Σ (p_i - y_i)
//! ```
//!
//! Default hyperparameters (constants at the bottom; tuned for CLIP 512-d unit vectors):
//! - `LR` = 0.1 (learning rate)
//! - `L2` = 1e-4 (L2 regularization — prevents weight explosion on sparse classes)
//! - `EPOCHS` = 200 (cheap: 512-d dot products; 200 passes < 1 s for typical catalogs)
//!
//! ## Blending
//!
//! [`score_blended`] computes the blended final confidence for a suggestion:
//!
//! ```text
//! knn_weight        = 1.0
//! classifier_weight = (sample_count / 50).clamp(0, 1)   // reaches full weight at 50 samples
//! blended           = (knn_weight * knn_score + classifier_weight * cls_score)
//!                     / (knn_weight + classifier_weight)
//! ```
//!
//! The weight is linear in `sample_count` so it scales meaningfully across the
//! real training range (≥10 samples by default, growing toward 50+):
//! - 10 samples → weight 0.20 (classifier contributes ~17% of final score)
//! - 25 samples → weight 0.50 (equal blend)
//! - 50 samples → weight 1.00 (full 50/50 blend with kNN)
//!
//! When no classifier exists for a tag the blended score equals the kNN score (weight = 0).
//!
//! ## `smarttags__classifiers` schema
//!
//! One row per tag (keyed by `full_path`):
//! - `tag_path`     TEXT PK — the tag's `full_path` string
//! - `weights`      BLOB NOT NULL — `dim × 4` bytes, f32-LE (the weight vector **w**)
//! - `bias`         REAL NOT NULL — the scalar bias **b**
//! - `sample_count` INTEGER NOT NULL — number of positive-class training examples
//! - `trained_at`   INTEGER NOT NULL — unix timestamp (for staleness detection)

use rusqlite::{Connection, OptionalExtension};

use super::store::{blob_to_embedding, embedding_to_blob};

// ── Schema ────────────────────────────────────────────────────────────────────

/// Create `smarttags__classifiers` if it does not yet exist. Idempotent.
/// Forget what was trained for tag paths that a core tag merge just removed (A5), and follow
/// pending suggestions to the path that replaced them. Returns
/// `(classifiers_dropped, suggestions_repointed)`.
///
/// Both tables key on the tag **path**, not an id, so a merge leaves them naming something
/// that no longer exists: a classifier trained for the dead path can never fire again, and a
/// pending suggestion for it can never be accepted. Dropping the classifier is right rather
/// than renaming it — the target tag's photo set just changed, so its old weights are stale
/// anyway and the next training pass rebuilds from the merged set.
///
/// Returns `Ok((0, 0))` when the tables do not exist yet (nothing indexed, nothing to clean).
pub fn forget_merged_tag_paths(
    conn: &Connection,
    dead_paths: &[String],
    target_path: &str,
) -> rusqlite::Result<(usize, usize)> {
    let mut dropped = 0usize;
    let mut repointed = 0usize;
    let classifiers = table_exists(conn, "smarttags__classifiers")?;
    let suggestions = table_exists(conn, "smarttags__suggestions")?;
    for path in dead_paths {
        if classifiers {
            dropped += conn.execute(
                "DELETE FROM smarttags__classifiers WHERE tag_path = ?1",
                [path],
            )?;
        }
        if suggestions {
            // (photo_id, path) is the PK: a photo already carrying a suggestion for the
            // target keeps it, and the redundant one for the dead path is removed.
            repointed += conn.execute(
                "UPDATE OR IGNORE smarttags__suggestions SET path = ?2 WHERE path = ?1",
                rusqlite::params![path, target_path],
            )?;
            conn.execute("DELETE FROM smarttags__suggestions WHERE path = ?1", [path])?;
        }
    }
    Ok((dropped, repointed))
}

fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
}

pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS smarttags__classifiers (
            tag_path     TEXT    PRIMARY KEY,
            weights      BLOB    NOT NULL,
            bias         REAL    NOT NULL,
            sample_count INTEGER NOT NULL,
            trained_at   INTEGER NOT NULL
        );",
    )?;
    Ok(())
}

// ── In-memory types ───────────────────────────────────────────────────────────

/// A trained logistic-regression classifier for one tag.
#[derive(Debug, Clone)]
pub struct Classifier {
    /// L2-penalized weight vector of the same dimension as the CLIP embedding.
    pub weights: Vec<f32>,
    /// Scalar bias term.
    pub bias: f32,
    /// Number of positive-class training examples used to train this model.
    pub sample_count: usize,
}

impl Classifier {
    /// Predict a probability in (0, 1) for one embedding (a unit vector of
    /// the same dimension as `self.weights`).
    pub fn predict(&self, x: &[f32]) -> f32 {
        let logit: f32 = self.weights.iter().zip(x).map(|(w, xi)| w * xi).sum::<f32>() + self.bias;
        sigmoid(logit)
    }
}

// ── Training ──────────────────────────────────────────────────────────────────

/// Hyperparameters — tuned for CLIP 512-d unit vectors.

/// Learning rate for gradient descent.
const LR: f32 = 0.1;
/// L2 regularization coefficient.
const L2: f32 = 1e-4;
/// Number of full-data passes.
const EPOCHS: usize = 200;

/// Train a logistic-regression binary classifier given positive and negative
/// embeddings. Returns `None` when `positives.is_empty()`.
///
/// - `positives` — CLIP embeddings of photos *confirmed* to carry the tag.
/// - `negatives` — CLIP embeddings of photos that do *not* carry the tag.
///
/// The returned [`Classifier`] is ready to be persisted via [`upsert_classifier`].
pub fn train(positives: &[Vec<f32>], negatives: &[Vec<f32>]) -> Option<Classifier> {
    if positives.is_empty() {
        return None;
    }
    let dim = positives[0].len();
    if dim == 0 {
        return None;
    }

    // Build the full training set: (embedding, label).
    let mut xs: Vec<&[f32]> = Vec::with_capacity(positives.len() + negatives.len());
    let mut ys: Vec<f32> = Vec::with_capacity(xs.capacity());
    for emb in positives {
        xs.push(emb);
        ys.push(1.0);
    }
    for emb in negatives {
        xs.push(emb);
        ys.push(0.0);
    }
    let n = xs.len() as f32;

    // Initialize weights to zero; bias to zero.
    let mut weights = vec![0.0f32; dim];
    let mut bias = 0.0f32;

    for _ in 0..EPOCHS {
        // Accumulate gradients over all samples.
        let mut grad_w = vec![0.0f32; dim];
        let mut grad_b = 0.0f32;

        for (&y, x) in ys.iter().zip(xs.iter()) {
            let logit: f32 = weights.iter().zip(*x).map(|(w, xi)| w * xi).sum::<f32>() + bias;
            let p = sigmoid(logit);
            let err = p - y; // ∂L/∂logit for one sample
            for (gw, xi) in grad_w.iter_mut().zip(*x) {
                *gw += err * xi;
            }
            grad_b += err;
        }

        // Average + L2 regularization on weights.
        for (gw, w) in grad_w.iter_mut().zip(weights.iter()) {
            *gw = *gw / n + L2 * w;
        }
        grad_b /= n;

        // Gradient descent step.
        for (w, gw) in weights.iter_mut().zip(grad_w.iter()) {
            *w -= LR * gw;
        }
        bias -= LR * grad_b;
    }

    Some(Classifier {
        sample_count: positives.len(),
        weights,
        bias,
    })
}

// ── Blending ──────────────────────────────────────────────────────────────────

/// Blend a kNN score with a classifier score.
///
/// The classifier's contribution scales linearly with `sample_count / 50.0`,
/// clamped to `[0.0, 1.0]`. This means:
/// - 10 samples → weight 0.20 (classifier is a minority voice)
/// - 25 samples → weight 0.50 (equal blend)
/// - 50+ samples → weight 1.00 (full 50/50 blend with kNN)
///
/// Classifiers are only stored when `sample_count >= min_train_samples` (default
/// 10), so the weight is always at least 0.20 for any stored classifier and
/// increases with more training data — satisfying the "weight scales with
/// sample_count" requirement from the spec.
///
/// When `classifier` is `None` the raw `knn_score` is returned unchanged.
pub fn score_blended(knn_score: f32, classifier: Option<&Classifier>, x: &[f32]) -> f32 {
    let Some(clf) = classifier else {
        return knn_score;
    };
    let cls_score = clf.predict(x);
    // Linear ramp: full classifier weight at 50 confirmed samples.
    let cls_weight = (clf.sample_count as f32 / 50.0).clamp(0.0, 1.0);
    if cls_weight <= 0.0 {
        return knn_score;
    }
    // kNN always has weight 1.0.
    let knn_weight = 1.0f32;
    (knn_weight * knn_score + cls_weight * cls_score) / (knn_weight + cls_weight)
}

// ── Persistence ───────────────────────────────────────────────────────────────

/// Save (or replace) a trained classifier row for `tag_path`.
pub fn upsert_classifier(
    conn: &Connection,
    tag_path: &str,
    clf: &Classifier,
) -> rusqlite::Result<()> {
    let blob = embedding_to_blob(&clf.weights);
    let now = unix_now();
    conn.execute(
        "INSERT INTO smarttags__classifiers (tag_path, weights, bias, sample_count, trained_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(tag_path) DO UPDATE SET
             weights      = excluded.weights,
             bias         = excluded.bias,
             sample_count = excluded.sample_count,
             trained_at   = excluded.trained_at",
        rusqlite::params![tag_path, blob, clf.bias as f64, clf.sample_count as i64, now],
    )?;
    Ok(())
}

/// Load the classifier for `tag_path`, or return `None` if absent.
pub fn load_classifier(
    conn: &Connection,
    tag_path: &str,
) -> Result<Option<Classifier>, String> {
    let row: Option<(Vec<u8>, f64, i64)> = conn
        .query_row(
            "SELECT weights, bias, sample_count
               FROM smarttags__classifiers
              WHERE tag_path = ?1",
            rusqlite::params![tag_path],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;

    let Some((blob, bias, sample_count)) = row else {
        return Ok(None);
    };
    let weights = blob_to_embedding(&blob)?;
    Ok(Some(Classifier {
        weights,
        bias: bias as f32,
        sample_count: sample_count as usize,
    }))
}

// ── Bulk training: `smarttags_train_classifiers` ─────────────────────────────

/// Outcome reported back to the Tauri command.
#[derive(Debug, Default)]
pub struct TrainOutcome {
    /// Tags examined (those with ≥ `min_samples` confirmed embeddings).
    pub examined: usize,
    /// Tags where the classifier was rebuilt.
    pub trained: usize,
    /// Tags skipped because their classifier is still fresh (nothing new since
    /// `trained_at`).
    pub skipped: usize,
}

/// Default minimum number of confirmed positive examples required to train a classifier.
pub const MIN_TRAIN_SAMPLES_DEFAULT: usize = 10;
/// Settings key for the minimum sample count.
pub const MIN_TRAIN_SAMPLES_KEY: &str = "smarttags.min_train_samples";

/// The outcome of [`scan_stale_tags`]: which tags need retraining, and the counters
/// that describe the tags it decided *not* to retrain.
#[derive(Debug, Clone, Default)]
pub struct StaleScan {
    /// Tags examined (those with ≥ `min_samples` confirmed CLIP embeddings).
    pub examined: usize,
    /// Tags whose classifier was still fresh, so they were not queued for training.
    pub skipped: usize,
    /// `full_path` of every tag that needs a classifier built or rebuilt.
    pub stale: Vec<String>,
}

/// One tag's training data, detached from the connection it was read through.
///
/// This is the unit that crosses the catalog-lock boundary: [`load_training_set`] fills
/// it under the lock, [`train`] consumes it with no lock held, and only the resulting
/// [`Classifier`] (a few KB) travels back to [`persist_classifiers`].
#[derive(Debug, Clone)]
pub struct TrainingSet {
    /// The tag this data trains a classifier for.
    pub tag_path: String,
    /// Embeddings of photos confirmed to carry the tag.
    pub positives: Vec<Vec<f32>>,
    /// Embeddings of photos that do not carry it, bounded to `3 × positives`.
    pub negatives: Vec<Vec<f32>>,
}

/// Find the tags that qualify for training and decide which of them are stale.
///
/// A tag "qualifies" when it has at least `min_samples` photos that both carry the tag
/// and have a CLIP embedding. Its classifier is "stale" when the row does not exist yet,
/// or when the most recent accept/reject for that tag's suggestions is newer than
/// `trained_at`.
///
/// This is the cheap half of training: it reads counts and timestamps, never embeddings,
/// so it is safe to run under the catalog lock.
pub fn scan_stale_tags(conn: &Connection, min_samples: usize) -> Result<StaleScan, String> {
    ensure_schema(conn).map_err(|e| e.to_string())?;
    super::store::ensure_schema(conn).map_err(|e| e.to_string())?;
    super::suggest::ensure_suggestions_schema(conn).map_err(|e| e.to_string())?;

    // ── 1. Find qualifying tags ───────────────────────────────────────────────
    // A tag "qualifies" when it has at least `min_samples` photos that (a) carry the
    // tag and (b) have a CLIP embedding.
    let qualifying: Vec<(String, usize, Option<i64>)> = {
        let mut stmt = conn
            .prepare(
                "SELECT t.full_path,
                        COUNT(DISTINCT e.photo_id) AS confirmed_count,
                        clf.trained_at
                   FROM tags t
                   JOIN photo_tags pt ON pt.tag_id = t.id
                   JOIN smarttags__embeddings e ON e.photo_id = pt.photo_id
                   LEFT JOIN smarttags__classifiers clf ON clf.tag_path = t.full_path
                  GROUP BY t.id
                 HAVING confirmed_count >= ?1
                  ORDER BY t.full_path",
            )
            .map_err(|e| e.to_string())?;
        // Collect before the block ends so `stmt` is dropped cleanly.
        let rows = stmt
            .query_map(rusqlite::params![min_samples as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as usize,
                    r.get::<_, Option<i64>>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        rows
    };

    let mut scan = StaleScan { examined: qualifying.len(), ..Default::default() };

    for (tag_path, _confirmed_count, clf_trained_at) in qualifying {
        // ── 2. Staleness check ────────────────────────────────────────────────
        // The classifier is stale when there is no row yet, or when the most
        // recent accept/reject for this tag path was written after trained_at.
        // We compare against `updated_at` (set by set_suggestion_state), NOT
        // `created_at` (which is the proposal timestamp and never changes after
        // the initial insert). For rows inserted before the updated_at column
        // existed the DEFAULT 0 means they read as epoch-0 — those will appear
        // stale and trigger a one-time retrain, which is the safe fallback.
        let latest_suggestion_ts: Option<i64> = conn
            .query_row(
                "SELECT MAX(updated_at) FROM smarttags__suggestions
                  WHERE path = ?1 AND state IN ('accepted', 'rejected')",
                rusqlite::params![tag_path],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .flatten();

        let is_stale = match (clf_trained_at, latest_suggestion_ts) {
            (None, _) => true,                  // no classifier yet
            (Some(_), None) => false,            // no feedback yet — nothing has changed
            (Some(ts_clf), Some(ts_sug)) => ts_sug > ts_clf,
        };

        if !is_stale {
            scan.skipped += 1;
            continue;
        }

        scan.stale.push(tag_path);
    }

    Ok(scan)
}

/// Load one tag's positive and negative embeddings.
///
/// Returns `None` when the tag has no usable positive embeddings — the caller counts
/// that as neither trained nor skipped, matching the original inline behaviour.
///
/// This is the expensive read: it pulls up to `4 × positives` embeddings into memory.
/// Callers holding the catalog lock should load **one** tag at a time and release the
/// lock before training, so peak memory stays at one tag's training set rather than the
/// whole catalog's.
pub fn load_training_set(
    conn: &Connection,
    tag_path: &str,
) -> Result<Option<TrainingSet>, String> {
    // ── 3. Load positive embeddings ───────────────────────────────────────
    let pos_blobs: Vec<Vec<u8>> = {
            let mut stmt = conn
                .prepare(
                    "SELECT e.vector
                       FROM smarttags__embeddings e
                       JOIN photo_tags pt ON pt.photo_id = e.photo_id
                       JOIN tags t ON t.id = pt.tag_id
                      WHERE t.full_path = ?1
                      ORDER BY e.photo_id",
                )
                .map_err(|e| e.to_string())?;
            // Collect before the block ends so `stmt` is dropped cleanly.
            let rows = stmt
                .query_map(rusqlite::params![tag_path], |r| r.get::<_, Vec<u8>>(0))
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            rows
        };

    let positives: Vec<Vec<f32>> = pos_blobs
        .iter()
        .filter_map(|b| blob_to_embedding(b).ok())
        .collect();

    if positives.is_empty() {
        return Ok(None);
    }

    // ── 4. Load a bounded negative sample ────────────────────────────────
    // Limit negatives to 3× positives so the training set is roughly
    // balanced and doesn't balloon on large catalogs.
    let neg_limit = (positives.len() * 3).max(1) as i64;
    let neg_blobs: Vec<Vec<u8>> = {
            let mut stmt = conn
                .prepare(
                    "SELECT e.vector
                       FROM smarttags__embeddings e
                      WHERE NOT EXISTS (
                          SELECT 1 FROM photo_tags pt
                            JOIN tags t ON t.id = pt.tag_id
                           WHERE pt.photo_id = e.photo_id
                             AND t.full_path = ?1
                      )
                      ORDER BY e.photo_id   -- deterministic; a random sample is a follow-up
                      LIMIT ?2",
                )
                .map_err(|e| e.to_string())?;
            // Collect before the block ends so `stmt` is dropped cleanly.
            let rows = stmt
                .query_map(rusqlite::params![tag_path, neg_limit], |r| {
                    r.get::<_, Vec<u8>>(0)
                })
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            rows
        };

    let negatives: Vec<Vec<f32>> = neg_blobs
        .iter()
        .filter_map(|b| blob_to_embedding(b).ok())
        .collect();

    Ok(Some(TrainingSet { tag_path: tag_path.to_string(), positives, negatives }))
}

/// Write a batch of trained classifiers back in **one** transaction, and report how many
/// rows were written.
///
/// Training is a read-snapshot → CPU → write-back cycle with the catalog lock released in
/// the middle, so the write has to be all-or-nothing: a partial write would leave some
/// tags with classifiers trained against one catalog state and some against another, and
/// `trained_at` would then mark the inconsistent set as fresh.
pub fn persist_classifiers(
    conn: &Connection,
    trained: &[(String, Classifier)],
) -> Result<usize, String> {
    if trained.is_empty() {
        return Ok(0);
    }
    ensure_schema(conn).map_err(|e| e.to_string())?;
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    for (tag_path, clf) in trained {
        upsert_classifier(&tx, tag_path, clf).map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(trained.len())
}

/// Train (or retrain) logistic-regression classifiers for every tag that has at least
/// `min_samples` confirmed CLIP embeddings, start to finish on one connection.
///
/// A classifier is "stale" when `trained_at` < the timestamp of the most recent
/// accept/reject for that tag's suggestions. We rebuild whenever that condition holds
/// *or* when the row doesn't exist yet.
///
/// For each qualifying tag we pull:
/// - **positives** — embeddings of photos confirmed to carry the tag (via `photo_tags`).
/// - **negatives** — a random sample (≤ `3 × positives` to keep training fast) of
///   embeddings from photos that do *not* carry the tag.
///
/// The negative sample is bounded to 3× the positive count to keep the training set
/// roughly balanced without loading tens of thousands of embeddings into RAM when a
/// catalog is large.
///
/// This composes [`scan_stale_tags`], [`load_training_set`], [`train`] and
/// [`persist_classifiers`] with no lock handling, which makes it the right entry point
/// for tests and for any caller that already owns its connection exclusively. The Tauri
/// command uses the four steps directly instead, so it can drop the catalog lock around
/// the CPU-bound [`train`] call.
pub fn train_all(conn: &Connection, min_samples: usize) -> Result<TrainOutcome, String> {
    let scan = scan_stale_tags(conn, min_samples)?;
    let mut trained = Vec::new();
    for tag_path in &scan.stale {
        let Some(set) = load_training_set(conn, tag_path)? else {
            continue;
        };
        if let Some(clf) = train(&set.positives, &set.negatives) {
            trained.push((set.tag_path, clf));
        }
    }
    let count = persist_classifiers(conn, &trained)?;
    Ok(TrainOutcome { examined: scan.examined, trained: count, skipped: scan.skipped })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[inline]
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    // ── Schema ─────────────────────────────────────────────────────────────────

    #[test]
    fn ensure_schema_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
    }

    // ── Sigmoid ────────────────────────────────────────────────────────────────

    #[test]
    fn sigmoid_known_values() {
        // sigmoid(0) = 0.5
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-6);
        // sigmoid(large) ≈ 1
        assert!(sigmoid(100.0) > 0.999);
        // sigmoid(-large) ≈ 0
        assert!(sigmoid(-100.0) < 0.001);
    }

    // ── Training on a linearly separable synthetic set ─────────────────────────
    //
    // Positive class: embeddings along +e₀ (first axis).
    // Negative class: embeddings along -e₀ (opposite direction).
    // A logistic classifier should learn w[0] >> 0, bias ≈ 0 and classify both
    // perfectly (probability near 1 for positives, near 0 for negatives).

    fn axis_emb(dim: usize, axis: usize, sign: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; dim];
        v[axis] = sign;
        v
    }

    #[test]
    fn train_linearly_separable_classifies_correctly() {
        let dim = 8;
        // 12 positives along +e₀
        let positives: Vec<Vec<f32>> = (0..12).map(|_| axis_emb(dim, 0, 1.0)).collect();
        // 12 negatives along -e₀
        let negatives: Vec<Vec<f32>> = (0..12).map(|_| axis_emb(dim, 0, -1.0)).collect();

        let clf = train(&positives, &negatives).expect("should train");
        assert_eq!(clf.sample_count, 12);
        assert_eq!(clf.weights.len(), dim);

        // Positive test point should score > 0.9
        let pos_score = clf.predict(&axis_emb(dim, 0, 1.0));
        assert!(
            pos_score > 0.9,
            "positive class must score > 0.9, got {pos_score}"
        );

        // Negative test point should score < 0.1
        let neg_score = clf.predict(&axis_emb(dim, 0, -1.0));
        assert!(
            neg_score < 0.1,
            "negative class must score < 0.1, got {neg_score}"
        );
    }

    #[test]
    fn train_with_noise_still_classifies() {
        // Add slight noise so we're not testing an identity collapse.
        let dim = 16;
        let rng_emb = |axis: usize, sign: f32, seed: usize| -> Vec<f32> {
            let mut v = vec![0.0f32; dim];
            v[axis] = sign;
            // Small perturbation on other dimensions.
            for i in 1..dim {
                v[i] = 0.05 * ((seed + i) as f32 % 7.0 - 3.0) / 3.0;
            }
            // L2-normalize so it resembles a CLIP embedding.
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            v.iter().map(|x| x / norm).collect()
        };

        let positives: Vec<Vec<f32>> = (0..15).map(|s| rng_emb(0, 1.0, s)).collect();
        let negatives: Vec<Vec<f32>> = (0..15).map(|s| rng_emb(0, -1.0, s + 100)).collect();

        let clf = train(&positives, &negatives).expect("should train on noisy data");

        let pos_score = clf.predict(&rng_emb(0, 1.0, 999));
        let neg_score = clf.predict(&rng_emb(0, -1.0, 998));
        assert!(pos_score > neg_score, "positive should score higher than negative");
        assert!(pos_score > 0.7, "positive should score > 0.7, got {pos_score}");
        assert!(neg_score < 0.3, "negative should score < 0.3, got {neg_score}");
    }

    #[test]
    fn train_no_positives_returns_none() {
        let clf = train(&[], &[vec![1.0f32, 0.0]]);
        assert!(clf.is_none(), "train with no positives must return None");
    }

    // ── Persistence round-trip ─────────────────────────────────────────────────

    fn mem_conn_with_schema() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn upsert_and_load_roundtrip() {
        let conn = mem_conn_with_schema();
        let clf = Classifier {
            weights: vec![1.0f32, -0.5, 0.3],
            bias: 0.1,
            sample_count: 42,
        };
        upsert_classifier(&conn, "Animals/Birds", &clf).unwrap();

        let loaded = load_classifier(&conn, "Animals/Birds")
            .unwrap()
            .expect("must be present after upsert");

        assert_eq!(loaded.sample_count, 42);
        assert!((loaded.bias - 0.1).abs() < 1e-5, "bias round-trip, got {}", loaded.bias);
        assert_eq!(loaded.weights.len(), 3);
        for (a, b) in clf.weights.iter().zip(loaded.weights.iter()) {
            assert!((a - b).abs() < 1e-6, "weight round-trip mismatch {a} vs {b}");
        }
    }

    #[test]
    fn load_missing_tag_returns_none() {
        let conn = mem_conn_with_schema();
        let result = load_classifier(&conn, "NonExistent/Tag").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn upsert_replaces_existing() {
        let conn = mem_conn_with_schema();
        let clf1 = Classifier { weights: vec![1.0f32], bias: 0.0, sample_count: 5 };
        let clf2 = Classifier { weights: vec![2.0f32], bias: 1.0, sample_count: 20 };

        upsert_classifier(&conn, "Tag/A", &clf1).unwrap();
        upsert_classifier(&conn, "Tag/A", &clf2).unwrap();

        let loaded = load_classifier(&conn, "Tag/A").unwrap().unwrap();
        assert_eq!(loaded.sample_count, 20, "second upsert must replace the first");
        assert!((loaded.weights[0] - 2.0).abs() < 1e-5);
    }

    // ── Blending ───────────────────────────────────────────────────────────────

    #[test]
    fn blend_no_classifier_returns_knn() {
        let score = score_blended(0.75, None, &[1.0f32]);
        assert!((score - 0.75).abs() < 1e-6);
    }

    #[test]
    fn blend_with_classifier_lies_between_knn_and_cls() {
        // A classifier that always predicts 1.0 for any input.
        let clf = Classifier {
            weights: vec![100.0f32, 0.0],
            bias: 0.0,
            sample_count: 10,
        };
        let x = vec![1.0f32, 0.0];
        let knn_score = 0.5;
        let blended = score_blended(knn_score, Some(&clf), &x);
        let cls_score = clf.predict(&x);

        assert!(blended > knn_score, "blended must be > knn when cls is higher");
        assert!(blended <= cls_score + 1e-5, "blended must not exceed cls_score");
    }

    #[test]
    fn blend_weight_scales_with_sample_count() {
        // Use a classifier that always predicts ≈1.0 (large positive bias) so
        // cls_score ≈ 1.0 regardless of input — this isolates the weight behaviour.
        // knn_score = 0.0, cls_score ≈ 1.0.
        // blended = (1.0 * 0.0 + cls_weight * 1.0) / (1.0 + cls_weight)
        //         = cls_weight / (1.0 + cls_weight)
        let x = vec![1.0f32];
        let knn = 0.0f32;

        // sample_count=0 → cls_weight = 0/50 = 0.0 → blended = knn = 0.0
        let clf_zero = Classifier { weights: vec![0.0f32], bias: 100.0, sample_count: 0 };
        let blended_zero = score_blended(knn, Some(&clf_zero), &x);
        assert!(
            (blended_zero - knn).abs() < 1e-5,
            "zero-sample classifier should not affect score, got {blended_zero}"
        );

        // sample_count=10 → cls_weight = 10/50 = 0.20
        // blended = 0.20 / 1.20 ≈ 0.1667
        let clf_10 = Classifier { weights: vec![0.0f32], bias: 100.0, sample_count: 10 };
        let blended_10 = score_blended(knn, Some(&clf_10), &x);
        let expected_10 = 0.20f32 / 1.20f32;
        assert!(
            (blended_10 - expected_10).abs() < 0.01,
            "10-sample blend ≈ {expected_10:.4}, got {blended_10}"
        );

        // sample_count=25 → cls_weight = 25/50 = 0.50
        // blended = 0.50 / 1.50 ≈ 0.3333
        let clf_25 = Classifier { weights: vec![0.0f32], bias: 100.0, sample_count: 25 };
        let blended_25 = score_blended(knn, Some(&clf_25), &x);
        let expected_25 = 0.50f32 / 1.50f32;
        assert!(
            (blended_25 - expected_25).abs() < 0.01,
            "25-sample blend ≈ {expected_25:.4}, got {blended_25}"
        );

        // sample_count=50 → cls_weight = 1.0 (max)
        // blended = 1.0 / 2.0 = 0.50
        let clf_50 = Classifier { weights: vec![0.0f32], bias: 100.0, sample_count: 50 };
        let blended_50 = score_blended(knn, Some(&clf_50), &x);
        assert!(
            (blended_50 - 0.50).abs() < 0.01,
            "50-sample blend ≈ 0.50, got {blended_50}"
        );

        // Weight increases monotonically with sample_count.
        assert!(
            blended_10 < blended_25 && blended_25 < blended_50,
            "blending weight must increase with sample_count: \
             blend_10={blended_10}, blend_25={blended_25}, blend_50={blended_50}"
        );

        // >=50 samples clamp at cls_weight=1.0 (flat region above 50).
        let clf_100 = Classifier { weights: vec![0.0f32], bias: 100.0, sample_count: 100 };
        let blended_100 = score_blended(knn, Some(&clf_100), &x);
        assert!(
            (blended_100 - blended_50).abs() < 1e-5,
            "100 and 50 samples must produce the same blend (both at max weight)"
        );
    }

    // ── train_all end-to-end ───────────────────────────────────────────────────

    fn full_mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE IF NOT EXISTS photos (
                 id   INTEGER PRIMARY KEY,
                 uuid TEXT NOT NULL DEFAULT '',
                 path TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS tags (
                 id            INTEGER PRIMARY KEY,
                 name          TEXT NOT NULL DEFAULT '',
                 name_norm     TEXT NOT NULL DEFAULT '',
                 parent_id     INTEGER,
                 full_path     TEXT NOT NULL DEFAULT '',
                 full_path_norm TEXT NOT NULL DEFAULT '',
                 description   TEXT NOT NULL DEFAULT '',
                 auto_rule     TEXT,
                 exportable    INTEGER NOT NULL DEFAULT 1,
                 private       INTEGER NOT NULL DEFAULT 0,
                 last_used_at  INTEGER,
                 created_at    INTEGER NOT NULL DEFAULT 0,
                 updated_at    INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS photo_tags (
                 photo_id   INTEGER NOT NULL,
                 tag_id     INTEGER NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (photo_id, tag_id)
             );
             CREATE TABLE IF NOT EXISTS smarttags__suggestions (
                 photo_id   INTEGER NOT NULL,
                 path       TEXT NOT NULL,
                 state      TEXT NOT NULL DEFAULT 'pending',
                 created_at INTEGER NOT NULL DEFAULT 0,
                 updated_at INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (photo_id, path)
             );",
        )
        .unwrap();
        super::super::store::ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    fn insert_photo(conn: &Connection, id: i64) {
        conn.execute(
            "INSERT INTO photos (id, uuid, path) VALUES (?1, '', '')",
            [id],
        )
        .unwrap();
    }

    fn insert_tag(conn: &Connection, id: i64, full_path: &str) {
        let name = full_path.split('/').last().unwrap_or(full_path);
        conn.execute(
            "INSERT INTO tags (id, name, name_norm, full_path, full_path_norm)
             VALUES (?1, ?2, lower(?2), ?3, lower(?3))",
            rusqlite::params![id, name, full_path],
        )
        .unwrap();
    }

    fn assign_tag(conn: &Connection, photo_id: i64, tag_id: i64) {
        conn.execute(
            "INSERT OR IGNORE INTO photo_tags(photo_id, tag_id) VALUES(?1, ?2)",
            [photo_id, tag_id],
        )
        .unwrap();
    }

    fn upsert_emb(conn: &Connection, photo_id: i64, emb: &[f32]) {
        let blob = super::super::store::embedding_to_blob(emb);
        super::super::store::upsert_embedding(conn, photo_id, &blob).unwrap();
    }

    fn unit_axis(dim: usize, axis: usize, sign: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; dim];
        v[axis] = sign;
        v
    }

    #[test]
    fn train_all_trains_qualifying_tags_and_skips_below_threshold() {
        let conn = full_mem_conn();
        let dim = 8;

        // Tag 1 "Birds" — 10 confirmed photos (meets the default min_samples=10)
        insert_tag(&conn, 1, "Animals/Birds");
        for id in 1i64..=10 {
            insert_photo(&conn, id);
            assign_tag(&conn, id, 1);
            upsert_emb(&conn, id, &unit_axis(dim, 0, 1.0));
        }

        // Tag 2 "Boats" — only 3 confirmed photos (below min_samples=10)
        insert_tag(&conn, 2, "Transportation/Boats");
        for id in 11i64..=13 {
            insert_photo(&conn, id);
            assign_tag(&conn, id, 2);
            upsert_emb(&conn, id, &unit_axis(dim, 1, 1.0));
        }

        // Add some negatives (untagged photos) so the classifier has negatives.
        for id in 20i64..=25 {
            insert_photo(&conn, id);
            upsert_emb(&conn, id, &unit_axis(dim, 0, -1.0));
        }

        // Add a fake accepted suggestion to trigger staleness (trained_at is NULL → stale).
        // updated_at is set to simulate set_suggestion_state being called at time 9999.
        conn.execute(
            "INSERT INTO smarttags__suggestions (photo_id, path, state, created_at, updated_at)
             VALUES (1, 'Animals/Birds', 'accepted', 1000, 9999)",
            [],
        )
        .unwrap();

        let outcome = train_all(&conn, 10).unwrap();
        assert_eq!(outcome.examined, 1, "only Birds should qualify");
        assert_eq!(outcome.trained, 1, "Birds should be trained");
        assert_eq!(outcome.skipped, 0);

        // The classifier for Birds should now exist and score well.
        let clf = load_classifier(&conn, "Animals/Birds").unwrap().unwrap();
        let pos_score = clf.predict(&unit_axis(dim, 0, 1.0));
        assert!(pos_score > 0.7, "Birds classifier must score positive embeddings > 0.7, got {pos_score}");

        // Boats should NOT have a classifier (too few samples).
        let no_clf = load_classifier(&conn, "Transportation/Boats").unwrap();
        assert!(no_clf.is_none(), "Boats should not have a classifier (below threshold)");
    }

    #[test]
    fn train_all_skips_fresh_classifiers() {
        let conn = full_mem_conn();
        let dim = 4;

        insert_tag(&conn, 1, "Sky");
        for id in 1i64..=12 {
            insert_photo(&conn, id);
            assign_tag(&conn, id, 1);
            upsert_emb(&conn, id, &unit_axis(dim, 0, 1.0));
        }

        // Manually insert a classifier row with a very recent trained_at (unix far future).
        // With no suggestions at all (latest_suggestion_ts = None), the classifier is NOT stale.
        let clf = Classifier {
            weights: vec![1.0f32, 0.0, 0.0, 0.0],
            bias: 0.0,
            sample_count: 12,
        };
        upsert_classifier(&conn, "Sky", &clf).unwrap();

        // Now train — it should skip "Sky" because there is no newer feedback.
        let outcome = train_all(&conn, 10).unwrap();
        assert_eq!(outcome.examined, 1);
        assert_eq!(outcome.skipped, 1, "fresh classifier with no feedback should be skipped");
        assert_eq!(outcome.trained, 0);
    }

    /// Regression test for the staleness-check bug: a suggestion that went through the
    /// normal pending→accepted flow has `created_at` = proposal time (old) but
    /// `updated_at` = acceptance time (new). The staleness check must use `updated_at`
    /// or classifiers will never retrain after normal user feedback.
    #[test]
    fn train_all_retrains_after_accept_using_updated_at() {
        let conn = full_mem_conn();
        let dim = 4;

        insert_tag(&conn, 1, "Forest");
        for id in 1i64..=10 {
            insert_photo(&conn, id);
            assign_tag(&conn, id, 1);
            upsert_emb(&conn, id, &unit_axis(dim, 0, 1.0));
        }

        // Simulate a classifier trained at t=500.
        let clf = Classifier {
            weights: vec![0.5f32, 0.0, 0.0, 0.0],
            bias: 0.0,
            sample_count: 10,
        };
        upsert_classifier(&conn, "Forest", &clf).unwrap();
        // Overwrite trained_at to a known value (500).
        conn.execute(
            "UPDATE smarttags__classifiers SET trained_at = 500 WHERE tag_path = 'Forest'",
            [],
        ).unwrap();

        // Insert a suggestion that was *proposed* at t=100 (created_at=100) but
        // *accepted* at t=600 (updated_at=600). This mirrors the real pending→accepted flow:
        // the row was inserted during suggest_tags (created_at=100) and updated_at was set
        // when the user accepted it via set_suggestion_state (updated_at=600).
        conn.execute(
            "INSERT INTO smarttags__suggestions (photo_id, path, state, created_at, updated_at)
             VALUES (1, 'Forest', 'accepted', 100, 600)",
            [],
        ).unwrap();

        // Add a negative embedding so training has something to work with.
        insert_photo(&conn, 20);
        upsert_emb(&conn, 20, &unit_axis(dim, 0, -1.0));

        // train_all must detect staleness via updated_at=600 > trained_at=500 and retrain.
        let outcome = train_all(&conn, 10).unwrap();
        assert_eq!(outcome.examined, 1, "Forest should qualify (10 samples)");
        assert_eq!(outcome.trained, 1, "Forest must retrain: updated_at(600) > trained_at(500)");
        assert_eq!(outcome.skipped, 0);
    }

    /// Regression test for issue #32: training should work on a catalog that has
    /// embeddings and tags but has never produced a suggestion (so
    /// smarttags__suggestions does not yet exist).
    ///
    /// Without the fix, scan_stale_tags would query smarttags__suggestions and fail with
    /// "no such table: smarttags__suggestions". With the fix, scan_stale_tags ensures
    /// the table exists before the query, so training succeeds.
    #[test]
    fn train_all_works_without_prior_suggestions() {
        let conn = Connection::open_in_memory().unwrap();
        // Manually set up the schema WITHOUT smarttags__suggestions, simulating a catalog
        // that has indexed embeddings and tags but never run suggestions.
        conn.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE IF NOT EXISTS photos (
                 id   INTEGER PRIMARY KEY,
                 uuid TEXT NOT NULL DEFAULT '',
                 path TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS tags (
                 id            INTEGER PRIMARY KEY,
                 name          TEXT NOT NULL DEFAULT '',
                 name_norm     TEXT NOT NULL DEFAULT '',
                 parent_id     INTEGER,
                 full_path     TEXT NOT NULL DEFAULT '',
                 full_path_norm TEXT NOT NULL DEFAULT '',
                 description   TEXT NOT NULL DEFAULT '',
                 auto_rule     TEXT,
                 exportable    INTEGER NOT NULL DEFAULT 1,
                 private       INTEGER NOT NULL DEFAULT 0,
                 last_used_at  INTEGER,
                 created_at    INTEGER NOT NULL DEFAULT 0,
                 updated_at    INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS photo_tags (
                 photo_id   INTEGER NOT NULL,
                 tag_id     INTEGER NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (photo_id, tag_id)
             );",
        )
        .unwrap();
        // Ensure embeddings and classifiers schemas, but NOT suggestions.
        super::super::store::ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
        // Note: we do NOT call super::suggest::ensure_suggestions_schema(&conn).unwrap();
        // That's the whole point — scan_stale_tags must handle the missing table gracefully.

        let dim = 8;

        // Insert a tag with 10 confirmed photos and embeddings (qualifies for training).
        insert_tag(&conn, 1, "Nature/Trees");
        for id in 1i64..=10 {
            insert_photo(&conn, id);
            assign_tag(&conn, id, 1);
            upsert_emb(&conn, id, &unit_axis(dim, 0, 1.0));
        }

        // Add some negative embeddings for training.
        for id in 20i64..=25 {
            insert_photo(&conn, id);
            upsert_emb(&conn, id, &unit_axis(dim, 0, -1.0));
        }

        // This should NOT fail with "no such table: smarttags__suggestions".
        // The fix ensures the table is created in scan_stale_tags before the staleness query.
        let outcome = train_all(&conn, 10).unwrap();
        assert_eq!(outcome.examined, 1, "Nature/Trees should qualify (10 samples)");
        assert_eq!(outcome.trained, 1, "Nature/Trees should be trained on first run");
        assert_eq!(outcome.skipped, 0);

        // Verify the classifier was actually persisted.
        let clf = load_classifier(&conn, "Nature/Trees").unwrap().unwrap();
        let pos_score = clf.predict(&unit_axis(dim, 0, 1.0));
        assert!(pos_score > 0.7, "Classifier must score positive embeddings > 0.7, got {pos_score}");
    }
}
