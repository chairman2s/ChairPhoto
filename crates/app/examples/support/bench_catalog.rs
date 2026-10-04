//! The benches' scratch catalogs (#168): generated JPEG originals, or the files of a
//! directory, catalogued in a directory of the bench's own — never the user's catalog.
//! Shared by `loupe_bench` and `darkroom_bench` (`#[path]`; not an example itself).

use chairphoto_core::app::AppState;
use chairphoto_core::catalog::Catalog;
use std::path::{Path, PathBuf};

/// `n` `w`×`h` JPEG originals with a different gradient each, in a catalog rooted in `dir`.
pub fn synthetic(state: &AppState, dir: &Path, n: usize, (w, h): (u32, u32)) -> PathBuf {
    let root = dir.join("photos");
    std::fs::create_dir_all(root.join("bench")).expect("bench dir");
    let catalog = Catalog::open(&dir.join("bench.chairphoto"), &root).expect("open the bench catalog");
    for i in 0..n {
        let path = root.join(format!("bench/b{i:05}.jpg"));
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x * 250 / w) as u8, (y * 250 / h) as u8, ((x + y + i as u32 * 37) / 20) as u8])
        });
        img.save(&path).expect("write a bench JPEG");
        catalog.upsert_photo(&path, None, 0, 1).expect("add a bench photo");
    }
    *state.catalog.lock().unwrap() = Some(catalog);
    root
}

/// Every file directly in `originals`, catalogued (by absolute path) in a catalog in `dir`.
pub fn originals(state: &AppState, dir: &Path, originals: &Path) {
    // The bench deletes `dir` afterwards: never the originals' directory or one holding it.
    assert!(!originals.starts_with(dir), "--dir must not hold --originals");
    std::fs::create_dir_all(dir).expect("bench dir");
    let catalog = Catalog::open(&dir.join("bench.chairphoto"), originals).expect("open the bench catalog");
    let mut files: Vec<PathBuf> = std::fs::read_dir(originals)
        .expect("read --originals")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|e| !e.eq_ignore_ascii_case("xmp")))
        .collect();
    files.sort();
    for path in files {
        catalog.upsert_photo(&path, None, 0, 1).expect("add a bench photo");
    }
    *state.catalog.lock().unwrap() = Some(catalog);
}

/// `--size WxH`, default 3000x2000.
pub fn size_arg() -> (u32, u32) {
    arg("--size").map_or((3000, 2000), |v| {
        let (w, h) = v.split_once('x').expect("--size WxH");
        (w.parse().expect("--size WxH"), h.parse().expect("--size WxH"))
    })
}

/// The value after `name` on the command line.
pub fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}
