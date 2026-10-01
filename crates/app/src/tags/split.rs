//! "Split “name”" (`TagSplitModal.tsx`): move the selected photos onto a new tag. A path
//! field (Enter previews), "Keep name on these photos too", a dry run — the real split rolled
//! back (`app::tags::split_tag`) — and Split only after it. Any edit drops the preview. A
//! committed split closes the dialog and reports on the status line
//! (`tag_tree::split_summary`). Photos in the selection that never carried the tag are
//! counted, not tagged.

use super::state::{bind_dialog, run_as, CatalogGuard, TagsState};
use crate::shell::style::Colors;
use crate::storage::{ui, CloseDialog};
use chairphoto_core::catalog::tag_maintenance::TagSplitReport;
use chairphoto_core::catalog::TagWithCount;
use chairphoto_model::tag_tree::{grouped, split_preview_lines, split_summary};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, EventEmitter, Subscription, TestSupportExt as _, Window};

pub struct TagSplit {
    tags: Entity<TagsState>,
    /// The tree this dialog opened over ([`bind_dialog`]): its jobs run under it.
    guard: CatalogGuard,
    pub source: TagWithCount,
    pub photo_ids: Vec<i64>,
    pub path: Entity<InputState>,
    pub keep_source: bool,
    pub preview: Option<TagSplitReport>,
    pub error: Option<String>,
    pub busy: bool,
    /// Bumped by every edit and run; a superseded result is dropped.
    attempt: u64,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for TagSplit {}

impl TagSplit {
    pub fn new(tags: Entity<TagsState>, source: TagWithCount, photo_ids: Vec<i64>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = cx.new(|cx| InputState::new(window, cx).placeholder("New tag path, e.g. Venue/Grieghallen"));
        let events = cx.subscribe(&path, |this: &mut Self, _, event: &InputEvent, cx| match event {
            InputEvent::Change => {
                this.attempt += 1;
                this.preview = None;
                cx.notify();
            }
            InputEvent::PressEnter { .. } => this.run(true, cx),
            _ => {}
        });
        let (guard, bound) = bind_dialog(&tags, cx);
        TagSplit { tags, guard, source, photo_ids, path, keep_source: false, preview: None, error: None, busy: false, attempt: 0, _subscriptions: vec![events, bound] }
    }

    pub fn toggle_keep(&mut self, cx: &mut Context<Self>) {
        self.keep_source = !self.keep_source;
        self.attempt += 1;
        self.preview = None;
        cx.notify();
    }

    /// Preview (`dry_run`), or split for real — only after a preview of the same inputs.
    pub fn run(&mut self, dry_run: bool, cx: &mut Context<Self>) {
        let target = self.path.read(cx).value().trim().to_string();
        if target.is_empty() || self.busy || (!dry_run && self.preview.is_none()) {
            return;
        }
        self.attempt += 1;
        let attempt = self.attempt;
        self.busy = true;
        self.error = None;
        let (source, ids, keep) = (self.source.tag.id, self.photo_ids.clone(), self.keep_source);
        run_as(
            &self.tags,
            &self.guard,
            cx,
            !dry_run,
            move |c| chairphoto_core::app::tags::split_tag(c, source, &ids, &target, keep, dry_run),
            move |s: &mut Self, r, cx| {
                s.busy = false;
                match r {
                    Ok(report) if dry_run => {
                        if s.attempt == attempt {
                            s.preview = Some(report);
                        }
                    }
                    Ok(report) => {
                        let line = split_summary(&report);
                        s.tags.update(cx, |t, cx| t.set_status(line, cx));
                        cx.emit(CloseDialog);
                    }
                    Err(e) => s.error = Some(e),
                }
                cx.notify();
            },
        );
        cx.notify();
    }
}

impl Render for TagSplit {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let n = self.photo_ids.len();
        let has_path = !self.path.read(cx).value().trim().is_empty();
        let tail = if self.keep_source {
            " Both tags will apply to them.".to_string()
        } else {
            format!(" They lose {}.", self.source.tag.full_path)
        };
        let keep = self.keep_source;
        let can_preview = !self.busy && has_path;
        let can_split = !self.busy && self.preview.is_some();
        ui::body()
            .id("tag-split")
            .child(div().text_size(px(13.)).text_color(colors.txt).child(format!("Split “{}”", self.source.tag.name)))
            .child(ui::sub(format!("{} selected photo{} move to a new tag.{tail}", grouped(n), if n == 1 { "" } else { "s" }), colors))
            .child(Input::new(&self.path).id("tag-split-path"))
            .child(super::toggle(
                "tag-split-keep",
                keep,
                format!("Keep {} on these photos too", self.source.tag.name),
                colors,
                cx.listener(|s, _, _, cx| s.toggle_keep(cx)),
            ))
            .children(self.error.clone().map(|e| ui::error("tag-split-error", e, colors)))
            .children(self.preview.as_ref().map(|r| {
                div()
                    .id("tag-split-preview")
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .children(split_preview_lines(r).into_iter().map(|l| div().text_size(px(12.)).child(format!("• {l}"))))
                    .test_support()
            }))
            .child(
                ui::row()
                    .child(ui::clickable(ui::chip("tag-split-preview-run", "Preview", can_preview, colors), can_preview, cx.listener(|s, _, _, cx| s.run(true, cx))))
                    .child(ui::clickable(
                        ui::chip("tag-split-run", if self.busy && self.preview.is_some() { "Splitting…" } else { "Split" }, can_split, colors),
                        can_split,
                        cx.listener(|s, _, _, cx| s.run(false, cx)),
                    )),
            )
    }
}
