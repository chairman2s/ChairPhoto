//! "Export N photo(s)" (`ExportPanel.tsx`): one-way export to a folder. Preset Hand-off (RAW
//! + XMP) or Show off (JPEG; the hint names the version it renders); a destination (default
//! `~/Pictures/Export`, typed, not persisted — React had no picker); "Reach hashtags": a tag
//! group and an optional limit, a live preview, **Copy** to the OS clipboard, and
//! `hashtags.txt` written with the export. "Export as bundle…" when an import batch is the
//! scope. Export runs as an owned background job ([`ExportState::start_photos`]) whose result
//! this dialog shows when it ends; Cancel stops it before its next photo.
//!
//! Bound to the catalog the photos were read from (the rows'): the tag groups, the preview and
//! the export run under that identity, and the dialog closes on a switch.

use super::state::{export_line, ExportEvent, ExportState};
use crate::albums::state::{bind_dialog, run_bound, AlbumsState};
use crate::shell::style::Colors;
use crate::storage::{ui, CloseDialog};
use chairphoto_core::app::exports::ExportRequest;
use chairphoto_core::app::{expand_home, CatalogIdentity, ExportKind};
use chairphoto_core::catalog::{ImportBatch, TagGroup};
use chairphoto_core::export::{ExportPreset, ExportResult};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, ClipboardItem, Context, Entity, EventEmitter, Subscription, Window};

/// React's default destination.
pub const DEFAULT_DEST: &str = "~/Pictures/Export";

/// The dialog asks its opener to open the bundle export for this batch.
#[derive(Debug, Clone)]
pub struct ExportBatch(pub ImportBatch);

/// What the export renders for Show off: the version active in the inspector.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveVersion {
    pub id: i64,
    pub name: String,
}

pub struct ExportPanel {
    albums: Entity<AlbumsState>,
    exports: Entity<ExportState>,
    from: CatalogIdentity,
    pub photo_ids: Vec<i64>,
    pub version: Option<ActiveVersion>,
    pub batch: Option<ImportBatch>,
    pub preset: ExportPreset,
    pub dest: Entity<InputState>,
    pub groups: Vec<TagGroup>,
    pub group: Option<i64>,
    pub limit: Entity<InputState>,
    /// The hashtag preview for the chosen group and limit.
    pub hashtags: Vec<String>,
    preview_seq: u64,
    pub copied: bool,
    pub busy: bool,
    pub result: Option<ExportResult>,
    pub error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for ExportPanel {}
impl EventEmitter<ExportBatch> for ExportPanel {}

impl ExportPanel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        albums: Entity<AlbumsState>,
        exports: Entity<ExportState>,
        from: CatalogIdentity,
        photo_ids: Vec<i64>,
        version: Option<ActiveVersion>,
        batch: Option<ImportBatch>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let dest = cx.new(|cx| InputState::new(window, cx).placeholder(DEFAULT_DEST).default_value(DEFAULT_DEST));
        let limit = cx.new(|cx| InputState::new(window, cx).placeholder("limit"));
        let mut subs = bind_dialog(&albums, Some(from), |s| s.rows_from(), cx);
        subs.push(cx.subscribe(&limit, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.preview(cx);
            }
        }));
        subs.push(cx.subscribe(&exports, |this: &mut Self, _, event: &ExportEvent, cx| match event {
            ExportEvent::PhotosEnded(result) if this.busy => {
                this.busy = false;
                match result {
                    Ok(r) => this.result = Some(r.clone()),
                    Err(e) => this.error = Some(e.clone()),
                }
                cx.notify();
            }
            // A switch closes the dialog through `bind_dialog`.
            _ => {}
        }));
        run_bound(&albums, from, cx, false, |c| c.list_tag_groups(), |s: &mut Self, r, cx| {
            if let Ok(groups) = r {
                s.groups = groups;
                cx.notify();
            }
        });
        ExportPanel {
            albums,
            exports,
            from,
            photo_ids,
            version,
            batch,
            preset: ExportPreset::HandOff,
            dest,
            groups: Vec::new(),
            group: None,
            limit,
            hashtags: Vec::new(),
            preview_seq: 0,
            copied: false,
            busy: false,
            result: None,
            error: None,
            _subscriptions: subs,
        }
    }

    /// The limit field as a number; empty or unparsable is no limit (React's `limitNum`).
    pub fn limit_value(&self, cx: &gpui_kit::App) -> Option<usize> {
        self.limit.read(cx).value().trim().parse::<usize>().ok()
    }

    pub fn set_preset(&mut self, preset: ExportPreset, cx: &mut Context<Self>) {
        self.preset = preset;
        cx.notify();
    }

    pub fn set_group(&mut self, group: Option<i64>, cx: &mut Context<Self>) {
        self.group = group;
        self.preview(cx);
    }

    /// Re-read the hashtag preview for the group and limit; an older read is dropped.
    fn preview(&mut self, cx: &mut Context<Self>) {
        self.copied = false;
        self.preview_seq += 1;
        let seq = self.preview_seq;
        let Some(group) = self.group else {
            self.hashtags.clear();
            cx.notify();
            return;
        };
        let limit = self.limit_value(cx);
        let albums = self.albums.clone();
        run_bound(&albums, self.from, cx, false, move |c| c.assemble_hashtag_bundle(group, limit), move |s: &mut Self, r, cx| {
            if s.preview_seq == seq {
                s.hashtags = r.unwrap_or_default();
                cx.notify();
            }
        });
        cx.notify();
    }

    /// Copy: the preview, space-separated, onto the OS clipboard.
    pub fn copy(&mut self, cx: &mut Context<Self>) {
        if self.hashtags.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(self.hashtags.join(" ")));
        self.copied = true;
        cx.notify();
    }

    /// Export: hand the request to the background job.
    pub fn run(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.result = None;
        let dest = self.dest.read(cx).value().trim().to_string();
        if dest.is_empty() {
            self.error = Some("Choose a destination folder.".into());
            cx.notify();
            return;
        }
        if self.busy || self.photo_ids.is_empty() {
            return;
        }
        self.busy = true;
        let request = ExportRequest {
            photo_ids: self.photo_ids.clone(),
            preset: self.preset,
            dest_dir: expand_home(&dest),
            hashtag_group_id: self.group,
            hashtag_limit: self.limit_value(cx),
            version_id: self.version.as_ref().map(|v| v.id),
        };
        let from = self.from;
        self.exports.update(cx, |e, cx| e.start_photos(request, from, cx));
        cx.notify();
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.exports.update(cx, |e, cx| e.cancel_kind(ExportKind::Photos, cx));
    }
}

