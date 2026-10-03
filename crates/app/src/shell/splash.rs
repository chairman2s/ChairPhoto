//! The startup splash (`src/components/Splash.tsx`): it covers the window from the first frame
//! until the catalog, the modules and the first photo list are in, reporting the real boot
//! stage with a stepped progress bar, then shows "Ready" and fades out over [`FADE`].
//!
//! The stages are what `AppModel`'s boot does (App.tsx's `initCatalog()` chain):
//!
//! | Stage | Until |
//! |---|---|
//! | Opening catalog… | `app::open_default_catalog` answered |
//! | Updating auto-tags… | `apply_auto_tags` ran (monochrome, for photos imported before the rule); a failure is ignored, as React's `.catch(() => {})` |
//! | Starting modules… | the first catalog read landed — the module registry restores the enabled modules on it |
//! | Loading photos… | the Library's first rows landed |
//!
//! The boot ends when both the init chain and the first photo list are in, whichever is last
//! (React's `finishBootPart`); a failed boot ends it too, so the splash never hangs over an
//! error. Only a launch that opens the default catalog shows it (`WireOptions::
//! open_default_catalog`); the headless tests' launches start without one.

use crate::shell::style::Colors;
use crate::view::RootView;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, relative, rgb, Animation, AnimationExt as _, AnyElement, FontWeight, TestSupportExt as _};
use std::time::Duration;

/// The fade-out (`.splash` CSS transition, and Splash.tsx's unmount timer).
pub const FADE: Duration = Duration::from_millis(450);

/// The boot stages, in order (`BOOT_STAGES`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootStage {
    OpeningCatalog,
    UpdatingAutoTags,
    StartingModules,
    LoadingPhotos,
}

impl BootStage {
    pub const ALL: [BootStage; 4] =
        [BootStage::OpeningCatalog, BootStage::UpdatingAutoTags, BootStage::StartingModules, BootStage::LoadingPhotos];

    pub fn label(self) -> &'static str {
        match self {
            BootStage::OpeningCatalog => "Opening catalog…",
            BootStage::UpdatingAutoTags => "Updating auto-tags…",
            BootStage::StartingModules => "Starting modules…",
            BootStage::LoadingPhotos => "Loading photos…",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// The splash's state. Pure; `AppModel` drives it and the root view draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct Splash {
    /// The stage shown; `None` once the boot is over (fading out, or gone).
    stage: Option<BootStage>,
    /// The last real stage, kept on screen while fading (`lastStage`).
    last: BootStage,
    init_done: bool,
    photos_done: bool,
    /// The fade has finished: nothing is drawn.
    gone: bool,
}

impl Splash {
    /// No splash: a launch that opens no default catalog (tests).
    pub fn inactive() -> Self {
        Splash { stage: None, last: BootStage::OpeningCatalog, init_done: false, photos_done: false, gone: true }
    }

    /// A boot starting: "Opening catalog…".
    pub fn booting() -> Self {
        Splash { stage: Some(BootStage::OpeningCatalog), gone: false, ..Self::inactive() }
    }

    /// The boot reached `stage`. Ignored once the boot is over.
    pub fn set_stage(&mut self, stage: BootStage) {
        if self.stage.is_some() {
            self.stage = Some(stage);
            self.last = stage;
        }
    }

    pub fn stage(&self) -> Option<BootStage> {
        self.stage
    }

    /// The init chain (catalog, auto-tags, modules) is done. Returns whether the boot just
    /// ended (the caller starts the fade).
    pub fn finish_init(&mut self) -> bool {
        self.init_done = true;
        self.end_if_done()
    }

    /// The first photo list is in. Returns whether the boot just ended.
    pub fn finish_photos(&mut self) -> bool {
        self.photos_done = true;
        self.end_if_done()
    }

    fn end_if_done(&mut self) -> bool {
        if self.stage.is_some() && self.init_done && self.photos_done {
            self.stage = None;
            return true;
        }
        false
    }

    /// The boot failed: end it now ("never leave the splash hanging on a failed boot").
    /// Returns whether it was still running.
    pub fn fail(&mut self) -> bool {
        self.stage.take().is_some()
    }

    /// The fade is over.
    pub fn set_gone(&mut self) {
        self.gone = true;
    }

