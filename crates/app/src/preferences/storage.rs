//! Preferences → Storage: the library folder (`LibrarySection`), Safety (`SafetyPanel.tsx`),
//! Local / NAS tiering with "Index existing NAS photos" (`TieringSection`) and Maintenance
//! (`MaintenanceSection`). Volumes is the storage ticket's [`crate::storage::volumes`].

use super::{heading, section, status, thousands, Ctx};
use crate::storage::{ui, CloseDialog, Runner};
use crate::shell::style::Colors;
use chairphoto_core::app::storage::OFFLOAD_AGE_SETTING;
use chairphoto_core::app::{catalogs, expand_home, scans, storage as core_storage};
use chairphoto_core::catalog::{SafetySummary, StorageTier};
use chairphoto_core::scanner::ScanResult;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, EventEmitter, PathPromptOptions, SharedString, Subscription, Window};

// --- library folder ---------------------------------------------------------------------

/// The library folder (= catalog root = local volume base): the path and Set.
pub struct LibrarySection {
    ctx: Ctx,
    pub root: Entity<InputState>,
    pub status: Option<String>,
    pub busy: bool,
}

impl LibrarySection {
    pub fn new(ctx: Ctx, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let root = cx.new(|cx| InputState::new(window, cx).placeholder("~/Pictures/Raw"));
        ctx.run_in(
            window,
            cx,
            |scope| scope.catalog(|c| Ok(c.root().to_string_lossy().to_string())),
            |s: &mut Self, result, window, cx| {
                if let Ok(path) = result {
                    s.root.update(cx, |i, cx| i.set_value(path, window, cx));
                }
            },
        );
        LibrarySection { ctx, root, status: None, busy: false }
    }

    /// Set: re-root the open catalog there (`reroot_open_catalog_as`, the two-phase
    /// transition that trips every job; refused once another catalog is open), then say to
    /// rescan.
    pub fn apply(&mut self, cx: &mut Context<Self>) {
        self.status = None;
        let root = self.root.read(cx).value().trim().to_string();
        if root.is_empty() || self.busy {
            cx.notify();
            return;
        }
        self.busy = true;
        cx.notify();
        self.ctx.run(
            cx,
            move |scope| catalogs::reroot_open_catalog_as(scope.state(), scope.identity()?, expand_home(&root)),
            |s: &mut Self, result, cx| {
                s.busy = false;
                match result {
                    Ok(()) => {
                        s.status = Some("Library folder set — re-scan to index it.".into());
                        s.ctx.status("Library folder changed — click Rescan library to index it.", cx);
                        s.ctx.changed(cx);
                    }
                    Err(e) => s.status = Some(e),
                }
            },
        );
    }
}

impl Render for LibrarySection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let busy = self.busy;
        section("prefs-library", "Library", colors)
            .child(ui::sub(
                "Your photo library folder. Photos are stored relative to it (it's also the local volume). Changing it \
                 re-roots the catalog — re-scan afterward.",
                colors,
            ))
            .child(
                ui::row()
                    .child(div().flex_1().child(Input::new(&self.root).id("library-root")))
                    .child(ui::clickable(ui::primary("library-set", "Set", !busy, colors), !busy, cx.listener(|s, _, _, cx| s.apply(cx)))),
            )
            .children(self.status.clone().map(|t| status("library-status", t, colors)))
    }
}

// --- safety ------------------------------------------------------------------------------

/// "Would I lose these photos if a disk died?" — the library's safety buckets
/// (`SafetyPanel.tsx`). Its "Show me" filters the grid to the bucket and closes Preferences.
pub struct SafetySection {
    ctx: Ctx,
    pub summary: Option<SafetySummary>,
    pub error: Option<String>,
}

impl EventEmitter<CloseDialog> for SafetySection {}

/// React's `since`: how long ago a unix time was, in words.
pub fn since(unix_secs: i64, now: i64) -> String {
    let days = (now - unix_secs).div_euclid(86_400);
    match days {
        d if d < 1 => "today".into(),
        1 => "1 day".into(),
        d if d < 60 => format!("{d} days"),
        d if d / 30 < 24 => format!("{} months", d / 30),
        d => format!("{} years", d / 365),
    }
}

impl SafetySection {
    pub fn new(ctx: Ctx, cx: &mut Context<Self>) -> Self {
        ctx.run(cx, |scope| scope.catalog(|c| c.library_safety_summary()), |s: &mut Self, result, _| match result {
            Ok(summary) => {
                s.summary = Some(summary);
                s.error = None;
            }
            Err(e) => s.error = Some(e),
        });
        SafetySection { ctx, summary: None, error: None }
    }

