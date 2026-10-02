//! What the Instagram command still takes from the shell's publishing helpers: the job-scoped
//! temp directory (core's `publishing::JobTempDir`).
//!
//! Flickr and SmugMug moved into core (`app::uploads`, `app::oauth`, `app::flickr`,
//! `app::smugmug`) with the GPUI publish targets (#124); their commands are thin wrappers.

#[cfg(feature = "instagram")]
pub(super) use crate::publishing::JobTempDir;
