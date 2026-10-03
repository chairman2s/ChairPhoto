//! The bench (`src/components/shell/Bench.tsx`): the 72 px strip under the stage. Left to
//! right: one background job's progress, else "N photos · M selected" and the status line;
//! the marking controls for the photo the inspector shows (Compare's focused pane, else the
//! active photo); the selection pile ("N on the table", the first three selected photos'
//! thumbnails with the marked one highlighted) with the selection's actions.
//!
//! The marking and pile sections appear only with an active photo / a selection. Marks
//! write through the one culling path the grid's keys use (`ShellState::apply_mark`,
//! Bench.tsx's "one code path" invariant), without the keys' auto-advance.

use crate::image_store::ImageState;
use crate::model::AppModel;
use crate::shell::actions::*;
use crate::shell::state::{Mark, ShellState, Surface};
use crate::shell::style::{grouped, Colors, COLOR_LABELS};
use crate::view::RootView;
use crate::loupe::zoom::fitted;
use chairphoto_core::catalog::{Photo, PickState};
use chairphoto_core::image_pool::ImageKind;
use chairphoto_model::darkroom::filmstrip::{cover_look, CoverLook};
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, relative, Action, AnyElement, Context, FontWeight, ObjectFit, SharedString, TestSupportExt as _,
};

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

/// How many of the selection's thumbnails the pile shows (App.tsx passed
/// `selection.photos.slice(0, 3)`).
pub const PILE_THUMBS: usize = 3;

/// The pile's thumbnails: the first [`PILE_THUMBS`] selected rows in row order (the selection's
/// `photos`), each with whether it is `shown` — the photo the bench marks, highlighted.
pub fn pile_thumbs<'a>(photos: &[&'a Photo], shown: Option<i64>) -> Vec<(&'a Photo, bool)> {
    photos.iter().take(PILE_THUMBS).map(|&p| (p, Some(p.id) == shown)).collect()
}

