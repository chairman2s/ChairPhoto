//! The `.rawf` decode cache (docs/plans/raw-foundation, slice 4): the decoder's 16-bit
//! linear output written to disk once, so a photo opened again — or stepped to as a
//! neighbour — skips the multi-second LibRaw decode and loads in a fraction of a second.
//!
//! # Layout
//!
//! `<cache>/chairphoto/rawf<FORMAT>-<decoder>/<fnv64(key)>.rawf`, where the key is the
//! original's path, mtime (ns) and size — the rule the preview tiers already use — and the
//! directory carries the LibRaw version string, so a decoder upgrade can never serve an old
//! decode. Directories for other decoders or formats are unreachable and [`trim_to`]
//! deletes them outright.
//!
//! # File format (little-endian)
//!
//! ```text
//! magic    8  b"CPRAWF03"
//! width    u32 · height u32 · orientation u8 (EXIF 1–8)
//! cam_mul  4×f32 · pre_mul 4×f32 · rgb_cam 9×f32
//! wbct     u8 rows + rows×4 f32 (K, R, G, B)
//! lens     u32 len + JSON of `lens::LensCorrection` (len 0 = none)
//! decoder  u16 len + UTF-8 · key u32 len + UTF-8 (the full key, so an fnv collision reads
//!          as a miss, never as another photo's pixels)
//! pixels   width×height×3 u16, interleaved RGB, row-major
//! ```
//!
//! Uncompressed on purpose: ~400 MB for a 67 MP Sony file, read back in well under a second
//! from NVMe, which is the whole point. Writes go to a temp file renamed into place, so a
//! crash mid-write leaves no half file under the real name. A hit touches the file's mtime,
//! so trimming by mtime is least-recently-used.
//!
//! Everything here is best-effort: a cache that cannot be read or written just means a
//! decode. It never fails an open.

use crate::raw::LinearDecode;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// Bumped when the file format or the decode contract changes (the decode contract lives in
/// `raw::decode_linear`: bit depth, white balance, clipping, inset crop).
/// 2: `pre_mul` and the camera's white-balance table joined the header (Kelvin white
/// balance); format-1 directories are unreachable and trimmed.
/// 3: the camera's lens-correction tables joined the header, so a cache hit with the
/// original unmounted still corrects (docs/plans/lens-corrections).
const FORMAT: u32 = 3;
const MAGIC: &[u8; 8] = b"CPRAWF03";
/// Far above any real table set (a few KB of JSON); a larger length is a damaged file.
const MAX_LENS_JSON: usize = 256 * 1024;
const EXT: &str = "rawf";

/// Settings key: the cache's size limit in GB. Default [`DEFAULT_BUDGET_GB`].
pub const BUDGET_KEY: &str = "develop.decodeCacheGb";
/// 20 GB ≈ 50 photos from a 67 MP Sony, ~250 from a 20 MP camera.
pub const DEFAULT_BUDGET_GB: u64 = 20;

/// What a cached decode is keyed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    key: String,
    decoder: String,
}

impl CacheKey {
    /// The key for `path` as it is on disk right now, for the linked decoder. `None` when the
    /// file cannot be statted.
    pub fn for_file(path: &Path) -> Option<CacheKey> {
        Self::for_file_with(path, crate::raw::decoder_version())
    }

    fn for_file_with(path: &Path, decoder: &str) -> Option<CacheKey> {
        let meta = std::fs::metadata(path).ok()?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Some(CacheKey {
            key: format!("{}|{mtime}|{}", path.display(), meta.len()),
            decoder: decoder.to_string(),
        })
    }
}

fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The cache's root: `<XDG cache>/chairphoto`.
pub(crate) fn root() -> PathBuf {
    crate::thumbnails::cache_dir().join("chairphoto")
}

/// The directory for this format and decoder. The decoder string is sanitized to a safe
/// file-name component.
fn dir_for(root: &Path, decoder: &str) -> PathBuf {
    let safe: String = decoder
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    root.join(format!("rawf{FORMAT}-{safe}"))
}

fn path_in(root: &Path, key: &CacheKey) -> PathBuf {
    dir_for(root, &key.decoder).join(format!("{:016x}.{EXT}", fnv1a(&key.key)))
}

