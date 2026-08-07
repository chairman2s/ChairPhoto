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

/// Shared by the two ONNX-backed plugins; see the module docs for why it must run before ort.
#[cfg(any(feature = "faces", feature = "smarttags"))]
pub mod onnx;

#[cfg(feature = "smarttags")]
pub mod smarttags;
