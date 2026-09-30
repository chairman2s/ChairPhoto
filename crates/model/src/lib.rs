//! ChairPhoto's UI model: the state and logic behind the front end, as plain Rust.
//!
//! Everything here is GPUI-free and Tauri-free, so it is unit-testable without a window and
//! a GPUI view can own it as an entity. Each module is a one-to-one port of a TypeScript
//! module from the React front end; its doc comment names the source and states where the
//! port had to choose between JavaScript and Rust semantics.

pub mod compare_duel;
pub mod deep_link;
pub mod js_compat;
pub mod library;
pub mod shell_timing;
pub mod tag_paste;
pub mod theme;
