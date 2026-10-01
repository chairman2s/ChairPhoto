//! The bench (`src/components/shell/Bench.tsx`): the 72 px strip under the stage. Left to
//! right: one background job's progress, else "N photos · M selected" and the status line;
//! the marking controls for the photo the inspector shows; the selection pile ("N on the
//! table") with the selection's actions.
//!
//! The marking and pile sections appear only with an active photo / a selection, which
//! only the Library view (#106) can make. Their marks write through the one culling path
//! the grid's keys will use, so they are wired when that path exists; until then a mark
//! reports "not yet ported" like every other unported action, instead of writing through
//! a second path the keys would not share (Bench.tsx's "one code path" invariant). The
//! pile's thumbnails come from the image layer (#101).

use crate::model::AppModel;
use crate::shell::actions::*;
use crate::shell::state::ShellState;
use crate::shell::style::{grouped, Colors, COLOR_LABELS};
use crate::view::RootView;
use chairphoto_core::catalog::PickState;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, relative, Action, AnyElement, Context, FontWeight, SharedString, TestSupportExt as _};

/// The bench's height (`.bench`).
pub const BENCH_H: f32 = 72.;

/// "N photos" and, with a selection, " · M selected".
pub fn count_line(total: usize, selected: usize) -> String {
    if selected > 0 {
        format!("{} photos · {} selected", grouped(total), grouped(selected))
    } else {
        format!("{} photos", grouped(total))
    }
}

