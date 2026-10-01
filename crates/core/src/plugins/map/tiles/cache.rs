//! The disk tile cache the OSM tile policy asks for ("keep a sufficient local cache";
//! "cache each tile for at least 7 days"; "use conditional requests").
//!
//! Layout: `<root>/<source cache id>/<z>/<x>/<y>.tile` holds the body, and `<y>.json` next
//! to it the validators (`ETag`, `Last-Modified`) and when the tile goes stale. The default
//! root is `$XDG_CACHE_HOME/chairphoto/tiles` (`~/.cache/…`): tiles are not catalog data, so
//! they live with the other caches, not in a catalog, and never travel with a bundle.
//!
//! Size is capped: after every [`SWEEP_EVERY`] writes, the oldest tiles (by last use — a read
//! touches the file's mtime) are removed until the cache is under 90 % of its cap.
//!
//! All of this is blocking file IO: call it from a blocking worker (the fetcher does).

use super::math::TileKey;
use super::source::TileSource;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;

/// The cache's default cap.
pub const DEFAULT_CAP_BYTES: u64 = 512 * 1024 * 1024;
/// How many writes between size sweeps.
pub const SWEEP_EVERY: u32 = 256;

/// What the server said about a tile, kept for revalidation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TileMeta {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Unix seconds after which the tile is stale and must be revalidated before use
    /// online (it is still shown offline).
    pub expires: i64,
}

/// A cached tile: its bytes and validators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedTile {
    pub bytes: Vec<u8>,
    pub meta: TileMeta,
}

impl CachedTile {
    pub fn is_fresh(&self, now: i64) -> bool {
        now < self.meta.expires
    }
}

pub struct TileCache {
    root: PathBuf,
    cap_bytes: u64,
    writes: AtomicU32,
}

/// `$XDG_CACHE_HOME/chairphoto/tiles`, or `~/.cache/chairphoto/tiles`.
pub fn default_root() -> PathBuf {
    crate::thumbnails::cache_dir().join("chairphoto").join("tiles")
}

