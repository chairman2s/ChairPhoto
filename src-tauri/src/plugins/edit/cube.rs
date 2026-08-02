//! Adobe/Resolve `.cube` 3D LUT files: parsing and trilinear sampling, plus a small
//! mtime-validated cache so the live-preview path (a render every ~120ms while a
//! slider drags) doesn't re-parse a 36k-line file each time. LUT files live in the
//! app-data `luts/` folder and edit records reference them by filename only, so a
//! catalog moved to another machine just needs the folder copied.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

pub struct CubeLut {
    size: usize,
    domain_min: [f32; 3],
    domain_max: [f32; 3],
    /// `size³` RGB triples in .cube order: R varies fastest, then G, then B.
    data: Vec<[f32; 3]>,
}

impl CubeLut {
    /// Parse `.cube` text: `LUT_3D_SIZE`, optional `DOMAIN_MIN`/`DOMAIN_MAX`/`TITLE`,
    /// `#` comments, then exactly N³ data rows.
    pub fn parse(text: &str) -> Result<CubeLut, String> {
        let mut size = 0usize;
        let mut domain_min = [0.0f32; 3];
        let mut domain_max = [1.0f32; 3];
        let mut data: Vec<[f32; 3]> = Vec::new();

        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let first = parts.next().unwrap();
            match first {
                "TITLE" => {}
                "LUT_1D_SIZE" => return Err("1D LUTs are not supported (need LUT_3D_SIZE)".into()),
                "LUT_3D_SIZE" => {
                    size = parts
                        .next()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| format!("line {}: bad LUT_3D_SIZE", lineno + 1))?;
                    if !(2..=128).contains(&size) {
                        return Err(format!("LUT_3D_SIZE {size} out of range (2–128)"));
                    }
                    data.reserve(size * size * size);
                }
                "DOMAIN_MIN" | "DOMAIN_MAX" => {
                    let mut v = [0.0f32; 3];
                    for x in v.iter_mut() {
                        *x = parts
                            .next()
                            .and_then(|s| s.parse().ok())
                            .ok_or_else(|| format!("line {}: bad {first}", lineno + 1))?;
                    }
                    if first == "DOMAIN_MIN" {
                        domain_min = v;
                    } else {
                        domain_max = v;
                    }
                }
                _ => {
                    // A data row: three floats.
                    let r: f32 = first
                        .parse()
                        .map_err(|_| format!("line {}: unrecognized line", lineno + 1))?;
                    let g: f32 = parts
                        .next()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| format!("line {}: bad data row", lineno + 1))?;
                    let b: f32 = parts
                        .next()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| format!("line {}: bad data row", lineno + 1))?;
                    data.push([r, g, b]);
                }
            }
        }

        if size == 0 {
            return Err("missing LUT_3D_SIZE".into());
        }
        let expected = size * size * size;
        if data.len() != expected {
            return Err(format!("expected {expected} data rows, got {}", data.len()));
        }
        for i in 0..3 {
            if domain_max[i] <= domain_min[i] {
                return Err("DOMAIN_MAX must exceed DOMAIN_MIN".into());
            }
        }
        Ok(CubeLut { size, domain_min, domain_max, data })
    }

    /// Sample the LUT at an RGB triple (any range; clamped to the domain), with
    /// trilinear interpolation over the 8 surrounding lattice points.
    pub fn sample(&self, rgb: [f32; 3]) -> [f32; 3] {
        let n = self.size;
        // Map each channel into continuous lattice coordinates [0, n-1].
        let mut t = [0.0f32; 3];
        let mut i0 = [0usize; 3];
        let mut i1 = [0usize; 3];
        for c in 0..3 {
            let x = (rgb[c] - self.domain_min[c]) / (self.domain_max[c] - self.domain_min[c]);
            let x = x.clamp(0.0, 1.0) * (n - 1) as f32;
            let f = x.floor();
            i0[c] = f as usize;
            i1[c] = (i0[c] + 1).min(n - 1);
            t[c] = x - f;
        }
        let at = |r: usize, g: usize, b: usize| self.data[r + g * n + b * n * n];
        let mut out = [0.0f32; 3];
        for c in 0..3 {
            // Interpolate along R, then G, then B.
            let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
            let c00 = lerp(at(i0[0], i0[1], i0[2])[c], at(i1[0], i0[1], i0[2])[c], t[0]);
            let c10 = lerp(at(i0[0], i1[1], i0[2])[c], at(i1[0], i1[1], i0[2])[c], t[0]);
            let c01 = lerp(at(i0[0], i0[1], i1[2])[c], at(i1[0], i0[1], i1[2])[c], t[0]);
            let c11 = lerp(at(i0[0], i1[1], i1[2])[c], at(i1[0], i1[1], i1[2])[c], t[0]);
            out[c] = lerp(lerp(c00, c10, t[1]), lerp(c01, c11, t[1]), t[2]);
        }
        out
    }
}

