//! Volume reachability cache — keeps NAS/mount stat calls OFF the catalog lock.
//!
//! Statting a slow or offline NAS mount (`Path::is_dir`) can block for seconds. When
//! that stat runs while the single catalog `Mutex` is held, the whole app serializes
//! behind it. This cache does the stats on a blocking worker (never under the lock)
//! and remembers each volume's reachability for a short TTL, so repeated status/render
//! passes reuse a fresh answer instead of re-statting every time.
//!
//! What reachability is allowed to do depends on the caller's [`ResolveMode`]. Under
//! `OriginalRequired` it only ever *reorders* or *annotates* work — never changing which
//! physical copy the resolver returns (see [`pick_existing`]), so the "best available copy"
//! invariant (AGENTS.md) holds even under a stale cache. `FastDisplay` deliberately trusts
//! a cached-unreachable volume and falls back instead of statting it; that answer is only
//! ever a display fallback and never a statement that a row or file is gone.

use crate::catalog::{PathCandidate, ResolveMode};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// A short-lived cache of per-volume reachability (`volume_id → (reachable, checked_at)`).
pub struct VolumeHealth {
    inner: RwLock<HashMap<i64, (bool, Instant)>>,
    ttl: Duration,
}

impl Default for VolumeHealth {
    fn default() -> Self {
        Self::new()
    }
}