    /// "Show me": filter first, then close — the number shown and the photos landed on are
    /// the same set.
    pub fn show(&mut self, tier: StorageTier, cx: &mut Context<Self>) {
        self.ctx.shell.update(cx, |s, cx| s.update_scope(cx, |l| l.set_storage_tier(tier)));
        cx.emit(CloseDialog);
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        &self,
        id: &'static str,
        label: &'static str,
        count: i64,
        tone: gpui_kit::Hsla,
        detail: String,
        action: Option<StorageTier>,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut row = ui::row()
            .items_start()
            .child(div().w(px(70.)).flex_none().text_size(px(16.)).text_color(tone).child(thousands(count)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_color(colors.txt).child(label))
                    .child(ui::sub(detail, colors)),
            );
        if let Some(tier) = action {
            row = row.child(ui::clickable(ui::chip(id, "Show me", true, colors), true, cx.listener(move |s, _, _, cx| s.show(tier, cx))));
        }
        row
    }
}

impl Render for SafetySection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let body = section("prefs-safety", "Safety", colors);
        if let Some(e) = &self.error {
            return body.child(ui::error("safety-error", e.clone(), colors));
        }
        let Some(s) = self.summary.clone() else {
            return body.child(ui::empty("safety-counting", "Counting…", colors));
        };
        if s.missing + s.at_risk + s.unverified + s.stale + s.safe == 0 {
            return body.child(ui::empty("safety-empty", "No photos in the catalog yet.", colors));
        }
        let now = chairphoto_core::app::now_secs();
        let tone = |bad: bool| if bad { colors.danger } else { colors.ok };
        let at_risk_detail = if s.at_risk == 0 {
            "Every photo has a copy at home.".to_string()
        } else if let Some(oldest) = s.oldest_at_risk {
            format!("No copy at home. The oldest has been waiting {}.", since(oldest, now))
        } else {
            "No copy at home.".to_string()
        };
        let stale_detail = if s.stale == 0 {
            "No local edit is newer than the copy at home."
        } else {
            "The photo is safe at home, but an edit made since is only on this machine."
        };
        let mut body = body
            .child(self.row("safety-show-at-risk", "At risk", s.at_risk, tone(s.at_risk > 0), at_risk_detail, (s.at_risk > 0).then_some(StorageTier::AtRisk), colors, cx))
            .child(self.row("safety-show-stale", "Edits not carried home", s.stale, tone(s.stale > 0), stale_detail.into(), (s.stale > 0).then_some(StorageTier::Stale), colors, cx))
            .child(self.row(
                "safety-unverified",
                "Unverified",
                s.unverified,
                colors.mute,
                "A copy at home that has never been hash-verified — its bytes have not been checked since it arrived.".into(),
                None,
                colors,
                cx,
            ))
            .child(self.row("safety-safe", "Safe", s.safe, colors.ok, "Verified at home, with its edits.".into(), None, colors, cx));
        if s.missing > 0 {
            body = body.child(self.row(
                "safety-missing",
                "No copy anywhere",
                s.missing,
                colors.danger,
                "No location on record. Usually a catalog row whose file was moved outside ChairPhoto.".into(),
                None,
                colors,
                cx,
            ));
        }
        if s.at_risk > 0 || s.stale > 0 {
            body = body.child(ui::sub(
                "“Show me” filters the grid to that bucket. Select there — Ctrl+A takes the lot — and use Back up in \
                 the toolbar to queue them. They copy when the NAS is reachable, and the topbar badge tracks what is \
                 still waiting.",
                colors,
            ));
        }
        if s.companions_unchecked > 0 {
            let n = s.companions_unchecked;
            body = body.child(ui::sub(
                format!(
                    "Freshness is as of the last scan. {n} carried sidecar{} not been looked at since being copied \
                     home, so the “edits not carried home” count is a floor rather than a total.",
                    if n == 1 { " has" } else { "s have" }
                ),
                colors,
            ));
        }
        body.child(ui::sub(
            "These counts cover the volumes ChairPhoto can see. Redundancy inside a storage device, and any off-site \
             backup, are invisible to it — a photo listed as safe here is safe as far as this catalog knows.",
            colors,
        ))
    }
}

// --- tiering and the NAS index --------------------------------------------------------------

