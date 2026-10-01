//! Preferences → Tags (`TagMaintenanceSection`): tidy redundant ancestor tags, find duplicate
//! tags (the 50 most similar, "Merge X away…"), find unused tags and delete them (a branch
//! with children behind a confirm). The merge itself opens the tag merge preview
//! (`TagMergeModal.tsx`), which the Tag panel ticket (#107) ports; until then the button says
//! so in the section's status line.

use super::{heading, section, status, thousands, Ctx};
use crate::model::not_yet_ported_line;
use crate::shell::style::Colors;
use crate::storage::{ui, Runner};
use chairphoto_core::catalog::tag_maintenance::{self, OrphanTag, SimilarTagPair, DEFAULT_MIN_SIMILARITY};
use gpui_kit::prelude::*;
use gpui_kit::TestSupportExt as _;
use gpui_kit::{div, px, Context, SharedString, Window};

/// How many duplicate pairs are listed.
pub const MAX_PAIRS: usize = 50;

/// What a Find button is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Looking {
    Duplicates,
    Orphans,
}

pub struct TagMaintenance {
    ctx: Ctx,
    pub status: Option<String>,
    pub duplicates: Option<Vec<SimilarTagPair>>,
    pub orphans: Option<Vec<OrphanTag>>,
    pub busy: Option<Looking>,
}

impl TagMaintenance {
    pub fn new(ctx: Ctx, _: &mut Context<Self>) -> Self {
        TagMaintenance { ctx, status: None, duplicates: None, orphans: None, busy: None }
    }

    pub fn tidy(&mut self, cx: &mut Context<Self>) {
        self.status = Some("Tidying…".into());
        cx.notify();
        self.ctx.run(cx, |scope| scope.catalog(|c| c.tidy_redundant_tags()), |s: &mut Self, result, cx| {
            match result {
                Ok(n) => {
                    s.status = Some(if n > 0 {
                        format!("Removed {n} redundant tag(s).")
                    } else {
                        "Already tidy — nothing to remove.".into()
                    });
                    s.ctx.changed(cx);
                }
                Err(e) => s.status = Some(e),
            }
        });
    }

    pub fn find_duplicates(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(Looking::Duplicates);
        self.status = None;
        cx.notify();
        self.ctx.run(
            cx,
            |scope| scope.catalog(|c| tag_maintenance::find_similar_tags(c.conn(), DEFAULT_MIN_SIMILARITY)),
            |s: &mut Self, result, _| {
                s.busy = None;
                match result {
                    Ok(pairs) => s.duplicates = Some(pairs),
                    Err(e) => s.status = Some(e),
                }
            },
        );
    }

    pub fn find_unused(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(Looking::Orphans);
        self.status = None;
        cx.notify();
        self.ctx.run(
            cx,
            |scope| scope.catalog(|c| tag_maintenance::find_orphan_tags(c.conn())),
            |s: &mut Self, result, _| {
                s.busy = None;
                match result {
                    Ok(orphans) => s.orphans = Some(orphans),
                    Err(e) => s.status = Some(e),
                }
            },
        );
    }

    /// "Merge X away…": the merge preview is #107's.
    pub fn merge(&mut self, cx: &mut Context<Self>) {
        self.status = Some(not_yet_ported_line("Merge tags", 107));
        cx.notify();
    }

