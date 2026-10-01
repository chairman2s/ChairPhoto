//! "Merge “name”" (`TagMergeModal.tsx`), in two deliberate steps: pick the target from a
//! filtered list (not the tag itself or its subtree; at most 200), then read the dry run — the
//! real merge rolled back (`app::tags::merge_tags` with `dry_run`) — and only then Merge. A
//! refusal (path collision, auto-tag target) arrives as the preview's error, naming its cause.
//! "Pick a different tag" goes back. A committed merge closes the dialog and puts its counts
//! on the status line (`tag_tree::merge_summary`).

use super::state::{bind_dialog, run_as, CatalogGuard, TagsState};
use crate::shell::style::Colors;
use crate::storage::{ui, CloseDialog};
use chairphoto_core::catalog::tag_maintenance::TagMergeReport;
use chairphoto_core::catalog::TagWithCount;
use chairphoto_model::tag_tree::{grouped, merge_candidates, merge_preview_lines, merge_summary};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, EventEmitter, SharedString, Subscription, TestSupportExt as _, Window};

pub struct TagMerge {
    tags: Entity<TagsState>,
    /// The tree this dialog opened over ([`bind_dialog`]): its jobs run under it.
    guard: CatalogGuard,
    pub source: TagWithCount,
    pub filter: Entity<InputState>,
    pub target: Option<TagWithCount>,
    pub preview: Option<TagMergeReport>,
    pub error: Option<String>,
    pub busy: bool,
    /// Bumped by every pick; a superseded dry run's report is dropped.
    attempt: u64,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for TagMerge {}

impl TagMerge {
    pub fn new(tags: Entity<TagsState>, source: TagWithCount, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Merge into…"));
        let changes = cx.subscribe(&filter, |_, _, _: &InputEvent, cx| cx.notify());
        let (guard, bound) = bind_dialog(&tags, cx);
        TagMerge { tags, guard, source, filter, target: None, preview: None, error: None, busy: false, attempt: 0, _subscriptions: vec![changes, bound] }
    }

    pub fn candidates(&self, cx: &gpui_kit::App) -> Vec<TagWithCount> {
        merge_candidates(&self.tags.read(cx).tags, &self.source, &self.filter.read(cx).value()).into_iter().cloned().collect()
    }

    /// Pick a target and run the dry run.
    pub fn pick(&mut self, target: TagWithCount, cx: &mut Context<Self>) {
        self.attempt += 1;
        let attempt = self.attempt;
        let (source, target_id) = (self.source.tag.id, target.tag.id);
        self.target = Some(target);
        self.preview = None;
        self.error = None;
        self.busy = true;
        run_as(&self.tags, &self.guard, cx, false, move |c| chairphoto_core::app::tags::merge_tags(c, &[source], target_id, true), move |s: &mut Self, r, cx| {
            if s.attempt != attempt {
                return;
            }
            s.busy = false;
            match r {
                Ok(report) => s.preview = Some(report),
                Err(e) => s.error = Some(e),
            }
            cx.notify();
        });
        cx.notify();
    }

    pub fn back(&mut self, cx: &mut Context<Self>) {
        self.attempt += 1;
        self.target = None;
        self.preview = None;
        self.error = None;
        self.busy = false;
        cx.notify();
    }

    /// Commit — only after a preview of this same target.
    pub fn commit(&mut self, cx: &mut Context<Self>) {
        let (Some(target), Some(_)) = (&self.target, &self.preview) else { return };
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        let (source, target_id) = (self.source.tag.id, target.tag.id);
        run_as(&self.tags, &self.guard, cx, true, move |c| chairphoto_core::app::tags::merge_tags(c, &[source], target_id, false), |s: &mut Self, r, cx| {
            s.busy = false;
            match r {
                Ok(report) => {
                    let line = merge_summary(&report);
                    s.tags.update(cx, |t, cx| t.set_status(line, cx));
                    cx.emit(CloseDialog);
                }
                Err(e) => s.error = Some(e),
            }
            cx.notify();
        });
        cx.notify();
    }
}

impl Render for TagMerge {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let n = self.source.photo_count.max(0) as usize;
        let mut body = ui::body()
            .id("tag-merge")
            .child(div().text_size(px(13.)).text_color(colors.txt).child(format!("Merge “{}”", self.source.tag.name)))
            .child(ui::sub(
                format!(
                    "{} — {} photo{}. The tag is removed; its photos, children, synonyms and album rules move to the tag you pick.",
                    self.source.tag.full_path,
                    grouped(n),
                    if n == 1 { "" } else { "s" }
                ),
                colors,
            ));
        let Some(target) = self.target.clone() else {
            let candidates = self.candidates(cx);
            let mut list = div().id("tag-merge-list").flex().flex_col().max_h(px(320.)).overflow_y_scroll();
            for t in &candidates {
                let pick = t.clone();
                list = list.child(ui::clickable(
                    div()
                        .id(SharedString::from(format!("tag-merge-into-{}", t.tag.id)))
                        .flex()
                        .px(px(8.))
                        .py(px(4.))
                        .rounded(px(4.))
                        .cursor_pointer()
                        .hover(|s| s.bg(colors.elev))
                        .text_size(px(12.))
                        .child(div().flex_1().child(t.tag.full_path.clone()))
                        .child(div().text_color(colors.mute).child(grouped(t.photo_count.max(0) as usize))),
                    true,
                    cx.listener(move |s, _, _, cx| s.pick(pick.clone(), cx)),
                ));
            }
            if candidates.is_empty() {
                let f = self.filter.read(cx).value();
                list = list.child(ui::empty("tag-merge-none", format!("No tag matches “{f}”."), colors));
            }
            return body.child(Input::new(&self.filter).id("tag-merge-filter")).child(list.test_support());
        };
        body = body.child(ui::sub(format!("{} → {}", self.source.tag.full_path, target.tag.full_path), colors));
        if self.busy && self.preview.is_none() {
            body = body.child(ui::sub("Working out what would change…", colors));
        }
        if let Some(e) = &self.error {
            body = body.child(ui::error("tag-merge-error", e.clone(), colors));
        }
        if let Some(r) = &self.preview {
            let lines = merge_preview_lines(r);
            let mut p = div().id("tag-merge-preview").flex().flex_col().gap(px(3.));
            if lines.is_empty() {
                p = p.child(ui::sub("Nothing to move — the tag is empty. Merging removes it.", colors));
            }
            for l in lines {
                p = p.child(div().text_size(px(12.)).child(format!("• {l}")));
            }
            if !r.terms_skipped.is_empty() {
                p = p.child(ui::sub(
                    format!("Kept as they are — {} already has them: {}", r.target_path, r.terms_skipped.join(", ")),
                    colors,
                ));
            }
            for w in &r.warnings {
                p = p.child(div().text_size(px(11.5)).text_color(colors.danger).child(w.clone()));
            }
            body = body.child(p.test_support());
        }
        let can_commit = !self.busy && self.preview.is_some();
        body.child(
            ui::row()
                .child(ui::clickable(
                    ui::chip("tag-merge-commit", if self.busy && self.preview.is_some() { "Merging…" } else { "Merge" }, can_commit, colors),
                    can_commit,
                    cx.listener(|s, _, _, cx| s.commit(cx)),
                ))
                .child(ui::clickable(
                    ui::chip("tag-merge-back", "Pick a different tag", !self.busy, colors),
                    !self.busy,
                    cx.listener(|s, _, _, cx| s.back(cx)),
                )),
        )
    }
}
