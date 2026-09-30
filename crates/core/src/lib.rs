//! ChairPhoto's core: the catalog, import, XMP, decode, background jobs and the module
//! backends — everything the app does, with no Tauri dependency.
//!
//! The Tauri shell (`src-tauri/`, package `chairphoto`) holds `run()`, the command surface,
//! the native media protocols and the Tauri plugins, and re-exports this crate at its root.
//! Anything a front end needs from here is `pub`; front ends reach it through [`app::AppState`]
//! and the domain modules, and hear from it through [`app::events`].

pub mod app;
pub mod appearance;
pub mod bundle;
pub mod burst;
pub mod catalog;
#[cfg(feature = "collage")]
pub mod collage;
pub mod companions;
pub mod crash_marker;
#[cfg(all(feature = "raw", feature = "edit"))]
pub mod develop;
pub mod develop_source;
pub mod export;
pub mod external_edit;
#[cfg(feature = "flickr")]
pub mod flickr;
pub mod image_pool;
#[cfg(feature = "instagram")]
pub mod instagram;
#[cfg(feature = "raw")]
pub mod lens;
#[cfg(feature = "localsend")]
pub mod localsend;
#[cfg(feature = "edit")]
pub mod media;
pub mod metadata;
#[cfg(any(feature = "flickr", feature = "smugmug"))]
pub mod oauth1;
pub mod phash;
pub mod phash_indexer;
pub mod plugins;
pub mod rapidraw;
#[cfg(feature = "raw")]
pub mod raw;
pub mod scanner;
pub mod sharpness;
pub mod sharpness_indexer;
pub mod sharpness_regions;
#[cfg(feature = "slideshow")]
pub mod slideshow;
#[cfg(feature = "smugmug")]
pub mod smugmug;
#[cfg(test)]
mod test_support;
pub mod thumbnails;
pub mod volume_health;
pub mod xmp;
