//! Burst grouping engine (H15b).
//!
//! A **pure function** (`group_into_clusters`) that takes a slice of [`BurstPhoto`]s and
//! returns a `Vec<Cluster>` — no I/O, no catalog, no async.
//!
//! ## Algorithm
//!
//! 1. **Sort** photos by `capture_time` (photos without a timestamp go in their own
//!    single-photo clusters at the end — they cannot form time groups).
//!
//! 2. **Time-gap split**: iterate in capture-time order. Open a new group whenever the
//!    gap from the previous photo exceeds `time_gap_secs` (default 15 s). All photos
//!    in a group were shot within `time_gap_secs` of their neighbours.
//!
//! 3. **Hamming-distance split within each time group**: walk the time-sorted group and
//!    compare each adjacent pair's dHash. When the distance exceeds `hamming_threshold`
//!    (default 10), they belong to different scenes (reframe / subject change) and
//!    the group is split at that boundary. This step is skipped for photos whose `phash`
//!    is `None` (not yet hashed); those photos are always kept in the same cluster as
//!    their neighbours.
//!
//! 4. **Representative selection** per cluster (see [`select_representative`]):
//!    - **Highest rating** among all cluster members (ties broken by sharpness).
//!    - **Highest sharpness** when ratings are equal (uses `photo.sharpness` when stored;
//!      falls back to the H16a scorer applied to thumbnail bytes when `sharpness` is
//!      `None` — the caller provides the optional `thumb_fn` for this).
//!
//! ## Settings
//!
//! Both thresholds are read from the `ai` plugin's settings:
//! - `ai.burst_time_gap_secs`     — default `15`
//! - `ai.burst_hamming_threshold` — default `10`
//!
//! Callers can override them by constructing [`BurstConfig`] directly (the default
//! impl uses the same defaults as above).

/// Configuration for the burst grouping engine. Construct from catalog settings or use
/// [`BurstConfig::default`] to get the standard thresholds.
#[derive(Debug, Clone)]
pub struct BurstConfig {
    /// Maximum gap (in seconds) between adjacent photos that still belongs to the same
    /// burst. Larger gap → new group.
    pub time_gap_secs: i64,
    /// Maximum Hamming distance between adjacent dHashes within a time group that still
    /// belongs to the same cluster. Larger distance → split. Ignored (treated as ∞) when
    /// a photo's hash is `None`.
    pub hamming_threshold: u32,
}

impl Default for BurstConfig {
    fn default() -> Self {
        BurstConfig {
            time_gap_secs: 15,
            hamming_threshold: 10,
        }
    }
}

/// One photo as seen by the burst grouping engine. All numeric fields except `id` and
/// `rating` are optional because they may not be available for every photo (pre-index,
/// offline, etc.).
#[derive(Debug, Clone)]
pub struct BurstPhoto {
    /// Stable row ID (so callers can map back to the catalog after clustering).
    pub id: i64,
    /// Capture timestamp, unix epoch seconds (parsed from `photos.capture_time`). `None`
    /// when the photo has no capture-time EXIF metadata.
    pub capture_ts: Option<i64>,
    /// 64-bit dHash (from `photos.phash`). `None` when not yet indexed.
    pub phash: Option<u64>,
    /// 0–5 star rating (from `photos.rating`). 0 = unrated.
    pub rating: i64,
    /// Pre-computed tiled sharpness score (from `photos.sharpness`). `None` = not yet
    /// scored; the caller may supply a `thumb_fn` for on-demand fallback scoring.
    pub sharpness: Option<f64>,
}

/// A cluster of visually similar photos shot within a continuous burst. One cluster →
/// one AI dispatch (only the representative frame is sent; suggestions propagate to all
/// members).
#[derive(Debug, Clone)]
pub struct Cluster {
    /// IDs of all photos in this cluster, in capture-time order (stable across calls).
    pub photo_ids: Vec<i64>,
    /// The index into `photo_ids` of the representative frame (the one to dispatch to
    /// the AI provider). Callers should use `photo_ids[representative_idx]`.
    pub representative_idx: usize,
}

impl Cluster {
    /// Convenience: the `id` of the representative photo.
    pub fn representative_id(&self) -> i64 {
        self.photo_ids[self.representative_idx]
    }
}