/// Local / NAS tiering (`offload_age_days`, Save, "Offload older now") and "Index existing
/// NAS photos" (the folder, Browse…, Index; Enter indexes).
pub struct TieringSection {
    ctx: Ctx,
    pub days: Entity<InputState>,
    pub nas: Entity<InputState>,
    pub status: Option<String>,
    pub busy: bool,
    _subscriptions: Vec<Subscription>,
}

/// The policy a typed day count means: blank is 0 (off); digits only.
pub fn parse_days(text: &str) -> u64 {
    let t = text.trim();
    if t.is_empty() {
        0
    } else {
        t.parse().unwrap_or(0)
    }
}

/// The NAS index's result line.
pub fn nas_index_line(r: &ScanResult) -> String {
    let errors = if r.errors > 0 { format!(", {} errors", r.errors) } else { String::new() };
    format!("Indexed {} new photo(s) from the NAS{errors}. Find them under the \"On NAS\" filter.", r.created)
}

impl TieringSection {
    pub fn new(ctx: Ctx, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let days = cx.new(|cx| InputState::new(window, cx).placeholder("90"));
        let nas = cx.new(|cx| InputState::new(window, cx).placeholder("/mnt/nas/Photos or similar"));
        let _subscriptions = vec![
            // Digits only (`replace(/[^0-9]/g, "")`).
            cx.subscribe_in(&days, window, |_, input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = input.read(cx).value().to_string();
                    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
                    if digits != text {
                        input.update(cx, |i, cx| i.set_value(digits, window, cx));
                    }
                }
            }),
            cx.subscribe_in(&nas, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.index_nas(cx);
                }
            }),
        ];
        ctx.run_in(
            window,
            cx,
            |scope| scope.catalog(|c| c.get_setting(OFFLOAD_AGE_SETTING)),
            |s: &mut Self, result, window, cx| {
                if let Ok(Some(v)) = result {
                    if v != "0" {
                        s.days.update(cx, |i, cx| i.set_value(v, window, cx));
                    }
                }
            },
        );
        TieringSection { ctx, days, nas, status: None, busy: false, _subscriptions }
    }

    /// The catalog this section is bound to (tests).
    #[cfg(test)]
    pub fn ctx_identity(&self) -> Option<chairphoto_core::app::CatalogIdentity> {
        self.ctx.identity
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        let n = parse_days(&self.days.read(cx).value());
        self.ctx.run(
            cx,
            move |scope| scope.catalog(|c| c.set_setting(OFFLOAD_AGE_SETTING, &n.to_string())),
            move |s: &mut Self, result, _| {
                s.status = Some(match result {
                    Ok(()) if n > 0 => format!("Saved — photos older than {n} day(s) will be offloaded to the NAS."),
                    Ok(()) => "Saved — automatic offload is off (photos stay on local disk).".into(),
                    Err(e) => e,
                })
            },
        );
    }

    /// "Offload older now" (`apply_offload_policy`).
    pub fn offload_now(&mut self, cx: &mut Context<Self>) {
        self.busy = true;
        self.status = Some("Offloading older photos to the NAS…".into());
        cx.notify();
        let offload = |scope: &super::Scope| core_storage::apply_offload_policy_as(scope.state(), scope.identity()?);
        self.ctx.run(cx, offload, |s: &mut Self, result, cx| {
            s.busy = false;
            match result {
                Ok(n) => {
                    s.status = Some(if n > 0 {
                        format!("Offloaded {n} photo(s) to the NAS (still visible in the library).")
                    } else {
                        "Nothing to offload (none old enough, or the NAS is unreachable).".into()
                    });
                    s.ctx.changed(cx);
                }
                Err(e) => s.status = Some(e),
            }
        });
    }

    /// Browse… for the NAS folder. The portal picker cannot start in the backup volume's
    /// folder as React's did (shell-apis.md § 5: no default folder).
    pub fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.status = None;
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose the NAS folder to index".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let picked = match rx.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Err(e)) => {
                    let line = format!("Couldn't open the folder picker: {e}");
                    this.update(cx, |s, cx| {
                        s.status = Some(line);
                        cx.notify();
                    })
                    .ok();
                    None
                }
                _ => None,
            };
            if let Some(path) = picked {
                this.update_in(cx, |s, window, cx| {
                    s.nas.update(cx, |i, cx| i.set_value(path.to_string_lossy().to_string(), window, cx));
                })
                .ok();
            }
        })
        .detach();
    }

    /// Index: scan the folder in place as NAS-resident (`scan_nas_folder`); its enrichment
    /// (Phase B) runs on after the result is in, on the same worker, as a rescan's does.
    pub fn index_nas(&mut self, cx: &mut Context<Self>) {
        let path = self.nas.read(cx).value().trim().to_string();
        if path.is_empty() {
            self.status = Some("Choose the NAS folder to index.".into());
            cx.notify();
            return;
        }
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = Some("Indexing NAS photos… (this can take a while for a large archive)".into());
        cx.notify();
        let (tx, rx) = futures::channel::oneshot::channel();
        let scope = self.ctx.scope();
        let scan = move || scans::scan_nas_folder_as(scope.state(), scope.identity()?, expand_home(&path));
        Runner::get(cx).spawn(move || match scan() {
            Ok((result, enrich)) => {
                let _ = tx.send(Ok(result));
                enrich.run();
            }
            Err(e) => {
                let _ = tx.send(Err(e));
            }
        });
        let ctx = self.ctx.clone();
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                if !ctx.live(cx) {
                    return;
                }
                s.busy = false;
                match result {
                    Ok(r) => {
                        s.status = Some(nas_index_line(&r));
                        s.ctx.changed(cx);
                    }
                    Err(e) => s.status = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Render for TieringSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let busy = self.busy;
        section("prefs-tiering", "Local / NAS tiering", colors)
            .child(ui::sub(
                "Keep recent photos on local disk and offload older ones (that already have a verified NAS backup) to \
                 free space. Offloaded photos stay in the library — the grid shows a kept thumbnail, and the full photo \
                 loads from the NAS when it's connected. Leave blank to keep everything on local disk.",
                colors,
            ))
            .child(
                ui::row()
                    .child(ui::sub("Keep photos newer than", colors))
                    .child(div().w(px(80.)).child(Input::new(&self.days).id("tiering-days")))
                    .child(ui::sub("days on local disk", colors))
                    .child(ui::clickable(ui::primary("tiering-save", "Save", true, colors), true, cx.listener(|s, _, _, cx| s.save(cx))))
                    .child(ui::clickable(
                        ui::chip("tiering-offload", "Offload older now", !busy, colors),
                        !busy,
                        cx.listener(|s, _, _, cx| s.offload_now(cx)),
                    )),
            )
            .child(heading("Index existing NAS photos", colors).mt(px(10.)))
            .child(ui::sub(
                "One-time: bring an archive that already lives on the NAS into the catalog in place — nothing is copied \
                 to local disk. Those photos appear under the \"On NAS\" tier and are viewable while the NAS is connected.",
                colors,
            ))
            .child(
                ui::row()
                    .child(div().flex_1().child(Input::new(&self.nas).id("tiering-nas")))
                    .child(ui::clickable(ui::chip("tiering-browse", "Browse...", !busy, colors), !busy, cx.listener(|s, _, window, cx| s.browse(window, cx))))
                    .child(ui::clickable(ui::primary("tiering-index", "Index", !busy, colors), !busy, cx.listener(|s, _, _, cx| s.index_nas(cx)))),
            )
            .children(self.status.clone().map(|t| status("tiering-status", t, colors)))
    }
}

