//! "Import from card" (`ImportPanel.tsx`): pick a card/source folder (Browse… scans at once),
//! preview every photo on it — each flagged `dup` when the library already holds it — pick
//! which to import ("Select all new" / "Select all" / "Clear"; the new ones start selected),
//! name the import, and hand off to the background import ([`StorageState::start_card_import`]),
//! which shows its progress on the bench and can be cancelled there.
//!
//! The listing (`scans::list_card_photos`) and every thumbnail (`thumbnails::thumbnail_bytes`,
//! at most [`THUMB_WORKERS`] at once, as React's `cardThumbnail` limit of 6) run on the
//! [`Runner`]; a newer listing drops an older one's results. The last source is kept in the
//! catalog setting `import.lastSource`, as React did.

use super::ui;
use super::{CloseDialog, Runner, StorageState};
use crate::shell::style::Colors;
use chairphoto_core::app::{expand_home, with_catalog, AppState};
use chairphoto_core::scanner::CardPhoto;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::TestSupportExt as _;
use gpui_kit::{
    div, img, px, Context, Entity, EventEmitter, Image, ImageFormat, ObjectFit, PathPromptOptions, SharedString,
    Subscription, Window,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

/// Settings key for the last-used source folder.
pub const LAST_SOURCE: &str = "import.lastSource";
/// Card thumbnails decoding at once.
pub const THUMB_WORKERS: usize = 6;

pub struct ImportPanel {
    app: AppState,
    storage: Entity<StorageState>,
    pub source: Entity<InputState>,
    pub name: Entity<InputState>,
    pub library_root: String,
    pub error: Option<String>,
    pub cards: Option<Vec<CardPhoto>>,
    pub scanning: bool,
    pub selected: HashSet<String>,
    thumbs: HashMap<String, Arc<Image>>,
    /// Bumped by every listing; results of an older one are dropped.
    listing: u64,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for ImportPanel {}

impl ImportPanel {
    pub fn new(storage: Entity<StorageState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let app = storage.read(cx).app_state().clone();
        let source = cx.new(|cx| InputState::new(window, cx).placeholder("/run/media/you/SDCARD/DCIM"));
        let name = cx.new(|cx| {
            InputState::new(window, cx).placeholder("e.g. Tønsberg 2026-06 (defaults to the card folder)")
        });
        let enter = cx.subscribe(&source, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.scan(None, cx);
            }
        });
        let mut this = ImportPanel {
            app,
            storage,
            source,
            name,
            library_root: String::new(),
            error: None,
            cards: None,
            scanning: false,
            selected: HashSet::new(),
            thumbs: HashMap::new(),
            listing: 0,
            _subscriptions: vec![enter],
        };
        this.restore(window, cx);
        this
    }

    /// The last-used source and the library root (where files land).
    fn restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.app.clone();
        let rx = Runner::get(cx).run(move || {
            with_catalog(&state, |c| Ok((c.get_setting(LAST_SOURCE)?, c.root().to_string_lossy().to_string())))
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok((last, root))) = rx.await else { return };
            this.update_in(cx, |s, window, cx| {
                s.library_root = root;
                if let Some(last) = last.filter(|l| !l.is_empty()) {
                    s.source.update(cx, |i, cx| i.set_value(last, window, cx));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// List the photos on the source folder; pre-select the new ones.
    pub fn scan(&mut self, source: Option<String>, cx: &mut Context<Self>) {
        let source = source.unwrap_or_else(|| self.source.read(cx).value().to_string()).trim().to_string();
        if source.is_empty() {
            self.error = Some("Choose the card / source folder to import from.".into());
            cx.notify();
            return;
        }
        self.error = None;
        self.scanning = true;
        self.cards = None;
        self.thumbs.clear();
        self.listing += 1;
        let listing = self.listing;
        let state = self.app.clone();
        let path = expand_home(&source);
        let rx = Runner::get(cx).run(move || chairphoto_core::app::scans::list_card_photos(&state, &path));
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                if s.listing != listing {
                    return;
                }
                s.scanning = false;
                match result {
                    Ok(list) => {
                        s.selected = list.iter().filter(|c| !c.is_duplicate).map(|c| c.path.clone()).collect();
                        s.load_thumbs(&list, cx);
                        s.cards = Some(list);
                    }
                    Err(e) => s.error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn load_thumbs(&mut self, list: &[CardPhoto], cx: &mut Context<Self>) {
        let listing = self.listing;
        let queue = Arc::new(Mutex::new(list.iter().map(|c| c.path.clone()).collect::<VecDeque<_>>()));
        let runner = Runner::get(cx);
        for _ in 0..THUMB_WORKERS.min(list.len()) {
            let queue = queue.clone();
            let (tx, mut rx) = futures::channel::mpsc::unbounded::<(String, Result<Vec<u8>, String>)>();
            runner.spawn(move || loop {
                let Some(path) = queue.lock().unwrap().pop_front() else { return };
                let bytes = chairphoto_core::thumbnails::thumbnail_bytes(std::path::Path::new(&path));
                if tx.unbounded_send((path, bytes)).is_err() {
                    return; // the panel is gone
                }
            });
            cx.spawn(async move |this, cx| {
                use futures::StreamExt as _;
                while let Some((path, bytes)) = rx.next().await {
                    let Ok(bytes) = bytes else { continue };
                    let keep = this
                        .update(cx, |s, cx| {
                            if s.listing != listing {
                                return false;
                            }
                            s.thumbs.insert(path, Arc::new(Image::from_bytes(ImageFormat::Jpeg, bytes)));
                            cx.notify();
                            true
                        })
                        .unwrap_or(false);
                    if !keep {
                        return;
                    }
                }
            })
            .detach();
        }
    }

    pub fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose the card / source folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let picked = match rx.await {
                Ok(Ok(Some(paths))) => Ok(paths.into_iter().next()),
                Ok(Ok(None)) => Ok(None),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Ok(None),
            };
            this.update_in(cx, |s, window, cx| match picked {
                Ok(Some(path)) => {
                    let path = path.to_string_lossy().to_string();
                    s.source.update(cx, |i, cx| i.set_value(path.clone(), window, cx));
                    s.scan(Some(path), cx);
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

    pub fn toggle(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.selected.remove(path) {
            self.selected.insert(path.to_string());
        }
        cx.notify();
    }

    pub fn select_new(&mut self, cx: &mut Context<Self>) {
        self.selected = self.cards.iter().flatten().filter(|c| !c.is_duplicate).map(|c| c.path.clone()).collect();
        cx.notify();
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selected = self.cards.iter().flatten().map(|c| c.path.clone()).collect();
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.selected.clear();
        cx.notify();
    }

    /// "Import N": remember the source, hand off to the background import, close.
    pub fn run(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        if self.selected.is_empty() {
            self.error = Some("Select at least one photo to import.".into());
            cx.notify();
            return;
        }
        let source = self.source.read(cx).value().trim().to_string();
        let name = self.name.read(cx).value().trim().to_string();
        let state = self.app.clone();
        let remember = source.clone();
        Runner::get(cx).spawn(move || {
            let _ = with_catalog(&state, |c| c.set_setting(LAST_SOURCE, &remember));
        });
        // In the card's own order, as React's `[...selected]` kept insertion order.
        let selected: Vec<String> =
            self.cards.iter().flatten().filter(|c| self.selected.contains(&c.path)).map(|c| c.path.clone()).collect();
        let path = expand_home(&source);
        self.storage.update(cx, |s, cx| s.start_card_import(path, name, selected, cx));
        cx.emit(CloseDialog);
    }
}

impl Render for ImportPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let mut body = ui::body()
            .id("import-panel")
            .child(ui::label("Card / source folder", colors))
            .child(
                ui::row()
                    .child(div().flex_1().child(Input::new(&self.source).id("import-source")))
                    .child(ui::clickable(
                        ui::chip("import-browse", "Browse…", true, colors),
                        true,
                        cx.listener(|s, _, window, cx| s.browse(window, cx)),
                    ))
                    .child(ui::clickable(
                        ui::chip("import-scan", if self.scanning { "Scanning…" } else { "Scan" }, !self.scanning, colors),
                        !self.scanning,
                        cx.listener(|s, _, _, cx| s.scan(None, cx)),
                    )),
            );
        if let Some(cards) = &self.cards {
            let total = cards.len();
            let new = cards.iter().filter(|c| !c.is_duplicate).count();
            body = body.child(
                ui::row()
                    .child(
                        div().id("import-counts").child(ui::sub(
                            format!(
                                "{} · {new} new · {} already imported · {} selected",
                                ui::plural(total, "photo", "photos"),
                                total - new,
                                self.selected.len()
                            ),
                            colors,
                        )).test_support(),
                    )
                    .child(div().flex_1())
                    .child(ui::clickable(ui::chip("select-new", "Select all new", true, colors), true, cx.listener(|s, _, _, cx| s.select_new(cx))))
                    .child(ui::clickable(ui::chip("select-all", "Select all", true, colors), true, cx.listener(|s, _, _, cx| s.select_all(cx))))
                    .child(ui::clickable(ui::chip("select-clear", "Clear", true, colors), true, cx.listener(|s, _, _, cx| s.clear(cx)))),
            );
            if total == 0 {
                body = body.child(ui::empty("no-card-photos", "No photos found in that folder.", colors));
            } else {
                body = body.child(
                    div()
                        .id("card-grid")
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(6.))
                        .max_h(px(320.))
                        .overflow_y_scroll()
                        .children(cards.iter().enumerate().map(|(i, c)| {
                            let sel = self.selected.contains(&c.path);
                            let path = c.path.clone();
                            let thumb = self.thumbs.get(&c.path).cloned();
                            let cell = div()
                                .id(SharedString::from(format!("card-{i}")))
                                .relative()
                                .w(px(112.))
                                .h(px(92.))
                                .rounded(px(6.))
                                .overflow_hidden()
                                .bg(colors.well)
                                .border_2()
                                .border_color(if sel { colors.accent } else { colors.border })
                                .when(c.is_duplicate, |d| d.opacity(0.6))
                                .cursor_pointer()
                                .child(match thumb {
                                    Some(image) => img(image).size_full().object_fit(ObjectFit::Cover).into_any_element(),
                                    None => div().size_full().into_any_element(),
                                })
                                .when(sel, |d| {
                                    d.child(div().absolute().top(px(3.)).left(px(5.)).text_color(colors.accent).child("✓"))
                                })
                                .when(c.is_duplicate, |d| {
                                    d.child(
                                        div().absolute().top(px(3.)).right(px(5.)).text_size(px(10.)).text_color(colors.mute).child("dup"),
                                    )
                                })
                                .child(
                                    div()
                                        .absolute()
                                        .bottom_0()
                                        .left_0()
                                        .right_0()
                                        .px(px(4.))
                                        .bg(colors.scrim)
                                        .text_size(px(10.))
                                        .text_color(colors.txt)
                                        .truncate()
                                        .child(c.name.clone()),
                                );
                            ui::clickable(cell, true, cx.listener(move |s, _, _, cx| s.toggle(&path, cx)))
                        })),
                );
            }
        }
        let n = self.selected.len();
        let can_run = self.cards.is_some() && n > 0;
        body = body
            .child(ui::label("Import name (optional)", colors))
            .child(Input::new(&self.name).id("import-name"))
            .child(ui::sub(
                format!(
                    "Files copy into {} under Year/Month/Day, then queue for NAS backup. Change the library folder in Preferences → Storage.",
                    if self.library_root.is_empty() { "the library" } else { &self.library_root }
                ),
                colors,
            ))
            .child(
                ui::row()
                    .child(ui::clickable(
                        ui::primary("import-run", if n > 0 { format!("Import {n}") } else { "Import".into() }, can_run, colors),
                        can_run,
                        cx.listener(|s, _, _, cx| s.run(cx)),
                    ))
                    .child(ui::sub("Runs in the background — progress shows on the bench, where it can be cancelled.", colors)),
            );
        if let Some(e) = &self.error {
            body = body.child(ui::error("import-error", e.clone(), colors));
        }
        body
    }
}
