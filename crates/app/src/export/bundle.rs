//! "Export batch as bundle" (`BundleExportDialog.tsx`): the batch's label; a destination
//! folder (typed, or Browse… through the portal picker); a filename defaulting to the label
//! (`.chairphoto` added when missing); "Export bundle" — an owned background job
//! ([`ExportState::start_bundle`]) whose progress shows on the bench and whose result this
//! dialog shows when it ends: exported, skipped offline, failed. Cancel stops it before its
//! next photo and leaves nothing at the destination.
//!
//! Bound to the catalog the batch list came from: the export runs under that identity and the
//! dialog closes on a switch.

use super::state::{bundle_line, ExportEvent, ExportState};
use crate::albums::state::{bind_dialog, AlbumsState};
use crate::shell::style::Colors;
use crate::storage::{ui, CloseDialog};
use chairphoto_core::app::{expand_home, CatalogIdentity, ExportKind};
use chairphoto_core::bundle::writer::BundleWriteResult;
use chairphoto_core::catalog::ImportBatch;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{div, Context, Entity, EventEmitter, PathPromptOptions, Subscription, Window};
use std::path::PathBuf;

/// The default filename: the label's last path segment with anything but `[A-Za-z0-9._-]`
/// replaced by `_`, plus `.chairphoto` (React's).
pub fn default_filename(label: &str) -> String {
    let last = label.rsplit('/').next().unwrap_or(label);
    let safe: String =
        last.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect();
    format!("{}.chairphoto", if safe.is_empty() { "export" } else { &safe })
}

/// Where the bundle goes: `folder/name`, with `.chairphoto` added when missing.
pub fn bundle_path(folder: &str, name: &str) -> PathBuf {
    let name = if name.ends_with(".chairphoto") { name.to_string() } else { format!("{name}.chairphoto") };
    expand_home(folder).join(name)
}

pub struct BundleExport {
    exports: Entity<ExportState>,
    from: CatalogIdentity,
    pub batch: ImportBatch,
    pub dest: Entity<InputState>,
    pub filename: Entity<InputState>,
    pub busy: bool,
    pub result: Option<BundleWriteResult>,
    pub error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for BundleExport {}

impl BundleExport {
    pub fn new(
        albums: &Entity<AlbumsState>,
        exports: Entity<ExportState>,
        from: CatalogIdentity,
        batch: ImportBatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let dest = cx.new(|cx| InputState::new(window, cx).placeholder("~/Documents"));
        let name = default_filename(&crate::shell::title_bar::batch_label(&batch));
        let filename = cx.new(|cx| InputState::new(window, cx).placeholder("my-trip.chairphoto").default_value(name));
        let mut subs = bind_dialog(albums, Some(from), |s| s.lists_from(), cx);
        subs.push(cx.subscribe(&exports, |this: &mut Self, _, event: &ExportEvent, cx| {
            if let ExportEvent::BundleEnded(result) = event {
                if !this.busy {
                    return;
                }
                this.busy = false;
                match result {
                    Ok(r) => this.result = Some(r.clone()),
                    Err(e) => this.error = Some(e.clone()),
                }
                cx.notify();
            }
        }));
        BundleExport { exports, from, batch, dest, filename, busy: false, result: None, error: None, _subscriptions: subs }
    }

    pub fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a destination folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let picked = match rx.await {
                Ok(Ok(Some(paths))) => Ok(paths.into_iter().next()),
                Ok(Err(e)) => Err(e.to_string()),
                _ => Ok(None),
            };
            this.update_in(cx, |s, window, cx| match picked {
                Ok(Some(path)) => {
                    let text = path.to_string_lossy().to_string();
                    s.dest.update(cx, |i, cx| i.set_value(text, window, cx));
                }
                Ok(None) => {}
                Err(e) => {
                    s.error = Some(format!("Couldn't open the folder picker: {e}"));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Export bundle.
    pub fn run(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.result = None;
        let folder = self.dest.read(cx).value().trim().to_string();
        let file = self.filename.read(cx).value().trim().to_string();
        if folder.is_empty() {
            self.error = Some("Choose a destination folder.".into());
        } else if file.is_empty() {
            self.error = Some("Enter a filename.".into());
        }
        if self.error.is_some() || self.busy {
            cx.notify();
            return;
        }
        self.busy = true;
        let (batch, dest, from) = (self.batch.id, bundle_path(&folder, &file), self.from);
        self.exports.update(cx, |e, cx| e.start_bundle(batch, dest, from, cx));
        cx.notify();
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        self.exports.update(cx, |e, cx| e.cancel_kind(ExportKind::Bundle, cx));
    }
}

impl Render for BundleExport {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let label = crate::shell::title_bar::batch_label(&self.batch);
        let mut body = ui::body()
            .id("bundle-export")
            .child(ui::label("Batch", colors))
            .child(ui::sub(label, colors))
            .child(ui::label("Destination folder", colors))
            .child(
                ui::row().child(div().flex_1().child(Input::new(&self.dest).id("bundle-export-dest"))).child(ui::clickable(
                    ui::chip("bundle-export-browse", "Browse…", true, colors),
                    true,
                    cx.listener(|s, _, window, cx| s.browse(window, cx)),
                )),
            )
            .child(ui::label("Bundle filename", colors))
            .child(Input::new(&self.filename).id("bundle-export-name"))
            .child(ui::sub(
                "Exports the batch's RAW originals, XMP sidecars, and catalog metadata (ratings, tags, versions) into a \
                 single .chairphoto zip. Originals that are offline/missing are skipped but their metadata travels.",
                colors,
            ))
            .child(
                ui::row()
                    .child(ui::clickable(
                        ui::primary("bundle-export-run", if self.busy { "Exporting…" } else { "Export bundle" }, !self.busy, colors),
                        !self.busy,
                        cx.listener(|s, _, _, cx| s.run(cx)),
                    ))
                    .when(self.busy, |r| {
                        r.child(ui::sub("Progress shows on the bench.", colors)).child(ui::clickable(
                            ui::chip("bundle-export-cancel", "Cancel", true, colors),
                            true,
                            cx.listener(|s, _, _, cx| s.cancel(cx)),
                        ))
                    }),
            );
        if let Some(r) = &self.result {
            body = body.child(div().id("bundle-export-result").child(ui::sub(bundle_line(r), colors)));
        }
        if let Some(e) = &self.error {
            body = body.child(ui::error("bundle-export-error", e.clone(), colors));
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_filename_comes_from_the_labels_last_segment() {
        assert_eq!(default_filename("/media/card/DCIM/100CANON"), "100CANON.chairphoto");
        assert_eq!(default_filename("My trip (2026)"), "My_trip__2026_.chairphoto");
        assert_eq!(default_filename(""), "export.chairphoto");
    }

    #[test]
    fn the_extension_is_added_once() {
        assert_eq!(bundle_path("/x", "a"), PathBuf::from("/x/a.chairphoto"));
        assert_eq!(bundle_path("/x", "a.chairphoto"), PathBuf::from("/x/a.chairphoto"));
    }
}
