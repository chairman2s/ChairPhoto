//! "New tags" (`TagCreateModal.tsx`): a multi-line field takes names, `a/b` paths or an
//! indented hierarchy (`tag_paste::parse_tag_paste`), optionally under a parent; a live
//! preview shows the first 30 with "… and N more" and "N tags total"; "Create N tags" creates
//! them in order on one worker and stops at the first failure, naming it. Escape closes (the
//! Dialog's own binding).

use super::state::{bind_dialog, run_as, CatalogGuard, TagsState};
use crate::shell::style::Colors;
use crate::storage::{ui, CloseDialog};
use chairphoto_model::tag_paste::parse_tag_paste;
use chairphoto_model::tag_tree::depth;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, EventEmitter, Focusable as _, Subscription, TestSupportExt as _, Window};

/// How many preview rows show.
pub const PREVIEW_CAP: usize = 30;

const PLACEHOLDER: &str = "Objects\n  Cooking Equipment\n    Grill\n      Gas Grill\n      Charcoal Grill\n\nIndent with spaces or tabs to build a hierarchy · a/b creates nested tags";

/// The full paths `text` creates under `parent` (React's `paths`).
pub fn paths_for(text: &str, parent: Option<&str>) -> Vec<String> {
    let parsed = parse_tag_paste(text);
    match parent {
        Some(p) => parsed.into_iter().map(|t| format!("{p}/{t}")).collect(),
        None => parsed,
    }
}

pub struct TagCreate {
    tags: Entity<TagsState>,
    /// The tree this dialog opened over ([`bind_dialog`]): its jobs run under it.
    guard: CatalogGuard,
    pub parent: Option<String>,
    pub text: Entity<TextareaState>,
    pub error: Option<String>,
    pub busy: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for TagCreate {}

impl TagCreate {
    pub fn new(tags: Entity<TagsState>, parent: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = cx.new(|cx| TextareaState::new(window, cx).placeholder(PLACEHOLDER).auto_grow(6, 14));
        let changes = cx.subscribe(&text, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.error = None;
                cx.notify();
            }
        });
        let focus = text.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        let (guard, bound) = bind_dialog(&tags, cx);
        TagCreate { tags, guard, parent, text, error: None, busy: false, _subscriptions: vec![changes, bound] }
    }

    pub fn paths(&self, cx: &gpui_kit::App) -> Vec<String> {
        paths_for(&self.text.read(cx).value(), self.parent.as_deref())
    }

    /// Create every path in order; the first failure stops the run and is named.
    pub fn create(&mut self, cx: &mut Context<Self>) {
        let paths = self.paths(cx);
        if paths.is_empty() || self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        run_as(
            &self.tags,
            &self.guard,
            cx,
            true,
            move |c| {
                for p in &paths {
                    if let Err(e) = c.create_tag(p) {
                        return Ok(Some(format!("Failed to create \"{p}\": {e}")));
                    }
                }
                Ok(None)
            },
            |s: &mut Self, r, cx| {
                s.busy = false;
                match r {
                    Ok(None) => cx.emit(CloseDialog),
                    Ok(Some(e)) | Err(e) => s.error = Some(e),
                }
                cx.notify();
            },
        );
        cx.notify();
    }
}

impl Render for TagCreate {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let paths = self.paths(cx);
        let base = self.parent.as_deref().map_or(0, depth);
        let n = paths.len();
        let mut preview = div().id("tag-create-preview").flex().flex_col().gap(px(2.)).p(px(8.)).rounded(px(6.)).bg(colors.well);
        if n == 0 {
            preview = preview.child(ui::sub("Type tag names above — they'll appear here", colors));
        } else {
            for p in paths.iter().take(PREVIEW_CAP) {
                let indent = depth(p).saturating_sub(base + 1);
                preview = preview.child(
                    div().pl(px(indent as f32 * 14.)).text_size(px(12.)).child(p.rsplit('/').next().unwrap_or(p).to_string()),
                );
            }
            if n > PREVIEW_CAP {
                preview = preview.child(ui::sub(format!("… and {} more", n - PREVIEW_CAP), colors));
            }
            preview = preview.child(ui::sub(format!("{n} tag{} total", if n == 1 { "" } else { "s" }), colors));
        }
        let label = if self.busy {
            "Creating…".to_string()
        } else if n == 0 {
            "Create tags".to_string()
        } else {
            format!("Create {n} tag{}", if n == 1 { "" } else { "s" })
        };
        let enabled = n > 0 && !self.busy;
        ui::body()
            .id("tag-create")
            .when_some(self.parent.clone(), |b, p| b.child(ui::sub(format!("New tags under \"{p}\""), colors)))
            .children(self.error.clone().map(|e| ui::error("tag-create-error", e, colors)))
            .child(div().id("tag-create-text").child(Textarea::new(&self.text)).test_support())
            .child(preview.test_support())
            .child(
                ui::row()
                    .child(ui::clickable(ui::chip("tag-create-cancel", "Cancel", true, colors), true, cx.listener(|_, _, _, cx| cx.emit(CloseDialog))))
                    .child(ui::clickable(ui::primary("tag-create-run", label, enabled, colors), enabled, cx.listener(|s, _, _, cx| s.create(cx)))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_nest_under_the_parent() {
        assert_eq!(paths_for("Grill\n  Gas", Some("Objects")), ["Objects/Grill", "Objects/Grill/Gas"]);
        assert_eq!(paths_for("a/b", None), ["a/b"]);
        assert!(paths_for("  \n", Some("X")).is_empty());
    }
}
