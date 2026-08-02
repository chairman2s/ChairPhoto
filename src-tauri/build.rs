fn main() {
    // Generate LibRaw FFI bindings against the *installed* header so the struct ABI
    // matches the linked library exactly (the older `libraw-rs-sys` crate pins libraw
    // 0.20 and can't decode the newest Sony compressed ARW). Only when the `raw`
    // feature is on — keeps `--no-default-features` builds free of LibRaw/libclang.
    if std::env::var("CARGO_FEATURE_RAW").is_ok() {
        generate_libraw_bindings();
    }
    tauri_build::build()
}

fn generate_libraw_bindings() {
    use std::path::PathBuf;

    // Discover and link the system LibRaw; emits the cargo link directives itself.
    let lib = pkg_config::Config::new()
        .probe("libraw")
        .expect("LibRaw not found (pkg-config `libraw`). Install libraw, or build with `--no-default-features --features ai,edit`.");

    let mut builder = bindgen::Builder::default()
        // Parse the C API only (the C++ class in libraw.h is behind `#ifdef __cplusplus`).
        .header_contents("wrapper.h", "#include <libraw/libraw.h>")
        .allowlist_function("libraw_.*")
        .allowlist_type("libraw_.*")
        .allowlist_type("LibRaw_.*")
        .allowlist_var("LIBRAW_.*")
        .layout_tests(false);
    for path in &lib.include_paths {
        builder = builder.clang_arg(format!("-I{}", path.display()));
    }

    let bindings = builder.generate().expect("bindgen failed to generate LibRaw bindings");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("libraw_bindings.rs"))
        .expect("failed to write LibRaw bindings");
}
