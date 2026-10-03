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
use chairphoto_core::app::{with_catalog, with_catalog_identified, AppState, CatalogIdentity};
use chairphoto_core::app::iptc::IptcSaveOutcome;
use chairphoto_core::catalog::{
    IdentityConflictAction, IdentityConflictOutcome, IdentityRepairSummary, OwedDismissal, OwedIptc, PendingIdentity,
    PendingIdentityField, PendingIdentitySummary,
};
use gpui_kit::prelude::*;
use gpui_kit::TestSupportExt as _;
use gpui_kit::{div, px, Context, Entity, EventEmitter, Hsla, SharedString, Subscription, Window};
use std::collections::HashMap;

/// IPC page size in React; here the page a single read returns.
pub const PAGE_SIZE: i64 = 500;

/// The page of the owed-IPTC list (#153) a single read returns.
pub const OWED_PAGE_SIZE: i64 = 100;

pub struct IdentityDebtPanel {
    app: AppState,
    storage: Entity<StorageState>,
    pub summary: Option<PendingIdentitySummary>,
    pub rows: Option<Vec<PendingIdentity>>,
    /// The catalog `rows` were read from: a resolution of one of them is bound to it.
    pub rows_from: Option<CatalogIdentity>,
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
    /// The photos owing IPTC to their sidecar (#153), one page, and the catalog it was read
    /// from: a Dismiss or Retry of one of them is bound to it.
    pub owed: Option<Vec<OwedIptc>>,
    pub owed_from: Option<CatalogIdentity>,
    pub owed_page: i64,
    pub owed_error: Option<String>,
    /// The photo a Dismiss or Retry is in flight for.
    pub owed_busy: Option<i64>,
    pub owed_result: Option<String>,
    /// Bumped by every owed-IPTC page read; an older read's rows are dropped.
    owed_seq: u64,
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
        let ended = cx.subscribe(&storage, |this: &mut Self, _, event: &StorageEvent, cx| match event {
            StorageEvent::RepairEnded => {
                // Repaired copies left the queue and every later offset shifted.
                this.page = 0;
                this.owed_page = 0;
                this.reload_summary(cx);
                this.reload_page(cx);
                this.reload_owed(cx);
            }
            StorageEvent::CatalogSwitched => {
                // The owed rows name another catalog's photos now: drop them (an action on
                // one would fail closed anyway) and read the new catalog's.
                this.owed = None;
                this.owed_from = None;
                this.owed_page = 0;
                this.owed_result = None;
                this.owed_error = None;
                // An action on the old list may still be in flight (waiting for a long-held
                // turn); it fails closed and its answer is dropped, so it must not keep the
                // new list's buttons disabled (review of #153, N2).
                this.owed_busy = None;
                this.reload_summary(cx);
                this.reload_owed(cx);
            }
            _ => {}
        });
        let observe = cx.observe(&storage, |_, _, cx| cx.notify());
        let mut this = IdentityDebtPanel {
            app,
            storage: storage.clone(),
            summary: None,
            rows: None,
            rows_from: None,
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
            owed: None,
            owed_from: None,
            owed_page: 0,
            owed_error: None,
            owed_busy: None,
            owed_result: None,
            owed_seq: 0,
            _subscriptions: vec![ended, observe],
        };
        this.reload_summary(cx);
        this.reload_page(cx);
        this.reload_owed(cx);
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
            .run(move || with_catalog_identified(&state, |c| c.list_pending_identity_page(PAGE_SIZE, offset, dismissed)));
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                if s.page_seq != seq {
                    return;
                }
                match result {
                    Ok((from, rows)) => {
                        s.rows = Some(rows);
                        s.rows_from = Some(from);
                    }
                    Err(e) => s.list_error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Read the current page of the photos owing IPTC, with the identity of its catalog.
    pub fn reload_owed(&mut self, cx: &mut Context<Self>) {
        self.owed_error = None;
        self.owed_seq += 1;
        let seq = self.owed_seq;
        let (state, offset) = (self.app.clone(), self.owed_page * OWED_PAGE_SIZE);
        let rx = Runner::get(cx)
            .run(move || chairphoto_core::app::iptc_owed::list_owed_iptc(&state, OWED_PAGE_SIZE, offset));
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                if s.owed_seq != seq {
                    return;
                }
                match result {
                    Ok((from, rows)) => {
                        s.owed = Some(rows);
                        s.owed_from = Some(from);
                    }
                    Err(e) => s.owed_error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn set_owed_page(&mut self, page: i64, cx: &mut Context<Self>) {
        self.owed_page = page.max(0);
        self.reload_owed(cx);
        cx.notify();
    }

    /// Dismiss (`retry == false`) or Retry one owed-IPTC row — in the catalog its row was
    /// read from (`CATALOG_CHANGED` once another is open), for the photo with its UUID. Then
    /// the counts are re-read: the panel's and the title bar's.
    ///
    /// `row` and `from` are what the button was drawn with, captured at render time — never
    /// looked up by index at click time. A list re-read (or a catalog switch's) can land and
    /// notify before the next draw, and a click against the frame on screen must still act
    /// on the row the user saw, in the catalog it was read from (review of #153, M1).
    pub fn act_on_owed(&mut self, row: OwedIptc, from: CatalogIdentity, retry: bool, cx: &mut Context<Self>) {
        if self.owed_busy.is_some() {
            return;
        }
        self.owed_busy = Some(row.photo_id);
        self.owed_error = None;
        self.owed_result = None;
        let state = self.app.clone();
        let epoch = self.storage.read(cx).epoch();
        // Retry waits for the sidecar's write turn and writes the sidecar: the runner's
        // blocking pool, never the UI thread or an async worker.
        let rx = Runner::get(cx).run(move || {
            use chairphoto_core::app::iptc_owed::{dismiss_owed_iptc_as, retry_owed_iptc_as};
            if retry {
                retry_owed_iptc_as(&state, Some(from), row.photo_id, &row.uuid).map(OwedAction::Retried)
            } else {
                dismiss_owed_iptc_as(&state, Some(from), row.photo_id, &row.uuid, row.generation)
                    .map(OwedAction::Dismissed)
            }
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the owed-IPTC worker stopped".into()));
            this.update(cx, |s, cx| {
                // An answer from before a catalog switch touches nothing: the switch freed the
                // buttons, and an action on the new catalog's list may be in flight now.
                if s.storage.read(cx).epoch() != epoch {
                    return;
                }
                s.owed_busy = None;
                match result {
                    Ok(done) => {
                        s.owed_result = Some(owed_action_message(&done));
                        s.reload_summary(cx);
                        s.reload_owed(cx);
                        // The title bar's debt count.
                        s.storage.update(cx, |st, cx| st.invalidate(cx));
                    }
                    // A refusal names what it refused; shown verbatim. The list and count are
                    // re-read too: a row the core refused (its photo gone) leaves the list
                    // (review of #153, N1). The re-read clears the error, so it is set after.
                    Err(e) => {
                        s.reload_summary(cx);
                        s.reload_owed(cx);
                        s.owed_error = Some(e);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn owed_section(&self, colors: Colors, cx: &mut Context<Self>) -> gpui_kit::Div {
        let mut section = div().flex().flex_col().gap(px(4.)).child(div().text_size(px(12.)).child("IPTC owed to sidecars")).child(
            ui::sub(
                "Each row is a photo whose catalog IPTC has fields its sidecar has not received: the sidecar write \
                 failed after the save. Retry writes them now; the repair pass retries them too. Dismiss stops \
                 owing them without writing anything — the catalog keeps its values and the sidecar keeps what it \
                 has (for a photo kept on read-only media).",
                colors,
            ),
        );
        if let Some(r) = &self.owed_result {
            section = section.child(div().id("owed-result").child(ui::sub(r.clone(), colors)).test_support());
        }
        if let Some(e) = &self.owed_error {
            section = section.child(ui::error("owed-error", e.clone(), colors));
        }
        let Some(rows) = self.owed.as_ref() else {
            return section.child(ui::empty("owed-loading", "Loading…", colors));
        };
        if rows.is_empty() && self.owed_page == 0 {
            return section.child(ui::empty("owed-empty", "No photo owes IPTC to its sidecar.", colors));
        }
        let header = |t: &'static str, w: f32| div().w(px(w)).flex_none().text_size(px(10.5)).text_color(colors.mute).child(t);
        section = section.child(
            div()
                .flex()
                .gap(px(8.))
                .child(div().flex_1().text_size(px(10.5)).text_color(colors.mute).child("Path"))
                .child(header("Fields", 160.))
                .child(header("Tries", 40.))
                .child(header("Last attempt", 130.))
                .child(header("Detail", 160.))
                .child(header("", 150.)),
        );
        // Each button carries the row and the catalog it was drawn with (review of #153, M1).
        let from = self.owed_from;
        let enabled = self.owed_busy.is_none() && from.is_some();
        let act = |row: OwedIptc, retry: bool| {
            cx.listener(move |s: &mut Self, _: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut Context<Self>| {
                if let Some(from) = from {
                    s.act_on_owed(row.clone(), from, retry, cx);
                }
            })
        };
        let mut list = div().id("owed-rows").flex().flex_col().max_h(px(240.)).overflow_y_scroll();
        for (i, r) in rows.iter().enumerate() {
            let id = |what: &str| SharedString::from(format!("{what}-{i}"));
            let cell = |w: f32, text: String| div().w(px(w)).flex_none().text_size(px(11.)).text_color(colors.dim).truncate().child(text);
            list = list.child(
                div()
                    .id(SharedString::from(format!("owed-row-{i}")))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .py(px(5.))
                    .border_b_1()
                    .border_color(colors.line)
                    .child(div().flex_1().min_w_0().text_size(px(11.)).truncate().child(r.path.clone()))
                    .child(cell(160., r.fields.join(", ")))
                    .child(cell(40., format!("{}×", r.attempts)))
                    .child(cell(130., when_line(r.last_attempt_at)))
                    .child(cell(160., if r.error.is_empty() { "—".into() } else { r.error.clone() }))
                    .child(
                        ui::row()
                            .w(px(150.))
                            .flex_none()
                            .child(ui::clickable(
                                ui::chip(id("owed-retry"), "Retry", enabled, colors),
                                enabled,
                                act(r.clone(), true),
                            ))
                            .child(ui::clickable(
                                ui::chip(id("owed-dismiss"), "Dismiss", enabled, colors),
                                enabled,
                                act(r.clone(), false),
                            )),
                    )
                    .test_support(),
            );
        }
        section = section.child(list);
        let shown = rows.len() as i64;
        let total = self.summary.map(|s| s.iptc_owed);
        let (can_prev, can_next) = (self.owed_page > 0, shown == OWED_PAGE_SIZE);
        section.child(
            ui::row()
                .child(
                    div()
                        .id("owed-paging")
                        .child(ui::sub(paging_label(self.owed_page * OWED_PAGE_SIZE, shown, total), colors))
                        .test_support(),
                )
                .child(ui::clickable(
                    ui::chip("owed-prev", "← Prev", can_prev, colors),
                    can_prev,
                    cx.listener(|s, _, _, cx| s.set_owed_page(s.owed_page - 1, cx)),
                ))
                .child(ui::clickable(
                    ui::chip("owed-next", "Next →", can_next, colors),
                    can_next,
                    cx.listener(|s, _, _, cx| s.set_owed_page(s.owed_page + 1, cx)),
                )),
        )
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

    /// Adopt / Overwrite / Dismiss / Restore one copy — in the catalog its row was read from
    /// (`CATALOG_CHANGED` once another is open: the ids and path would name another copy).
    pub fn resolve(&mut self, index: usize, action: IdentityConflictAction, cx: &mut Context<Self>) {
        let Some(p) = self.rows.as_ref().and_then(|r| r.get(index)).cloned() else { return };
        let Some(from) = self.rows_from else { return };
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
            chairphoto_core::app::identity::resolve_identity_conflict_as(
                &state,
                from,
                p.photo_id,
                p.volume_id,
                &p.relative_path,
                action,
            )
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
    // #148: IPTC the catalog holds that a sidecar has not received; the pass retries it too.
    if s.iptc_owed > 0 {
        let (photos, owe) = if s.iptc_owed == 1 { ("photo", "owes") } else { ("photos", "owe") };
        line += &format!("; {} {photos} {owe} IPTC to a sidecar", s.iptc_owed);
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
    // Only when the pass met owed IPTC (#148): permanent zeros would read as a failure mode.
    if s.iptc_written + s.iptc_unreachable + s.iptc_failed > 0 {
        parts.push(format!(
            "IPTC written {}, still unreachable {}, failed {}",
            s.iptc_written, s.iptc_unreachable, s.iptc_failed
        ));
    }
    let lead = if s.aborted {
        format!("Stopped after {} of {}", s.done(), s.total)
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

/// What a Dismiss or Retry of one owed-IPTC row did.
#[derive(Debug, Clone)]
pub enum OwedAction {
    /// Dismissed, or why not: the photo is gone, or its debt changed since the row was read.
    Dismissed(OwedDismissal),
    Retried(IptcSaveOutcome),
}

/// The line after a Dismiss or Retry, stated from what the core answered (React's
/// `owedActionMessage`).
pub fn owed_action_message(done: &OwedAction) -> String {
    use chairphoto_core::catalog::IptcSidecarState;
    match done {
        OwedAction::Dismissed(OwedDismissal::Dismissed) => {
            "Dismissed. The catalog keeps its IPTC; the sidecar was not written.".into()
        }
        OwedAction::Dismissed(OwedDismissal::Changed) => {
            "Not dismissed: this photo's owed IPTC changed since the list was read. Check the refreshed row.".into()
        }
        OwedAction::Dismissed(OwedDismissal::Gone) => {
            "Not dismissed: this photo is no longer in the catalog.".into()
        }
        OwedAction::Retried(o) => match (o.sidecar, &o.reason) {
            (IptcSidecarState::Written, _) => "Written to the sidecar.".into(),
            (IptcSidecarState::Unchanged, _) => "Nothing is owed any more: it was written or dismissed meanwhile.".into(),
            (IptcSidecarState::Pending, Some(why)) => format!("Still pending ({why})."),
            (IptcSidecarState::Pending, None) => "Still pending.".into(),
        },
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
        let can_start = !repair.running && summary.is_some_and(|s| s.total > 0 || s.iptc_owed > 0);
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
        // #153: the photos owing IPTC, listed whenever any do (or the list is paged past).
        let owes_iptc = summary.is_some_and(|s| s.iptc_owed > 0)
            || self.owed.as_ref().is_some_and(|r| !r.is_empty())
            || self.owed_page > 0
            || self.owed_result.is_some()
            || self.owed_error.is_some();
        if owes_iptc {
            body = body.child(self.owed_section(colors, cx).id("owed-iptc").test_support());
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
        PendingIdentitySummary { total, conflicts, dismissed, iptc_owed: 0 }
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

    /// #148: the cases IdentityDebtPanel.test.ts pins for owed IPTC.
    #[test]
    fn owed_iptc_is_named_only_when_there_is_some() {
        let owing = |total, iptc_owed| PendingIdentitySummary { iptc_owed, ..summary(total, 0, 0) };
        assert!(!summary_headline(Some(&owing(1, 0))).contains("IPTC"));
        assert_eq!(
            summary_headline(Some(&owing(0, 1))),
            "0 copies owe their identity to a sidecar; 1 photo owes IPTC to a sidecar"
        );
        assert_eq!(
            summary_headline(Some(&owing(2, 3))),
            "2 copies owe their identity to a sidecar; 3 photos owe IPTC to a sidecar"
        );
        let s = IdentityRepairSummary {
            iptc_written: 2,
            iptc_unreachable: 1,
            iptc_failed: 1,
            total: 9,
            aborted: true,
            ..Default::default()
        };
        let line = repair_summary_line(&s);
        assert!(line.contains("IPTC written 2, still unreachable 1, failed 1"), "{line}");
        assert!(line.starts_with("Stopped after 4 of 9"), "{line}");
        assert!(!repair_summary_line(&IdentityRepairSummary::default()).contains("IPTC"));
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

    /// Review of #153, L3: a refused Dismiss says why — the photo is gone, or its debt
    /// changed — rather than one wording for both.
    #[test]
    fn owed_dismiss_messages_tell_gone_from_changed() {
        let say = |d| owed_action_message(&OwedAction::Dismissed(d));
        assert!(say(OwedDismissal::Dismissed).starts_with("Dismissed."));
        assert_eq!(say(OwedDismissal::Gone), "Not dismissed: this photo is no longer in the catalog.");
        assert!(say(OwedDismissal::Changed).starts_with("Not dismissed: this photo's owed IPTC changed"));
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
