//! The Darkroom's pure logic, ported from `src/components/darkroom/` (ticket #103): record
//! maths for proofs and duels, Kelvin white balance, history labels, the stage record, the
//! source badge and the lens hint. View code (the rails, stage, strip) ports with the GPUI
//! views that draw it.

pub mod history;
pub mod kelvin;
pub mod spreads;
pub mod stage_json;