/// Where `key`'s decode lives (whether or not it exists yet).
pub fn cache_path(key: &CacheKey) -> PathBuf {
    path_in(&root(), key)
}

/// The cached decode for `key`, or `None` on a miss, a mismatch, or any read error.
pub fn read(key: &CacheKey) -> Option<LinearDecode> {
    read_in(&root(), key)
}

pub(crate) fn read_in(root: &Path, key: &CacheKey) -> Option<LinearDecode> {
    let path = path_in(root, key);
    let file = std::fs::File::open(&path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut r = std::io::BufReader::with_capacity(1 << 20, file);
    let decoded = read_body(&mut r, key, len)?;
    // Least-recently-used: a hit counts as a use.
    if let Ok(f) = std::fs::File::options().write(true).open(&path) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
    Some(decoded)
}

fn read_body(r: &mut impl Read, key: &CacheKey, file_len: u64) -> Option<LinearDecode> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).ok()?;
    if &magic != MAGIC {
        return None;
    }
    let width = read_u32(r)?;
    let height = read_u32(r)?;
    let mut o = [0u8; 1];
    r.read_exact(&mut o).ok()?;
    let orientation = image::metadata::Orientation::from_exif(o[0])?;
    let mut cam_mul = [0f32; 4];
    for v in &mut cam_mul {
        *v = read_f32(r)?;
    }
    let mut pre_mul = [0f32; 4];
    for v in &mut pre_mul {
        *v = read_f32(r)?;
    }
    let mut rgb_cam = [[0f32; 3]; 3];
    for row in &mut rgb_cam {
        for v in row.iter_mut() {
            *v = read_f32(r)?;
        }
    }
    let mut n_wbct = [0u8; 1];
    r.read_exact(&mut n_wbct).ok()?;
    let mut wbct = Vec::with_capacity(n_wbct[0] as usize);
    for _ in 0..n_wbct[0] {
        wbct.push([read_f32(r)?, read_f32(r)?, read_f32(r)?, read_f32(r)?]);
    }
    let lens_len = read_u32(r)? as usize;
    if lens_len > MAX_LENS_JSON {
        return None;
    }
    let lens = if lens_len == 0 {
        None
    } else {
        let mut b = vec![0u8; lens_len];
        r.read_exact(&mut b).ok()?;
        let lens: crate::lens::LensCorrection = serde_json::from_slice(&b).ok()?;
        // Damaged tables are a miss (a fresh decode reads them again), never a hit that
        // silently renders uncorrected.
        if !lens.validate() {
            return None;
        }
        Some(lens)
    };
    let decoder_len = read_u16(r)? as usize;
    let decoder = read_string(r, decoder_len)?;
    let key_len = read_u32(r)? as usize;
    let stored_key = read_string(r, key_len)?;
    if decoder != key.decoder || stored_key != key.key {
        return None; // another decoder, or an fnv collision: a miss, never other pixels
    }
    let n = (width as u64).checked_mul(height as u64)?.checked_mul(3)?;
    let header = 8 + 4 + 4 + 1 + 16 + 16 + 36 + 1 + 16 * wbct.len() as u64 + 4 + lens_len as u64 + 2 + decoder.len() as u64 + 4 + stored_key.len() as u64;
    if header + n * 2 != file_len {
        return None; // truncated or padded: not a file this writer produced
    }
    let rgb16 = read_u16s(r, n as usize)?;
    Some(LinearDecode { width, height, rgb16, orientation, cam_mul, pre_mul, rgb_cam, wbct, lens })
}

/// Read `n` little-endian `u16`s. On a little-endian machine the file's bytes already are
/// the values, so they are read straight into the buffer with no per-value loop — which
/// matters beyond release builds: a 67 MP decode is 200 million values, and a loop over
/// them takes ~11 s at opt-level 0 (`tauri dev`), longer than the LibRaw decode it saves.
fn read_u16s(r: &mut impl Read, n: usize) -> Option<Vec<u16>> {
    let mut v = vec![0u16; n];
    // SAFETY: `v` owns `n` initialised u16s = `2n` bytes; u8 has alignment 1, so viewing
    // that memory as a byte slice is valid for its whole length, and the view ends before
    // `v` is used again.
    let bytes = unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr().cast::<u8>(), n * 2) };
    r.read_exact(bytes).ok()?;
    if cfg!(target_endian = "big") {
        for x in v.iter_mut() {
            *x = u16::from_le(*x);
        }
    }
    Some(v)
}

