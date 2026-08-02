//! Seed / match / cluster engine (H13c) — the recognition brain of the faces plugin.
//!
//! Turns indexed faces (raw detections, `state='unassigned'`, `source='detect'`) into
//! person-tag assignments, bootstrapping from the catalog's existing **photo-level person
//! tags** (who is in the photo, from years of manual tagging). See docs/face-tagging.md
//! "Seeding & matching".
//!
//! The pipeline [`run_matching`] executes, in order:
//!
//! 1. **Auto-seed** — a photo with **exactly 1** detected face AND **exactly 1** assigned
//!    person tag ⇒ that face is `confirmed`, `source='seed'`. A seed is data, not a
//!    suggestion (auditable + revocable), but flagged as machine-derived.
//! 2. **Per-person centroids** — the L2-normalized mean of each person's confirmed,
//!    non-ignored face embeddings.
//! 3. **Constrained match** — a photo with N unassigned faces and M assigned person tags
//!    (with centroids) ⇒ optimal one-to-one assignment via Hungarian
//!    ([`hungarian_min_cost`], cost `1 − cosine`) ⇒ `suggested` rows + `match_confidence`.
//!    The photo's own tags constrain the space, which is what makes this far more accurate
//!    than open-set matching.
//! 4. **Open matching** — remaining unassigned faces ⇒ nearest person centroid above the
//!    similarity threshold ([`MatchSettings::threshold`]) ⇒ `suggested`.
//! 5. **Clustering** — everything left ⇒ incremental greedy clustering: join the nearest
//!    existing cluster centroid within threshold, else start a new cluster (Immich-style;
//!    deliberately *not* batch DBSCAN so it evolves as photos arrive).
//!
//! ## Idempotence & safety
//!
//! Re-running is safe and proposes nothing new when nothing changed:
//! - **Confirmed** / **ignored** / **manual** faces are never touched (never downgraded).
//! - A **rejected** `(face, person)` pair (recorded by [`reject`]) is never re-proposed.
//! - Existing `suggested` rows are re-evaluated from scratch each run: the matcher first
//!   resets every still-`suggested`/`unassigned` face back to a clean slate, then recomputes.
//!   So a run that finds the same best match writes back the same row — no churn — and a run
//!   where a centroid moved updates the suggestion rather than piling up duplicates.
//!
//! ## Testability
//!
//! Nothing here depends on ONNX or the models — it is pure math over `f32` embeddings plus
//! SQLite. The whole engine (seed edge cases, Hungarian correctness on a known cost matrix,
//! threshold boundary, rejection memory, ignore exclusion, idempotent re-run) is unit-tested
//! on synthetic embeddings. No `#[cfg(feature = "faces")]` gates the logic; only the wiring
//! command in `commands.rs` is feature-gated.

use rusqlite::{Connection, OptionalExtension};

use super::engine::cosine;
use super::store::{blob_to_embedding, embedding_to_blob};

// ── Settings keys & defaults ────────────────────────────────────────────────────

/// Setting key for the people-root tag branch. Person tags are strict descendants of this
/// branch's `full_path` (e.g. root `People` ⇒ `People/Alice`, `People/Family/Bob`). The root
/// itself is not a person.
pub const PEOPLE_ROOT_SETTING: &str = "faces.people_root";
/// Default people-root branch when the setting is unset.
pub const PEOPLE_ROOT_DEFAULT: &str = "People";

/// Setting key for the cosine-similarity match threshold (open matching + clustering).
pub const THRESHOLD_SETTING: &str = "faces.match_threshold";
/// Default match threshold. A candidate must exceed this cosine similarity to a centroid to
/// be suggested (open matching) or to join a cluster.
pub const THRESHOLD_DEFAULT: f32 = 0.45;

// ── Face states & sources (string constants matching the store schema) ──────────

const STATE_UNASSIGNED: &str = "unassigned";
const STATE_SUGGESTED: &str = "suggested";
const STATE_CONFIRMED: &str = "confirmed";
const STATE_IGNORED: &str = "ignored";

const SOURCE_SEED: &str = "seed";
const SOURCE_MATCH: &str = "match";
const SOURCE_MANUAL: &str = "manual";

// ── Public settings bundle ──────────────────────────────────────────────────────

/// Resolved matcher settings, loaded from the `settings` table (with defaults).
#[derive(Debug, Clone)]
pub struct MatchSettings {
    /// `full_path` of the people-root tag branch (person tags are its strict descendants).
    pub people_root: String,
    /// Cosine-similarity threshold for open matching + clustering.
    pub threshold: f32,
}

impl Default for MatchSettings {
    fn default() -> Self {
        Self {
            people_root: PEOPLE_ROOT_DEFAULT.to_string(),
            threshold: THRESHOLD_DEFAULT,
        }
    }
}

impl MatchSettings {
    /// Load the settings from the `settings` key-value table, falling back to defaults for
    /// any missing/blank/unparseable key.
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let people_root = get_setting(conn, PEOPLE_ROOT_SETTING)?
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| PEOPLE_ROOT_DEFAULT.to_string());
        let threshold = get_setting(conn, THRESHOLD_SETTING)?
            .and_then(|s| s.trim().parse::<f32>().ok())
            .filter(|t| t.is_finite())
            .unwrap_or(THRESHOLD_DEFAULT);
        Ok(Self {
            people_root,
            threshold,
        })
    }
}

fn get_setting(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
        r.get::<_, String>(0)
    })
    .optional()
}

// ── Run summary ─────────────────────────────────────────────────────────────────

/// Counters returned by [`run_matching`], reported to the UI so a run can say what it did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct MatchOutcome {
    /// Faces auto-seeded to `confirmed` this run (step 1).
    pub seeded: usize,
    /// Faces given a `suggested` row by constrained (Hungarian) matching (step 3).
    pub constrained: usize,
    /// Faces given a `suggested` row by open nearest-centroid matching (step 4).
    pub open: usize,
    /// Faces assigned to a cluster (step 5), whether an existing or newly created one.
    pub clustered: usize,
    /// Number of distinct people that had a usable centroid this run.
    pub people: usize,
}

// ── Progress reporting ──────────────────────────────────────────────────────────

/// Pipeline steps reported through the [`run_matching_with_progress`] callback, in
/// execution order. `label()` is what the settings panel shows verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchPhase {
    Seed,
    Constrained,
    Open,
    Cluster,
}

impl MatchPhase {
    pub fn label(self) -> &'static str {
        match self {
            MatchPhase::Seed => "seeding from tags",
            MatchPhase::Constrained => "matching tagged photos",
            MatchPhase::Open => "matching known people",
            MatchPhase::Cluster => "clustering unknowns",
        }
    }
}

/// Progress callback: `(phase, done, total)`. Called once with `done = 0` when a phase
/// starts (so empty phases still announce themselves) and then per processed item —
/// callers who forward this to the UI should throttle.
pub type MatchProgress<'a> = &'a mut dyn FnMut(MatchPhase, usize, usize);

// ── Internal working types ──────────────────────────────────────────────────────

/// A face row loaded for matching.
struct FaceRow {
    id: i64,
    photo_id: i64,
    embedding: Vec<f32>,
}

/// One person's centroid, keyed by tag id.
struct Centroid {
    tag_id: i64,
    vec: Vec<f32>,
}

// ── The pipeline ────────────────────────────────────────────────────────────────