// --- maintenance -------------------------------------------------------------------------

/// Which removal Maintenance runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// Gone from both the library and the (reachable) backup.
    Unavailable,
    /// Every copy is a 0-byte file.
    Empty,
}

/// The confirm's body: the count, up to 12 paths, and what is (not) touched.
pub fn removal_confirm(kind: Removal, paths: &[String]) -> String {
    let mut preview: Vec<String> = paths.iter().take(12).map(|p| format!("• {p}")).collect();
    if paths.len() > 12 {
        preview.push(format!("…and {} more", paths.len() - 12));
    }
    let preview = preview.join("\n");
    let n = paths.len();
    match kind {
        Removal::Unavailable => format!(
            "Remove {n} photo(s) from the catalog whose file is gone from both your library and the NAS backup?\n\n\
             {preview}\n\nThis deletes only the catalog entries (tags, ratings, versions). No files on disk or on the \
             NAS are touched. Photos on an offline volume are never removed."
        ),
        Removal::Empty => format!(
            "Remove {n} photo(s) whose only file is empty (0 bytes — the image data is gone)?\n\n{preview}\n\nThis \
             deletes only the catalog entries; the empty files on disk/NAS are left as-is."
        ),
    }
}

/// "Removed 1 unavailable entry (no files deleted)." / "… entries …".
pub fn removed_line(kind: Removal, n: usize) -> String {
    let what = match kind {
        Removal::Unavailable => "unavailable",
        Removal::Empty => "empty",
    };
    format!("Removed {n} {what} entr{} (no files deleted).", if n == 1 { "y" } else { "ies" })
}

