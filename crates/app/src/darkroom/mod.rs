//! The Darkroom (#111): the Develop surface. `basicEditor` folds in here (decision in #104):
//! the `edit` feature gates the whole module, and there is no edit-renderer contribution
//! point.
//!
//! - [`stage`]: the frame source — fast/full tiers through the image pool, generation-ordered,
//!   catalog-bound, timed (#101, #111).
//! - [`session`]: [`Darkroom`], the surface's state — the open photo, its working record,
//!   source state, autosave and filmstrip.
//! - [`view`]: [`DarkroomView`], the bar, stage, tone strip, filmstrip and the tone and
//!   effects rails.
//!
//! The rails of #112 (history, presets, lens, crop & rotate, versions, cover, proof sheet,
//! duels) attach to the seams `session` documents.

pub mod session;
pub mod stage;
pub mod view;

#[cfg(test)]
mod tests;

pub use session::Darkroom;
pub use stage::{DarkroomStage, FrameStats, FrameTier, StageFailure, StageFrame, FAST_EDGE, FAST_INTERVAL, FULL_EDGE, SETTLE};
pub use view::DarkroomView;