/// Run the full seed / match / cluster pipeline over the indexed faces.
///
/// Idempotent and re-runnable: confirmed/ignored/manual faces are never modified, rejected
/// pairs are never re-proposed, and stale `suggested`/cluster assignments are cleared and
/// recomputed each run so nothing accumulates. Returns per-step counters.
///
/// `now` is the timestamp stamped on new rows (unix seconds), injected for deterministic tests.
pub fn run_matching(conn: &Connection, settings: &MatchSettings, now: i64) -> rusqlite::Result<MatchOutcome> {
    run_matching_with_progress(conn, settings, now, &mut |_, _, _| {})
}

/// [`run_matching`] with a progress callback — see [`MatchProgress`] for the contract.
pub fn run_matching_with_progress(
    conn: &Connection,
    settings: &MatchSettings,
    now: i64,
    progress: MatchProgress,
) -> rusqlite::Result<MatchOutcome> {
    super::store::ensure_schema(conn)?;

    let mut outcome = MatchOutcome::default();

    // Resolve the set of person tag ids (strict descendants of the people root branch).
    let person_tags = person_tag_ids(conn, &settings.people_root)?;

    // Step 1 — auto-seed.
    outcome.seeded = auto_seed(conn, &person_tags, now, progress)?;

    // Clear stale suggestions/cluster assignments so the run is idempotent and never stacks
    // duplicate rows. Only faces that are still unassigned/suggested are reset — confirmed,
    // ignored and manual faces are left untouched.
    reset_pending(conn)?;

    // Step 2 — per-person centroids from confirmed, non-ignored faces.
    let centroids = compute_centroids(conn, &person_tags)?;
    outcome.people = centroids.len();

    // Load all faces still pending a decision (unassigned, with an embedding).
    let pending = load_pending_faces(conn)?;

    // Track which faces got resolved by constrained/open matching so clustering ignores them.
    let mut resolved: std::collections::HashSet<i64> = std::collections::HashSet::new();

    if !centroids.is_empty() {
        // Step 3 — constrained match: per photo, N faces × M person-tags on that photo.
        outcome.constrained = constrained_match(
            conn,
            &pending,
            &centroids,
            &person_tags,
            &mut resolved,
            now,
            progress,
        )?;

        // Step 4 — open matching: nearest centroid above threshold for the remainder.
        outcome.open = open_match(
            conn,
            &pending,
            &centroids,
            settings.threshold,
            &mut resolved,
            now,
            progress,
        )?;
    }

    // Step 5 — incremental greedy clustering for everything left.
    outcome.clustered =
        cluster_leftovers(conn, &pending, settings.threshold, &resolved, now, progress)?;

    Ok(outcome)
}

// ── Person-tag resolution ───────────────────────────────────────────────────────

/// All tag ids that are **strict descendants** of the people-root branch — i.e. whose
/// `full_path` starts with `<root>/`. The root itself is excluded (it's a container, not a
/// person). Returns an empty set if the root branch does not exist.
fn person_tag_ids(conn: &Connection, people_root: &str) -> rusqlite::Result<std::collections::HashSet<i64>> {
    let prefix = format!("{people_root}/");
    // Match strict descendants by character-length prefix. SQLite `substr`/`length` count
    // CHARACTERS, so we let SQLite compute `length(?1)` from the bound prefix rather than
    // passing a Rust byte length — otherwise a non-ASCII people-root would silently mismatch
    // (mirrors catalog/mod.rs's `substr(full_path, 1, length(...)) = ...` idiom).
    let mut stmt = conn.prepare(
        "SELECT id FROM tags WHERE substr(full_path, 1, length(?1)) = ?1",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![prefix],
        |r| r.get::<_, i64>(0),
    )?;
    rows.collect()
}

/// The set of person tag ids assigned to a given photo (intersection of its `photo_tags`
/// with the person-tag set).
fn photo_person_tags(
    conn: &Connection,
    photo_id: i64,
    person_tags: &std::collections::HashSet<i64>,
) -> rusqlite::Result<Vec<i64>> {
    let mut stmt =
        conn.prepare("SELECT tag_id FROM photo_tags WHERE photo_id = ?1")?;
    let rows = stmt.query_map([photo_id], |r| r.get::<_, i64>(0))?;
    let mut out = Vec::new();
    for tid in rows {
        let tid = tid?;
        if person_tags.contains(&tid) {
            out.push(tid);
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

// ── Step 1: auto-seed ───────────────────────────────────────────────────────────

/// Auto-seed: a photo with **exactly one** detected face and **exactly one** assigned person
/// tag becomes a confirmed seed for that person. Skips photos already carrying a confirmed/
/// ignored/manual face (idempotent), and honours rejection memory. Returns the seed count.
fn auto_seed(
    conn: &Connection,
    person_tags: &std::collections::HashSet<i64>,
    now: i64,
    progress: MatchProgress,
) -> rusqlite::Result<usize> {
    if person_tags.is_empty() {
        return Ok(0);
    }

    // Candidate photos: exactly one face row, and that face is still unassigned (not already
    // confirmed/ignored/manual/suggested — seeding only fires on a clean detection).
    let mut stmt = conn.prepare(
        "SELECT photo_id, MIN(id) AS face_id, COUNT(*) AS n
           FROM faces__faces
          GROUP BY photo_id
         HAVING n = 1",
    )?;
    let candidates: Vec<(i64, i64)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    progress(MatchPhase::Seed, 0, candidates.len());
    let mut seeded = 0usize;
    for (done, &(photo_id, face_id)) in candidates.iter().enumerate() {
        progress(MatchPhase::Seed, done + 1, candidates.len());
        // The single face must still be a raw unassigned detection.
        let state: String = conn.query_row(
            "SELECT state FROM faces__faces WHERE id = ?1",
            [face_id],
            |r| r.get(0),
        )?;
        if state != STATE_UNASSIGNED {
            continue;
        }

        // The photo must have exactly one *person* tag.
        let tags = photo_person_tags(conn, photo_id, person_tags)?;
        if tags.len() != 1 {
            continue; // 2 faces + 1 tag handled by the COUNT above; here: 1 face + ≠1 person tag.
        }
        let tag_id = tags[0];

        // Respect rejection memory — never seed a pair the user rejected.
        if is_rejected(conn, face_id, tag_id)? {
            continue;
        }

        conn.execute(
            "UPDATE faces__faces
                SET person_tag_id = ?2, state = ?3, source = ?4,
                    match_confidence = 1.0, cluster_id = NULL
              WHERE id = ?1",
            rusqlite::params![face_id, tag_id, STATE_CONFIRMED, SOURCE_SEED],
        )?;
        let _ = now; // seeds carry no created_at rewrite; timestamp reserved for future audit.
        seeded += 1;
    }
    Ok(seeded)
}

// ── Reset pending state (idempotence) ───────────────────────────────────────────

/// Clear machine-made, non-confirmed decisions so the run recomputes from a clean slate:
/// every face that is still `suggested` (from a prior run) or `unassigned` is returned to
/// `unassigned` with no person/cluster/confidence. Confirmed, ignored and manual faces are
/// left exactly as they are — the matcher never downgrades a human (or seed) decision.
fn reset_pending(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE faces__faces
            SET state = ?1, person_tag_id = NULL, match_confidence = NULL,
                cluster_id = NULL, source = ?2
          WHERE state IN (?1, ?3)",
        rusqlite::params![STATE_UNASSIGNED, super::indexer::SOURCE_DETECT, STATE_SUGGESTED],
    )?;
    Ok(())
}

// ── Step 2: per-person centroids ────────────────────────────────────────────────

/// Compute one L2-normalized centroid per person from that person's **confirmed** (and not
/// ignored) faces that carry an embedding. People with no usable confirmed face get no
/// centroid (and so are skipped in steps 3–4). Ignored faces are excluded by the `state`
/// filter — they never influence a centroid.
fn compute_centroids(
    conn: &Connection,
    person_tags: &std::collections::HashSet<i64>,
) -> rusqlite::Result<Vec<Centroid>> {
    let mut stmt = conn.prepare(
        "SELECT person_tag_id, embedding
           FROM faces__faces
          WHERE state = ?1 AND person_tag_id IS NOT NULL AND embedding IS NOT NULL",
    )?;
    let rows = stmt.query_map([STATE_CONFIRMED], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
    })?;

    let mut sums: std::collections::HashMap<i64, (Vec<f32>, usize)> =
        std::collections::HashMap::new();
    for row in rows {
        let (tag_id, blob) = row?;
        if !person_tags.contains(&tag_id) {
            continue; // person tag was moved out of the people branch — ignore.
        }
        let emb = match blob_to_embedding(&blob) {
            Ok(e) if !e.is_empty() => e,
            _ => continue,
        };
        let entry = sums.entry(tag_id).or_insert_with(|| (vec![0.0; emb.len()], 0));
        if entry.0.len() != emb.len() {
            continue; // dimension mismatch — skip defensively.
        }
        for (acc, v) in entry.0.iter_mut().zip(emb.iter()) {
            *acc += v;
        }
        entry.1 += 1;
    }

    let mut out: Vec<Centroid> = sums
        .into_iter()
        .filter_map(|(tag_id, (sum, n))| {
            if n == 0 {
                return None;
            }
            let mean: Vec<f32> = sum.iter().map(|s| s / n as f32).collect();
            let normed = l2_normalize(&mean);
            Some(Centroid {
                tag_id,
                vec: normed,
            })
        })
        .collect();
    // Deterministic order for stable tests.
    out.sort_by_key(|c| c.tag_id);
    Ok(out)
}

// ── Load pending faces ──────────────────────────────────────────────────────────

/// Load all faces that are still `unassigned` and carry an embedding — the candidates for
/// steps 3–5. Ordered by id for determinism.
fn load_pending_faces(conn: &Connection) -> rusqlite::Result<Vec<FaceRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, photo_id, embedding
           FROM faces__faces
          WHERE state = ?1 AND embedding IS NOT NULL
          ORDER BY id",
    )?;
    let rows = stmt.query_map([STATE_UNASSIGNED], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Vec<u8>>(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, photo_id, blob) = row?;
        if let Ok(emb) = blob_to_embedding(&blob) {
            if !emb.is_empty() {
                out.push(FaceRow {
                    id,
                    photo_id,
                    embedding: emb,
                });
            }
        }
    }
    Ok(out)
}

