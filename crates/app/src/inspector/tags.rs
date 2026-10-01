//! The inspector's tags tab: the slot the Tag panel and tag editor ticket (#107) fills.
//!
//! `PhotoInspector.tsx`'s tags tab held the photo's tag chips (× removes from the whole
//! selection), Copy tags / Paste N → M, the add-tag input with autocomplete, and "From nearby
//! photos"; `Inspector.tsx` put QuickTagGroups under them. Those are #107's (tag editing,
//! `TagEditor`, QuickTagGroups). This ticket (#108) owns the tab itself: [`tags_tab`] is the
//! element the shell renders first on the tab, and the enabled modules' inspector panels
//! (`PanelSlot::Inspector`) follow it (`shell::inspector`).
//!
//! **For #107:** replace [`tags_tab`]'s body (or make it render your entity); the shell passes
//! the photo shown and calls it only while a photo is active.

use crate::shell::style::Colors;
use chairphoto_core::catalog::Photo;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, AnyElement, FontWeight, TestSupportExt as _};

/// The tags tab's own content for `photo`, above the module panels. A placeholder until the
/// Tag panel (#107) fills it.
pub fn tags_tab(_photo: &Photo, colors: Colors) -> AnyElement {
    div()
        .id("inspector-tags-slot")
        .flex()
        .flex_col()
        .gap(px(6.))
        .px(px(14.))
        .py(px(10.))
        .child(div().text_size(px(10.)).font_weight(FontWeight::BOLD).text_color(colors.mute).child("TAGS"))
        .child(
            div()
                .text_size(px(11.))
                .text_color(colors.mute)
                .child("Tag chips, copy/paste and nearby suggestions come with the Tag panel (#107)."),
        )
        .test_support()
        .into_any_element()
}