/// Write `v` as little-endian `u16`s: the buffer's own bytes on a little-endian machine (no
/// per-value loop, see [`read_u16s`]), a chunked conversion otherwise.
fn write_u16s(w: &mut impl Write, v: &[u16]) -> std::io::Result<()> {
    if cfg!(target_endian = "little") {
        // SAFETY: a `&[u16]` of len n is 2n initialised bytes; u8 has alignment 1.
        let bytes = unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), v.len() * 2) };
        return w.write_all(bytes);
    }
    let mut buf = Vec::with_capacity(1 << 20);
    for chunk in v.chunks(1 << 19) {
        buf.clear();
        for x in chunk {
            buf.extend_from_slice(&x.to_le_bytes());
        }
        w.write_all(&buf)?;
    }
    Ok(())
}

fn read_u16(r: &mut impl Read) -> Option<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b).ok()?;
    Some(u16::from_le_bytes(b))
}
fn read_u32(r: &mut impl Read) -> Option<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).ok()?;
    Some(u32::from_le_bytes(b))
}
fn read_f32(r: &mut impl Read) -> Option<f32> {
    read_u32(r).map(f32::from_bits)
}
fn read_string(r: &mut impl Read, len: usize) -> Option<String> {
    if len > 64 * 1024 {
        return None;
    }
    let mut b = vec![0u8; len];
    r.read_exact(&mut b).ok()?;
    String::from_utf8(b).ok()
}

/// Write `d` as `key`'s cached decode: temp file, then rename into place.
pub fn write(key: &CacheKey, d: &LinearDecode) -> Result<(), String> {
    write_in(&root(), key, d)
}

pub(crate) fn write_in(root: &Path, key: &CacheKey, d: &LinearDecode) -> Result<(), String> {
    let lens_json = match &d.lens {
        Some(l) => serde_json::to_vec(l).map_err(|e| format!("could not encode the lens tables: {e}"))?,
        None => Vec::new(),
    };
    if lens_json.len() > MAX_LENS_JSON {
        return Err("lens tables too large for the decode cache".into());
    }
    let path = path_in(root, key);
    let dir = path.parent().ok_or("cache path has no directory")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // Unique per process and call, so two writers of one key never share a temp file.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{:016x}.{}-{n}.tmp", fnv1a(&key.key), std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp)?);
        w.write_all(MAGIC)?;
        w.write_all(&d.width.to_le_bytes())?;
        w.write_all(&d.height.to_le_bytes())?;
        w.write_all(&[d.orientation.to_exif()])?;
        for v in d.cam_mul {
            w.write_all(&v.to_bits().to_le_bytes())?;
        }
        for v in d.pre_mul {
            w.write_all(&v.to_bits().to_le_bytes())?;
        }
        for row in d.rgb_cam {
            for v in row {
                w.write_all(&v.to_bits().to_le_bytes())?;
            }
        }
        let rows = &d.wbct[..d.wbct.len().min(u8::MAX as usize)];
        w.write_all(&[rows.len() as u8])?;
        for row in rows {
            for v in row {
                w.write_all(&v.to_bits().to_le_bytes())?;
            }
        }
        w.write_all(&(lens_json.len() as u32).to_le_bytes())?;
        w.write_all(&lens_json)?;
        w.write_all(&(key.decoder.len() as u16).to_le_bytes())?;
        w.write_all(key.decoder.as_bytes())?;
        w.write_all(&(key.key.len() as u32).to_le_bytes())?;
        w.write_all(key.key.as_bytes())?;
        write_u16s(&mut w, &d.rgb16)?;
        w.flush()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("could not write the decode cache: {e}"));
    }
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("could not install the decode cache file: {e}")
    })
}

