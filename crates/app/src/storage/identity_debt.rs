//! "Identity debt" (`IdentityDebtPanel.tsx`): every photo *copy* whose sidecar does not carry
//! its identity yet, one row per copy with each owed field (State, Field, Tries, Last attempt,
//! Detail), the summary line, the explanation, the repair pass, "Show dismissed (N)", the
//! conflict resolutions (Adopt, Overwrite… behind an inline confirm, Dismiss; Restore for a
//! dismissed copy) and 500-row pages (Prev / Next / "Back to first page").
//!
//! The summary and the page are two independent reads, so a slow page never delays the
//! header. The repair pass is [`StorageState`]'s job — it outlives this panel, and opening the
//! panel re-attaches to a pass already running ([`StorageState::reattach_repair`]). Resolving
//! a conflict does not stop a running pass (the core gives each queue row an owner). The pure
//! line builders are React's exported helpers, ported with their tests.

use super::state::StorageEvent;
use super::ui;
use super::{CloseDialog, Runner, StorageState};
use crate::shell::style::Colors;
use chairphoto_core::app::{with_catalog, AppState};
use chairphoto_core::catalog::{
    IdentityConflictAction, IdentityConflictOutcome, IdentityRepairSummary, PendingIdentity, PendingIdentityField,
    PendingIdentitySummary,
};
use gpui_kit::prelude::*;
use gpui_kit::TestSupportExt as _;
use gpui_kit::{div, px, Context, Entity, EventEmitter, Hsla, SharedString, Subscription, Window};
use std::collections::HashMap;

/// IPC page size in React; here the page a single read returns.
pub const PAGE_SIZE: i64 = 500;

pub struct IdentityDebtPanel {
    app: AppState,
    storage: Entity<StorageState>,
    pub summary: Option<PendingIdentitySummary>,
    pub rows: Option<Vec<PendingIdentity>>,
    volumes: HashMap<i64, String>,
    pub page: i64,
    pub show_dismissed: bool,
    pub summary_error: Option<String>,
    pub list_error: Option<String>,
    pub action_error: Option<String>,
    /// The copy whose Overwrite awaits confirmation.
    pub confirm_overwrite: Option<String>,
    /// The copy a resolution is in flight for.
    pub resolving: Option<String>,
    pub resolve_result: Option<String>,
    /// Bumped by every page read; an older read's rows are dropped.
    page_seq: u64,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for IdentityDebtPanel {}

/// A copy's key, as React's `rowKey`.
pub fn row_key(p: &PendingIdentity) -> String {
    format!("{}-{}-{}", p.photo_id, p.volume_id, p.relative_path)
}

impl IdentityDebtPanel {
    pub fn new(storage: Entity<StorageState>, cx: &mut Context<Self>) -> Self {
        let app = storage.read(cx).app_state().clone();
        let ended = cx.subscribe(&storage, |this: &mut Self, _, event: &StorageEvent, cx| {
            if let StorageEvent::RepairEnded = event {
                // Repaired copies left the queue and every later offset shifted.
                this.page = 0;
                this.reload_summary(cx);
                this.reload_page(cx);
            }
        });
        let observe = cx.observe(&storage, |_, _, cx| cx.notify());
        let mut this = IdentityDebtPanel {
            app,
            storage: storage.clone(),
            summary: None,
            rows: None,
            volumes: HashMap::new(),
            page: 0,
            show_dismissed: false,
            summary_error: None,
            list_error: None,
            action_error: None,
            confirm_overwrite: None,
            resolving: None,
            resolve_result: None,
            page_seq: 0,
            _subscriptions: vec![ended, observe],
        };
        this.reload_summary(cx);
        this.reload_page(cx);
        this.load_volumes(cx);
        storage.update(cx, |s, cx| s.reattach_repair(cx));
        this
    }

