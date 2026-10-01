//! [`StackDialog`]: the "Stack bursts" modal (`StackProposalsDialog.tsx`).
//!
//! Burst clustering, phash similarity and timestamp proximity said as one sentence — *these
//! frames are one moment, and this is the keeper* — with the frames in front of the user so
//! they can disagree. Accepting collapses a group through the stacking that already ships
//! (nothing is deleted; the inspector's Unstack reverses it); acceptance is per group and
//! never automatic. The proposals and the acceptance are the core's
//! (`chairphoto_core::stack_proposals`), run off the UI thread.
//!
//! The dialog is an entity the root view holds while it is open; closing drops it, and with
//! it any answer still on its way (the `this.update` of a dropped entity fails). A catalog
//! switch closes it (`RootView`) and gives the grid its focus back.
//!
//! **Catalog identity.** The dialog is opened over ids from the grid's rows, so it is bound
//! to the catalog those rows came from (`ShellState::rows_from`): the proposals are read and
//! every Stack is written through `with_catalog_as`, which fails closed once another catalog
//! is open — also in the window before `catalog:switched` closes the dialog.
//!
//! **Thumbnails** are requested for the groups on screen plus [`OVERSCAN_GROUPS`] either
//! side, never for all of up to 200 proposals at once, and whatever scrolled away (or was
//! stacked or skipped) is released, as is everything when the dialog closes — so it cannot
//! flood the image layer and evict the grid's thumbnails.

use crate::image_store::{ImageState, ImageStore};
use crate::keymap::contexts;
use crate::library::CloseDialog;
use crate::model::AppModel;
use crate::shell::state::ShellState;
use crate::shell::style::Colors;
use chairphoto_core::app::{with_catalog_as, AppState, CatalogIdentity};
use chairphoto_core::image_pool::ImageKind;
use chairphoto_core::stack_proposals::{StackProposal, StackProposals};
use gpui_kit::prelude::*;
use gpui_kit::{
    div, img, px, relative, AnyElement, Context, Entity, EventEmitter, FocusHandle, FontWeight, ObjectFit,
    ScrollHandle, SharedString, TestSupportExt as _, Window,
};
use std::collections::{HashMap, HashSet};

/// Groups past the visible ones, either side, whose thumbnails are requested too.
pub const OVERSCAN_GROUPS: usize = 2;
/// Groups whose thumbnails are requested before the body's first layout says which show.
pub const INITIAL_GROUPS: usize = 4;

/// Asks the root view to close the dialog.
pub struct Closed;

/// What became of a group in this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Stacked,
    Skipped,
}

/// The dialog's state. Groups are keyed by the proposal's original keeper id.
pub struct StackDialog {
    app: AppState,
    shell: Entity<ShellState>,
    images: Entity<ImageStore>,
    focus: FocusHandle,
    /// The catalog the ids (and so the proposals) came from.
    pub from: CatalogIdentity,
    scroll: ScrollHandle,
    /// The thumbnails requested for the groups last on screen: the ones to release.
    pub requested: HashSet<i64>,
    pub result: Option<StackProposals>,
    pub error: Option<String>,
    /// Keeper chosen by hand, per group.
    pub keepers: HashMap<i64, i64>,
    pub done: HashMap<i64, Outcome>,
    /// The group being stacked right now.
    pub busy: Option<i64>,
}

impl EventEmitter<Closed> for StackDialog {}

/// "1m 12s" / "4s" — a burst's span reads better than a raw second count.
pub fn span(secs: i64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let (m, s) = (secs / 60, secs % 60);
    if s > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{m}m")
    }
}