/// Bytes the cache holds right now, across every format and decoder directory.
pub fn usage_bytes() -> u64 {
    usage_in(&root())
}

fn cache_dirs(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .map(|d| {
            d.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("rawf"))
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}

fn usage_in(root: &Path) -> u64 {
    cache_dirs(root)
        .iter()
        .flat_map(|d| std::fs::read_dir(d).into_iter().flatten().flatten())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Bring the cache within `budget_bytes`: delete directories for other decoders or formats
/// (unreachable), leftover temp files, then the least recently used decodes until it fits.
/// Best-effort; returns the bytes freed.
pub fn trim_to(budget_bytes: u64) -> u64 {
    trim_in(&root(), crate::raw::decoder_version(), budget_bytes)
}

pub(crate) fn trim_in(root: &Path, decoder: &str, budget_bytes: u64) -> u64 {
    let current = dir_for(root, decoder);
    let mut freed = 0u64;
    for dir in cache_dirs(root) {
        if dir != current {
            freed += usage_in_dir(&dir);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    let Ok(entries) = std::fs::read_dir(&current) else { return freed };
    let now = std::time::SystemTime::now();
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        let Ok(m) = e.metadata() else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        let mtime = m.modified().unwrap_or(now);
        if name.ends_with(".tmp") {
            // A writer that died mid-write. A live one is at most seconds old.
            if now.duration_since(mtime).map(|d| d.as_secs() > 600).unwrap_or(false) {
                freed += m.len();
                let _ = std::fs::remove_file(&p);
            }
            continue;
        }
        if p.extension().is_some_and(|x| x == EXT) {
            files.push((mtime, m.len(), p));
        }
    }
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    files.sort_by_key(|(mtime, _, _)| *mtime); // oldest first
    for (_, len, path) in files {
        if total <= budget_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
            freed += len;
        }
    }
    freed
}

fn usage_in_dir(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::metadata::Orientation;

    fn root(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("chairphoto-rawf-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn decode(w: u32, h: u32, seed: u16) -> LinearDecode {
        LinearDecode {
            width: w,
            height: h,
            rgb16: (0..w * h * 3).map(|i| (i as u16).wrapping_mul(31).wrapping_add(seed)).collect(),
            orientation: Orientation::Rotate90,
            cam_mul: [2.1, 1.0, 1.6, 1.0],
            pre_mul: [2.4, 1.0, 1.3, 0.0],
            rgb_cam: [[1.7, -0.6, -0.1], [-0.2, 1.5, -0.3], [0.0, -0.4, 1.4]],
            wbct: vec![[2500.0, 1395.0, 1024.0, 3366.0], [6000.0, 2561.0, 1024.0, 1518.0]],
            lens: None,
        }
    }

    /// Tables shaped like an A7 IV + FE 85mm F1.8 frame's (vignetting and CA, no distortion).
    fn lens() -> crate::lens::LensCorrection {
        let knots: Vec<f32> = (0..16).map(|i| i as f32 / 15.0).collect();
        let radial = |values: Vec<f32>| crate::lens::Radial { knots: knots.clone(), values };
        crate::lens::LensCorrection {
            source: "Sony built-in".into(),
            vignetting: Some(radial((0..16).map(|i| 1.0 + i as f32 * 0.05).collect())),
            distortion: None,
            chromatic: Some([radial(vec![0.9998; 16]), radial(vec![1.0003; 16])]),
        }
    }

    fn key(k: &str, decoder: &str) -> CacheKey {
        CacheKey { key: k.into(), decoder: decoder.into() }
    }

    /// Ignored bench: a cache hit's cost on a 67 MP-sized decode, split into the file read
    /// and the conversion to the engine's working image. Run in both profiles:
    /// `cargo test [--release] develop::cache::tests::bench_cache_hit -- --ignored --nocapture`.
    #[test]
    #[ignore = "decode-cache bench; prints timings"]
    fn bench_cache_hit() {
        let r = root("bench");
        let (w, h) = (6656u32, 9984u32); // the A7R VI picture, portrait
        let d = decode(w, h, 3);
        let k = key("/bench.ARW|1|1", "dec");
        let t = std::time::Instant::now();
        write_in(&r, &k, &d).unwrap();
        let write = t.elapsed();
        drop(d);
        let t = std::time::Instant::now();
        let back = read_in(&r, &k).unwrap();
        let read = t.elapsed();
        let t = std::time::Instant::now();
        let img = crate::develop::working_image_from(back);
        let convert = t.elapsed();
        println!(
            "{{\"bench\":\"cache_hit\",\"profile\":\"{}\",\"px\":\"{w}x{h}\",\"write_ms\":{},\"read_ms\":{},\"to_working_ms\":{},\"working_mb\":{}}}",
            crate::plugins::edit::timing::PROFILE,
            write.as_millis(),
            read.as_millis(),
            convert.as_millis(),
            img.bytes() / (1024 * 1024)
        );
        std::fs::remove_dir_all(&r).ok();
    }

    #[test]
    fn cache_roundtrip_is_lossless() {
        let r = root("roundtrip");
        let d = decode(37, 21, 7);
        let k = key("/photos/a.ARW|123|456", "0.22.0-test");
        write_in(&r, &k, &d).unwrap();
        let back = read_in(&r, &k).expect("a hit");
        assert_eq!((back.width, back.height), (37, 21));
        assert_eq!(back.rgb16, d.rgb16);
        assert_eq!(back.orientation, d.orientation);
        assert_eq!(back.cam_mul, d.cam_mul);
        assert_eq!(back.pre_mul, d.pre_mul);
        assert_eq!(back.wbct, d.wbct);
        assert_eq!(back.rgb_cam, d.rgb_cam);
        assert_eq!(back.lens, None);
        // No temp file is left behind.
        let dir = dir_for(&r, "0.22.0-test");
        assert!(std::fs::read_dir(&dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));
        std::fs::remove_dir_all(&r).ok();
    }

    /// The lens tables ride in the header: a hit with the original unmounted still has them.
    #[test]
    fn cache_roundtrip_keeps_the_lens_tables() {
        let r = root("lens");
        let d = LinearDecode { lens: Some(lens()), ..decode(9, 5, 2) };
        let k = key("/photos/b.ARW|1|2", "dec");
        write_in(&r, &k, &d).unwrap();
        let back = read_in(&r, &k).expect("a hit");
        assert_eq!(back.lens, d.lens);
        assert_eq!(back.rgb16, d.rgb16);
        std::fs::remove_dir_all(&r).ok();
    }

    /// The file is little-endian whatever the host: the first pixel value 0x0102 is stored
    /// as the bytes 02 01 right after the header.
    #[test]
    fn pixels_are_stored_little_endian() {
        let r = root("endian");
        let mut d = decode(1, 1, 0);
        d.rgb16 = vec![0x0102, 0x0304, 0x0506];
        let k = key("/e|1|1", "dec");
        write_in(&r, &k, &d).unwrap();
        let bytes = std::fs::read(path_in(&r, &k)).unwrap();
        assert_eq!(&bytes[bytes.len() - 6..], &[0x02, 0x01, 0x04, 0x03, 0x06, 0x05]);
        assert_eq!(read_in(&r, &k).unwrap().rgb16, d.rgb16);
        std::fs::remove_dir_all(&r).ok();
    }

    #[test]
    fn cache_rejects_other_decoder_version() {
        let r = root("decoder");
        let d = decode(8, 8, 1);
        write_in(&r, &key("/p.ARW|1|2", "0.22.0"), &d).unwrap();
        assert!(read_in(&r, &key("/p.ARW|1|2", "0.23.0")).is_none());
        assert!(read_in(&r, &key("/p.ARW|1|2", "0.22.0")).is_some());
        std::fs::remove_dir_all(&r).ok();
    }

    #[test]
    fn a_changed_file_is_a_miss() {
        let r = root("changed");
        let f = r.join("a.ARW");
        std::fs::write(&f, b"one").unwrap();
        let k1 = CacheKey::for_file_with(&f, "dec").unwrap();
        write_in(&r, &k1, &decode(4, 4, 0)).unwrap();
        std::fs::write(&f, b"replaced, and longer").unwrap();
        let k2 = CacheKey::for_file_with(&f, "dec").unwrap();
        assert_ne!(k1, k2);
        assert!(read_in(&r, &k2).is_none());
        std::fs::remove_dir_all(&r).ok();
    }

    #[test]
    fn a_hash_collision_or_a_damaged_file_is_a_miss() {
        let r = root("collide");
        let k = key("/a.ARW|1|1", "dec");
        write_in(&r, &k, &decode(6, 6, 3)).unwrap();
        // Same file name (same hash) but a different stored key: pretend a collision by
        // asking for another key whose path we force to the same file.
        let other = key("/b.ARW|9|9", "dec");
        std::fs::copy(path_in(&r, &k), path_in(&r, &other)).unwrap();
        assert!(read_in(&r, &other).is_none(), "a stored key that differs is never served");
        // Truncated: a miss, not garbage pixels.
        let p = path_in(&r, &k);
        let bytes = std::fs::read(&p).unwrap();
        std::fs::write(&p, &bytes[..bytes.len() - 10]).unwrap();
        assert!(read_in(&r, &k).is_none());
        std::fs::write(&p, b"not a rawf file at all").unwrap();
        assert!(read_in(&r, &k).is_none());
        std::fs::remove_dir_all(&r).ok();
    }

    #[test]
    fn cache_trim_keeps_newest_within_budget() {
        let r = root("trim");
        let mut sizes = Vec::new();
        for i in 0..5 {
            let k = key(&format!("/p{i}.ARW|1|1"), "dec");
            write_in(&r, &k, &decode(10, 10, i)).unwrap();
            let p = path_in(&r, &k);
            // Distinct, increasing mtimes: p0 is the least recently used.
            let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 + i as u64 * 60);
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
            sizes.push(std::fs::metadata(&p).unwrap().len());
        }
        let budget = sizes[2] + sizes[3] + sizes[4];
        let freed = trim_in(&r, "dec", budget);
        assert_eq!(freed, sizes[0] + sizes[1]);
        for i in 0..5 {
            let present = path_in(&r, &key(&format!("/p{i}.ARW|1|1"), "dec")).exists();
            assert_eq!(present, i >= 2, "p{i}");
        }
        assert_eq!(usage_in(&r), budget);
        std::fs::remove_dir_all(&r).ok();
    }

    #[test]
    fn a_read_counts_as_a_use_for_trimming() {
        let r = root("lru");
        let (ka, kb) = (key("/a|1|1", "dec"), key("/b|1|1", "dec"));
        for (i, k) in [&ka, &kb].into_iter().enumerate() {
            write_in(&r, k, &decode(10, 10, i as u16)).unwrap();
            let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 + i as u64 * 60);
            std::fs::File::options().write(true).open(path_in(&r, k)).unwrap().set_modified(t).unwrap();
        }
        assert!(read_in(&r, &ka).is_some()); // a was older; reading it makes b the oldest
        let one = std::fs::metadata(path_in(&r, &ka)).unwrap().len();
        trim_in(&r, "dec", one);
        assert!(path_in(&r, &ka).exists());
        assert!(!path_in(&r, &kb).exists());
        std::fs::remove_dir_all(&r).ok();
    }

    #[test]
    fn trimming_removes_other_decoders_and_stale_temp_files() {
        let r = root("olddec");
        write_in(&r, &key("/a|1|1", "old"), &decode(4, 4, 0)).unwrap();
        write_in(&r, &key("/a|1|1", "new"), &decode(4, 4, 0)).unwrap();
        let stale = dir_for(&r, "new").join(".dead.tmp");
        std::fs::write(&stale, b"half a file").unwrap();
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&stale).unwrap().set_modified(t).unwrap();
        let fresh = dir_for(&r, "new").join(".live.tmp");
        std::fs::write(&fresh, b"being written").unwrap();
        trim_in(&r, "new", u64::MAX);
        assert!(!dir_for(&r, "old").exists(), "an old decoder's directory is unreachable");
        assert!(read_in(&r, &key("/a|1|1", "new")).is_some());
        assert!(!stale.exists());
        assert!(fresh.exists(), "a temp file that may still be being written stays");
        std::fs::remove_dir_all(&r).ok();
    }
}
