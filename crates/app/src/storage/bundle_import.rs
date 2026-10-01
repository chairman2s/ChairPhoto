//! "Import bundle" (`BundleImportDialog.tsx`): a `.chairphoto` path (Browse… previews at
//! once; Enter or Check previews the typed path), the preview (batch label, photos, new,
//! already in the catalog, the no-op notice), then "Import N new" — the background import
//! ([`StorageState::start_bundle_import`]), whose result this dialog shows when it ends.
//!
//! The portal picker has no file-type filter (shell-apis.md § 5), so a picked file that does
//! not end in `.chairphoto` is refused here with a message instead of being opened.

use super::state::{bundle_import_line, StorageEvent};
use super::ui;
use super::{CloseDialog, Runner, StorageState};
use crate::shell::style::Colors;
use chairphoto_core::app::bundles::BundlePreview;
use chairphoto_core::app::{expand_home, AppState};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::TestSupportExt as _;
use gpui_kit::{div, Context, Entity, EventEmitter, PathPromptOptions, Subscription, Window};

pub struct BundleImport {
    app: AppState,
    storage: Entity<StorageState>,
    pub path: Entity<InputState>,
    pub preview: Option<BundlePreview>,
    pub previewing: bool,
    pub importing: bool,
    /// The finished import's line, or `None`.
    pub result: Option<String>,
    pub error: Option<String>,
    /// Bumped by every preview and by every edit of the path; an older preview is dropped.
    preview_seq: u64,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for BundleImport {}

impl BundleImport {
    pub fn new(storage: Entity<StorageState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let app = storage.read(cx).app_state().clone();
        let path = cx.new(|cx| InputState::new(window, cx).placeholder("~/Documents/my-trip.chairphoto"));
        let input = cx.subscribe(&path, |this: &mut Self, _, event: &InputEvent, cx| match event {
            InputEvent::PressEnter { .. } => this.check(None, cx),
            InputEvent::Change => {
                // React cleared the preview and result on every edit.
                this.preview_seq += 1;
                this.preview = None;
                this.previewing = false;
                this.result = None;
                cx.notify();
            }
            _ => {}
        });
        let ended = cx.subscribe(&storage, |this: &mut Self, _, event: &StorageEvent, cx| {
            if let StorageEvent::BundleImported(result) = event {
                if !this.importing {
                    return;
                }
                this.importing = false;
                match result {
                    Ok(r) => this.result = Some(bundle_import_line(r)),
                    Err(e) => this.error = Some(e.clone()),
                }
                cx.notify();
            }
        });
        BundleImport {
            app,
            storage,
            path,
            preview: None,
            previewing: false,
            importing: false,
            result: None,
            error: None,
            preview_seq: 0,
            _subscriptions: vec![input, ended],
        }
    }

    /// Preview the bundle at `path` (default: the field's text).
    pub fn check(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        let path = path.unwrap_or_else(|| self.path.read(cx).value().to_string()).trim().to_string();
        self.preview = None;
        self.result = None;
        self.error = None;
        if path.is_empty() {
            cx.notify();
            return;
        }
        self.preview_seq += 1;
        let seq = self.preview_seq;
        self.previewing = true;
        let state = self.app.clone();
        let rx = Runner::get(cx)
            .run(move || chairphoto_core::app::bundles::preview_bundle(&state, &expand_home(&path)));
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                if s.preview_seq != seq {
                    return;
                }
                s.previewing = false;
                match result {
                    Ok(p) => s.preview = Some(p),
                    Err(e) => s.error = Some(format!("Could not read bundle: {e}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a .chairphoto bundle".into()),
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
                    if path.extension().is_none_or(|e| e != "chairphoto") {
                        s.error = Some(format!("Not a .chairphoto bundle: {text}"));
                        cx.notify();
                        return;
                    }
                    s.path.update(cx, |i, cx| i.set_value(text.clone(), window, cx));
                    s.check(Some(text), cx);
                }
                Ok(None) => {}
                Err(e) => {
                    s.error = Some(format!("Couldn't open the file picker: {e}"));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// "Import N new".
    pub fn run(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.result = None;
        let path = self.path.read(cx).value().trim().to_string();
        if path.is_empty() {
            self.error = Some("Choose a .chairphoto bundle file to import.".into());
            cx.notify();
            return;
        }
        if self.preview.is_none() || self.importing {
            return;
        }
        self.importing = true;
        self.storage.update(cx, |s, cx| s.start_bundle_import(expand_home(&path), cx));
        cx.notify();
    }
}

impl Render for BundleImport {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let has_path = !self.path.read(cx).value().trim().is_empty();
        let mut body = ui::body()
            .id("bundle-import")
            .child(ui::label("Bundle file (.chairphoto)", colors))
            .child(
                ui::row()
                    .child(div().flex_1().child(Input::new(&self.path).id("bundle-path")))
                    .child(ui::clickable(
                        ui::chip("bundle-browse", "Browse…", true, colors),
                        true,
                        cx.listener(|s, _, window, cx| s.browse(window, cx)),
                    ))
                    .child(ui::clickable(
                        ui::chip("bundle-check", if self.previewing { "Checking…" } else { "Check" }, has_path && !self.previewing, colors),
                        has_path && !self.previewing,
                        cx.listener(|s, _, _, cx| s.check(None, cx)),
                    )),
            );
        if let (Some(p), None) = (&self.preview, &self.result) {
            let label = if p.batch_label.is_empty() { "(unnamed)" } else { &p.batch_label };
            let mut counts = format!("{} in bundle · {} new", ui::plural(p.total, "photo", "photos"), p.new_count);
            if p.existing > 0 {
                counts += &format!(" · {} already in catalog", p.existing);
            }
            body = body.child(ui::sub(format!("Batch: {label}"), colors)).child(div().id("bundle-counts").child(ui::sub(counts, colors)).test_support());
            if p.new_count == 0 && p.existing > 0 {
                body = body.child(ui::sub("All photos are already present — re-importing will be a no-op (safe to run).", colors));
            }
        }
        let can_run = !self.importing && has_path && self.preview.is_some();
        let label = match (&self.preview, self.importing) {
            (_, true) => "Importing…".to_string(),
            (Some(p), _) if p.new_count > 0 => format!("Import {} new", p.new_count),
            (Some(_), _) => "Import (no-op)".to_string(),
            (None, _) => "Import bundle".to_string(),
        };
        body = body
            .child(ui::sub(
                "Originals copy into your library under Year/Month/Day. Existing photos (matched by UUID) are never \
                 overwritten — the import is always additive. Progress shows on the bench; the dialog can be closed \
                 once import starts.",
                colors,
            ))
            .child(
                ui::row()
                    .child(ui::clickable(ui::primary("bundle-run", label, can_run, colors), can_run, cx.listener(|s, _, _, cx| s.run(cx))))
                    .when(self.importing, |r| r.child(ui::sub("Runs in the background — progress shows on the bench.", colors))),
            );
        if let Some(r) = &self.result {
            body = body.child(div().id("bundle-result").child(ui::sub(r.clone(), colors)).test_support());
        }
        if let Some(e) = &self.error {
            body = body.child(ui::error("bundle-error", e.clone(), colors));
        }
        body
    }
}
