//! The Darkroom's pure logic, ported from `src/components/darkroom/` (tickets #103, #111):
//! record maths for proofs and duels, Kelvin white balance, history labels, the stage record,
//! the source badge and the lens hint (#103); the filmstrip window, the tone strip's drag
//! maths, render timing and the edit controls' record transitions (#111); the framing maths,
//! preset application and history stepping (#112). View code (the rails, stage, strip) lives with the GPUI views that draw it (`crates/app/src/darkroom`).

pub mod controls;
pub mod develop_source;
pub mod filmstrip;
pub mod geometry;
pub mod history;
pub mod kelvin;
pub mod lens_rail;
pub mod render_timing;
pub mod spreads;
pub mod stage_json;
pub mod tone_strip;