fn radio(id: &'static str, label: &'static str, on: bool, colors: Colors) -> gpui_kit::Stateful<gpui_kit::Div> {
    ui::chip(id, if on { format!("● {label}") } else { format!("○ {label}") }, true, colors)
        .when(on, |c| c.text_color(colors.txt).border_color(colors.accent))
}

impl Render for ExportPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let n = self.photo_ids.len();
        let mut body = ui::body()
            .id("export-panel")
            .child(div().id("export-title").child(ui::sub(format!("Export {n} photo(s)"), colors)))
            .child(ui::label("Preset", colors))
            .child(
                ui::row()
                    .child(ui::clickable(
                        radio("export-handoff", "Hand-off (RAW + XMP)", self.preset == ExportPreset::HandOff, colors),
                        true,
                        cx.listener(|s, _, _, cx| s.set_preset(ExportPreset::HandOff, cx)),
                    ))
                    .child(ui::clickable(
                        radio("export-showoff", "Show off (JPEG)", self.preset == ExportPreset::ShowOff, colors),
                        true,
                        cx.listener(|s, _, _, cx| s.set_preset(ExportPreset::ShowOff, cx)),
                    )),
            );
        if self.preset == ExportPreset::ShowOff {
            let hint = match &self.version {
                Some(v) => format!("Renders the “{}” version at full resolution.", v.name),
                None => "Renders the full-resolution original (no edit selected) at full resolution.".into(),
            };
            body = body.child(div().id("export-hint").child(ui::sub(hint, colors)));
        }
        let this = cx.entity().downgrade();
        let groups = self.groups.clone();
        let group_label = self
            .group
            .and_then(|g| self.groups.iter().find(|x| x.id == g))
            .map_or("None".to_string(), |g| g.name.clone());
        let group_menu = Button::new("export-group").outline().small().label(group_label).dropdown_menu(move |menu: PopupMenu, _, _| {
            let pick = |id: Option<i64>| {
                let this = this.clone();
                move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                    this.update(cx, |p, cx| p.set_group(id, cx)).ok();
                }
            };
            let menu = menu.item(PopupMenuItem::new("None").on_click(pick(None)));
            groups.iter().fold(menu, |menu, g| menu.item(PopupMenuItem::new(g.name.clone()).on_click(pick(Some(g.id)))))
        });
        body = body
            .child(ui::label("Destination", colors))
            .child(Input::new(&self.dest).id("export-dest"))
            .child(ui::label("Reach hashtags (optional)", colors))
            .child(
                ui::row()
                    .child(group_menu)
                    .when(self.group.is_some(), |r| r.child(div().w(px(90.)).child(Input::new(&self.limit).small()))),
            );
        if !self.hashtags.is_empty() {
            body = body.child(
                ui::row()
                    .child(div().id("export-hashtags").flex_1().child(ui::sub(self.hashtags.join(" "), colors)))
                    .child(ui::clickable(
                        ui::chip("export-copy", if self.copied { "Copied" } else { "Copy" }, true, colors),
                        true,
                        cx.listener(|s, _, _, cx| s.copy(cx)),
                    )),
            );
        }
        let can_run = !self.busy && n > 0;
        body = body.child(ui::sub("Written as hashtags.txt alongside the export.", colors)).child(
            ui::row()
                .child(ui::clickable(
                    ui::primary("export-run", if self.busy { "Exporting…" } else { "Export" }, can_run, colors),
                    can_run,
                    cx.listener(|s, _, _, cx| s.run(cx)),
                ))
                .when(self.busy, |r| {
                    r.child(ui::clickable(ui::chip("export-cancel", "Cancel", true, colors), true, cx.listener(|s, _, _, cx| s.cancel(cx))))
                }),
        );
        if let Some(batch) = self.batch.clone() {
            let label = crate::shell::title_bar::batch_label(&batch);
            body = body
                .child(ui::label("Bundle export", colors))
                .child(ui::row().child(ui::clickable(
                    ui::chip("export-as-bundle", "Export as bundle…", true, colors),
                    true,
                    // The opener closes this dialog and opens the bundle export.
                    cx.listener(move |_, _, _, cx| cx.emit(ExportBatch(batch.clone()))),
                )))
                .child(ui::sub(
                    format!(
                        "Packages the \"{label}\" batch's originals, XMP sidecars, and metadata into a single .chairphoto \
                         file that can be imported on another machine."
                    ),
                    colors,
                ));
        }
        if let Some(r) = &self.result {
            body = body.child(div().id("export-result").child(ui::sub(export_line(r), colors)));
        }
        if let Some(e) = &self.error {
            body = body.child(ui::error("export-error", e.clone(), colors));
        }
        body
    }
}