/// Split `photos` into burst clusters according to `cfg`.
///
/// `thumb_fn` is called **only** when a photo's `sharpness` is `None` and we need a
/// tiebreaker between candidates with equal ratings. It receives the photo's `id` and
/// should return the thumbnail bytes for on-demand Laplacian scoring; returning `None`
/// (or the closure being `None` itself) makes the sharpness fall back to `0.0` for
/// that photo. The closure is never called if every candidate in the cluster already
/// has a `sharpness` score.
///
/// The returned clusters are ordered by the capture time of their first member. Single-
/// photo clusters (either genuinely alone or without a timestamp) are included.
pub fn group_into_clusters<F>(
    mut photos: Vec<BurstPhoto>,
    cfg: &BurstConfig,
    thumb_fn: Option<&F>,
) -> Vec<Cluster>
where
    F: Fn(i64) -> Option<Vec<u8>> + ?Sized,
{
    if photos.is_empty() {
        return Vec::new();
    }

    // Separate timestamped from un-timestamped. Un-timestamped cannot form time groups.
    let (mut timed, untimed): (Vec<_>, Vec<_>) =
        photos.drain(..).partition(|p| p.capture_ts.is_some());

    // Sort timestamped photos by capture time (then by id for stability).
    timed.sort_by_key(|p| (p.capture_ts.unwrap_or(0), p.id));

    // Step 1 + 2: time-gap split into time groups, then Hamming-split within each group.
    let mut clusters: Vec<Vec<BurstPhoto>> = Vec::new();

    let mut group: Vec<BurstPhoto> = Vec::new();
    for photo in timed {
        if let Some(last) = group.last() {
            let gap = photo.capture_ts.unwrap_or(0) - last.capture_ts.unwrap_or(0);
            if gap > cfg.time_gap_secs {
                // Time gap exceeded — flush the current group.
                flush_group_into_clusters(group, cfg, &mut clusters);
                group = Vec::new();
            }
        }
        group.push(photo);
    }
    if !group.is_empty() {
        flush_group_into_clusters(group, cfg, &mut clusters);
    }

    // Un-timestamped photos each go into their own single-photo cluster at the end.
    for photo in untimed {
        clusters.push(vec![photo]);
    }

    // Build `Cluster` structs with representative indices.
    clusters
        .into_iter()
        .map(|members| build_cluster(members, thumb_fn))
        .collect()
}

/// Flush one time-group into one or more clusters by Hamming-distance splitting.
fn flush_group_into_clusters(
    group: Vec<BurstPhoto>,
    cfg: &BurstConfig,
    clusters: &mut Vec<Vec<BurstPhoto>>,
) {
    let mut current: Vec<BurstPhoto> = Vec::new();
    for photo in group {
        if let Some(prev) = current.last() {
            // Only split when BOTH photos have a hash; if either is None, keep together.
            if let (Some(h1), Some(h2)) = (prev.phash, photo.phash) {
                let d = crate::phash::hamming_distance(h1, h2);
                if d > cfg.hamming_threshold {
                    // Visual discontinuity — flush the current cluster.
                    clusters.push(std::mem::take(&mut current));
                }
            }
        }
        current.push(photo);
    }
    if !current.is_empty() {
        clusters.push(current);
    }
}

/// Convert a `Vec<BurstPhoto>` into a `Cluster`, picking the best representative.
fn build_cluster<F>(members: Vec<BurstPhoto>, thumb_fn: Option<&F>) -> Cluster
where
    F: Fn(i64) -> Option<Vec<u8>> + ?Sized,
{
    let rep_idx = select_representative(&members, thumb_fn);
    Cluster {
        photo_ids: members.iter().map(|p| p.id).collect(),
        representative_idx: rep_idx,
    }
}

/// Choose the index of the best representative in `members`:
/// 1. Highest `rating` (5-star wins over unrated).
/// 2. Among equal-rating members, highest `sharpness` (stored or computed on demand).
///
/// Returns `0` when `members` is empty (degenerate; callers never call this on an empty
/// slice because `flush_group_into_clusters` never emits empty vecs).
pub(crate) fn select_representative<F>(members: &[BurstPhoto], thumb_fn: Option<&F>) -> usize
where
    F: Fn(i64) -> Option<Vec<u8>> + ?Sized,
{
    if members.is_empty() {
        return 0;
    }

    // Find the maximum rating in this cluster.
    let max_rating = members.iter().map(|p| p.rating).max().unwrap_or(0);

    // Among photos with that rating, pick the sharpest.
    let candidates: Vec<usize> = members
        .iter()
        .enumerate()
        .filter(|(_, p)| p.rating == max_rating)
        .map(|(i, _)| i)
        .collect();

    if candidates.len() == 1 {
        return candidates[0];
    }

    // Tiebreak by sharpness. Resolve each candidate's sharpness, falling back to
    // the thumbnail scorer when the stored value is absent.
    candidates
        .into_iter()
        .max_by(|&a, &b| {
            let sa = resolve_sharpness(&members[a], thumb_fn);
            let sb = resolve_sharpness(&members[b], thumb_fn);
            sa.partial_cmp(&sb)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Stable tiebreaker by id so the result is deterministic.
                .then_with(|| members[b].id.cmp(&members[a].id))
        })
        .unwrap_or(0)
}