impl VolumeHealth {
    /// A cache with the default 15-second TTL.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            ttl: Duration::from_secs(15),
        }
    }

    /// A cache with an explicit TTL (used by tests).
    #[cfg(test)]
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// Refresh reachability for `volumes` (`(id, base_path)` pairs) and return the full
    /// `id → reachable` map. Fresh cached entries (younger than the TTL) are reused; the
    /// rest are re-statted.
    ///
    /// CRITICAL: the actual `is_dir` stats are done into a LOCAL map first, and only then
    /// is the write guard taken (briefly) to merge the results in. The write guard is
    /// NEVER held across a stat — a hung NFS stat must not block readers.
    ///
    /// MUST be called only from a blocking/worker context (each stat may block on I/O).
    pub fn refresh(&self, volumes: &[(i64, String)]) -> HashMap<i64, bool> {
        let now = Instant::now();

        // Pass 1: decide, using only the read guard, which volumes are still fresh.
        // Collect fresh answers and the list of volumes we must stat. The read guard is
        // dropped before any statting happens.
        let mut result: HashMap<i64, bool> = HashMap::with_capacity(volumes.len());
        let mut to_stat: Vec<(i64, String)> = Vec::new();
        {
            let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
            for (id, base) in volumes {
                match guard.get(id) {
                    Some(&(reachable, checked_at)) if now.duration_since(checked_at) < self.ttl => {
                        result.insert(*id, reachable);
                    }
                    _ => to_stat.push((*id, base.clone())),
                }
            }
        }

        // Pass 2: stat the stale/unknown volumes into a LOCAL map (NO lock held here).
        let mut freshly_statted: Vec<(i64, bool, Instant)> = Vec::with_capacity(to_stat.len());
        for (id, base) in &to_stat {
            let reachable = Path::new(base).is_dir();
            freshly_statted.push((*id, reachable, Instant::now()));
            result.insert(*id, reachable);
        }

        // Pass 3: merge the fresh results in under a brief write guard (no I/O here).
        if !freshly_statted.is_empty() {
            let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
            for (id, reachable, at) in freshly_statted {
                guard.insert(id, (reachable, at));
            }
        }

        result
    }

    /// The cached reachability of a volume if it's still fresh, else `None`. Never stats.
    pub fn reachable(&self, volume_id: i64) -> Option<bool> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard.get(&volume_id).and_then(|&(reachable, checked_at)| {
            if checked_at.elapsed() < self.ttl {
                Some(reachable)
            } else {
                None
            }
        })
    }

    /// Drop all cached entries (e.g. after volumes change or the catalog is re-rooted).
    pub fn invalidate(&self) {
        self.inner.write().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

// Per-thread count of candidate existence stats performed by `pick_existing`, so unit
// tests can assert how much filesystem work a `ResolveMode` actually avoids rather than
// timing it. Thread-local: `cargo test` runs tests in parallel threads, and each test
// resolves on its own thread.
#[cfg(test)]
thread_local! {
    static CANDIDATE_STATS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The single existence check for one candidate copy. Funnelled through one function so
/// the test counter above sees every stat [`pick_existing`] performs.
fn candidate_exists(path: &Path) -> bool {
    #[cfg(test)]
    CANDIDATE_STATS.with(|c| c.set(c.get() + 1));
    path.exists()
}

/// Take and reset this thread's candidate-stat count (tests only).
#[cfg(test)]
pub fn take_candidate_stats() -> usize {
    CANDIDATE_STATS.with(|c| c.replace(0))
}

/// Pick the best *available* physical copy from a resolver's candidate list, statting
/// OFF the catalog lock. `candidates` must already be in read-preference order (best
/// role first, catalog-root fallback last); a copy is the answer only if it exists on disk.
///
/// `mode` is the caller's resolver policy (see [`ResolveMode`]) and decides what happens to
/// candidates whose volume the reachability cache currently believes is unreachable:
///   1. Both statting modes first stat the candidates whose volume is cached-reachable,
///      unknown, or has no volume (the legacy catalog-root fallback) — the likely-live
///      ones, in priority order.
///   2. [`ResolveMode::OriginalRequired`] then stats the deferred (cached-unreachable)
///      ones too, so the cache may only REORDER stats, never change the answer: a stale
///      "unreachable" flag costs one deferred stat and "best available copy" (AGENTS.md)
///      still holds. [`ResolveMode::FastDisplay`] skips this pass and answers `None`
///      immediately, trading that guarantee for never touching an offline/hung NAS — only
///      legal where `None` means "show the persistent thumbnail", never where it decides
///      that a row or a file is gone.
///
/// [`ResolveMode::PathClassification`] asks no filesystem question at all and therefore
/// always yields `None` here; such callers want [`crate::catalog::Catalog::volume_for_path`]
/// or [`crate::catalog::Catalog::photo_path_candidates`] instead.
///
/// Must be called from a blocking/worker context (each stat may block on I/O).
pub fn pick_existing(
    candidates: &[PathCandidate],
    health: &VolumeHealth,
    mode: ResolveMode,
) -> Option<PathBuf> {
    if !mode.stats_filesystem() {
        return None;
    }
    let mut deferred: Vec<&PathCandidate> = Vec::new();

    // Pass 1: candidates on reachable/unknown volumes (and the root fallback).
    for cand in candidates {
        let likely_live = match cand.volume_id {
            None => true, // legacy catalog-root fallback — always try
            Some(vid) => health.reachable(vid) != Some(false),
        };
        if likely_live {
            if candidate_exists(&cand.path) {
                return Some(cand.path.clone());
            }
        } else {
            deferred.push(cand);
        }
    }

    // Pass 2: the cached-unreachable ones — the cache might be stale. Skipped by
    // FastDisplay, which would rather fall back than wait on an offline volume.
    if mode.verifies_unreachable() {
        for cand in deferred {
            if candidate_exists(&cand.path) {
                return Some(cand.path.clone());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp dir for one test, removed on drop. See `crate::test_support`.
    fn temp_dir(tag: &str) -> crate::test_support::TestTmpDir {
        crate::test_support::TestTmpDir::new(&format!("vh-{tag}"))
    }

    #[test]
    fn ttl_zero_always_stats() {
        let dir = temp_dir("ttl-zero");
        let base = dir.to_string_lossy().to_string();
        let health = VolumeHealth::with_ttl(Duration::ZERO);
        let vols = vec![(1i64, base.clone())];

        // First call stats: the dir exists → reachable.
        assert_eq!(health.refresh(&vols).get(&1), Some(&true));
        // reachable() never returns a stale value with ttl=0.
        assert_eq!(health.reachable(1), None);

        // Remove the dir; with ttl=0 the next refresh re-stats and sees it gone.
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(health.refresh(&vols).get(&1), Some(&false));
    }

    #[test]
    fn ttl_max_uses_cache_on_second_call() {
        let dir = temp_dir("ttl-max");
        let base = dir.to_string_lossy().to_string();
        let health = VolumeHealth::with_ttl(Duration::MAX);
        let vols = vec![(7i64, base.clone())];

        // First call stats and caches "true".
        assert_eq!(health.refresh(&vols).get(&7), Some(&true));
        // Remove the dir — but the cached "true" is still fresh (ttl=MAX), so the
        // second call must return the cached value WITHOUT re-statting.
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(health.refresh(&vols).get(&7), Some(&true));
        assert_eq!(health.reachable(7), Some(true));
    }

    #[test]
    fn invalidate_forces_a_restat() {
        let dir = temp_dir("invalidate");
        let base = dir.to_string_lossy().to_string();
        let health = VolumeHealth::with_ttl(Duration::MAX);
        let vols = vec![(3i64, base.clone())];

        assert_eq!(health.refresh(&vols).get(&3), Some(&true));
        assert_eq!(health.reachable(3), Some(true));
        health.invalidate();
        assert_eq!(health.reachable(3), None);
    }

    #[test]
    fn pick_existing_prefers_priority_and_falls_back_across_passes() {
        use crate::catalog::LocationRole;
        let dir = temp_dir("pick-existing");
        let cache_dir = dir.join("cache");
        let primary_dir = dir.join("primary");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&primary_dir).unwrap();
        let cache_file = cache_dir.join("a.arw");
        let primary_file = primary_dir.join("a.arw");

        let cands = vec![
            PathCandidate {
                path: cache_file.clone(),
                role: LocationRole::LocalCache,
                volume_id: Some(1),
            },
            PathCandidate {
                path: primary_file.clone(),
                role: LocationRole::Primary,
                volume_id: Some(2),
            },
        ];

        // No cache primed: both volumes are "unknown", so pass 1 stats in priority order.
        // The local-cache copy exists → it wins over the primary (best available copy).
        std::fs::write(&cache_file, b"x").unwrap();
        std::fs::write(&primary_file, b"x").unwrap();
        let health = VolumeHealth::with_ttl(Duration::MAX);
        assert_eq!(
            pick_existing(&cands, &health, ResolveMode::OriginalRequired),
            Some(cache_file.clone())
        );

        // Now mark volume 1 cached-unreachable and remove its file. Pass 1 skips it and
        // finds the primary (exists) — graceful fallback, answer still correct.
        std::fs::remove_file(&cache_file).unwrap();
        health.refresh(&[(1i64, "/nonexistent/deadbeef/mount".to_string())]);
        assert_eq!(health.reachable(1), Some(false));
        assert_eq!(
            pick_existing(&cands, &health, ResolveMode::OriginalRequired),
            Some(primary_file.clone())
        );

        // A cached-unreachable volume that is in fact the ONLY copy is still found — pass 2
        // stats the deferred candidate, so a stale flag never wrongly yields None.
        std::fs::remove_file(&primary_file).unwrap();
        std::fs::write(&cache_file, b"x").unwrap();
        assert_eq!(
            pick_existing(&cands, &health, ResolveMode::OriginalRequired),
            Some(cache_file)
        );
    }

    /// The offline-NAS split: the ONLY copy sits on a volume the cache believes is
    /// unreachable, and the flag is stale (the file is actually there).
    ///
    /// - `FastDisplay` must not stat that volume at all — it answers `None` at once so the
    ///   caller can serve the persistent thumbnail instead of waiting on the NAS.
    /// - `OriginalRequired` must still find the copy; a cached flag may never turn a
    ///   present original into "gone".
    ///
    /// The stat counter makes the avoided filesystem work observable rather than timed.
    #[test]
    fn fast_display_skips_cached_unreachable_volumes_that_original_required_still_stats() {
        use crate::catalog::LocationRole;
        let dir = temp_dir("fast-display");
        let nas_dir = dir.join("nas");
        std::fs::create_dir_all(&nas_dir).unwrap();
        let nas_file = nas_dir.join("a.arw");
        std::fs::write(&nas_file, b"x").unwrap();
        let missing_local = dir.join("local").join("a.arw"); // never created

        let cands = vec![
            PathCandidate {
                path: missing_local.clone(),
                role: LocationRole::LocalCache,
                volume_id: Some(1),
            },
            PathCandidate {
                path: nas_file.clone(),
                role: LocationRole::Backup,
                volume_id: Some(2),
            },
        ];

        // Force the offline condition: volume 2's base path does not exist, so a refresh
        // caches "unreachable" — while the candidate FILE itself is still there. That is
        // exactly a stale flag (NAS unmounted at check time, remounted since).
        let health = VolumeHealth::with_ttl(Duration::MAX);
        health.refresh(&[(2i64, dir.join("unmounted").to_string_lossy().to_string())]);
        assert_eq!(health.reachable(2), Some(false), "volume 2 is cached-unreachable");

        let _ = take_candidate_stats();
        assert_eq!(
            pick_existing(&cands, &health, ResolveMode::FastDisplay),
            None,
            "FastDisplay falls back instead of waiting on a cached-unreachable volume"
        );
        let fast_stats = take_candidate_stats();
        assert_eq!(
            fast_stats, 1,
            "FastDisplay stats only the likely-live candidate, never the deferred one"
        );

        assert_eq!(
            pick_existing(&cands, &health, ResolveMode::OriginalRequired),
            Some(nas_file),
            "OriginalRequired must still find the copy: the cache may only reorder stats"
        );
        let strict_stats = take_candidate_stats();
        assert_eq!(
            strict_stats, 2,
            "OriginalRequired pays one extra stat to verify the cached-unreachable copy"
        );
        assert!(
            fast_stats < strict_stats,
            "the mode split is what saves the stat ({fast_stats} < {strict_stats})"
        );
    }

    /// `FastDisplay` only ever *defers* to the fallback because of the CACHE. With no
    /// cached "unreachable" verdict it stats normally and finds the same copy the strict
    /// mode does — so an unmounted-but-unknown volume is never silently written off.
    #[test]
    fn fast_display_matches_strict_when_reachability_is_unknown() {
        use crate::catalog::LocationRole;
        let dir = temp_dir("fast-display-unknown");
        let nas_dir = dir.join("nas");
        std::fs::create_dir_all(&nas_dir).unwrap();
        let nas_file = nas_dir.join("a.arw");
        std::fs::write(&nas_file, b"x").unwrap();

        let cands = vec![PathCandidate {
            path: nas_file.clone(),
            role: LocationRole::Backup,
            volume_id: Some(2),
        }];
        let health = VolumeHealth::with_ttl(Duration::MAX); // nothing cached
        assert_eq!(health.reachable(2), None);
        assert_eq!(
            pick_existing(&cands, &health, ResolveMode::FastDisplay),
            Some(nas_file.clone())
        );
        assert_eq!(
            pick_existing(&cands, &health, ResolveMode::OriginalRequired),
            Some(nas_file)
        );
    }

    /// `PathClassification` asks no filesystem question, so it performs zero stats even
    /// when a candidate plainly exists. It is a DB-rows-only mode.
    #[test]
    fn path_classification_never_stats() {
        use crate::catalog::LocationRole;
        let dir = temp_dir("path-classification");
        let file = dir.join("a.arw");
        std::fs::write(&file, b"x").unwrap();
        let cands = vec![PathCandidate {
            path: file,
            role: LocationRole::Primary,
            volume_id: None,
        }];
        let health = VolumeHealth::with_ttl(Duration::MAX);
        let _ = take_candidate_stats();
        assert_eq!(
            pick_existing(&cands, &health, ResolveMode::PathClassification),
            None
        );
        assert_eq!(take_candidate_stats(), 0);
    }
}
