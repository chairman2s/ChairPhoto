fn main() {
    // The `raw` feature compiles the vendored LibRaw (crates/core/vendor/LibRaw, a pinned
    // git submodule) into the binary and generates FFI bindings from *that tree's* header, so
    // the struct ABI matches the compiled library by construction. Only when the feature is
    // on — keeps `--no-default-features` builds free of C++/libclang. The link directives
    // below propagate to every binary that links this crate (the GPUI app, the tests).
    if std::env::var("CARGO_FEATURE_RAW").is_ok() {
        build_vendored_libraw();
    }
}

fn build_vendored_libraw() {
    use std::path::PathBuf;

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor = manifest.join("vendor").join("LibRaw");
    let header = vendor.join("libraw").join("libraw.h");
    assert!(
        header.is_file(),
        "vendored LibRaw is missing at {} — run `git submodule update --init` (or build with \
         `--no-default-features --features ai,edit`)",
        vendor.display()
    );
    // The submodule is pinned; a bump changes this file, which is enough of a trigger. Listing
    // every source would make cargo stat ~80 files per build for nothing.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", vendor.join("libraw").join("libraw_version.h").display());

    // Every .cpp under src/ — the same set Makefile.dist's LIB_OBJECTS names; the DNG-SDK and
    // RawSpeed glue compile to nothing without their USE_* defines.
    let mut sources = Vec::new();
    collect_cpp(&vendor.join("src"), &mut sources);
    sources.sort();
    assert!(sources.len() > 50, "unexpectedly few LibRaw sources ({})", sources.len());

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .include(&vendor)
        .files(&sources)
        .define("USE_ZLIB", None)
        // Makefile.dist's flags: -O3, warnings off. Forced even in dev profiles — an
        // unoptimized demosaic of a 67 MP file takes a minute.
        .opt_level(3)
        .warnings(false)
        .flag_if_supported("-w")
        .flag_if_supported("-std=c++17");
    // OpenMP parallelizes the demosaic (LibRaw checks the compiler's own `_OPENMP`). gcc and
    // clang spell the runtime differently; without either the decode is merely single-threaded.
    let compiler = build.get_compiler();
    let openmp = if compiler.is_like_clang() { "omp" } else { "gomp" };
    if build.is_flag_supported("-fopenmp").unwrap_or(false) {
        build.flag("-fopenmp");
        println!("cargo:rustc-link-lib={openmp}");
    }
    build.compile("raw");
    println!("cargo:rustc-link-lib=z");

    let bindings = bindgen::Builder::default()
        // Parse the C API only (the C++ class in libraw.h is behind `#ifdef __cplusplus`).
        .header(header.to_string_lossy())
        .clang_arg(format!("-I{}", vendor.display()))
        .allowlist_function("libraw_.*")
        .allowlist_type("libraw_.*")
        .allowlist_type("LibRaw_.*")
        .allowlist_var("LIBRAW_.*")
        .layout_tests(false)
        .generate()
        .expect("bindgen failed to generate LibRaw bindings");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("libraw_bindings.rs"))
        .expect("failed to write LibRaw bindings");
}

fn collect_cpp(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read LibRaw src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_cpp(&path, out);
        } else if path.extension().is_some_and(|e| e == "cpp") {
            out.push(path);
        }
    }
}
