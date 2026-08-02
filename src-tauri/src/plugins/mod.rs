//! Optional plugin backends, each behind a Cargo feature so it can be compiled out.
//! See docs/plugin-system.md.

#[cfg(feature = "ai")]
pub mod ai;

#[cfg(feature = "edit")]
pub mod edit;

#[cfg(feature = "faces")]
pub mod faces;

#[cfg(feature = "map")]
pub mod map;

#[cfg(feature = "smarttags")]
pub mod smarttags;