// ── Step 3: constrained (Hungarian) match ───────────────────────────────────────

/// Constrained match: for each photo with ≥1 pending face AND ≥1 assigned person tag that has
/// a centroid, solve the optimal one-to-one assignment (Hungarian, cost = `1 − cosine`) of the
/// photo's faces to its people. Only assignments involving a real (non-padding) face and
/// person are written as `suggested`. Rejected pairs are made infinitely costly so they are
/// never chosen. Returns the number of faces suggested.
fn constrained_match(
    conn: &Connection,
    pending: &[FaceRow],
    centroids: &[Centroid],
    person_tags: &std::collections::HashSet<i64>,
    resolved: &mut std::collections::HashSet<i64>,
    now: i64,
    progress: MatchProgress,
) -> rusqlite::Result<usize> {
    // Group pending faces by photo, preserving id order.
    let mut by_photo: std::collections::BTreeMap<i64, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, f) in pending.iter().enumerate() {
        by_photo.entry(f.photo_id).or_default().push(i);
    }

    let centroid_by_tag: std::collections::HashMap<i64, &Vec<f32>> =
        centroids.iter().map(|c| (c.tag_id, &c.vec)).collect();

    let total_photos = by_photo.len();
    progress(MatchPhase::Constrained, 0, total_photos);
    let mut suggested = 0usize;
    for (done, (photo_id, face_idxs)) in by_photo.into_iter().enumerate() {
        progress(MatchPhase::Constrained, done + 1, total_photos);
        let ptags = photo_person_tags(conn, photo_id, person_tags)?;
        // Only people that actually have a centroid can be matched.
        let people: Vec<i64> = ptags
            .into_iter()
            .filter(|t| centroid_by_tag.contains_key(t))
            .collect();
        if people.is_empty() {
            continue; // no constraining people → leave to open matching.
        }

        let rows = face_idxs.len();
        let cols = people.len();
        // Build the cost matrix, padded to square. Cost = 1 - cosine to that person's centroid.
        // Padding entries (nonexistent face/person) cost PAD so they never beat a real pair.
        let dim = rows.max(cols);
        const PAD: f32 = 10.0;
        const REJECTED: f32 = 1e6;
        let mut cost = vec![vec![PAD; dim]; dim];
        for (ri, &fi) in face_idxs.iter().enumerate() {
            let emb = &pending[fi].embedding;
            for (ci, &tag_id) in people.iter().enumerate() {
                if is_rejected(conn, pending[fi].id, tag_id)? {
                    cost[ri][ci] = REJECTED;
                    continue;
                }
                let sim = cosine(emb, centroid_by_tag[&tag_id]);
                cost[ri][ci] = 1.0 - sim;
            }
        }

        let assignment = hungarian_min_cost(&cost);
        for (ri, &fi) in face_idxs.iter().enumerate() {
            let ci = assignment[ri];
            if ci >= cols {
                continue; // matched to padding — no real person for this face.
            }
            let c = cost[ri][ci];
            if c >= REJECTED || c >= PAD {
                continue; // rejected pair or padding cost — skip.
            }
            let tag_id = people[ci];
            let confidence = 1.0 - c; // cosine similarity.
            write_suggestion(conn, pending[fi].id, tag_id, confidence, now)?;
            resolved.insert(pending[fi].id);
            suggested += 1;
        }
    }
    Ok(suggested)
}

// ── Step 4: open matching ───────────────────────────────────────────────────────

/// Open matching: for each still-unresolved pending face, find the nearest person centroid;
/// if its cosine similarity exceeds the threshold and the pair isn't rejected, write a
/// `suggested` row. Returns the number suggested.
fn open_match(
    conn: &Connection,
    pending: &[FaceRow],
    centroids: &[Centroid],
    threshold: f32,
    resolved: &mut std::collections::HashSet<i64>,
    now: i64,
    progress: MatchProgress,
) -> rusqlite::Result<usize> {
    progress(MatchPhase::Open, 0, pending.len());
    let mut suggested = 0usize;
    for (done, f) in pending.iter().enumerate() {
        progress(MatchPhase::Open, done + 1, pending.len());
        // Skip faces already resolved by constrained matching (step 3).
        if resolved.contains(&f.id) {
            continue;
        }
        // Find the best centroid strictly above the threshold, honouring rejection memory.
        let mut best: Option<(i64, f32)> = None;
        for c in centroids {
            if is_rejected(conn, f.id, c.tag_id)? {
                continue;
            }
            let sim = cosine(&f.embedding, &c.vec);
            if sim > threshold && best.map(|(_, s)| sim > s).unwrap_or(true) {
                best = Some((c.tag_id, sim));
            }
        }
        if let Some((tag_id, sim)) = best {
            write_suggestion(conn, f.id, tag_id, sim, now)?;
            resolved.insert(f.id);
            suggested += 1;
        }
    }
    Ok(suggested)
}

// ── Step 5: incremental greedy clustering ───────────────────────────────────────

