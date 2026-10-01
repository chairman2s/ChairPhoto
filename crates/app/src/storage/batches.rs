//! The collection browser's "import batches" section (`BatchesPanel.tsx`): each batch's
//! last-path-segment title and photo count; a click filters the library to that batch (a
//! second click on the active one clears it); ⬇ "Export as bundle" per row (Albums and
//! export, #115); "No imports yet". The list is the shell's (`Lists::batches`), re-read
//! whenever the catalog is (after an import or a scan, too).

use crate::shell::state::ShellState;
use crate::shell::style::{grouped, Colors};
use crate::shell::title_bar::batch_label;
use crate::view::RootView;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, AnyElement, Context, SharedString, TestSupportExt as _};

impl RootView {
    pub(crate) fn render_batches(&self, shell: &ShellState, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let batches = &shell.lists.batches;
        if batches.is_empty() {
            return div()
                .id("batches-empty")
                .px(px(14.))
                .py(px(4.))
                .text_size(px(11.))
                .text_color(colors.mute)
                .child("No imports yet")
                .test_support()
                .into_any_element();
        }
        let active = shell.library.scope().batch_id;
        div()
            .id("batches")
            .flex()
            .flex_col()
            .children(batches.iter().map(|b| {
                let on = active == Some(b.id);
                let batch = b.clone();
                div()
                    .id(SharedString::from(format!("batch-{}", b.id)))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(25.))
                    .px(px(14.))
                    .cursor_pointer()
                    .text_color(if on { colors.txt } else { colors.dim })
                    .when(on, |r| r.bg(colors.sel))
                    .hover(|s| s.text_color(colors.txt).bg(colors.elev))
                    .child(div().flex_1().min_w_0().truncate().child(batch_label(b)))
                    .child(div().text_size(px(10.5)).text_color(colors.mute).child(grouped(b.photo_count.max(0) as usize)))
                    .child(
                        div()
                            .id(SharedString::from(format!("batch-export-{}", b.id)))
                            .text_size(px(11.))
                            .text_color(colors.mute)
                            .hover(|s| s.text_color(colors.txt))
                            .child("⬇")
                            .tooltip(crate::shell::title_bar::tooltip(
                                "Export this batch as a .chairphoto bundle for transfer to another machine",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.model.update(cx, |m, cx| m.not_yet_ported("Export a bundle", 115, cx));
                            }))
                            .test_support(),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let next = if on { None } else { Some(batch.clone()) };
                        this.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.select_batch(next)));
                    }))
                    .test_support()
            }))
            .into_any_element()
    }
}