/// Return the sharpness score for `photo`. If `photo.sharpness` is already stored, use
/// it directly. Otherwise, if `thumb_fn` is provided, load the thumbnail, decode it, and
/// compute the tiled Laplacian-variance score (the H16a scorer). Falls back to `0.0` when
/// neither is available.
fn resolve_sharpness<F>(photo: &BurstPhoto, thumb_fn: Option<&F>) -> f64
where
    F: Fn(i64) -> Option<Vec<u8>> + ?Sized,
{
    if let Some(s) = photo.sharpness {
        return s;
    }
    if let Some(f) = thumb_fn {
        if let Some(bytes) = f(photo.id) {
            if let Ok(img) = image::load_from_memory(&bytes) {
                return crate::sharpness::score_image(&img);
            }
        }
    }
    0.0
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `BurstPhoto` with just a capture timestamp (no hash, no sharpness).
    fn p(id: i64, ts: i64, rating: i64) -> BurstPhoto {
        BurstPhoto { id, capture_ts: Some(ts), phash: None, rating, sharpness: None }
    }

    /// Build a `BurstPhoto` with a timestamp and a dHash.
    fn ph(id: i64, ts: i64, phash: u64) -> BurstPhoto {
        BurstPhoto {
            id,
            capture_ts: Some(ts),
            phash: Some(phash),
            rating: 0,
            sharpness: None,
        }
    }

    /// Build a `BurstPhoto` with timestamp, hash, rating, and sharpness.
    fn full(id: i64, ts: i64, phash: u64, rating: i64, sharpness: f64) -> BurstPhoto {
        BurstPhoto {
            id,
            capture_ts: Some(ts),
            phash: Some(phash),
            rating,
            sharpness: Some(sharpness),
        }
    }

    const NO_THUMB: Option<&dyn Fn(i64) -> Option<Vec<u8>>> = None;

    fn default_cfg() -> BurstConfig {
        BurstConfig::default()
    }

    // ── Gap splitting ────────────────────────────────────────────────────────

    #[test]
    fn gap_split_forms_two_clusters() {
        // Photos at t=0,5,10 are within 15s → one cluster;
        // a photo at t=40 is >15s after t=10 → a new cluster.
        let photos = vec![
            p(1, 0, 0),
            p(2, 5, 0),
            p(3, 10, 0),
            p(4, 40, 0),
        ];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        assert_eq!(clusters.len(), 2, "should split into two clusters");
        assert_eq!(clusters[0].photo_ids, vec![1, 2, 3]);
        assert_eq!(clusters[1].photo_ids, vec![4]);
    }

    #[test]
    fn single_photo_forms_own_cluster() {
        let photos = vec![p(42, 100, 3)];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].photo_ids, vec![42]);
        assert_eq!(clusters[0].representative_idx, 0);
    }

    #[test]
    fn photos_without_timestamp_each_get_own_cluster() {
        let photos = vec![
            BurstPhoto { id: 1, capture_ts: None, phash: None, rating: 0, sharpness: None },
            BurstPhoto { id: 2, capture_ts: None, phash: None, rating: 0, sharpness: None },
        ];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        assert_eq!(clusters.len(), 2, "each un-timestamped photo is its own cluster");
        assert_eq!(clusters[0].photo_ids, vec![1]);
        assert_eq!(clusters[1].photo_ids, vec![2]);
    }

    #[test]
    fn exactly_at_gap_boundary_stays_same_cluster() {
        // A gap of exactly `time_gap_secs` (15 s) should NOT split (strictly >).
        let photos = vec![p(1, 0, 0), p(2, 15, 0)];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        assert_eq!(clusters.len(), 1, "gap == threshold → same cluster");
    }

    #[test]
    fn one_second_over_gap_splits() {
        let photos = vec![p(1, 0, 0), p(2, 16, 0)];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        assert_eq!(clusters.len(), 2, "gap > threshold → split");
    }

    #[test]
    fn three_separate_bursts() {
        // Three bursts of 2 photos each, separated by >15s gaps.
        let photos = vec![
            p(1, 0, 0),
            p(2, 3, 0),
            p(3, 60, 0),
            p(4, 63, 0),
            p(5, 200, 0),
            p(6, 202, 0),
        ];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        assert_eq!(clusters.len(), 3);
        assert_eq!(clusters[0].photo_ids, vec![1, 2]);
        assert_eq!(clusters[1].photo_ids, vec![3, 4]);
        assert_eq!(clusters[2].photo_ids, vec![5, 6]);
    }

    // ── Hash (Hamming) splitting ─────────────────────────────────────────────

    #[test]
    fn hamming_split_within_time_group() {
        // Two photos very close in time but with completely different hashes.
        // hamming_distance(0, u64::MAX) = 64, which is >> default threshold of 10.
        let photos = vec![
            ph(1, 0, 0x0000_0000_0000_0000u64),
            ph(2, 5, 0xFFFF_FFFF_FFFF_FFFFu64),
        ];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        assert_eq!(clusters.len(), 2, "large Hamming distance should split the cluster");
        assert_eq!(clusters[0].photo_ids, vec![1]);
        assert_eq!(clusters[1].photo_ids, vec![2]);
    }

    #[test]
    fn similar_hashes_within_time_group_stay_together() {
        // Two photos with nearly identical hashes (distance 2, well below threshold 10).
        let h1 = 0b0000_0000_0000_0000u64;
        let h2 = 0b0000_0000_0000_0011u64; // 2 bits different
        let photos = vec![ph(1, 0, h1), ph(2, 5, h2)];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        assert_eq!(clusters.len(), 1, "similar hashes should stay in one cluster");
        assert_eq!(clusters[0].photo_ids, vec![1, 2]);
    }

    #[test]
    fn missing_hash_does_not_split() {
        // A photo without a hash is always kept with its neighbours.
        let photos = vec![
            ph(1, 0, 0x0000_0000_0000_0000u64),
            BurstPhoto { id: 2, capture_ts: Some(5), phash: None, rating: 0, sharpness: None },
            ph(3, 10, 0xFFFF_FFFF_FFFF_FFFFu64),
        ];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        // photo 2 has no hash → no split between 1↔2 or 2↔3
        assert_eq!(clusters.len(), 1, "missing hash should not trigger a split");
        assert_eq!(clusters[0].photo_ids, vec![1, 2, 3]);
    }

    #[test]
    fn hash_split_after_gap_split() {
        // Two time groups; the second group contains a hash split.
        let h_a = 0x0000_0000_0000_0000u64;
        let h_b = 0xFFFF_FFFF_FFFF_FFFFu64;
        let photos = vec![
            ph(1, 0, h_a),
            ph(2, 5, h_a),   // same cluster as 1
            ph(3, 100, h_a), // new time group (gap 95s)
            ph(4, 105, h_b), // hash split within second time group
        ];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        assert_eq!(clusters.len(), 3);
        assert_eq!(clusters[0].photo_ids, vec![1, 2]);
        assert_eq!(clusters[1].photo_ids, vec![3]);
        assert_eq!(clusters[2].photo_ids, vec![4]);
    }

    // ── Representative selection ─────────────────────────────────────────────

    #[test]
    fn highest_rating_wins() {
        // Photo 2 has rating 3, photo 1 has rating 1 → photo 2 is representative.
        let photos = vec![
            full(1, 0, 0, 1, 500.0),
            full(2, 3, 0, 3, 100.0), // higher rating, lower sharpness
        ];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        assert_eq!(clusters.len(), 1);
        assert_eq!(
            clusters[0].representative_id(),
            2,
            "highest rating must win over higher sharpness"
        );
    }

    #[test]
    fn sharpness_breaks_rating_tie() {
        // Same rating → sharper photo wins.
        let photos = vec![
            full(1, 0, 0, 2, 300.0),
            full(2, 3, 0, 2, 800.0), // same rating, higher sharpness
        ];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        assert_eq!(clusters.len(), 1);
        assert_eq!(
            clusters[0].representative_id(),
            2,
            "higher sharpness must win when ratings tie"
        );
    }

    #[test]
    fn rating_beats_sharpness_ordering() {
        // Confirm: rating is the primary key, sharpness is secondary.
        let photos = vec![
            full(1, 0, 0, 0, 9999.0), // unrated, extremely sharp
            full(2, 3, 0, 5, 1.0),    // 5-star, barely sharp
        ];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        assert_eq!(clusters.len(), 1);
        assert_eq!(
            clusters[0].representative_id(),
            2,
            "5-star must beat unrated even if unrated is sharper"
        );
    }

    #[test]
    fn unscored_photo_falls_back_to_zero_without_thumb_fn() {
        // When sharpness is None and no thumb_fn is provided, score defaults to 0.
        // The rated photo should still win over the unscored one.
        let photos = vec![
            BurstPhoto { id: 1, capture_ts: Some(0), phash: None, rating: 0, sharpness: None },
            BurstPhoto {
                id: 2,
                capture_ts: Some(3),
                phash: None,
                rating: 0,
                sharpness: Some(500.0),
            },
        ];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        assert_eq!(clusters.len(), 1);
        assert_eq!(
            clusters[0].representative_id(),
            2,
            "stored sharpness 500 must beat implicit 0 for unscored"
        );
    }

    #[test]
    fn thumb_fn_is_called_for_unscored_tiebreak() {
        // When ratings tie and one photo is unscored, the thumb_fn should be invoked to
        // compute its sharpness on demand. We synthesise JPEG-like bytes that decode to
        // a sharp image using `image` to build a checkerboard and encode it.
        use image::{GrayImage, Luma};
        // Build a sharp checkerboard and encode as PNG bytes (still decodable by `image`).
        let checker = GrayImage::from_fn(64, 64, |x, y| {
            Luma([if (x / 4 + y / 4) % 2 == 0 { 255u8 } else { 0u8 }])
        });
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(checker)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let sharp_png = buf.into_inner();

        // Photo 1: rating 0, sharpness None  → needs the thumb_fn
        // Photo 2: rating 0, sharpness 1.0   → pre-scored low
        // Expected: photo 1 wins (the checkerboard scores >> 1.0).
        let photos = vec![
            BurstPhoto { id: 1, capture_ts: Some(0), phash: None, rating: 0, sharpness: None },
            BurstPhoto {
                id: 2,
                capture_ts: Some(3),
                phash: None,
                rating: 0,
                sharpness: Some(1.0),
            },
        ];

        let thumb_fn_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_c = thumb_fn_calls.clone();
        let sharp_bytes = sharp_png;

        let thumb_fn = move |id: i64| -> Option<Vec<u8>> {
            calls_c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if id == 1 { Some(sharp_bytes.clone()) } else { None }
        };

        let clusters = group_into_clusters(photos, &default_cfg(), Some(&thumb_fn));
        assert_eq!(clusters.len(), 1);
        assert_eq!(
            clusters[0].representative_id(),
            1,
            "photo with higher on-demand sharpness should be chosen"
        );
        assert!(
            thumb_fn_calls.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "thumb_fn must be called for the unscored photo"
        );
    }

    #[test]
    fn empty_input_returns_empty_clusters() {
        let clusters = group_into_clusters::<fn(i64) -> Option<Vec<u8>>>(
            vec![],
            &default_cfg(),
            None,
        );
        assert!(clusters.is_empty());
    }

    #[test]
    fn input_order_does_not_matter_clusters_are_time_sorted() {
        // Input is deliberately out of time order — output must still be correctly grouped.
        let photos = vec![
            p(3, 10, 0),
            p(1, 0, 0),
            p(4, 200, 0), // well beyond 15s gap
            p(2, 5, 0),
        ];
        let clusters = group_into_clusters(photos, &default_cfg(), NO_THUMB);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].photo_ids, vec![1, 2, 3], "cluster 1: sorted by time");
        assert_eq!(clusters[1].photo_ids, vec![4], "cluster 2: isolated photo");
    }

    #[test]
    fn custom_thresholds_respected() {
        // Use a very large time gap (100s) — all photos stay in one time group.
        // Use a very small Hamming threshold (1) — even a 2-bit diff causes a split.
        let cfg = BurstConfig { time_gap_secs: 100, hamming_threshold: 1 };
        let h1 = 0b00u64;
        let h2 = 0b11u64; // 2 bits different → split with threshold 1
        let photos = vec![
            ph(1, 0, h1),
            ph(2, 50, h2), // within 100s gap but hashes differ by 2
        ];
        let clusters = group_into_clusters(photos, &cfg, NO_THUMB);
        assert_eq!(
            clusters.len(),
            2,
            "Hamming distance 2 should split with threshold 1"
        );
    }

    #[test]
    fn select_representative_three_way_tie_picks_sharpest() {
        // Three photos, same rating, different sharpness → sharpest wins.
        let members = vec![
            BurstPhoto { id: 1, capture_ts: Some(0), phash: None, rating: 2, sharpness: Some(100.0) },
            BurstPhoto { id: 2, capture_ts: Some(1), phash: None, rating: 2, sharpness: Some(500.0) },
            BurstPhoto { id: 3, capture_ts: Some(2), phash: None, rating: 2, sharpness: Some(300.0) },
        ];
        let idx = select_representative::<fn(i64) -> Option<Vec<u8>>>(&members, None);
        assert_eq!(idx, 1, "member at index 1 has the highest sharpness");
    }
}