    /// Delete an unused tag; a branch (its sub-tags go too) asks first.
    pub fn delete(&mut self, tag_id: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(orphan) = self.orphans.as_ref().and_then(|o| o.iter().find(|o| o.id == tag_id)).cloned() else {
            return;
        };
        let answer = orphan.has_children.then(|| {
            ui::confirm(
                window,
                cx,
                "Delete tag branch".into(),
                format!(
                    "Delete {} and every tag under it?\n\nNone of them hold photos, but the sub-tags are removed too.",
                    orphan.path
                )
                .into(),
                "Delete",
            )
        });
        let runner = Runner::get(cx);
        // Bound to the catalog the orphan list was read from: the id names its tag only.
        let scope = self.ctx.scope();
        cx.spawn(async move |this, cx| {
            if let Some(answer) = answer {
                if answer.await != Ok(true) {
                    return;
                }
            }
            let deleted = runner.run(move || scope.catalog(|c| c.delete_tag(orphan.id))).await;
            this.update(cx, |s, cx| {
                if !s.ctx.live(cx) {
                    return;
                }
                match deleted {
                    Ok(Ok(())) => {
                        if let Some(list) = &mut s.orphans {
                            list.retain(|o| o.id != orphan.id);
                        }
                        s.status = Some(format!("Deleted {}.", orphan.path));
                        s.ctx.changed(cx);
                    }
                    Ok(Err(e)) => s.status = Some(e),
                    Err(_) => s.status = Some("The delete stopped.".into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

fn badge(text: &'static str, colors: Colors) -> gpui_kit::Div {
    div().px(px(6.)).rounded_full().border_1().border_color(colors.border).text_size(px(10.)).text_color(colors.mute).child(text)
}

impl Render for TagMaintenance {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let idle = self.busy.is_none();
        let mut body = section("prefs-tags", "Tidy tags", colors)
            .child(ui::sub(
                "Remove redundant ancestor tags across your library — when a photo has a more specific tag (e.g. \
                 Harbor/Marina), the parent it implies (Harbor) is dropped. New tagging keeps tags to leaves automatically.",
                colors,
            ))
            .child(ui::row().child(ui::clickable(ui::chip("tags-tidy", "Tidy redundant tags", true, colors), true, cx.listener(|s, _, _, cx| s.tidy(cx)))))
            .child(heading("Duplicate tags", colors).mt(px(10.)))
            .child(ui::sub(
                "Tags whose names look alike, most similar first. Found by name — two tags meaning the same thing under \
                 unlike names (Bike and Velocipede) will not appear here. Merging is your call: “Cycling” and “Cycles” \
                 may be a typo, or two real things.",
                colors,
            ))
            .child(ui::row().child(ui::clickable(
                ui::chip(
                    "tags-find-duplicates",
                    if self.busy == Some(Looking::Duplicates) { "Looking…" } else { "Find duplicate tags" },
                    idle,
                    colors,
                ),
                idle,
                cx.listener(|s, _, _, cx| s.find_duplicates(cx)),
            )));
        match &self.duplicates {
            Some(pairs) if pairs.is_empty() => body = body.child(status("tags-no-duplicates", "No similar tag names found.", colors)),
            Some(pairs) => {
                let mut list = div().id("tags-duplicates").flex().flex_col().gap(px(8.));
                for (i, pair) in pairs.iter().take(MAX_PAIRS).enumerate() {
                    list = list.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(
                                ui::row()
                                    .child(pair.a_path.clone())
                                    .child(ui::sub(thousands(pair.a_photos as i64), colors))
                                    .child(div().text_color(colors.mute).child("vs"))
                                    .child(pair.b_path.clone())
                                    .child(ui::sub(thousands(pair.b_photos as i64), colors)),
                            )
                            .child(ui::sub(pair.reason.clone(), colors))
                            .child(
                                ui::row()
                                    .child(ui::clickable(
                                        ui::chip(SharedString::from(format!("tags-merge-a-{i}")), format!("Merge {} away…", pair.a_path), true, colors),
                                        true,
                                        cx.listener(|s, _, _, cx| s.merge(cx)),
                                    ))
                                    .child(ui::clickable(
                                        ui::chip(SharedString::from(format!("tags-merge-b-{i}")), format!("Merge {} away…", pair.b_path), true, colors),
                                        true,
                                        cx.listener(|s, _, _, cx| s.merge(cx)),
                                    )),
                            ),
                    );
                }
                if pairs.len() > MAX_PAIRS {
                    list = list.child(ui::sub(format!("Showing the {MAX_PAIRS} most similar of {}.", pairs.len()), colors));
                }
                body = body.child(list.test_support());
            }
            None => {}
        }
        body = body
            .child(heading("Unused tags", colors).mt(px(10.)))
            .child(ui::sub(
                "Tags holding no photos anywhere beneath them. An empty branch is marked as such — deleting it removes \
                 the tags under it too.",
                colors,
            ))
            .child(ui::row().child(ui::clickable(
                ui::chip(
                    "tags-find-unused",
                    if self.busy == Some(Looking::Orphans) { "Looking…" } else { "Find unused tags" },
                    idle,
                    colors,
                ),
                idle,
                cx.listener(|s, _, _, cx| s.find_unused(cx)),
            )));
        match &self.orphans {
            Some(list) if list.is_empty() => body = body.child(status("tags-no-orphans", "Every tag is in use.", colors)),
            Some(list) => {
                let mut rows = div().id("tags-orphans").flex().flex_col().gap(px(6.));
                for o in list {
                    let id = o.id;
                    rows = rows.child(
                        ui::row()
                            .child(div().flex_1().min_w_0().child(o.path.clone()))
                            .when(o.has_children, |r| r.child(badge("branch", colors)))
                            .when(o.is_auto_tag, |r| r.child(badge("auto-tag", colors)))
                            .child(ui::clickable(
                                ui::chip(SharedString::from(format!("tags-delete-{id}")), "Delete", true, colors),
                                true,
                                cx.listener(move |s, _, window, cx| s.delete(id, window, cx)),
                            )),
                    );
                }
                body = body.child(rows.test_support());
            }
            None => {}
        }
        body.children(self.status.clone().map(|t| status("tags-status", t, colors)))
    }
}