/// "Compacted: 120 MB → 80 MB (reclaimed 40 MB)." / "Already compact (80 MB)."
pub fn compact_line(before: i64, after: i64) -> String {
    let mb = |n: i64| format!("{:.0} MB", n as f64 / 1024. / 1024.);
    let saved = before - after;
    if saved > 0 {
        format!("Compacted: {} → {} (reclaimed {}).", mb(before), mb(after), mb(saved))
    } else {
        format!("Already compact ({}).", mb(after))
    }
}

/// Maintenance: remove unavailable / empty photos (find, confirm, purge — catalog rows only,
/// never a file) and compact the catalog.
pub struct MaintenanceSection {
    ctx: Ctx,
    pub status: Option<String>,
    pub busy: bool,
}

impl MaintenanceSection {
    pub fn new(ctx: Ctx, _: &mut Context<Self>) -> Self {
        MaintenanceSection { ctx, status: None, busy: false }
    }

    fn set_status(&mut self, line: impl Into<String>, cx: &mut Context<Self>) {
        self.status = Some(line.into());
        cx.notify();
    }

    /// Find what `kind` would remove; if anything, confirm with the list; then purge.
    pub fn remove(&mut self, kind: Removal, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.set_status(
            match kind {
                Removal::Unavailable => "Checking for unavailable photos…",
                Removal::Empty => "Checking for empty (0-byte) photos…",
            },
            cx,
        );
        let runner = Runner::get(cx);
        // The check and the removal both bound to the section's catalog: the confirm names
        // that catalog's photos, so the purge must not run in another.
        let scope = self.ctx.scope();
        let ctx = self.ctx.clone();
        cx.spawn_in(window, async move |this, cx| {
            let finder = scope.clone();
            let found = runner
                .run(move || {
                    finder.catalog(|c| match kind {
                        Removal::Unavailable => c.find_unavailable_photos(),
                        Removal::Empty => c.find_empty_photos(),
                    })
                })
                .await;
            // Ends the run: the line, not busy any more.
            let finish = |this: &gpui_kit::WeakEntity<Self>, line: String, changed: bool, cx: &mut gpui_kit::AsyncWindowContext| {
                this.update(cx, |s, cx| {
                    if !s.ctx.live(cx) {
                        return;
                    }
                    s.busy = false;
                    s.set_status(line, cx);
                    if changed {
                        s.ctx.changed(cx);
                    }
                })
                .ok();
            };
            let found = match found {
                Ok(Ok(found)) => found,
                Ok(Err(e)) => return finish(&this, e, false, cx),
                Err(_) => return finish(&this, "The check stopped.".into(), false, cx),
            };
            if found.is_empty() {
                let line = match kind {
                    Removal::Unavailable => "All photos are available — nothing to remove.",
                    Removal::Empty => "No empty (0-byte) photos found.",
                };
                return finish(&this, line.into(), false, cx);
            }
            let paths: Vec<String> = found.into_iter().map(|(_, p)| p).collect();
            let title = match kind {
                Removal::Unavailable => "Remove unavailable photos",
                Removal::Empty => "Remove empty photos",
            };
            let Ok(answer) = this.update_in(cx, |s, window, cx| {
                if !s.ctx.live(cx) {
                    return None;
                }
                Some(ui::confirm(window, cx, title.into(), removal_confirm(kind, &paths).into(), "Remove"))
            }) else {
                return;
            };
            let Some(answer) = answer else { return };
            if answer.await != Ok(true) {
                return finish(&this, "Cancelled.".into(), false, cx);
            }
            if !cx.update(|_, cx| ctx.live(cx)).unwrap_or(false) {
                return;
            }
            let purged = runner
                .run(move || {
                    scope.catalog(|c| match kind {
                        Removal::Unavailable => c.purge_unavailable_photos(),
                        Removal::Empty => c.purge_empty_photos(),
                    })
                })
                .await;
            match purged {
                Ok(Ok(removed)) => finish(&this, removed_line(kind, removed.len()), true, cx),
                Ok(Err(e)) => finish(&this, e, false, cx),
                Err(_) => finish(&this, "The removal stopped.".into(), true, cx),
            }
        })
        .detach();
    }