/// Incremental greedy clustering for faces that matched no known person: join the nearest
/// existing cluster centroid within threshold, else start a new cluster. The cluster centroid
/// is updated (running L2-normalized mean) as members join. Returns the number of faces
/// clustered.
fn cluster_leftovers(
    conn: &Connection,
    pending: &[FaceRow],
    threshold: f32,
    resolved: &std::collections::HashSet<i64>,
    now: i64,
    progress: MatchProgress,
) -> rusqlite::Result<usize> {
    // Rebuild clusters from scratch every run so the pipeline is idempotent. The only cluster
    // rows that survive between runs hold *leftover* (still-pending) faces — `reset_pending`
    // has already cleared their `cluster_id`, and `name_cluster` deletes a cluster the moment
    // it's named + its members confirmed. So the persisted rows carry no state we must keep;
    // re-growing them incrementally would re-add each leftover face on every run, inflating
    // `size` and drifting the running-mean centroid. Instead we clear the table and rebuild it
    // fresh from the current leftovers — exactly how `suggested` rows are recomputed each run.
    conn.execute("DELETE FROM faces__clusters", [])?;
    let mut clusters: Vec<(i64, Vec<f32>, usize)> = Vec::new();

    progress(MatchPhase::Cluster, 0, pending.len());
    let mut clustered = 0usize;
    for (done, f) in pending.iter().enumerate() {
        progress(MatchPhase::Cluster, done + 1, pending.len());
        if resolved.contains(&f.id) {
            continue;
        }
        // Nearest existing cluster within threshold.
        let mut best: Option<(usize, f32)> = None; // (index into `clusters`, sim)
        for (i, (_, cen, _)) in clusters.iter().enumerate() {
            let sim = cosine(&f.embedding, cen);
            if sim > threshold && best.map(|(_, s)| sim > s).unwrap_or(true) {
                best = Some((i, sim));
            }
        }

        let cluster_id = match best {
            Some((idx, _)) => {
                // Join: update the running mean centroid.
                let (cid, cen, size) = &mut clusters[idx];
                let n = *size as f32;
                let updated: Vec<f32> = cen
                    .iter()
                    .zip(f.embedding.iter())
                    .map(|(c, v)| (c * n + v) / (n + 1.0))
                    .collect();
                *cen = l2_normalize(&updated);
                *size += 1;
                let cid = *cid;
                conn.execute(
                    "UPDATE faces__clusters SET centroid = ?2, size = ?3 WHERE id = ?1",
                    rusqlite::params![cid, embedding_to_blob(cen), *size as i64],
                )?;
                cid
            }
            None => {
                // New cluster seeded from this face's (normalized) embedding.
                let cen = l2_normalize(&f.embedding);
                conn.execute(
                    "INSERT INTO faces__clusters (centroid, size, created_at) VALUES (?1, 1, ?2)",
                    rusqlite::params![embedding_to_blob(&cen), now],
                )?;
                let cid = conn.last_insert_rowid();
                clusters.push((cid, cen, 1));
                cid
            }
        };

        conn.execute(
            "UPDATE faces__faces SET cluster_id = ?2 WHERE id = ?1",
            rusqlite::params![f.id, cluster_id],
        )?;
        clustered += 1;
    }
    Ok(clustered)
}

// ── Write helpers ───────────────────────────────────────────────────────────────

/// Write a `suggested` row for a face: set the person, confidence and `source='match'`.
fn write_suggestion(
    conn: &Connection,
    face_id: i64,
    tag_id: i64,
    confidence: f32,
    _now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE faces__faces
            SET person_tag_id = ?2, state = ?3, source = ?4,
                match_confidence = ?5, cluster_id = NULL
          WHERE id = ?1",
        rusqlite::params![face_id, tag_id, STATE_SUGGESTED, SOURCE_MATCH, confidence as f64],
    )?;
    Ok(())
}

// ── Rejection memory ────────────────────────────────────────────────────────────

/// True if the `(face, person)` pair has been rejected by the user.
fn is_rejected(conn: &Connection, face_id: i64, person_tag_id: i64) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM faces__rejections WHERE face_id = ?1 AND person_tag_id = ?2",
        rusqlite::params![face_id, person_tag_id],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
}

// ── Mutations (accept / reject / ignore / assign) ───────────────────────────────
//
// These are the low-level DB mutations behind the Tauri commands. `accept` also needs to
// assign the person tag to the *photo* through the catalog's `assign_tag` path (XMP export +
// merge-safe). That side effect lives in commands.rs, which owns the `Catalog`; here we only
// return the `(photo_id, person_tag_id)` the caller must assign.

/// Confirm a suggested/unassigned face: mark it `confirmed`. Returns `(photo_id, person_tag_id)`
/// so the caller can `assign_tag` the photo through the catalog (XMP + merge). Errors if the
/// face has no person assigned (nothing to confirm).
pub fn accept(conn: &Connection, face_id: i64) -> rusqlite::Result<(i64, i64)> {
    let row: Option<(i64, Option<i64>)> = conn
        .query_row(
            "SELECT photo_id, person_tag_id FROM faces__faces WHERE id = ?1",
            [face_id],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
        )
        .optional()?;
    let (photo_id, person) = row.ok_or_else(|| {
        rusqlite::Error::QueryReturnedNoRows
    })?;
    let person_tag_id = person.ok_or_else(|| {
        // No person to confirm — surface as a constraint failure.
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
            Some("face has no assigned person to accept".into()),
        )
    })?;
    conn.execute(
        "UPDATE faces__faces
            SET state = ?2, match_confidence = 1.0, cluster_id = NULL
          WHERE id = ?1",
        rusqlite::params![face_id, STATE_CONFIRMED],
    )?;
    Ok((photo_id, person_tag_id))
}

