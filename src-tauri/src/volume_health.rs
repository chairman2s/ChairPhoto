//! Volume reachability cache — keeps NAS/mount stat calls OFF the catalog lock.
//!
//! Statting a slow or offline NAS mount (`Path::is_dir`) can block for seconds. When
//! that stat runs while the single catalog `Mutex` is held, the whole app serializes
//! behind it. This cache does the stats on a blocking worker (never under the lock)
//! and remembers each volume's reachability for a short TTL, so repeated status/render
//! passes reuse a fresh answer instead of re-statting every time.
//!
//! Reachability here only ever *reorders* or *annotates* work — it must never change
//! which physical copy the resolver returns (see [`pick_existing`]). The resolver's
//! "best available copy" invariant (AGENTS.md) is preserved even under a stale cache.

use crate::catalog::PathCandidate;
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

/// Pick the best *available* physical copy from a resolver's candidate list, statting
/// OFF the catalog lock. `candidates` must already be in read-preference order (best
/// role first, catalog-root fallback last); a copy is the answer only if it exists on disk.
///
/// The reachability cache may only REORDER the stats, never change the answer. Two passes
/// preserve "best available copy" (AGENTS.md) even under a stale cache:
///   1. stat candidates whose volume is cached-reachable, unknown, or has no volume
///      (the legacy catalog-root fallback) — the likely-live ones, in priority order;
///   2. only if none of those exist, stat the deferred (cached-unreachable) ones too.
///
/// So a stale "unreachable" flag merely DEFERS a stat; if that copy is in fact present
/// and is the best-priority one, pass 2 still finds it. This must be called from a
/// blocking/worker context (each `exists()` may block on I/O).
pub fn pick_existing(candidates: &[PathCandidate], health: &VolumeHealth) -> Option<PathBuf> {
    let mut deferred: Vec<&PathCandidate> = Vec::new();

    // Pass 1: candidates on reachable/unknown volumes (and the root fallback).
    for cand in candidates {
        let likely_live = match cand.volume_id {
            None => true, // legacy catalog-root fallback — always try
            Some(vid) => health.reachable(vid) != Some(false),
        };
        if likely_live {
            if cand.path.exists() {
                return Some(cand.path.clone());
            }
        } else {
            deferred.push(cand);
        }
    }

    // Pass 2: the cached-unreachable ones — the cache might be stale.
    for cand in deferred {
        if cand.path.exists() {
            return Some(cand.path.clone());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp dir for one test (mirrors the integration tests' approach; avoids a
    /// `tempfile` dev-dependency).
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("chairphoto-vh-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
        assert_eq!(pick_existing(&cands, &health), Some(cache_file.clone()));

        // Now mark volume 1 cached-unreachable and remove its file. Pass 1 skips it and
        // finds the primary (exists) — graceful fallback, answer still correct.
        std::fs::remove_file(&cache_file).unwrap();
        health.refresh(&[(1i64, "/nonexistent/deadbeef/mount".to_string())]);
        assert_eq!(health.reachable(1), Some(false));
        assert_eq!(pick_existing(&cands, &health), Some(primary_file.clone()));

        // A cached-unreachable volume that is in fact the ONLY copy is still found — pass 2
        // stats the deferred candidate, so a stale flag never wrongly yields None.
        std::fs::remove_file(&primary_file).unwrap();
        std::fs::write(&cache_file, b"x").unwrap();
        assert_eq!(pick_existing(&cands, &health), Some(cache_file));
    }
}