    /// Anything to draw (booting, or fading out).
    pub fn showing(&self) -> bool {
        !self.gone
    }

    /// Fading out: the boot is over but the overlay is still up.
    pub fn hiding(&self) -> bool {
        self.stage.is_none() && !self.gone
    }

    /// The bar's fill, 0–100: one step per stage out of five, full while fading.
    pub fn progress(&self) -> f32 {
        if self.hiding() {
            100.
        } else {
            (self.last.index() + 1) as f32 * 100. / (BootStage::ALL.len() + 1) as f32
        }
    }

    /// The line under the bar: the stage, or "Ready" while fading.
    pub fn label(&self) -> &'static str {
        if self.hiding() {
            "Ready"
        } else {
            self.last.label()
        }
    }
}

impl RootView {
    /// `.splash`: the logo, the name, the bar and the stage, centred over the whole window. It
    /// takes the pointer while up (the window under it is not ready), and fades out once the
    /// boot is over.
    pub(crate) fn render_splash(&self, splash: &Splash, colors: Colors) -> AnyElement {
        // `.splash-logo`: a disc in three vertical bands, red · white · blue.
        let logo = div()
            .size(px(64.))
            .rounded_full()
            .overflow_hidden()
            .flex()
            .flex_row()
            .child(div().h_full().w(relative(0.46)).bg(rgb(0xdc2626)))
            .child(div().h_full().w(relative(0.08)).bg(rgb(0xf8fafc)))
            .child(div().h_full().flex_1().bg(rgb(0x1e40af)));
        let overlay = div()
            .id("splash")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(14.))
            .bg(colors.canvas)
            .child(logo)
            .child(div().text_size(px(22.)).font_weight(FontWeight::EXTRA_BOLD).text_color(colors.txt).child("ChairPhoto"))
            .child(
                div()
                    .w(px(220.))
                    .h(px(3.))
                    .rounded(px(2.))
                    .bg(colors.panel)
                    .overflow_hidden()
                    .child(div().h_full().rounded(px(2.)).bg(colors.accent).w(relative(splash.progress() / 100.))),
            )
            .child(
                div()
                    .id("splash-stage")
                    .min_h(px(16.))
                    .text_size(px(12.))
                    .text_color(colors.mute)
                    .child(splash.label())
                    .aria_label(splash.label())
                    .test_support(),
            )
            .test_support();
        if splash.hiding() {
            overlay
                .with_animation("splash-fade", Animation::new(FADE), |el, t| el.opacity(1. - t))
                .into_any_element()
        } else {
            overlay.into_any_element()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Splash.tsx's arithmetic: stage i of 4 fills (i + 1) / 5; fading is full and "Ready".
    #[test]
    fn the_bar_steps_through_the_stages_then_reads_ready() {
        let mut s = Splash::booting();
        assert!(s.showing() && !s.hiding());
        assert_eq!((s.progress(), s.label()), (20., "Opening catalog…"));
        s.set_stage(BootStage::UpdatingAutoTags);
        assert_eq!((s.progress(), s.label()), (40., "Updating auto-tags…"));
        s.set_stage(BootStage::StartingModules);
        assert_eq!(s.progress(), 60.);
        s.set_stage(BootStage::LoadingPhotos);
        assert_eq!((s.progress(), s.label()), (80., "Loading photos…"));
        assert!(!s.finish_init(), "the photos are not in yet");
        assert!(s.finish_photos(), "both parts in: the boot ends");
        assert!(s.hiding());
        assert_eq!((s.progress(), s.label()), (100., "Ready"));
        s.set_stage(BootStage::OpeningCatalog);
        assert_eq!(s.stage(), None, "a stage after the end is ignored");
        s.set_gone();
        assert!(!s.showing());
    }

    /// Whichever part lands last ends it, once; a failure ends it at once.
    #[test]
    fn either_part_may_land_last_and_a_failure_ends_it() {
        let mut s = Splash::booting();
        assert!(!s.finish_photos());
        assert!(s.finish_init());
        assert!(!s.finish_init(), "it ends once");
        let mut f = Splash::booting();
        assert!(f.fail());
        assert!(f.hiding(), "a failed boot fades out rather than hanging");
        assert_eq!(f.label(), "Ready");
        assert!(!f.fail());
        assert!(!Splash::inactive().showing(), "no boot, no splash");
    }
}