/// Reject the face's currently-suggested/assigned person: remember the `(face, person)` pair
/// so it is never re-proposed, and return the face to `unassigned`. No-op detail: if the face
/// has no person assigned there is nothing to remember and the face is simply left unassigned.
pub fn reject(conn: &Connection, face_id: i64, now: i64) -> rusqlite::Result<()> {
    let person: Option<i64> = conn
        .query_row(
            "SELECT person_tag_id FROM faces__faces WHERE id = ?1",
            [face_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();
    if let Some(person_tag_id) = person {
        conn.execute(
            "INSERT OR IGNORE INTO faces__rejections (face_id, person_tag_id, rejected_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![face_id, person_tag_id, now],
        )?;
    }
    conn.execute(
        "UPDATE faces__faces
            SET state = ?2, person_tag_id = NULL, match_confidence = NULL, cluster_id = NULL
          WHERE id = ?1",
        rusqlite::params![face_id, STATE_UNASSIGNED],
    )?;
    Ok(())
}

/// Mark a face `ignored`: excluded from centroids and suggestions but kept so re-indexing
/// doesn't resurrect it. Clears any person/cluster association.
pub fn ignore(conn: &Connection, face_id: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE faces__faces
            SET state = ?2, person_tag_id = NULL, match_confidence = NULL, cluster_id = NULL
          WHERE id = ?1",
        rusqlite::params![face_id, STATE_IGNORED],
    )?;
    Ok(())
}

/// Manually assign a face to a specific person tag and confirm it (`source='manual'`).
/// Returns `(photo_id, tag_id)` so the caller can `assign_tag` the photo. A manual assignment
/// clears any stale rejection of that exact pair (the user changed their mind).
pub fn assign(conn: &Connection, face_id: i64, tag_id: i64) -> rusqlite::Result<(i64, i64)> {
    let photo_id: i64 = conn.query_row(
        "SELECT photo_id FROM faces__faces WHERE id = ?1",
        [face_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "DELETE FROM faces__rejections WHERE face_id = ?1 AND person_tag_id = ?2",
        rusqlite::params![face_id, tag_id],
    )?;
    conn.execute(
        "UPDATE faces__faces
            SET person_tag_id = ?2, state = ?3, source = ?4,
                match_confidence = 1.0, cluster_id = NULL
          WHERE id = ?1",
        rusqlite::params![face_id, tag_id, STATE_CONFIRMED, SOURCE_MANUAL],
    )?;
    Ok((photo_id, tag_id))
}

/// Confirm all faces in a cluster against a (freshly created/bound) person tag: assign the tag,
/// mark them `confirmed` with `source='manual'`, and clear their cluster membership. Returns the
/// distinct `(photo_id)` list the caller must `assign_tag` for. The tag itself is created by the
/// caller (which owns the `Catalog`) — here we only bind the member faces.
pub fn name_cluster(conn: &Connection, cluster_id: i64, tag_id: i64) -> rusqlite::Result<Vec<i64>> {
    // Collect the photos the members belong to (for photo-level tag assignment).
    let photos: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT photo_id FROM faces__faces WHERE cluster_id = ?1",
        )?;
        let rows = stmt.query_map([cluster_id], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    conn.execute(
        "UPDATE faces__faces
            SET person_tag_id = ?2, state = ?3, source = ?4,
                match_confidence = 1.0, cluster_id = NULL
          WHERE cluster_id = ?1",
        rusqlite::params![cluster_id, tag_id, STATE_CONFIRMED, SOURCE_MANUAL],
    )?;
    // The cluster is now empty; drop its bookkeeping row.
    conn.execute("DELETE FROM faces__clusters WHERE id = ?1", [cluster_id])?;
    Ok(photos)
}

// ── Math helpers ────────────────────────────────────────────────────────────────

/// L2-normalize a vector into a unit vector (safe on a zero vector → returns it unchanged).
fn l2_normalize(v: &[f32]) -> Vec<f32> {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter().map(|x| x / norm).collect()
    } else {
        v.to_vec()
    }
}

// ── Hungarian algorithm (Kuhn–Munkres, O(n³)) ───────────────────────────────────

/// Solve the rectangular-padded-to-square **minimum-cost assignment** problem with the
/// Jonker-style O(n³) Kuhn–Munkres algorithm (the standard potentials/augmenting-path form).
///
/// Input `cost` must be a square `n × n` matrix (callers pad rectangular problems with a
/// large constant). Returns a `Vec<usize>` of length `n` where `out[r]` is the column assigned
/// to row `r`; the assignment is a permutation minimizing the total cost.
///
/// Uses `f64` internally for numerical headroom. This is hand-rolled (not the `pathfinding`
/// crate) so the plugin carries no extra dependency and the algorithm is unit-tested directly.
pub fn hungarian_min_cost(cost: &[Vec<f32>]) -> Vec<usize> {
    let n = cost.len();
    if n == 0 {
        return Vec::new();
    }
    debug_assert!(cost.iter().all(|row| row.len() == n), "cost must be square");

    const INF: f64 = f64::INFINITY;
    // 1-indexed potentials/arrays following the classic e-maxx implementation.
    let mut u = vec![0.0f64; n + 1];
    let mut v = vec![0.0f64; n + 1];
    let mut p = vec![0usize; n + 1]; // p[j] = row matched to column j (0 = none)
    let mut way = vec![0usize; n + 1];

    for i in 1..=n {
        p[0] = i;
        let mut j0 = 0usize;
        let mut minv = vec![INF; n + 1];
        let mut used = vec![false; n + 1];
        loop {
            used[j0] = true;
            let i0 = p[j0];
            let mut delta = INF;
            let mut j1 = 0usize;
            for j in 1..=n {
                if !used[j] {
                    let cur = cost[i0 - 1][j - 1] as f64 - u[i0] - v[j];
                    if cur < minv[j] {
                        minv[j] = cur;
                        way[j] = j0;
                    }
                    if minv[j] < delta {
                        delta = minv[j];
                        j1 = j;
                    }
                }
            }
            for j in 0..=n {
                if used[j] {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    minv[j] -= delta;
                }
            }
            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }
        // Augment along the found path.
        loop {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }

    // p[j] = row assigned to column j → invert to row → column.
    let mut result = vec![0usize; n];
    for j in 1..=n {
        if p[j] >= 1 {
            result[p[j] - 1] = j - 1;
        }
    }
    result
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::faces::store;

    // ── Test scaffolding ────────────────────────────────────────────────────────

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE photos (id INTEGER PRIMARY KEY, uuid TEXT DEFAULT '', path TEXT DEFAULT '');
             CREATE TABLE tags (id INTEGER PRIMARY KEY, full_path TEXT NOT NULL);
             CREATE TABLE photo_tags (photo_id INTEGER, tag_id INTEGER, created_at INTEGER,
                                      PRIMARY KEY (photo_id, tag_id));
             CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        store::ensure_schema(&conn).unwrap();
        conn
    }

    fn add_photo(conn: &Connection, id: i64) {
        conn.execute("INSERT INTO photos (id) VALUES (?1)", [id]).unwrap();
    }

    /// Insert a person tag under the default `People` root, returning its id.
    fn add_person(conn: &Connection, id: i64, name: &str) -> i64 {
        conn.execute(
            "INSERT INTO tags (id, full_path) VALUES (?1, ?2)",
            rusqlite::params![id, format!("People/{name}")],
        )
        .unwrap();
        id
    }

    fn tag_photo(conn: &Connection, photo_id: i64, tag_id: i64) {
        conn.execute(
            "INSERT OR IGNORE INTO photo_tags (photo_id, tag_id, created_at) VALUES (?1, ?2, 0)",
            rusqlite::params![photo_id, tag_id],
        )
        .unwrap();
    }

    /// A deterministic synthetic unit embedding built around a "prototype axis" `axis`, with a
    /// little `jitter` so two faces of the same person are close but not identical.
    fn embed(axis: usize, jitter: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; 512];
        v[axis] = 1.0;
        // Spread a small amount of energy into a neighbouring axis to simulate variation.
        v[(axis + 1) % 512] = jitter;
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / norm).collect()
    }

    /// Insert a detected (unassigned) face on a photo, returning its id.
    fn add_face(conn: &Connection, photo_id: i64, emb: &[f32]) -> i64 {
        let blob = embedding_to_blob(emb);
        store::insert_face(
            conn,
            photo_id,
            "[0,0,0.1,0.1]",
            "[]",
            0.99,
            Some(&blob),
            super::super::indexer::SOURCE_DETECT,
            0,
        )
        .unwrap()
    }

    fn face_state(conn: &Connection, face_id: i64) -> (String, Option<i64>, String) {
        conn.query_row(
            "SELECT state, person_tag_id, source FROM faces__faces WHERE id = ?1",
            [face_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .unwrap()
    }

    fn count_state(conn: &Connection, state: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM faces__faces WHERE state = ?1",
            [state],
            |r| r.get(0),
        )
        .unwrap()
    }

    // ── Hungarian correctness ─────────────────────────────────────────────────────

    /// Known 3×3 cost matrix with a unique optimal assignment.
    #[test]
    fn hungarian_known_matrix() {
        // Optimal: r0->c1 (2), r1->c0 (3), r2->c2 (4) = 9. (Classic textbook example.)
        let cost = vec![
            vec![4.0, 2.0, 8.0],
            vec![3.0, 6.0, 7.0],
            vec![9.0, 5.0, 4.0],
        ];
        let a = hungarian_min_cost(&cost);
        assert_eq!(a, vec![1, 0, 2]);
        let total: f32 = a.iter().enumerate().map(|(r, &c)| cost[r][c]).sum();
        assert!((total - 9.0).abs() < 1e-4, "total cost = {total}");
    }

    /// Identity is optimal when the diagonal is cheapest.
    #[test]
    fn hungarian_diagonal_optimal() {
        let cost = vec![
            vec![1.0, 9.0, 9.0],
            vec![9.0, 1.0, 9.0],
            vec![9.0, 9.0, 1.0],
        ];
        assert_eq!(hungarian_min_cost(&cost), vec![0, 1, 2]);
    }

    /// A cross assignment is optimal when off-diagonal is cheapest (2×2).
    #[test]
    fn hungarian_cross_optimal() {
        let cost = vec![vec![5.0, 1.0], vec![1.0, 5.0]];
        assert_eq!(hungarian_min_cost(&cost), vec![1, 0]);
    }

    /// 1×1 and empty edge cases.
    #[test]
    fn hungarian_trivial_sizes() {
        assert_eq!(hungarian_min_cost(&[vec![3.0]]), vec![0]);
        let empty: Vec<Vec<f32>> = Vec::new();
        assert!(hungarian_min_cost(&empty).is_empty());
    }

    // ── Auto-seed rules ───────────────────────────────────────────────────────────

    /// 1 face + 1 person tag → seeded (confirmed, source=seed).
    #[test]
    fn seed_one_face_one_tag() {
        let conn = mem_conn();
        add_photo(&conn, 1);
        let alice = add_person(&conn, 100, "Alice");
        tag_photo(&conn, 1, alice);
        let f = add_face(&conn, 1, &embed(0, 0.0));

        let out = run_matching(&conn, &MatchSettings::default(), 1000).unwrap();
        assert_eq!(out.seeded, 1);
        let (state, person, source) = face_state(&conn, f);
        assert_eq!(state, "confirmed");
        assert_eq!(person, Some(alice));
        assert_eq!(source, "seed");
    }

    /// 2 faces + 1 person tag → NO seed (ambiguous which face is the person).
    #[test]
    fn seed_two_faces_one_tag_no_seed() {
        let conn = mem_conn();
        add_photo(&conn, 1);
        let alice = add_person(&conn, 100, "Alice");
        tag_photo(&conn, 1, alice);
        add_face(&conn, 1, &embed(0, 0.0));
        add_face(&conn, 1, &embed(1, 0.0));

        let out = run_matching(&conn, &MatchSettings::default(), 1000).unwrap();
        assert_eq!(out.seeded, 0, "2 faces + 1 tag must not seed");
        assert_eq!(count_state(&conn, "confirmed"), 0);
    }

    /// 1 face + 2 person tags → NO seed (ambiguous which person the face is).
    #[test]
    fn seed_one_face_two_tags_no_seed() {
        let conn = mem_conn();
        add_photo(&conn, 1);
        let alice = add_person(&conn, 100, "Alice");
        let bob = add_person(&conn, 101, "Bob");
        tag_photo(&conn, 1, alice);
        tag_photo(&conn, 1, bob);
        add_face(&conn, 1, &embed(0, 0.0));

        let out = run_matching(&conn, &MatchSettings::default(), 1000).unwrap();
        assert_eq!(out.seeded, 0, "1 face + 2 person tags must not seed");
        assert_eq!(count_state(&conn, "confirmed"), 0);
    }

    /// A non-person tag (outside the People branch) does not count toward the seed rule.
    #[test]
    fn seed_ignores_non_person_tags() {
        let conn = mem_conn();
        add_photo(&conn, 1);
        let alice = add_person(&conn, 100, "Alice");
        // A landscape tag not under People — must be ignored by the person-tag filter.
        conn.execute(
            "INSERT INTO tags (id, full_path) VALUES (200, 'Places/Beach')",
            [],
        )
        .unwrap();
        tag_photo(&conn, 1, alice);
        tag_photo(&conn, 1, 200);
        let f = add_face(&conn, 1, &embed(0, 0.0));

        let out = run_matching(&conn, &MatchSettings::default(), 1000).unwrap();
        assert_eq!(out.seeded, 1, "non-person tag must not block the 1+1 seed");
        assert_eq!(face_state(&conn, f).1, Some(alice));
    }

    // ── Constrained (Hungarian) match ─────────────────────────────────────────────

    /// A photo with 2 faces and 2 person tags (both with centroids) gets each face suggested
    /// to the correct person via Hungarian assignment.
    #[test]
    fn constrained_match_assigns_correctly() {
        let conn = mem_conn();
        let alice = add_person(&conn, 100, "Alice");
        let bob = add_person(&conn, 101, "Bob");

        // Seed centroids: single-face+single-tag photos for each.
        add_photo(&conn, 1);
        tag_photo(&conn, 1, alice);
        add_face(&conn, 1, &embed(0, 0.0)); // Alice axis 0

        add_photo(&conn, 2);
        tag_photo(&conn, 2, bob);
        add_face(&conn, 2, &embed(10, 0.0)); // Bob axis 10

        // The target photo: two faces (one near Alice, one near Bob), both tags present.
        add_photo(&conn, 3);
        tag_photo(&conn, 3, alice);
        tag_photo(&conn, 3, bob);
        let fa = add_face(&conn, 3, &embed(0, 0.05)); // close to Alice
        let fb = add_face(&conn, 3, &embed(10, 0.05)); // close to Bob

        let out = run_matching(&conn, &MatchSettings::default(), 1000).unwrap();
        assert_eq!(out.seeded, 2, "both single-face photos seed");
        assert_eq!(out.constrained, 2, "both target faces constrained-matched");

        assert_eq!(face_state(&conn, fa), ("suggested".into(), Some(alice), "match".into()));
        assert_eq!(face_state(&conn, fb), ("suggested".into(), Some(bob), "match".into()));
    }

    // ── Open matching + threshold boundary ────────────────────────────────────────

    /// A face in an untagged photo matches the nearest centroid above threshold.
    #[test]
    fn open_match_above_threshold() {
        let conn = mem_conn();
        let alice = add_person(&conn, 100, "Alice");
        add_photo(&conn, 1);
        tag_photo(&conn, 1, alice);
        add_face(&conn, 1, &embed(0, 0.0)); // seeds Alice's centroid (axis 0)

        // Untagged photo, face very close to Alice.
        add_photo(&conn, 2);
        let f = add_face(&conn, 2, &embed(0, 0.05));

        let out = run_matching(&conn, &MatchSettings::default(), 1000).unwrap();
        assert_eq!(out.open, 1);
        assert_eq!(face_state(&conn, f).1, Some(alice));
    }

    /// A far-away face (cosine below threshold) is NOT open-matched; it clusters instead.
    #[test]
    fn open_match_below_threshold_clusters() {
        let conn = mem_conn();
        let alice = add_person(&conn, 100, "Alice");
        add_photo(&conn, 1);
        tag_photo(&conn, 1, alice);
        add_face(&conn, 1, &embed(0, 0.0)); // Alice on axis 0

        // Untagged photo, face on an orthogonal axis (cosine ~0 << 0.45).
        add_photo(&conn, 2);
        let f = add_face(&conn, 2, &embed(50, 0.0));

        let out = run_matching(&conn, &MatchSettings::default(), 1000).unwrap();
        assert_eq!(out.open, 0, "orthogonal face must not open-match");
        assert_eq!(out.clustered, 1, "it must fall through to clustering");
        // Face stays unassigned (no person) but gets a cluster_id.
        let (state, person, _) = face_state(&conn, f);
        assert_eq!(state, "unassigned");
        assert_eq!(person, None);
        let cid: Option<i64> = conn
            .query_row("SELECT cluster_id FROM faces__faces WHERE id = ?1", [f], |r| r.get(0))
            .unwrap();
        assert!(cid.is_some(), "leftover face must be clustered");
    }

    /// Threshold boundary: a similarity exactly equal to the threshold does NOT match
    /// (strictly-greater rule), just above does.
    #[test]
    fn threshold_boundary_is_strict() {
        let conn = mem_conn();
        let alice = add_person(&conn, 100, "Alice");
        add_photo(&conn, 1);
        tag_photo(&conn, 1, alice);
        // Alice centroid = unit vector on axis 0.
        add_face(&conn, 1, &embed(0, 0.0));

        // Craft a face whose cosine to axis-0 is exactly 0.45.
        let mut v = vec![0.0f32; 512];
        v[0] = 0.45;
        // remaining magnitude on an orthogonal axis so the vector is unit and cosine = 0.45.
        v[300] = (1.0 - 0.45f32 * 0.45).sqrt();
        add_photo(&conn, 2);
        let f_eq = add_face(&conn, 2, &v);

        let mut settings = MatchSettings::default();
        settings.threshold = 0.45;
        let out = run_matching(&conn, &settings, 1000).unwrap();
        // Exactly at threshold → not open-matched (strict >).
        assert_eq!(face_state(&conn, f_eq).1, None, "cosine == threshold must not match");
        assert!(out.open == 0);
    }

    // ── Rejection memory ──────────────────────────────────────────────────────────

    /// Once a (face, person) pair is rejected it is never re-proposed on re-run.
    #[test]
    fn rejection_is_remembered() {
        let conn = mem_conn();
        let alice = add_person(&conn, 100, "Alice");
        add_photo(&conn, 1);
        tag_photo(&conn, 1, alice);
        add_face(&conn, 1, &embed(0, 0.0)); // seeds Alice

        add_photo(&conn, 2);
        let f = add_face(&conn, 2, &embed(0, 0.05)); // would open-match Alice

        // First run suggests Alice.
        let out1 = run_matching(&conn, &MatchSettings::default(), 1000).unwrap();
        assert_eq!(out1.open, 1);
        assert_eq!(face_state(&conn, f).1, Some(alice));

        // User rejects.
        reject(&conn, f, 1001).unwrap();
        assert_eq!(face_state(&conn, f).1, None);

        // Re-run must NOT re-propose Alice for this face → it clusters instead.
        let out2 = run_matching(&conn, &MatchSettings::default(), 1002).unwrap();
        assert_eq!(out2.open, 0, "rejected pair must not be re-proposed");
        assert_eq!(face_state(&conn, f).1, None);
        assert_eq!(out2.clustered, 1);
    }

    // ── Ignore exclusion ──────────────────────────────────────────────────────────

    /// An ignored face is excluded from centroids and never suggested; a re-run leaves it
    /// ignored.
    #[test]
    fn ignore_excludes_from_everything() {
        let conn = mem_conn();
        let alice = add_person(&conn, 100, "Alice");
        add_photo(&conn, 1);
        tag_photo(&conn, 1, alice);
        add_face(&conn, 1, &embed(0, 0.0)); // Alice centroid

        // A photobomber face we ignore.
        add_photo(&conn, 2);
        let bomber = add_face(&conn, 2, &embed(0, 0.05));
        ignore(&conn, bomber).unwrap();

        let out = run_matching(&conn, &MatchSettings::default(), 1000).unwrap();
        assert_eq!(out.open, 0);
        assert_eq!(out.clustered, 0, "ignored face must not cluster");
        let (state, person, _) = face_state(&conn, bomber);
        assert_eq!(state, "ignored");
        assert_eq!(person, None);
    }

    /// A confirmed face that is later marked ignored no longer contributes to a centroid.
    #[test]
    fn ignored_confirmed_face_drops_from_centroid() {
        let conn = mem_conn();
        let alice = add_person(&conn, 100, "Alice");
        // Two confirmed Alice faces via seeds.
        add_photo(&conn, 1);
        tag_photo(&conn, 1, alice);
        let f1 = add_face(&conn, 1, &embed(0, 0.0));
        run_matching(&conn, &MatchSettings::default(), 1000).unwrap();
        assert_eq!(face_state(&conn, f1).0, "confirmed");

        // Ignore the only confirmed face → Alice has no centroid now.
        ignore(&conn, f1).unwrap();

        add_photo(&conn, 2);
        let f = add_face(&conn, 2, &embed(0, 0.05));
        let out = run_matching(&conn, &MatchSettings::default(), 1001).unwrap();
        assert_eq!(out.people, 0, "ignored confirmed face leaves person centroid-less");
        assert_eq!(out.open, 0);
        assert_eq!(face_state(&conn, f).1, None);
    }

    // ── Idempotent re-run ─────────────────────────────────────────────────────────

    /// Re-running with no changes proposes nothing new and never downgrades confirmed rows.
    #[test]
    fn rerun_is_idempotent() {
        let conn = mem_conn();
        let alice = add_person(&conn, 100, "Alice");
        add_photo(&conn, 1);
        tag_photo(&conn, 1, alice);
        let seed = add_face(&conn, 1, &embed(0, 0.0));

        add_photo(&conn, 2);
        let f = add_face(&conn, 2, &embed(0, 0.05));

        let out1 = run_matching(&conn, &MatchSettings::default(), 1000).unwrap();
        assert_eq!(out1.seeded, 1);
        assert_eq!(out1.open, 1);
        let suggested_after_1 = count_state(&conn, "suggested");
        let confirmed_after_1 = count_state(&conn, "confirmed");

        // Re-run: same inputs.
        let out2 = run_matching(&conn, &MatchSettings::default(), 1001).unwrap();
        // Seed already confirmed → not re-seeded.
        assert_eq!(out2.seeded, 0, "confirmed seed must not be re-seeded");
        // Same single open match again (recomputed, not stacked).
        assert_eq!(out2.open, 1);
        assert_eq!(count_state(&conn, "suggested"), suggested_after_1, "no duplicate suggestions");
        assert_eq!(count_state(&conn, "confirmed"), confirmed_after_1, "confirmed count stable");
        // The confirmed seed is untouched.
        assert_eq!(face_state(&conn, seed), ("confirmed".into(), Some(alice), "seed".into()));
        // The suggestion still points at Alice.
        assert_eq!(face_state(&conn, f).1, Some(alice));

        // No duplicate face rows were created.
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM faces__faces", [], |r| r.get(0)).unwrap();
        assert_eq!(total, 2);
    }

    /// Re-running clustering with no changes must NOT grow existing clusters: cluster count,
    /// per-cluster `size`, and each face's `cluster_id` all stay stable, and `out.clustered`
    /// reports the same number each run. (Regression: a prior version grew the persisted
    /// `faces__clusters` rows on every re-run — size 2 → 4 → 6 …, corrupting the People count.)
    #[test]
    fn clustering_is_idempotent_on_rerun() {
        let conn = mem_conn();
        // Two untagged photos with similar faces (no known people) → one 2-member cluster.
        add_photo(&conn, 1);
        let f1 = add_face(&conn, 1, &embed(7, 0.0));
        add_photo(&conn, 2);
        let f2 = add_face(&conn, 2, &embed(7, 0.03));

        let out1 = run_matching(&conn, &MatchSettings::default(), 1000).unwrap();
        assert_eq!(out1.clustered, 2);

        let clusters_after_1 = |conn: &Connection| -> Vec<(i64, i64)> {
            let mut stmt = conn
                .prepare("SELECT id, size FROM faces__clusters ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let before = clusters_after_1(&conn);
        assert_eq!(before.len(), 1, "one cluster");
        assert_eq!(before[0].1, 2, "cluster holds both faces");
        let cid1: i64 = conn
            .query_row("SELECT cluster_id FROM faces__faces WHERE id = ?1", [f1], |r| r.get(0))
            .unwrap();

        // Re-run with identical inputs.
        let out2 = run_matching(&conn, &MatchSettings::default(), 1001).unwrap();
        assert_eq!(out2.clustered, 2, "same faces re-cluster, count unchanged");

        // Exactly one cluster, size still 2 (not 4) — no membership double-count.
        let after = clusters_after_1(&conn);
        assert_eq!(after.len(), 1, "still one cluster after re-run");
        assert_eq!(after[0].1, 2, "cluster size must NOT inflate on re-run");
        // Both faces still clustered together.
        let cid1b: i64 = conn
            .query_row("SELECT cluster_id FROM faces__faces WHERE id = ?1", [f1], |r| r.get(0))
            .unwrap();
        let cid2b: i64 = conn
            .query_row("SELECT cluster_id FROM faces__faces WHERE id = ?1", [f2], |r| r.get(0))
            .unwrap();
        assert_eq!(cid1b, cid2b, "faces still share a cluster");
        let _ = cid1; // (cluster id may be renumbered when rebuilt; membership is what matters)
    }

    // ── accept / assign side-effect returns ──────────────────────────────────────

    /// accept returns (photo, person) and confirms; assign confirms manually + clears rejection.
    #[test]
    fn accept_and_assign_mutations() {
        let conn = mem_conn();
        let alice = add_person(&conn, 100, "Alice");
        let bob = add_person(&conn, 101, "Bob");
        add_photo(&conn, 1);
        tag_photo(&conn, 1, alice);
        add_face(&conn, 1, &embed(0, 0.0)); // seed Alice

        add_photo(&conn, 2);
        let f = add_face(&conn, 2, &embed(0, 0.05));
        run_matching(&conn, &MatchSettings::default(), 1000).unwrap();
        assert_eq!(face_state(&conn, f).1, Some(alice));

        // Accept it.
        let (photo, person) = accept(&conn, f).unwrap();
        assert_eq!((photo, person), (2, alice));
        assert_eq!(face_state(&conn, f).0, "confirmed");

        // Manually reassign to Bob.
        let (photo2, tag2) = assign(&conn, f, bob).unwrap();
        assert_eq!((photo2, tag2), (2, bob));
        assert_eq!(face_state(&conn, f), ("confirmed".into(), Some(bob), "manual".into()));
    }

    /// accept on a face with no assigned person is an error.
    #[test]
    fn accept_without_person_errors() {
        let conn = mem_conn();
        add_photo(&conn, 1);
        let f = add_face(&conn, 1, &embed(0, 0.0));
        assert!(accept(&conn, f).is_err());
    }

    // ── name_cluster ──────────────────────────────────────────────────────────────

    /// Naming a cluster binds a person tag to all its members and confirms them.
    #[test]
    fn name_cluster_confirms_members() {
        let conn = mem_conn();
        // Two untagged photos with similar faces (no known people) → one cluster.
        add_photo(&conn, 1);
        let f1 = add_face(&conn, 1, &embed(7, 0.0));
        add_photo(&conn, 2);
        let f2 = add_face(&conn, 2, &embed(7, 0.03));

        let out = run_matching(&conn, &MatchSettings::default(), 1000).unwrap();
        assert_eq!(out.clustered, 2, "both similar faces land in one cluster");
        let cid: i64 = conn
            .query_row("SELECT cluster_id FROM faces__faces WHERE id = ?1", [f1], |r| r.get(0))
            .unwrap();
        let cid2: i64 = conn
            .query_row("SELECT cluster_id FROM faces__faces WHERE id = ?1", [f2], |r| r.get(0))
            .unwrap();
        assert_eq!(cid, cid2, "similar faces share a cluster");

        // Name it "Carol".
        let carol = add_person(&conn, 102, "Carol");
        let photos = name_cluster(&conn, cid, carol).unwrap();
        assert_eq!(photos.len(), 2);
        assert!(photos.contains(&1) && photos.contains(&2));
        assert_eq!(face_state(&conn, f1), ("confirmed".into(), Some(carol), "manual".into()));
        assert_eq!(face_state(&conn, f2), ("confirmed".into(), Some(carol), "manual".into()));
        // Cluster row is gone.
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM faces__clusters WHERE id = ?1", [cid], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }

    /// Person tags resolve correctly under a NON-ASCII people-root. `substr`/`length` count
    /// characters, so binding a Rust byte-length would over-count for multibyte roots and
    /// silently match nothing. (Regression: matcher no-op'd for a non-ASCII root.)
    #[test]
    fn person_tags_resolve_under_non_ascii_root() {
        let conn = mem_conn();
        // Non-ASCII root "Persøner" (ø is 2 UTF-8 bytes → byte len ≠ char len).
        conn.execute(
            "INSERT INTO tags (id, full_path) VALUES (100, 'Persøner/Ålev')",
            [],
        )
        .unwrap();
        // A sibling outside the branch must not match.
        conn.execute(
            "INSERT INTO tags (id, full_path) VALUES (200, 'Places/Beach')",
            [],
        )
        .unwrap();

        let ids = person_tag_ids(&conn, "Persøner").unwrap();
        assert!(ids.contains(&100), "descendant of non-ASCII root must resolve");
        assert!(!ids.contains(&200), "outside-branch tag must not resolve");

        // End-to-end: a 1-face + 1-person-tag photo under the non-ASCII root seeds.
        add_photo(&conn, 1);
        tag_photo(&conn, 1, 100);
        let f = add_face(&conn, 1, &embed(0, 0.0));
        let mut settings = MatchSettings::default();
        settings.people_root = "Persøner".to_string();
        let out = run_matching(&conn, &settings, 1000).unwrap();
        assert_eq!(out.seeded, 1, "seed must fire under a non-ASCII root");
        assert_eq!(face_state(&conn, f).1, Some(100));
    }

    // ── Settings loading ──────────────────────────────────────────────────────────

    /// Settings load from the table with defaults for missing/blank keys.
    #[test]
    fn settings_load_with_overrides() {
        let conn = mem_conn();
        // Defaults when unset.
        let s = MatchSettings::load(&conn).unwrap();
        assert_eq!(s.people_root, "People");
        assert!((s.threshold - 0.45).abs() < 1e-6);

        // Overrides.
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, 'Faces/Person'), (?2, '0.6')",
            [PEOPLE_ROOT_SETTING, THRESHOLD_SETTING],
        )
        .unwrap();
        let s2 = MatchSettings::load(&conn).unwrap();
        assert_eq!(s2.people_root, "Faces/Person");
        assert!((s2.threshold - 0.6).abs() < 1e-6);

        // Blank / garbage falls back to defaults.
        conn.execute(
            "UPDATE settings SET value = '  ' WHERE key = ?1",
            [PEOPLE_ROOT_SETTING],
        )
        .unwrap();
        conn.execute(
            "UPDATE settings SET value = 'notanumber' WHERE key = ?1",
            [THRESHOLD_SETTING],
        )
        .unwrap();
        let s3 = MatchSettings::load(&conn).unwrap();
        assert_eq!(s3.people_root, "People");
        assert!((s3.threshold - 0.45).abs() < 1e-6);
    }
}
