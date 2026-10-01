//! "Open catalog" (`CatalogSwitcher.tsx`): the recent catalogs (name, path, relative last
//! opened; a click switches) and "New catalog" (name, folder + Browse…, Create). The switch
//! is the core's `catalogs::switch_catalog` on the [`Runner`]: it trips every job, swaps the
//! catalog and sends `catalog:switched`, which resets the model, the shell and the storage
//! jobs. On success the dialog asks to close.
//!
//! One difference: the folder field expands a leading `~` (React passed the text through,
//! so its own placeholder `~/Pictures/My Catalog` created a literal `~` folder).

use super::ui;
use super::{CloseDialog, RecentRegistry, Runner};
use crate::shell::style::Colors;
use chairphoto_core::app::{expand_home, AppState, RecentCatalog};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, EventEmitter, PathPromptOptions, SharedString, Window};
use std::path::PathBuf;

pub struct CatalogSwitcher {
    app: AppState,
    pub recents: Option<Vec<RecentCatalog>>,
    pub busy: bool,
    pub error: Option<String>,
    pub show_new: bool,
    pub name: Entity<InputState>,
    pub folder: Entity<InputState>,
}

impl EventEmitter<CloseDialog> for CatalogSwitcher {}

impl CatalogSwitcher {
    pub fn new(app: AppState, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("My Catalog"));
        let folder = cx.new(|cx| InputState::new(window, cx).placeholder("~/Pictures/My Catalog"));
        let mut this = CatalogSwitcher { app, recents: None, busy: false, error: None, show_new: false, name, folder };
        this.load(cx);
        this
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let registry = RecentRegistry::get(cx);
        let rx = Runner::get(cx).run(move || match registry {
            Some(dir) => chairphoto_core::app::load_recent_catalogs_in(&dir),
            None => chairphoto_core::app::load_recent_catalogs(),
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                match result {
                    Ok(list) => s.recents = Some(list),
                    Err(e) => {
                        s.recents = Some(Vec::new());
                        s.error = Some(e);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the `i`th recent catalog.
    pub fn open_recent(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(rc) = self.recents.as_ref().and_then(|r| r.get(i)).cloned() else { return };
        self.switch(PathBuf::from(&rc.catalog_path), PathBuf::from(&rc.root), false, rc.name, cx);
    }

    /// "Create catalog": `<folder>/<name>.chairphoto`, photos rooted at `<folder>`.
    pub fn create(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let name = self.name.read(cx).value().trim().to_string();
        let folder = self.folder.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.error = Some("Give the catalog a name.".into());
        } else if folder.is_empty() {
            self.error = Some("Choose a folder for the new catalog.".into());
        }
        if self.error.is_some() {
            cx.notify();
            return;
        }
        let folder = expand_home(&folder);
        self.switch(folder.join(format!("{name}.chairphoto")), folder, true, name, cx);
    }

    fn switch(&mut self, path: PathBuf, root: PathBuf, create: bool, name: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        cx.notify();
        let state = self.app.clone();
        let registry = RecentRegistry::get(cx);
        let (tx, rx) = futures::channel::oneshot::channel();
        Runner::get(cx).spawn(move || {
            let result = chairphoto_core::app::catalogs::switch_catalog_in(
                &state,
                registry.as_deref(),
                &path,
                &root,
                create,
                Some(name),
            );
            match result {
                Ok(resume) => {
                    let _ = tx.send(Ok(()));
                    // The new catalog's interrupted enrichment (I6d), on this worker.
                    if let Some(resume) = resume {
                        resume.run();
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the switch worker stopped".into()));
            this.update(cx, |s, cx| {
                s.busy = false;
                match result {
                    Ok(()) => cx.emit(CloseDialog),
                    Err(e) => s.error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Browse… for the new catalog's folder (xdg portal).
    pub fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose the catalog folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let picked = match rx.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            if let Some(path) = picked {
                this.update_in(cx, |s, window, cx| {
                    s.folder.update(cx, |f, cx| f.set_value(path.to_string_lossy().to_string(), window, cx));
                })
                .ok();
            }
        })
        .detach();
    }
}

/// React's `formatDate`: Today / Yesterday / N days ago, else the date.
pub fn relative_day(ts: i64, now: i64) -> String {
    let days = (now - ts).div_euclid(86_400);
    match days {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => format!("{days} days ago"),
        _ => date_line(ts),
    }
}

/// "Nov 14, 2023" (UTC).
pub fn date_line(ts: i64) -> String {
    let (y, m, d) = civil_from_days(ts.div_euclid(86_400));
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{} {d}, {y}", MONTHS[(m - 1) as usize])
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian (H. Hinnant).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

impl Render for CatalogSwitcher {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let busy = self.busy;
        let now = chairphoto_core::app::now_secs();
        let mut body = ui::body().id("catalog-switcher").child(ui::label("Recent catalogs", colors));
        match &self.recents {
            None => body = body.child(ui::sub("Loading…", colors)),
            Some(list) if list.is_empty() => body = body.child(ui::empty("no-recent", "No recent catalogs.", colors)),
            Some(list) => {
                body = body.child(div().flex().flex_col().gap(px(2.)).children(list.iter().enumerate().map(|(i, rc)| {
                    let row = div()
                        .id(SharedString::from(format!("recent-{i}")))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(px(6.))
                        .hover(|s| s.bg(colors.elev))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_size(px(12.5)).text_color(colors.txt).child(rc.name.clone()))
                                .child(div().text_size(px(10.5)).text_color(colors.mute).truncate().child(rc.catalog_path.clone())),
                        )
                        .child(div().text_size(px(11.)).text_color(colors.dim).child(relative_day(rc.last_opened, now)))
                        .when(!busy, |r| r.cursor_pointer());
                    ui::clickable(row, !busy, cx.listener(move |s, _, _, cx| s.open_recent(i, cx)))
                })));
            }
        }
        body = body.child(div().h(px(1.)).bg(colors.line)).child(
            ui::row()
                .child(ui::label("New catalog", colors))
                .child(ui::clickable(
                    ui::chip("toggle-new", if self.show_new { "Cancel" } else { "Create new…" }, !busy, colors),
                    !busy,
                    cx.listener(|s, _, _, cx| {
                        s.show_new = !s.show_new;
                        cx.notify();
                    }),
                )),
        );
        if self.show_new {
            let name = self.name.read(cx).value();
            body = body
                .child(ui::label("Catalog name", colors))
                .child(Input::new(&self.name).id("new-name"))
                .child(ui::label("Folder (photos root and catalog file location)", colors))
                .child(
                    ui::row()
                        .child(div().flex_1().child(Input::new(&self.folder).id("new-folder")))
                        .child(ui::clickable(
                            ui::chip("browse-folder", "Browse…", !busy, colors),
                            !busy,
                            cx.listener(|s, _, window, cx| s.browse(window, cx)),
                        )),
                )
                .child(ui::sub(
                    format!(
                        "Creates {}.chairphoto inside the chosen folder. Photos you import will be placed under that folder.",
                        if name.is_empty() { "CatalogName".to_string() } else { name.to_string() }
                    ),
                    colors,
                ))
                .child(ui::clickable(
                    ui::primary("create-catalog", if busy { "Creating…" } else { "Create catalog" }, !busy, colors),
                    !busy,
                    cx.listener(|s, _, _, cx| s.create(cx)),
                ));
        }
        if let Some(e) = &self.error {
            body = body.child(ui::error("switcher-error", e.clone(), colors));
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_opened_reads_like_reacts() {
        let now = 1_759_300_000; // 2025-10-01
        assert_eq!(relative_day(now - 60, now), "Today");
        assert_eq!(relative_day(now - 86_400, now), "Yesterday");
        assert_eq!(relative_day(now - 3 * 86_400, now), "3 days ago");
        assert_eq!(relative_day(0, now), "Jan 1, 1970");
        assert_eq!(relative_day(1_700_000_000, now), "Nov 14, 2023");
    }
}