/// Load a LUT by filename from `dir`, through the process-wide cache (validated by
/// file mtime). Returns `None` — never an error — when the file is missing or fails
/// to parse: a render must still succeed on a machine that lacks the LUT file.
pub fn load(dir: &Path, file: &str) -> Option<Arc<CubeLut>> {
    // Refuse path separators: edit records reference LUTs by bare filename only.
    if file.contains('/') || file.contains('\\') || file.is_empty() {
        return None;
    }
    static CACHE: OnceLock<Mutex<HashMap<String, (SystemTime, Arc<CubeLut>)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let path = dir.join(file);
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
    if let Some((cached_mtime, lut)) = cache.lock().unwrap().get(file) {
        if *cached_mtime == mtime {
            return Some(lut.clone());
        }
    }
    let text = std::fs::read_to_string(&path).ok()?;
    match CubeLut::parse(&text) {
        Ok(lut) => {
            let lut = Arc::new(lut);
            cache
                .lock()
                .unwrap()
                .insert(file.to_string(), (mtime, lut.clone()));
            Some(lut)
        }
        Err(e) => {
            eprintln!("[edit] ignoring bad LUT {file}: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDENTITY_2: &str = "\
# identity
LUT_3D_SIZE 2
0 0 0
1 0 0
0 1 0
1 1 0
0 0 1
1 0 1
0 1 1
1 1 1
";

    #[test]
    fn identity_lut_is_a_noop() {
        let lut = CubeLut::parse(IDENTITY_2).unwrap();
        for rgb in [[0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [0.25, 0.5, 0.75]] {
            let out = lut.sample(rgb);
            for c in 0..3 {
                assert!((out[c] - rgb[c]).abs() < 1e-5, "identity failed for {rgb:?} → {out:?}");
            }
        }
    }

    #[test]
    fn swap_lut_swaps_channels() {
        // Output rows are (g, b, r) of the lattice point → sampling must swap.
        let swapped = "\
LUT_3D_SIZE 2
0 0 0
0 0 1
1 0 0
1 0 1
0 1 0
0 1 1
1 1 0
1 1 1
";
        let lut = CubeLut::parse(swapped).unwrap();
        let out = lut.sample([1.0, 0.0, 0.0]); // pure red in
        assert!(out[0] < 1e-5 && out[2] > 1.0 - 1e-5, "expected r→b swap, got {out:?}");
    }

    #[test]
    fn domain_is_respected() {
        let scaled = "\
LUT_3D_SIZE 2
DOMAIN_MIN 0 0 0
DOMAIN_MAX 2 2 2
0 0 0
1 0 0
0 1 0
1 1 0
0 0 1
1 0 1
0 1 1
1 1 1
";
        let lut = CubeLut::parse(scaled).unwrap();
        let out = lut.sample([1.0, 1.0, 1.0]); // mid-domain → 0.5
        for c in 0..3 {
            assert!((out[c] - 0.5).abs() < 1e-5, "mid-domain must be 0.5, got {out:?}");
        }
    }

    #[test]
    fn load_resolves_caches_and_rejects_paths() {
        let dir = std::env::temp_dir().join(format!("chairphoto-lut-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("id.cube"), IDENTITY_2).unwrap();
        assert!(load(&dir, "id.cube").is_some());
        assert!(load(&dir, "id.cube").is_some(), "second load = cache hit");
        assert!(load(&dir, "../id.cube").is_none(), "path separators must be rejected");
        assert!(load(&dir, "missing.cube").is_none(), "missing file → None, not error");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn malformed_luts_error() {
        assert!(CubeLut::parse("").is_err(), "empty");
        assert!(CubeLut::parse("LUT_1D_SIZE 2\n0\n1\n").is_err(), "1D");
        assert!(CubeLut::parse("LUT_3D_SIZE 2\n0 0 0\n").is_err(), "row count");
        assert!(CubeLut::parse("LUT_3D_SIZE 1\n0 0 0\n").is_err(), "size range");
        assert!(CubeLut::parse("LUT_3D_SIZE 2\nnot a row\n").is_err(), "garbage");
    }
}