impl RootView {
    pub(crate) fn render_bench(&self, shell: &ShellState, model: &AppModel, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let selection = shell.library.selection();
        let selected = selection.ids.len();
        let total = shell.scope_info.total.or(model.catalog.as_ref().map(|c| c.photo_count)).unwrap_or(0);
        let ready = model.catalog.is_some();

        let prog = match shell.jobs.bench_progress() {
            Some(p) => {
                let fill = p.total.map_or(0.4, |t| (p.done as f32 / t.max(1) as f32).clamp(0., 1.));
                div()
                    .id("bench-progress")
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(colors.mute)
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(p.label.clone()),
                    )
                    .child(
                        div()
                            .mt(px(6.))
                            .h(px(3.))
                            .rounded_full()
                            .bg(colors.elev)
                            .child(div().h_full().rounded_full().bg(colors.accent).w(relative(fill))),
                    )
                    // An import can be stopped before its next file (Storage and import, #114).
                    .when(shell.jobs.import.is_some(), |d| {
                        d.child(
                            div()
                                .id("bench-cancel-import")
                                .mt(px(4.))
                                .text_size(px(10.5))
                                .text_color(colors.dim)
                                .cursor_pointer()
                                .hover(|s| s.text_color(colors.txt))
                                .child("Cancel import")
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(crate::shell::actions::CancelImport), cx)
                                })
                                .test_support(),
                        )
                    })
                    .test_support()
                    .into_any_element()
            }
            None => div()
                .child(
                    div()
                        .id("bench-count")
                        .text_size(px(11.))
                        .text_color(colors.dim)
                        .child(count_line(total, selected))
                        .test_support(),
                )
                .when(!model.status.is_empty(), |d| {
                    d.child(
                        div()
                            .id("bench-status")
                            .mt(px(3.))
                            .text_size(px(10.5))
                            .text_color(colors.mute)
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(model.status.clone())
                            .test_support(),
                    )
                })
                .into_any_element(),
        };

        let mut bench = div()
            .id("bench")
            .flex()
            .flex_none()
            .flex_row()
            .items_center()
            .gap(px(18.))
            .h(px(BENCH_H))
            .px(px(14.))
            .bg(colors.panel)
            .border_t_1()
            .border_color(colors.line)
            .min_w_0()
            .child(div().w(px(214.)).flex_none().min_w_0().child(prog));

        if let Some(active) = selection.active {
            let filename = active.path.rsplit('/').next().unwrap_or(&active.path).to_string();
            let rating = active.rating;
            let mut stars = div().flex().gap(px(3.));
            for n in 1..=5i64 {
                let on = n <= rating as i64;
                stars = stars.child(
                    div()
                        .id(SharedString::from(format!("bench-star-{n}")))
                        .text_size(px(18.))
                        .line_height(relative(1.))
                        .cursor_pointer()
                        .text_color(if on { colors.rating } else { colors.mute })
                        .opacity(if on { 1. } else { 0.5 })
                        .child("★")
                        .on_click(cx.listener(|this, _, _, cx| this.mark(cx))),
                );
            }
            let pick = |id: &'static str, label: &'static str, on: bool| {
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    .h(px(26.))
                    .px(px(11.))
                    .rounded_full()
                    .border_1()
                    .text_size(px(11.5))
                    .cursor_pointer()
                    .child(label)
                    .when(on, |b| b.bg(colors.sel).border_color(colors.ok).text_color(colors.ok))
                    .when(!on, |b| b.border_color(colors.border).text_color(colors.dim).hover(|s| s.text_color(colors.txt)))
            };
            let mut dots = div().flex().items_center().gap(px(5.));
            for label in COLOR_LABELS {
                let on = active.label.eq_ignore_ascii_case(label.name);
                dots = dots.child(
                    crate::shell::command_pill::label_dot(
                        SharedString::from(format!("bench-label-{}", label.name)),
                        Some(label.color()),
                        on,
                        colors,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.mark(cx))),
                );
            }
            dots = dots.child(
                crate::shell::command_pill::label_dot("bench-label-none".into(), None, false, colors)
                    .on_click(cx.listener(|this, _, _, cx| this.mark(cx))),
            );
            bench = bench.child(div().flex_none().w(px(1.)).h(px(38.)).bg(colors.border)).child(
                div()
                    .id("bench-mark")
                    .child(
                        div()
                            .flex()
                            .gap(px(4.))
                            .mb(px(7.))
                            .text_size(px(9.5))
                            .text_color(colors.mute)
                            .child("MARKING")
                            .child(div().text_color(colors.dim).font_weight(FontWeight::MEDIUM).child(filename)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(14.))
                            .child(stars)
                            .child(
                                pick("bench-pick", "Pick", active.pick_state == PickState::Pick)
                                    .on_click(cx.listener(|this, _, _, cx| this.mark(cx))),
                            )
                            .child(
                                pick("bench-reject", "Reject", active.pick_state == PickState::Reject)
                                    .on_click(cx.listener(|this, _, _, cx| this.mark(cx))),
                            )
                            .child(dots),
                    ),
            );
        }

        if selected > 0 {
            let can_compare = selected >= 2;
            let can_export = !selection.targets.is_empty();
            let can_publish = selection.active_id.is_some();
            let can_back_up = ready;
            let txt = |id: &'static str, label: &'static str, enabled: bool, action: Box<dyn Action>| {
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    .h(px(30.))
                    .px(px(12.))
                    .rounded_full()
                    .border_1()
                    .border_color(colors.border)
                    .text_size(px(11.5))
                    .text_color(colors.dim)
                    .whitespace_nowrap()
                    .child(label)
                    .when(enabled, move |b| {
                        b.cursor_pointer()
                            .hover(|s| s.text_color(colors.txt))
                            .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
                    })
                    .when(!enabled, |b| b.opacity(0.4))
                    .test_support()
            };
            bench = bench.child(
                div()
                    .id("bench-pile")
                    .ml_auto()
                    .flex()
                    .items_center()
                    .gap(px(11.))
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(colors.mute)
                            .whitespace_nowrap()
                            .child(format!("{} on the table", grouped(selected))),
                    )
                    .child(txt("bench-compare", "Compare", can_compare, Box::new(OpenCompare)))
                    .child(txt("bench-stack", "Stack", ready, Box::new(ProposeStacks)))
                    .child(txt("bench-cull", "Cull", ready, Box::new(StartCullSession)))
                    .child(txt("bench-analyse", "Analyse", ready, Box::new(AnalyseBurst)))
                    .child(txt("bench-export", "Export", can_export, Box::new(ExportSelection)))
                    .child(txt("bench-publish", "Publish", can_publish, Box::new(PublishSelection)))
                    .child(txt("bench-backup", "Back up", can_back_up, Box::new(BackUpSelection)))
                    .child(
                        div()
                            .id("bench-clear")
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(26.))
                            .rounded_full()
                            .text_color(colors.mute)
                            .cursor_pointer()
                            .hover(|s| s.text_color(colors.txt).bg(colors.elev))
                            .child("✕")
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(ClearSelection), cx))
                            .test_support(),
                    ),
            );
        }
        bench.into_any_element()
    }

    /// A bench mark (star, pick/reject, label). See the module docs: marks go through the
    /// culling path the Library view (#106) brings, so they are not wired twice.
    fn mark(&mut self, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.not_yet_ported("Marking from the bench", 106, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_count_line_matches_reacts() {
        assert_eq!(count_line(0, 0), "0 photos");
        assert_eq!(count_line(12345, 0), "12,345 photos");
        assert_eq!(count_line(12345, 2), "12,345 photos · 2 selected");
    }
}