    pub fn reload_summary(&mut self, cx: &mut Context<Self>) {
        self.summary_error = None;
        let state = self.app.clone();
        let rx = Runner::get(cx).run(move || with_catalog(&state, |c| c.summarize_pending_identity()));
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                match result {
                    Ok(v) => s.summary = Some(v),
                    Err(e) => s.summary_error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn reload_page(&mut self, cx: &mut Context<Self>) {
        self.list_error = None;
        self.page_seq += 1;
        let seq = self.page_seq;
        let (state, offset, dismissed) = (self.app.clone(), self.page * PAGE_SIZE, self.show_dismissed);
        let rx = Runner::get(cx)
            .run(move || with_catalog(&state, |c| c.list_pending_identity_page(PAGE_SIZE, offset, dismissed)));
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                if s.page_seq != seq {
                    return;
                }
                match result {
                    Ok(rows) => s.rows = Some(rows),
                    Err(e) => s.list_error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_volumes(&mut self, cx: &mut Context<Self>) {
        let state = self.app.clone();
        let rx = Runner::get(cx).run(move || with_catalog(&state, |c| c.volume_rows()));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(vols)) = rx.await else { return };
            this.update(cx, |s, cx| {
                s.volumes = vols.into_iter().map(|v| (v.id, v.name)).collect();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn set_page(&mut self, page: i64, cx: &mut Context<Self>) {
        self.page = page.max(0);
        self.reload_page(cx);
        cx.notify();
    }

    pub fn set_show_dismissed(&mut self, on: bool, cx: &mut Context<Self>) {
        self.show_dismissed = on;
        self.page = 0;
        self.reload_page(cx);
        cx.notify();
    }

    /// Adopt / Overwrite / Dismiss / Restore one copy.
    pub fn resolve(&mut self, index: usize, action: IdentityConflictAction, cx: &mut Context<Self>) {
        let Some(p) = self.rows.as_ref().and_then(|r| r.get(index)).cloned() else { return };
        let key = row_key(&p);
        if self.resolving.is_some() {
            return;
        }
        self.resolving = Some(key);
        self.action_error = None;
        self.resolve_result = None;
        let state = self.app.clone();
        let epoch = self.storage.read(cx).epoch();
        let rx = Runner::get(cx).run(move || {
            chairphoto_core::app::identity::resolve_identity_conflict(&state, p.photo_id, p.volume_id, &p.relative_path, action)
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the resolve worker stopped".into()));
            this.update(cx, |s, cx| {
                s.resolving = None;
                if s.storage.read(cx).epoch() != epoch {
                    return;
                }
                match result {
                    Ok(outcome) => {
                        s.resolve_result = Some(resolution_message(&outcome));
                        s.confirm_overwrite = None;
                        s.page = 0;
                        s.reload_summary(cx);
                        s.reload_page(cx);
                    }
                    // A refusal names what it refused; shown verbatim.
                    Err(e) => s.action_error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn row_actions(&self, i: usize, p: &PendingIdentity, colors: Colors, cx: &mut Context<Self>) -> gpui_kit::Div {
        let key = row_key(p);
        let busy = self.resolving.as_deref() == Some(key.as_str());
        let id = |what: &str| SharedString::from(format!("{what}-{i}"));
        let action = |what: &str, label: &'static str, act: IdentityConflictAction, cx: &mut Context<Self>| {
            ui::clickable(ui::chip(id(what), label, !busy, colors), !busy, cx.listener(move |s, _, _, cx| s.resolve(i, act, cx)))
        };
        if conflict_field(p).is_some() {
            if self.confirm_overwrite.as_deref() == Some(key.as_str()) {
                return ui::row()
                    .child(ui::sub("Replace the identifier in the file?", colors))
                    .child(ui::clickable(
                        ui::danger_chip(id("overwrite-confirm"), "Overwrite", !busy, colors),
                        !busy,
                        cx.listener(move |s, _, _, cx| s.resolve(i, IdentityConflictAction::Overwrite, cx)),
                    ))
                    .child(ui::clickable(
                        ui::chip(id("overwrite-cancel"), "Cancel", !busy, colors),
                        !busy,
                        cx.listener(|s, _, _, cx| {
                            s.confirm_overwrite = None;
                            cx.notify();
                        }),
                    ));
            }
            return ui::row()
                .child(action("adopt", "Adopt", IdentityConflictAction::Adopt, cx))
                .child(ui::clickable(
                    ui::chip(id("overwrite"), "Overwrite…", !busy, colors),
                    !busy,
                    cx.listener(move |s, _, _, cx| {
                        s.confirm_overwrite = s.rows.as_ref().and_then(|r| r.get(i)).map(row_key);
                        cx.notify();
                    }),
                ))
                .child(action("dismiss", "Dismiss", IdentityConflictAction::Dismiss, cx));
        }
        if dismissed_field(p).is_some() {
            return ui::row().child(action("restore", "Restore", IdentityConflictAction::Restore, cx));
        }
        ui::row()
    }
}

// --- React's pure helpers -----------------------------------------------------------------

/// The header line, e.g. "3 copies owe their identity to a sidecar — 1 conflict".
pub fn summary_headline(summary: Option<&PendingIdentitySummary>) -> String {
    let Some(s) = summary else { return "Loading…".into() };
    let (copies, owe) = if s.total == 1 { ("copy", "owes") } else { ("copies", "owe") };
    let mut line = format!("{} {copies} {owe} their identity to a sidecar", s.total);
    if s.conflicts > 0 {
        line += &format!(" — {} conflict{}", s.conflicts, if s.conflicts == 1 { "" } else { "s" });
    }
    if s.dismissed > 0 {
        line += &format!("{} {} dismissed", if s.conflicts > 0 { "," } else { " —" }, s.dismissed);
    }
    line
}

/// What a finished (or stopped) pass did. A stopped pass says so: its counters are partial.
pub fn repair_summary_line(s: &IdentityRepairSummary) -> String {
    let mut parts = vec![
        format!("bound {}", s.bound),
        format!("still unreachable {}", s.unreachable),
        format!("conflict {}", s.conflicts),
        format!("failed {}", s.failed),
    ];
    if s.superseded > 0 {
        parts.push(format!("{} decided elsewhere while the pass ran", s.superseded));
    }
    let lead = if s.aborted {
        format!("Stopped after {} of {}", s.bound + s.unreachable + s.conflicts + s.failed + s.superseded, s.total)
    } else {
        "Finished".into()
    };
    format!("{lead} — {}", parts.join(" · "))
}

/// "Repairing… 120 of 74488", or "Repairing…" before the queue is counted.
pub fn repair_progress_line(done: usize, total: usize) -> String {
    if total > 0 {
        format!("Repairing… {done} of {total}")
    } else {
        "Repairing…".into()
    }
}

/// The conflicted `identifier` field of a copy (only `identifier` can conflict).
pub fn conflict_field(p: &PendingIdentity) -> Option<&PendingIdentityField> {
    p.fields.iter().find(|f| f.field == "identifier" && f.state == "conflict")
}

/// The dismissed `identifier` field of a copy, the one Restore puts back.
pub fn dismissed_field(p: &PendingIdentity) -> Option<&PendingIdentityField> {
    p.fields.iter().find(|f| f.field == "identifier" && f.state == "dismissed")
}

/// The line after a resolution, stated from the outcome the core returned.
pub fn resolution_message(o: &IdentityConflictOutcome) -> String {
    match o.action.as_str() {
        "adopt" => {
            let others = if o.rechecked_copies > 0 {
                format!(
                    " {} other cop{} of this photo re-checked.",
                    o.rechecked_copies,
                    if o.rechecked_copies == 1 { "y" } else { "ies" }
                )
            } else {
                String::new()
            };
            format!("Adopted {} from the sidecar. The catalog now uses it.{others}", o.catalog_uuid)
        }
        "overwrite" => {
            let backup = match &o.sidecar_backup {
                Some(b) => format!(" The previous sidecar is at {b}."),
                None => " An earlier backup of this sidecar was already kept and left untouched.".into(),
            };
            format!("Overwrote the sidecar with {}, replacing {}.{backup}", o.catalog_uuid, o.previous_sidecar_uuid)
        }
        "dismiss" => "Dismissed. The copy stays on the record but is neither retried nor counted as debt.".into(),
        _ => "Restored. The copy is queued again.".into(),
    }
}

/// "Showing N–M of T", never smaller than what is shown.
pub fn paging_label(offset: i64, shown: i64, total: Option<i64>) -> String {
    if shown == 0 {
        return if total == Some(0) { "No pending copies".into() } else { "No rows on this page".into() };
    }
    let (from, to) = (offset + 1, offset + shown);
    match total {
        None => format!("Showing {from}–{to}"),
        Some(t) => format!("Showing {from}–{to} of {}", t.max(to)),
    }
}

fn state_label(state: &str) -> &'static str {
    match state {
        "unreachable" => "Unreachable",
        "unwritable" => "Unwritable",
        "conflict" => "Conflict",
        "dismissed" => "Dismissed",
        _ => "Unknown",
    }
}

/// Unreachable and Dismissed read as normal; Unwritable warns; Conflict asks for a decision.
fn state_color(state: &str, colors: Colors) -> Hsla {
    match state {
        "unwritable" => colors.danger,
        "conflict" => colors.accent,
        _ => colors.dim,
    }
}

fn when_line(unix: i64) -> String {
    if unix == 0 {
        return "never".into();
    }
    // Local time needs a time-zone database the app does not carry; UTC, labelled.
    let secs = unix.rem_euclid(86_400);
    format!("{} {:02}:{:02} UTC", super::catalog_switcher::date_line(unix), secs / 3600, (secs % 3600) / 60)
}

impl Render for IdentityDebtPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let repair = self.storage.read(cx).repair.clone();
        let summary = self.summary;
        let mut body = ui::body()
            .id("identity-debt")
            .child(div().id("debt-headline").child(ui::sub(summary_headline(summary.as_ref()), colors)).test_support())
            .child(ui::sub(
                "Each row is one copy of a photo, listing every identity field it still owes (a copy can owe both its \
                 UUID and its import batch). Unreachable means that copy could not be found just now — normal, not an \
                 error, whether its volume is offline or the file was moved, renamed, or deleted outside ChairPhoto; a \
                 repair pass picks it up again once the file is reachable at its known location. Unwritable means the \
                 file was found but its sidecar could not be written (read-only storage, a corrupt sidecar, a full \
                 disk) — see Detail for why. Conflict means the file's sidecar already carries a different identity — \
                 the file is left untouched until you decide: Adopt the identity that is in the file (changes the \
                 catalog, never the file), Overwrite the file with the catalog's (destroys the identifier that was \
                 there; the sidecar is backed up first), or Dismiss the copy (changes nothing, stops the retries). \
                 Adopting an identity another photo already holds is refused — no two photos may share one.",
                colors,
            ));
        let can_start = !repair.running && summary.is_some_and(|s| s.total > 0);
        let mut controls = ui::row().child(ui::clickable(
            ui::primary("repair-start", if repair.running { "Repairing…" } else { "Start repair pass" }, can_start, colors),
            can_start,
            cx.listener(|s, _, _, cx| s.storage.update(cx, |st, cx| st.start_repair(cx))),
        ));
        if repair.running {
            let (done, total) = repair.progress.unwrap_or((0, 0));
            controls = controls
                .child(ui::clickable(
                    ui::chip("repair-cancel", "Cancel", true, colors),
                    true,
                    cx.listener(|s, _, _, cx| s.storage.update(cx, |st, cx| st.cancel_repair(cx))),
                ))
                .child(div().id("repair-progress").child(ui::sub(repair_progress_line(done, total), colors)).test_support());
        }
        let dismissed_n = summary.map_or(0, |s| s.dismissed);
        controls = controls.child(ui::clickable(
            ui::chip(
                "show-dismissed",
                format!(
                    "{} Show dismissed{}",
                    if self.show_dismissed { "☑" } else { "☐" },
                    if dismissed_n > 0 { format!(" ({dismissed_n})") } else { String::new() }
                ),
                true,
                colors,
            ),
            true,
            cx.listener(|s, _, _, cx| s.set_show_dismissed(!s.show_dismissed, cx)),
        ));
        if let Some(r) = &repair.result {
            controls = controls.child(div().id("repair-result").child(ui::sub(repair_summary_line(r), colors)).test_support());
        }
        body = body.child(controls);
        if let Some(r) = &self.resolve_result {
            body = body.child(div().id("resolve-result").child(ui::sub(r.clone(), colors)).test_support());
        }
        for (id, e) in [
            ("summary-error", &self.summary_error),
            ("list-error", &self.list_error),
            ("action-error", &self.action_error),
            ("repair-error", &repair.error),
        ] {
            if let Some(e) = e {
                body = body.child(ui::error(id, e.clone(), colors));
            }
        }
        let Some(rows) = self.rows.clone() else {
            return body.child(ui::empty("debt-loading", "Loading…", colors));
        };
        if rows.is_empty() && self.page == 0 {
            let mut empty = div().id("debt-empty").flex().flex_wrap().items_center().gap(px(6.)).child(ui::sub(
                "No identity debt — every known copy is bound.",
                colors,
            ));
            if !self.show_dismissed && dismissed_n > 0 {
                empty = empty
                    .child(ui::sub(
                        format!("{dismissed_n} dismissed cop{} hidden.", if dismissed_n == 1 { "y is" } else { "ies are" }),
                        colors,
                    ))
                    .child(ui::clickable(
                        ui::chip("show-dismissed-empty", "Show dismissed", true, colors),
                        true,
                        cx.listener(|s, _, _, cx| s.set_show_dismissed(true, cx)),
                    ));
            }
            return body.child(empty.test_support());
        }
        let header = |t: &'static str, w: f32| div().w(px(w)).flex_none().text_size(px(10.5)).text_color(colors.mute).child(t);
        body = body.child(
            div()
                .flex()
                .gap(px(8.))
                .child(header("State", 86.))
                .child(div().flex_1().text_size(px(10.5)).text_color(colors.mute).child("Path"))
                .child(header("Volume", 90.))
                .child(header("Relative path (on volume)", 150.))
                .child(header("Field", 80.))
                .child(header("Tries", 40.))
                .child(header("Last attempt", 130.))
                .child(header("Detail", 160.))
                .child(header("Resolve", 200.)),
        );
        if rows.is_empty() {
            body = body.child(
                ui::row().child(ui::sub("No rows on this page.", colors)).child(ui::clickable(
                    ui::chip("debt-first-page", "Back to first page", true, colors),
                    true,
                    cx.listener(|s, _, _, cx| s.set_page(0, cx)),
                )),
            );
        } else {
            let mut list = div().id("debt-rows").flex().flex_col().max_h(px(360.)).overflow_y_scroll();
            for (i, p) in rows.iter().enumerate() {
                let per_field = |w: f32, f: &dyn Fn(&PendingIdentityField) -> (String, Hsla)| {
                    div().w(px(w)).flex_none().flex().flex_col().children(p.fields.iter().map(|field| {
                        let (text, color) = f(field);
                        div().h(px(22.)).text_size(px(11.)).text_color(color).truncate().child(text)
                    }))
                };
                let volume = self.volumes.get(&p.volume_id).cloned().unwrap_or_else(|| format!("volume {}", p.volume_id));
                let actions = self.row_actions(i, p, colors, cx);
                list = list.child(
                    div()
                        .id(SharedString::from(format!("debt-row-{i}")))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .py(px(5.))
                        .border_b_1()
                        .border_color(colors.line)
                        .child(per_field(86., &|f| (state_label(&f.state).into(), state_color(&f.state, colors))))
                        .child(div().flex_1().min_w_0().text_size(px(11.)).truncate().child(p.path.clone()))
                        .child(div().w(px(90.)).flex_none().text_size(px(11.)).truncate().child(volume))
                        .child(div().w(px(150.)).flex_none().text_size(px(11.)).truncate().child(p.relative_path.clone()))
                        .child(per_field(80., &|f| {
                            (if f.field == "identifier" { "UUID".into() } else { "import batch".into() }, colors.dim)
                        }))
                        .child(per_field(40., &|f| (format!("{}×", f.attempts), colors.dim)))
                        .child(per_field(130., &|f| (when_line(f.last_attempt_at), colors.dim)))
                        .child(per_field(160., &|f| {
                            (if f.error.is_empty() { "—".into() } else { f.error.clone() }, colors.dim)
                        }))
                        .child(div().w(px(200.)).flex_none().child(actions))
                        .test_support(),
                );
            }
            body = body.child(list);
        }
        let shown = rows.len() as i64;
        let total = summary.map(|s| s.total + if self.show_dismissed { s.dismissed } else { 0 });
        let (can_prev, can_next) = (self.page > 0, shown == PAGE_SIZE);
        body.child(
            ui::row()
                .child(div().id("debt-paging").child(ui::sub(paging_label(self.page * PAGE_SIZE, shown, total), colors)).test_support())
                .child(ui::clickable(ui::chip("debt-prev", "← Prev", can_prev, colors), can_prev, cx.listener(|s, _, _, cx| s.set_page(s.page - 1, cx))))
                .child(ui::clickable(ui::chip("debt-next", "Next →", can_next, colors), can_next, cx.listener(|s, _, _, cx| s.set_page(s.page + 1, cx)))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(total: i64, conflicts: i64, dismissed: i64) -> PendingIdentitySummary {
        PendingIdentitySummary { total, conflicts, dismissed }
    }

    /// The cases IdentityDebtPanel.test.ts pinned for `summaryHeadline`.
    #[test]
    fn summary_headline_matches_react() {
        assert_eq!(summary_headline(None), "Loading…");
        assert_eq!(summary_headline(Some(&summary(1, 0, 0))), "1 copy owes their identity to a sidecar");
        assert_eq!(summary_headline(Some(&summary(3, 1, 0))), "3 copies owe their identity to a sidecar — 1 conflict");
        assert_eq!(summary_headline(Some(&summary(3, 2, 4))), "3 copies owe their identity to a sidecar — 2 conflicts, 4 dismissed");
        assert_eq!(summary_headline(Some(&summary(0, 0, 2))), "0 copies owe their identity to a sidecar — 2 dismissed");
    }

    #[test]
    fn a_stopped_pass_says_it_stopped() {
        let mut s = IdentityRepairSummary { bound: 2, unreachable: 1, total: 10, ..Default::default() };
        assert_eq!(repair_summary_line(&s), "Finished — bound 2 · still unreachable 1 · conflict 0 · failed 0");
        s.aborted = true;
        s.superseded = 1;
        assert_eq!(
            repair_summary_line(&s),
            "Stopped after 4 of 10 — bound 2 · still unreachable 1 · conflict 0 · failed 0 · 1 decided elsewhere while the pass ran"
        );
        assert_eq!(repair_progress_line(0, 0), "Repairing…");
        assert_eq!(repair_progress_line(120, 74488), "Repairing… 120 of 74488");
    }

    #[test]
    fn paging_never_claims_fewer_than_shown() {
        assert_eq!(paging_label(0, 0, Some(0)), "No pending copies");
        assert_eq!(paging_label(500, 0, Some(400)), "No rows on this page");
        assert_eq!(paging_label(0, 4, Some(3)), "Showing 1–4 of 4");
        assert_eq!(paging_label(500, 2, None), "Showing 501–502");
    }

    #[test]
    fn resolution_messages_follow_the_outcome() {
        let o = |action: &str, backup: Option<&str>, rechecked: usize| IdentityConflictOutcome {
            action: action.into(),
            photo_id: 1,
            catalog_uuid: "C".into(),
            previous_sidecar_uuid: "P".into(),
            rechecked_copies: rechecked,
            sidecar_backup: backup.map(Into::into),
        };
        assert_eq!(
            resolution_message(&o("adopt", None, 2)),
            "Adopted C from the sidecar. The catalog now uses it. 2 other copies of this photo re-checked."
        );
        assert_eq!(resolution_message(&o("overwrite", Some("/b.xmp"), 0)), "Overwrote the sidecar with C, replacing P. The previous sidecar is at /b.xmp.");
        assert!(resolution_message(&o("dismiss", None, 0)).starts_with("Dismissed."));
        assert_eq!(resolution_message(&o("restore", None, 0)), "Restored. The copy is queued again.");
    }
}