impl TileCache {
    pub fn new(root: impl Into<PathBuf>, cap_bytes: u64) -> Self {
        TileCache { root: root.into(), cap_bytes, writes: AtomicU32::new(0) }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn paths(&self, source: &TileSource, key: TileKey) -> (PathBuf, PathBuf) {
        let dir = self.root.join(source.cache_id()).join(key.z.to_string()).join(key.x.to_string());
        (dir.join(format!("{}.tile", key.y)), dir.join(format!("{}.json", key.y)))
    }

    /// The cached tile, if both its files read back; marks it used. A missing or corrupt
    /// entry is a miss.
    pub fn get(&self, source: &TileSource, key: TileKey) -> Option<CachedTile> {
        let (data, meta) = self.paths(source, key);
        let meta: TileMeta = serde_json::from_slice(&std::fs::read(meta).ok()?).ok()?;
        let bytes = std::fs::read(&data).ok()?;
        if let Ok(f) = std::fs::File::options().write(true).open(&data) {
            let _ = f.set_modified(SystemTime::now());
        }
        Some(CachedTile { bytes, meta })
    }

    /// Store a tile (body and validators), atomically per file: a reader sees the old entry
    /// or the new one, never a torn write.
    pub fn put(&self, source: &TileSource, key: TileKey, bytes: &[u8], meta: &TileMeta) -> std::io::Result<()> {
        let (data, meta_path) = self.paths(source, key);
        write_atomic(&data, bytes)?;
        write_atomic(&meta_path, &serde_json::to_vec(meta).map_err(std::io::Error::other)?)?;
        if self.writes.fetch_add(1, Ordering::Relaxed) + 1 >= SWEEP_EVERY {
            self.writes.store(0, Ordering::Relaxed);
            self.sweep();
        }
        Ok(())
    }

    /// Replace only the validators (a `304 Not Modified`).
    pub fn put_meta(&self, source: &TileSource, key: TileKey, meta: &TileMeta) -> std::io::Result<()> {
        let (_, meta_path) = self.paths(source, key);
        write_atomic(&meta_path, &serde_json::to_vec(meta).map_err(std::io::Error::other)?)
    }

    /// Bytes of tile bodies held.
    pub fn size(&self) -> u64 {
        self.entries().iter().map(|e| e.1).sum()
    }

    fn entries(&self) -> Vec<(PathBuf, u64, SystemTime)> {
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(read) = std::fs::read_dir(&dir) else { continue };
            for entry in read.flatten() {
                let path = entry.path();
                let Ok(md) = entry.metadata() else { continue };
                if md.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "tile") {
                    out.push((path, md.len(), md.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
                }
            }
        }
        out
    }

    /// Remove the least recently used tiles until the cache is under 90 % of its cap.
    /// Returns how many were removed.
    pub fn sweep(&self) -> usize {
        let mut entries = self.entries();
        let mut total: u64 = entries.iter().map(|e| e.1).sum();
        if total <= self.cap_bytes {
            return 0;
        }
        let target = self.cap_bytes / 10 * 9;
        entries.sort_by_key(|e| e.2);
        let mut removed = 0;
        for (path, len, _) in entries {
            if total <= target {
                break;
            }
            let _ = std::fs::remove_file(path.with_extension("json"));
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
                removed += 1;
            }
        }
        removed
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| std::io::Error::other("no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("tile"),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A private directory under the temp dir, removed on drop.
    pub(crate) struct TempDir(pub PathBuf);

    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("cp-tiles-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const K: TileKey = TileKey { z: 3, x: 2, y: 1 };

    #[test]
    fn put_then_get_round_trips_body_and_validators() {
        let dir = TempDir::new("rt");
        let cache = TileCache::new(&dir.0, DEFAULT_CAP_BYTES);
        let src = TileSource::default();
        assert_eq!(cache.get(&src, K), None);
        let meta = TileMeta { etag: Some("\"abc\"".into()), last_modified: Some("Tue, 01 Sep 2026 10:00:00 GMT".into()), expires: 42 };
        cache.put(&src, K, b"png bytes", &meta).unwrap();
        let got = cache.get(&src, K).unwrap();
        assert_eq!(got, CachedTile { bytes: b"png bytes".to_vec(), meta: meta.clone() });
        assert!(got.is_fresh(41) && !got.is_fresh(42));
        // Another source's tile with the same key is another entry.
        let other = TileSource::parse("https://example.org/{z}/{x}/{y}.png").unwrap();
        assert_eq!(cache.get(&other, K), None);
        // Validators alone.
        cache.put_meta(&src, K, &TileMeta { expires: 99, ..meta }).unwrap();
        assert_eq!(cache.get(&src, K).unwrap().meta.expires, 99);
        assert_eq!(cache.get(&src, K).unwrap().bytes, b"png bytes");
    }

    #[test]
    fn a_corrupt_entry_is_a_miss() {
        let dir = TempDir::new("corrupt");
        let cache = TileCache::new(&dir.0, DEFAULT_CAP_BYTES);
        let src = TileSource::default();
        cache.put(&src, K, b"x", &TileMeta::default()).unwrap();
        let (_, meta) = cache.paths(&src, K);
        std::fs::write(meta, b"{not json").unwrap();
        assert_eq!(cache.get(&src, K), None);
    }

    /// Over the cap, the least recently *used* tiles go first, down to 90 % of it.
    #[test]
    fn sweep_evicts_least_recently_used_down_to_ninety_percent() {
        let dir = TempDir::new("sweep");
        let cache = TileCache::new(&dir.0, 1000);
        let src = TileSource::default();
        let key = |y| TileKey { z: 5, x: 1, y };
        for y in 0..5 {
            cache.put(&src, key(y), &[0u8; 300], &TileMeta::default()).unwrap();
            // Distinct, increasing mtimes without sleeping.
            let (data, _) = cache.paths(&src, key(y));
            let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 + y as u64);
            std::fs::File::options().write(true).open(data).unwrap().set_modified(t).unwrap();
        }
        assert_eq!(cache.size(), 1500);
        // Using tile 0 makes it the newest.
        assert!(cache.get(&src, key(0)).is_some());
        assert_eq!(cache.sweep(), 2, "1500 → 900, 90 % of the cap");
        let left: Vec<u32> = (0..5).filter(|&y| cache.get(&src, key(y)).is_some()).collect();
        assert_eq!(left, [0, 3, 4]);
        assert_eq!(cache.sweep(), 0, "under the cap");
    }
}