    /// "Compact database now" (SQLite VACUUM, which also sheds retired columns).
    pub fn compact(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.set_status("Compacting the catalog… this can take a moment.", cx);
        let vacuum = |scope: &super::Scope| catalogs::vacuum_catalog_as(scope.state(), scope.identity()?);
        self.ctx.run(cx, vacuum, |s: &mut Self, result, _| {
            s.busy = false;
            s.status = Some(match result {
                Ok(r) => compact_line(r.before_bytes, r.after_bytes),
                Err(e) => e,
            });
        });
    }
}

impl Render for MaintenanceSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let ok = !self.busy;
        section("prefs-maintenance", "Maintenance", colors)
            .child(ui::sub(
                "Remove catalog entries whose original is gone from both your library and the backup, or whose only \
                 file is empty (0 bytes). Only the catalog entry is deleted — never a file. Photos that may still be on \
                 an offline volume are left alone.",
                colors,
            ))
            .child(
                ui::row()
                    .child(ui::clickable(
                        ui::chip("maintenance-unavailable", "Remove unavailable photos", ok, colors),
                        ok,
                        cx.listener(|s, _, window, cx| s.remove(Removal::Unavailable, window, cx)),
                    ))
                    .child(ui::clickable(
                        ui::chip("maintenance-empty", "Remove empty (0-byte) photos", ok, colors),
                        ok,
                        cx.listener(|s, _, window, cx| s.remove(Removal::Empty, window, cx)),
                    )),
            )
            .child(heading("Compact database", colors).mt(px(10.)))
            .child(ui::sub(
                "Reclaim disk space left behind by deletions and defragment the catalog (SQLite VACUUM). Your data is \
                 unchanged. Worth running after large removals; it briefly needs ~double the catalog size in free space.",
                colors,
            ))
            .child(ui::sub(
                "This also drops retired storage the catalog no longer uses — currently a per-metadata column that was \
                 written on every scan and never read. On a large library that column is worth a few hundred MB, and \
                 removing it rewrites the metadata table, which is why it happens here rather than silently at startup.",
                colors,
            ))
            .child(ui::row().child(ui::clickable(
                ui::chip("maintenance-compact", "Compact database now", ok, colors),
                ok,
                cx.listener(|s, _, _, cx| s.compact(cx)),
            )))
            .children(self.status.clone().map(|t| status("maintenance-status", SharedString::from(t), colors)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_reads_like_reacts() {
        let now = 1_000_000_000;
        let day = 86_400;
        assert_eq!(since(now - 3600, now), "today");
        assert_eq!(since(now - day, now), "1 day");
        assert_eq!(since(now - 59 * day, now), "59 days");
        assert_eq!(since(now - 90 * day, now), "3 months");
        assert_eq!(since(now - 800 * day, now), "2 years");
    }

    #[test]
    fn the_lines_match_reacts() {
        assert_eq!(parse_days(""), 0);
        assert_eq!(parse_days(" 90 "), 90);
        assert_eq!(removed_line(Removal::Unavailable, 1), "Removed 1 unavailable entry (no files deleted).");
        assert_eq!(removed_line(Removal::Empty, 3), "Removed 3 empty entries (no files deleted).");
        assert_eq!(compact_line(120 << 20, 80 << 20), "Compacted: 120 MB → 80 MB (reclaimed 40 MB).");
        assert_eq!(compact_line(80 << 20, 80 << 20), "Already compact (80 MB).");
        let r = ScanResult { scanned: 4, imported: 4, created: 3, errors: 1, skipped: 0 };
        assert_eq!(nas_index_line(&r), "Indexed 3 new photo(s) from the NAS, 1 errors. Find them under the \"On NAS\" filter.");
    }

    /// The confirm lists at most 12 paths and says how many more.
    #[test]
    fn the_removal_confirm_lists_twelve_paths() {
        let paths: Vec<String> = (0..14).map(|i| format!("2026/p{i}.ARW")).collect();
        let body = removal_confirm(Removal::Unavailable, &paths);
        assert!(body.starts_with("Remove 14 photo(s) from the catalog"), "{body}");
        assert!(body.contains("• 2026/p11.ARW\n…and 2 more"), "{body}");
        assert!(!body.contains("p12.ARW"));
    }
}