/// A frame's sharpness as the dialog prints it: "—" unscored, no decimals from 100.
pub fn score(v: Option<f64>) -> String {
    match v {
        None => "—".into(),
        Some(v) if v >= 100. => format!("{v:.0}"),
        Some(v) => format!("{v:.1}"),
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The summary line under the title.
pub fn summary(r: &StackProposals) -> String {
    let mut line = if r.proposals.is_empty() {
        format!(
            "No groups in {} — nothing here was shot within {}s of a similar frame.",
            plural(r.considered, "photo", "photos"),
            r.time_gap_secs
        )
    } else {
        format!(
            "{} in {} photos. Frames within {}s of each other and closer than {} in visual difference.",
            plural(r.proposals.len(), "group", "groups"),
            r.considered,
            r.time_gap_secs,
            r.hamming_threshold
        )
    };
    if r.skipped_stacked > 0 {
        let was = if r.skipped_stacked == 1 { "already-stacked photo was" } else { "already-stacked photos were" };
        line += &format!(" {} {was} left out.", r.skipped_stacked);
    }
    if r.truncated {
        line += &format!(" Only the first {} are shown — run again after these.", r.proposals.len());
    }
    line
}

impl StackDialog {
    /// Open over `photo_ids`, read from the catalog `from` names, and start proposing, off
    /// the UI thread.
    pub fn new(
        model: &Entity<AppModel>,
        shell: Entity<ShellState>,
        images: Entity<ImageStore>,
        photo_ids: Vec<i64>,
        from: CatalogIdentity,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let app = model.read(cx).state().clone();
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let state = app.clone();
        let read = cx.background_executor().spawn(async move {
            with_catalog_as(&state, from, |c| chairphoto_core::stack_proposals::propose_stacks(c, &photo_ids))
        });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |d, cx| {
                match result {
                    Ok(r) => d.result = Some(r),
                    Err(e) => d.error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        // Closing (or a switch) drops the dialog: stop wanting what it asked for.
        cx.on_release(|d: &mut Self, cx| {
            let requested = std::mem::take(&mut d.requested);
            if !requested.is_empty() {
                d.images.update(cx, |store, _| {
                    store.release_pending(|k| k.kind != ImageKind::Thumb || !requested.contains(&k.photo))
                });
            }
        })
        .detach();
        Self {
            app,
            shell,
            images,
            focus,
            from,
            scroll: ScrollHandle::new(),
            requested: HashSet::new(),
            result: None,
            error: None,
            keepers: HashMap::new(),
            done: HashMap::new(),
            busy: None,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// The keeper a group would stack under: the one picked by hand, else the engine's.
    pub fn keeper_of(&self, p: &StackProposal) -> i64 {
        self.keepers.get(&p.keeper_id).copied().unwrap_or(p.keeper_id)
    }

    pub fn pick_keeper(&mut self, group: i64, photo: i64, cx: &mut Context<Self>) {
        self.keepers.insert(group, photo);
        cx.notify();
    }

    pub fn skip(&mut self, group: i64, cx: &mut Context<Self>) {
        self.done.insert(group, Outcome::Skipped);
        cx.notify();
    }

    /// Stack one group under its keeper, off the UI thread; then the grid re-reads its rows
    /// so the frames that collapsed leave it.
    pub fn accept(&mut self, group: i64, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(p) = self.result.as_ref().and_then(|r| r.proposals.iter().find(|p| p.keeper_id == group)) else {
            return;
        };
        let keeper = self.keeper_of(p);
        let members: Vec<i64> = p.members.iter().map(|m| m.photo_id).collect();
        self.busy = Some(group);
        let state = self.app.clone();
        let from = self.from;
        let write = cx.background_executor().spawn(async move {
            with_catalog_as(&state, from, |c| chairphoto_core::stack_proposals::apply_stack_proposal(c, keeper, &members))
        });
        cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |d, cx| {
                d.busy = None;
                match result {
                    Ok(_) => {
                        d.done.insert(group, Outcome::Stacked);
                        d.shell.update(cx, |s, cx| s.refresh_rows(cx));
                    }
                    Err(e) => d.error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(Closed);
    }

    /// The pending groups on screen at the body's last layout, plus [`OVERSCAN_GROUPS`]
    /// either side; before the first layout, the first [`INITIAL_GROUPS`]. `lead` is how
    /// many body children precede the first group.
    fn group_window(&self, lead: usize, groups: usize) -> std::ops::Range<usize> {
        if self.scroll.bounds_for_item(lead).is_none() {
            return 0..groups.min(INITIAL_GROUPS);
        }
        let top = self.scroll.top_item().saturating_sub(lead);
        let bottom = self.scroll.bottom_item().saturating_sub(lead);
        let start = top.saturating_sub(OVERSCAN_GROUPS).min(groups);
        start..(bottom + 1 + OVERSCAN_GROUPS).min(groups).max(start)
    }

    /// Request the thumbnails of `groups` (the window), release the ones asked for before
    /// that left it, and answer what the store holds for the window.
    fn request_window(&mut self, groups: &[StackProposal], cx: &mut Context<Self>) -> HashMap<i64, ImageState> {
        let ids: Vec<(i64, ImageKind)> =
            groups.iter().flat_map(|p| p.members.iter().map(|m| (m.photo_id, ImageKind::Thumb))).collect();
        let keep: HashSet<i64> = ids.iter().map(|&(id, _)| id).collect();
        let dropped: HashSet<i64> = self.requested.difference(&keep).copied().collect();
        let thumbs = self.images.update(cx, |store, _| {
            store.request_batch(&ids);
            if !dropped.is_empty() {
                store.release_pending(|k| k.kind != ImageKind::Thumb || !dropped.contains(&k.photo));
            }
            ids.iter().map(|&(id, kind)| (id, store.get(id, kind))).collect()
        });
        self.requested = keep;
        thumbs
    }

    fn render_group(&self, p: &StackProposal, thumbs: &HashMap<i64, ImageState>, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let group = p.keeper_id;
        let keeper = self.keeper_of(p);
        let overridden = keeper != p.keeper_id;
        let busy = self.busy == Some(group);
        let mut head = div()
            .flex()
            .flex_wrap()
            .gap(px(6.))
            .text_size(px(12.))
            .child(div().font_weight(FontWeight::SEMIBOLD).child(format!("{} frames", p.members.len())))
            .child(format!("over {}", span(p.span_secs)));
        if let Some(d) = p.max_distance {
            head = head.child(format!("· up to {d} apart visually"));
        }
        head = head.child(div().ml_auto().text_color(colors.dim).child(if overridden {
            "keeper chosen by hand".to_string()
        } else {
            format!("keeper: {}", p.reason)
        }));

        let mut frames = div().flex().flex_wrap().gap(px(6.)).mt(px(8.));
        for m in &p.members {
            let is_keeper = m.photo_id == keeper;
            let photo = m.photo_id;
            let thumb = match thumbs.get(&photo) {
                Some(ImageState::Ready(l)) => img(l.image.clone()).size_full().object_fit(ObjectFit::Cover).into_any_element(),
                _ => div().size_full().into_any_element(),
            };
            let title: SharedString = if is_keeper {
                format!("{} — the keeper: the others stack under this one", m.file_name).into()
            } else {
                format!("{} — click to keep this frame instead", m.file_name).into()
            };
            let mut meta = div().flex().gap(px(3.)).text_size(px(10.)).text_color(colors.dim);
            if is_keeper {
                meta = meta.child(div().text_color(colors.rating).child("♛"));
            }
            meta = meta.child(score(m.sharpness));
            if m.rating > 0 {
                meta = meta.child(div().text_color(colors.rating).child("★".repeat(m.rating as usize)));
            }
            frames = frames.child(
                div()
                    .id(SharedString::from(format!("frame-{group}-{photo}")))
                    .w(px(96.))
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .p(px(3.))
                    .rounded(px(4.))
                    .border_2()
                    .border_color(if is_keeper { colors.accent } else { colors.border })
                    .cursor_pointer()
                    .child(div().relative().w_full().h(px(64.)).bg(colors.well).overflow_hidden().child(thumb).when(
                        m.child_count > 0,
                        |d| {
                            d.child(
                                div()
                                    .absolute()
                                    .bottom(px(2.))
                                    .right(px(3.))
                                    .text_size(px(10.))
                                    .text_color(colors.txt)
                                    .child(format!("▤ {}", m.child_count)),
                            )
                        },
                    ))
                    .child(meta)
                    .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(title.clone()).build(window, cx))
                    .on_click(cx.listener(move |d, _, _, cx| d.pick_keeper(group, photo, cx)))
                    .test_support(),
            );
        }

        let mut caveats = div().flex().flex_col().gap(px(2.)).mt(px(6.)).text_size(px(11.)).text_color(colors.mute);
        if p.unscored > 0 {
            caveats = caveats.child(format!(
                "{} no sharpness score yet, so {} not weighed in picking the keeper.",
                if p.unscored == 1 { "1 frame has".to_string() } else { format!("{} frames have", p.unscored) },
                if p.unscored == 1 { "it was" } else { "they were" },
            ));
        }
        if p.absorbed_children > 0 {
            caveats = caveats.child(format!(
                "{} stacked under these frames will move onto the keeper — stacks stay one level deep.",
                plural(p.absorbed_children as usize, "photo", "photos")
            ));
        }

        let button = |id: SharedString, label: String, primary: bool| {
            div()
                .id(id)
                .h(px(28.))
                .px(px(12.))
                .flex()
                .items_center()
                .rounded_full()
                .border_1()
                .text_size(px(11.5))
                .child(label)
                .when(primary, |b| b.bg(colors.accent).border_color(colors.accent).text_color(colors.onaccent))
                .when(!primary, |b| b.border_color(colors.border).text_color(colors.dim))
                .when(busy, |b| b.opacity(0.5))
                .when(!busy, |b| b.cursor_pointer())
        };
        div()
            .id(SharedString::from(format!("group-{group}")))
            .flex()
            .flex_col()
            .p(px(10.))
            .rounded(px(6.))
            .border_1()
            .border_color(colors.line)
            .child(head)
            .child(frames)
            .child(caveats)
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .mt(px(8.))
                    .child(
                        button(
                            format!("stack-{group}").into(),
                            if busy { "Stacking…".into() } else { format!("Stack {} under this", p.members.len() - 1) },
                            true,
                        )
                        .on_click(cx.listener(move |d, _, _, cx| d.accept(group, cx)))
                        .test_support(),
                    )
                    .child(
                        button(format!("skip-{group}").into(), "Skip".into(), false)
                            .on_click(cx.listener(move |d, _, _, cx| {
                                if d.busy != Some(group) {
                                    d.skip(group, cx)
                                }
                            }))
                            .test_support(),
                    ),
            )
            .test_support()
            .into_any_element()
    }
}

impl Render for StackDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let pending: Vec<StackProposal> = self
            .result
            .as_ref()
            .map(|r| r.proposals.iter().filter(|p| !self.done.contains_key(&p.keeper_id)).cloned().collect())
            .unwrap_or_default();
        // The body's children before the first group: the error, the summary and its note.
        let lead = usize::from(self.error.is_some())
            + self.result.as_ref().map_or(usize::from(self.error.is_none()), |r| 1 + usize::from(!r.proposals.is_empty()));
        let window = self.group_window(lead, pending.len());
        let thumbs = self.request_window(&pending[window], cx);
        let stacked = self.done.values().filter(|o| **o == Outcome::Stacked).count();

        let mut body = div()
            .id("stack-body")
            .flex()
            .flex_col()
            .gap(px(10.))
            .p(px(14.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex_1()
            .min_h_0()
            .test_support();
        if let Some(e) = &self.error {
            body = body.child(div().id("stack-error").text_size(px(12.)).text_color(colors.danger).child(e.clone()).test_support());
        }
        match &self.result {
            None if self.error.is_none() => {
                body = body.child(div().text_size(px(12.)).text_color(colors.mute).child("Looking for groups…"));
            }
            None => {}
            Some(r) => {
                body = body.child(
                    {
                        let line = summary(r);
                        div()
                            .id("stack-summary")
                            .text_size(px(12.))
                            .text_color(colors.dim)
                            .aria_label(line.clone())
                            .child(line)
                            .test_support()
                    },
                );
                if !r.proposals.is_empty() {
                    body = body.child(div().text_size(px(11.)).text_color(colors.mute).child(
                        "Stacking hides the other frames from the grid and keeps them under the keeper, where the \
                         inspector's Stack section can unstack any of them again. Nothing is deleted.",
                    ));
                }
                for p in &pending {
                    body = body.child(self.render_group(p, &thumbs, colors, cx));
                }
                if stacked > 0 {
                    body = body.child(
                        div()
                            .id("stack-done")
                            .text_size(px(12.))
                            .text_color(colors.dim)
                            .child(format!("Stacked {} this session.", plural(stacked, "group", "groups")))
                            .aria_label(format!("Stacked {} this session.", plural(stacked, "group", "groups")))
                            .test_support(),
                    );
                }
            }
        }

        div()
            .id("stack-dialog-scrim")
            .absolute()
            .inset_0()
            // Modal: nothing under the scrim (the grid's focus-on-mouse-down) sees the mouse.
            .occlude()
            .bg(colors.scrim)
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(|d, _, _, cx| d.close(cx)))
            .child(
                div()
                    .id("stack-dialog")
                    .key_context(contexts::STACK_DIALOG)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(|d, _: &CloseDialog, _, cx| d.close(cx)))
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .w(px(760.))
                    .max_w(relative(0.92))
                    .h(relative(0.82))
                    .flex()
                    .flex_col()
                    .rounded(px(10.))
                    .bg(colors.panel)
                    .border_1()
                    .border_color(colors.border)
                    .text_color(colors.txt)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .px(px(14.))
                            .h(px(44.))
                            .border_b_1()
                            .border_color(colors.line)
                            .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child("Stack bursts"))
                            .child(
                                div()
                                    .id("stack-close")
                                    .ml_auto()
                                    .h(px(26.))
                                    .px(px(11.))
                                    .flex()
                                    .items_center()
                                    .rounded_full()
                                    .border_1()
                                    .border_color(colors.border)
                                    .text_size(px(11.5))
                                    .text_color(colors.dim)
                                    .cursor_pointer()
                                    .child("Close")
                                    .on_click(cx.listener(|d, _, _, cx| d.close(cx)))
                                    .test_support(),
                            ),
                    )
                    .child(body)
                    .test_support(),
            )
            .test_support()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_read_as_minutes_and_seconds() {
        assert_eq!(span(4), "4s");
        assert_eq!(span(60), "1m");
        assert_eq!(span(72), "1m 12s");
    }

    #[test]
    fn scores_print_like_the_dialog() {
        assert_eq!(score(None), "—");
        assert_eq!(score(Some(12.345)), "12.3");
        assert_eq!(score(Some(320.4)), "320");
    }

    fn proposals(groups: usize, considered: usize, skipped: usize, truncated: bool) -> StackProposals {
        StackProposals {
            proposals: (0..groups as i64)
                .map(|g| StackProposal {
                    keeper_id: g,
                    reason: String::new(),
                    span_secs: 0,
                    max_distance: None,
                    unscored: 0,
                    absorbed_children: 0,
                    members: Vec::new(),
                })
                .collect(),
            considered,
            skipped_stacked: skipped,
            truncated,
            time_gap_secs: 15,
            hamming_threshold: 10,
        }
    }

    #[test]
    fn the_summary_matches_reacts_wording() {
        assert_eq!(
            summary(&proposals(0, 1, 0, false)),
            "No groups in 1 photo — nothing here was shot within 15s of a similar frame."
        );
        assert_eq!(
            summary(&proposals(2, 9, 1, false)),
            "2 groups in 9 photos. Frames within 15s of each other and closer than 10 in visual difference. \
             1 already-stacked photo was left out."
        );
        assert!(summary(&proposals(1, 3, 2, true))
            .ends_with("2 already-stacked photos were left out. Only the first 1 are shown — run again after these."));
    }
}
