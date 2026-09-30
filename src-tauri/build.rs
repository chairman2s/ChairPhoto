fn main() {
    // The vendored LibRaw is the core crate's to compile (crates/core/build.rs); the shell only
    // runs Tauri's own codegen (context, capabilities, icons).
    tauri_build::build()
}
