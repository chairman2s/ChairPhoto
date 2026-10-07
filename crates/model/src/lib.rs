//! ChairPhoto's UI model: the state and logic behind the front end, as plain Rust.
//!
//! Everything here is GPUI-free and Tauri-free, so it is unit-testable without a window and
//! a GPUI view can own it as an entity. Each module is a one-to-one port of a TypeScript
//! module from the React front end; its doc comment names the source and states where the
//! port had to choose between JavaScript and Rust semantics ([`js_compat`] holds those rules).

pub mod collage;
pub mod compare_duel;
pub mod darkroom;
pub mod deep_link;
pub mod editing;
pub mod js_compat;
pub mod library;
pub mod obsidian;
pub mod presets;
pub mod publishing;
pub mod shell_timing;
pub mod slideshow;
pub mod statistics;
pub mod storage_outcome;
pub mod tag_graph;
pub mod tag_paste;
pub mod tag_tree;
pub mod theme;