impl RootView {
    /// Ask for the pile's thumbnails through the image layer (decoded on the pool, never
    /// here), under the bench's own claim, which each frame becomes exactly the pile's
    /// thumbnails: the grid's releases leave them alone while the pile shows them. Each asks
    /// for the cover look its row names, as the grid does; once they are pending or cached
    /// nothing is sent.
    ///
    /// A selection that moves on — or ends, or a surface without the bench — lets go of the
    /// rest, and releases those of them the grid did not ask for last frame either. The grid
    /// asks for the same tiers without a claim, so a plain
    /// [`set_claim`](crate::image_store::ImageStore::set_claim) would cancel a visible
    /// tile's render.
    pub(crate) fn request_bench_thumbs(&mut self, cx: &mut Context<Self>) {
        let (wanted, from) = {
            let shell = self.shell.read(cx);
            let wanted: Vec<(i64, Option<CoverLook>)> = if shell.surface == Surface::Library {
                pile_thumbs(&shell.library.selection().photos, None)
                    .into_iter()
                    .map(|(p, _)| (p.id, cover_look(p.cover_token.as_deref())))
                    .collect()
            } else {
                Vec::new()
            };
            (wanted, shell.rows_from())
        };
        let (owner, library) = (self.bench_claim, self.library.clone());
        self.images.update(cx, |store, cx| {
            let tiers: Vec<(i64, ImageKind)> = wanted.iter().map(|&(id, _)| (id, ImageKind::Thumb)).collect();
            let gone: Vec<i64> =
                store.claim(owner).into_iter().filter(|t| !tiers.contains(t)).map(|(id, _)| id).collect();
            store.hold(owner, tiers.iter().copied());
            if !gone.is_empty() {
                let grid_wants = library.read(cx).requested_thumbs();
                store.release_pending(|k| {
                    k.kind != ImageKind::Thumb || !gone.contains(&k.photo) || grid_wants.contains(&k.photo)
                });
            }
            match from {
                _ if wanted.is_empty() => {}
                Some(from) => store.request_look_batch(from, &wanted, cx),
                None => store.request_batch(&tiers),
            }
        });
    }

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
                    // An export stops before its next photo (Albums and export, #115).
                    .when(shell.jobs.import.is_none() && (shell.jobs.export_photos.is_some() || shell.jobs.export_bundle.is_some()), |d| {
                        d.child(
                            div()
                                .id("bench-cancel-export")
                                .mt(px(4.))
                                .text_size(px(10.5))
                                .text_color(colors.dim)
                                .cursor_pointer()
                                .hover(|s| s.text_color(colors.txt))
                                .child("Cancel export")
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(crate::shell::actions::CancelExport), cx)
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

        // The photo the inspector shows ([`ShellState::loupe_target`], React's `shellPhoto`):
        // in Compare the focused pane, which is also the one `apply_mark` writes, so the
        // toggles below are resolved against the photo they mark.
        if let Some(active) = shell.loupe_target() {
            let filename = active.path.rsplit('/').next().unwrap_or(&active.path).to_string();
            let rating = active.rating;
            let mut stars = div().flex().gap(px(3.));
            for n in 1..=5i64 {
                let on = n <= rating as i64;
                // Clicking the active star clears the rating (Bench.tsx).
                let mark = Mark::Rating(if n == rating { 0 } else { n });
                stars = stars.child(
                    div()
                        .id(SharedString::from(format!("bench-star-{n}")))
                        .text_size(px(18.))
                        .line_height(relative(1.))
                        .cursor_pointer()
                        .text_color(if on { colors.rating } else { colors.mute })
                        .opacity(if on { 1. } else { 0.5 })
                        .child("★")
                        .on_click(cx.listener(move |this, _, _, cx| this.mark(mark.clone(), cx)))
                        .test_support(),
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
                let mark = Mark::Label(if on { String::new() } else { label.name.to_string() });
                dots = dots.child(
                    crate::shell::command_pill::label_dot(
                        SharedString::from(format!("bench-label-{}", label.name)),
                        Some(label.color()),
                        on,
                        colors,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.mark(mark.clone(), cx)))
                    .test_support(),
                );
            }
            let has_label = !active.label.is_empty();
            dots = dots.child(
                crate::shell::command_pill::label_dot("bench-label-none".into(), None, false, colors).on_click(
                    cx.listener(move |this, _, _, cx| {
                        if has_label {
                            this.mark(Mark::Label(String::new()), cx)
                        }
                    }),
                ).test_support(),
            );
            let picked = active.pick_state == PickState::Pick;
            let rejected = active.pick_state == PickState::Reject;
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
                            .child(
                                div()
                                    .id("bench-mark-name")
                                    .text_color(colors.dim)
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(filename.clone())
                                    .aria_label(filename)
                                    .test_support(),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(14.))
                            .child(stars)
                            .child(
                                pick("bench-pick", "Pick", picked).on_click(cx.listener(move |this, _, _, cx| {
                                    this.mark(Mark::Pick(if picked { PickState::None } else { PickState::Pick }), cx)
                                })).test_support(),
                            )
                            .child(
                                pick("bench-reject", "Reject", rejected).on_click(cx.listener(move |this, _, _, cx| {
                                    this.mark(Mark::Pick(if rejected { PickState::None } else { PickState::Reject }), cx)
                                })).test_support(),
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
                    .child(self.render_pile_strip(shell, &selection.photos, colors, cx))
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

    /// The pile's strip (`.bench-strip`): up to [`PILE_THUMBS`] 52×35 thumbnails, the photo
    /// the bench marks ringed in the accent (`.thumbwrap.hl`). What the image layer holds
    /// is drawn; a thumbnail not there yet is an empty well.
    fn render_pile_strip(&self, shell: &ShellState, photos: &[&Photo], colors: Colors, cx: &Context<Self>) -> AnyElement {
        let shown = shell.loupe_target().map(|p| p.id);
        let store = self.images.read(cx);
        let mut strip = div().id("bench-strip").flex().flex_none().gap(px(4.));
        for (photo, hl) in pile_thumbs(photos, shown) {
            let image = match store.peek(photo.id, ImageKind::Thumb) {
                ImageState::Ready(l) => fitted(("bench-thumb-picture", photo.id as u64), l.image.clone(), ObjectFit::Cover).into_any_element(),
                _ => div().size_full().bg(colors.well).into_any_element(),
            };
            let name = photo.path.rsplit('/').next().unwrap_or(&photo.path);
            let label = if hl { format!("{name} (marking)") } else { name.to_string() };
            strip = strip.child(
                div()
                    .id(("bench-thumb", photo.id as u64))
                    .flex_none()
                    .w(px(52.))
                    .h(px(35.))
                    .rounded(px(3.))
                    .overflow_hidden()
                    .when(hl, |d| d.border_2().border_color(colors.accent))
                    .child(image)
                    .aria_label(label)
                    .test_support(),
            );
        }
        strip.into_any_element()
    }

    /// A bench mark (star, pick/reject, label): the culling keys' write path
    /// (`ShellState::apply_mark`), without their auto-advance — clicking a star must not
    /// move the selection.
    fn mark(&mut self, mark: Mark, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| s.apply_mark(mark, false, cx));
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
