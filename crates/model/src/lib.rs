//! ChairPhoto's UI-free model: the pure logic of the React front end, ported to plain Rust
//! for the GPUI app (issue #92). No GPUI, no Tauri — everything here is testable headless.

pub mod darkroom;
pub mod editing;
pub mod js_compat;
pub mod presets;
