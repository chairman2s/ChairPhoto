//! `PublishPanel` (publishing.tsx): one photo to an OAuth service — the version (default: the
//! active one), title, description, tags when the service has them (prefilled from the photo's
//! keywords until edited), the album when it has them (the cached list at once, Refresh,
//! "+ New", the last one remembered), Publish. A successful publish records the publication
//! under the module's marker, with the service's URL when it returned a web address, and says
//! so on the status line.
//!
//! Every service call runs on a worker ([`crate::storage::Runner`]); the panel's photo id is
//! bound to the catalog it was read from ([`PublishSubject::catalog`]).

use super::{record_publications, Album, PublishRequest, PublishService, PublishSubject, VersionPicker, NO_ROWS};
use crate::model::AppModel;
use crate::modules::{dialog, ModuleSettings};
use crate::shell::style::Colors;
use crate::shell::ShellState;
use crate::storage::{ui, Runner};
use chairphoto_core::app::AppState;
use chairphoto_model::publishing::{default_album, publication_url};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, App, ClickEvent, Context, Entity, SharedString, Subscription, TestSupportExt as _, Window};
use std::sync::Arc;

/// Settings keys (namespaced `<module id>.` by [`ModuleSettings`]).
pub const ALBUMS_CACHE: &str = "albums_cache";
pub const LAST_ALBUM: &str = "last_album";

pub struct PublishPanel {
    app: AppState,
    model: Entity<AppModel>,
    settings: ModuleSettings,
    marker: SharedString,
    service: Arc<dyn PublishService>,
    pub subject: PublishSubject,
    pub versions: VersionPicker,
    pub title: Entity<InputState>,
    pub description: Entity<TextareaState>,
    pub tags: Entity<TextareaState>,
    /// The user edited Tags: the prefill no longer overwrites it.
    pub tags_touched: bool,
    /// Set while the panel writes the prefill itself, so its own change is not an edit.
    prefilling: bool,
    pub albums: Vec<Album>,
    pub album: String,
    pub new_album: Entity<InputState>,
    /// Clear the new-album field on the next render (it needs the window).
    new_album_cleared: bool,
    pub creating: bool,
    pub busy: bool,
    pub status: String,
    _subscriptions: Vec<Subscription>,
}

impl PublishPanel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        app: AppState,
        model: Entity<AppModel>,
        shell: &Entity<ShellState>,
        settings: ModuleSettings,
        marker: SharedString,
        service: Arc<dyn PublishService>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subject = PublishSubject::take(shell, cx);
        let title = cx.new(|cx| InputState::new(window, cx));
        let description = cx.new(|cx| TextareaState::new(window, cx).rows(2));
        let tags = cx.new(|cx| TextareaState::new(window, cx).rows(2));
        let new_album = cx.new(|cx| InputState::new(window, cx).placeholder("New album name"));
        let subscriptions = vec![
            dialog::close_on_switch(&model, window, cx),
            cx.subscribe(&tags, |this: &mut Self, _, event: &InputEvent, _| {
                if matches!(event, InputEvent::Change) && !this.prefilling {
                    this.tags_touched = true;
                }
            }),
            cx.subscribe(&new_album, |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.create_album(cx);
                }
            }),
        ];
        let mut panel = PublishPanel {
            versions: VersionPicker::new(&subject),
            app,
            model,
            settings,
            marker,
            service,
            subject,
            title,
            description,
            tags,
            tags_touched: false,
            prefilling: false,
            albums: Vec::new(),
            album: String::new(),
            new_album,
            new_album_cleared: false,
            creating: false,
            busy: false,
            status: String::new(),
            _subscriptions: subscriptions,
        };
        VersionPicker::load(&panel.app, &panel.subject, cx, |p: &mut Self, versions, cx| {
            p.versions.versions = versions;
            cx.notify();
        });
        panel.prefill_tags(window, cx);
        panel.load_albums(cx);
        panel
    }

    fn photo(&self) -> Option<i64> {
        self.subject.active.map(|a| a.id)
    }

    /// Tags from the photo's keywords, until the user edits the field.
    fn prefill_tags(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(photo), Some(catalog)) = (self.photo(), self.subject.catalog) else { return };
        if !self.service.has_tags() {
            return;
        }
        let (service, settings) = (self.service.clone(), self.settings.clone());
        let rx = Runner::get(cx).run(move || service.suggest_tags(&settings, catalog, photo));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(tags)) = rx.await else { return };
            this.update_in(cx, |p, window, cx| {
                if p.tags_touched {
                    return;
                }
                p.prefilling = true;
                p.tags.update(cx, |t, cx| t.set_value(tags, window, cx));
                p.prefilling = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The cached album list at once; the service only when there is no cache yet.
    fn load_albums(&mut self, cx: &mut Context<Self>) {
        if !self.service.has_albums() {
            return;
        }
        let (service, settings) = (self.service.clone(), self.settings.clone());
        let rx = Runner::get(cx).run(move || {
            let cached: Vec<Album> = settings
                .get(ALBUMS_CACHE)
                .ok()
                .flatten()
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default();
            let last = settings.get(LAST_ALBUM).ok().flatten();
            if !cached.is_empty() {
                return Ok((cached, last));
            }
            let fetched = service.list_albums(&settings)?;
            cache_albums(&settings, &fetched);
            Ok::<_, String>((fetched, last))
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("Couldn't read the albums".into()));
            this.update(cx, |p, cx| {
                match result {
                    Ok((albums, last)) => {
                        let uris: Vec<&str> = albums.iter().map(|a| a.uri.as_str()).collect();
                        p.album = default_album(&uris, last.as_deref()).unwrap_or_default().to_string();
                        p.albums = albums;
                    }
                    Err(e) => p.status = e,
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Choose an album and remember it as the default for next time.
    pub fn select_album(&mut self, uri: String, cx: &mut Context<Self>) {
        self.album = uri.clone();
        let settings = self.settings.clone();
        Runner::get(cx).spawn(move || {
            if let Err(e) = settings.set(LAST_ALBUM, &uri) {
                eprintln!("publish: couldn't remember the album: {e}");
            }
        });
        cx.notify();
    }

    /// Refresh: re-fetch the albums from the service.
    pub fn refresh_albums(&mut self, cx: &mut Context<Self>) {
        self.status = "Refreshing albums…".into();
        let (service, settings) = (self.service.clone(), self.settings.clone());
        let rx = Runner::get(cx).run(move || {
            let albums = service.list_albums(&settings)?;
            cache_albums(&settings, &albums);
            Ok::<_, String>(albums)
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("Couldn't refresh the albums".into()));
            this.update(cx, |p, cx| {
                match result {
                    Ok(albums) => {
                        let keep = albums.iter().any(|a| a.uri == p.album);
                        let first = albums.first().map(|a| a.uri.clone()).unwrap_or_default();
                        p.albums = albums;
                        p.status.clear();
                        if !keep {
                            p.select_album(first, cx);
                        }
                    }
                    Err(e) => p.status = e,
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// "+ New" / Enter: create an album, put it first and choose it.
    pub fn create_album(&mut self, cx: &mut Context<Self>) {
        let name = self.new_album.read(cx).value().trim().to_string();
        if name.is_empty() || self.creating || !self.service.can_create_album() {
            return;
        }
        self.creating = true;
        self.status = "Creating album…".into();
        let (service, settings) = (self.service.clone(), self.settings.clone());
        let rx = Runner::get(cx).run(move || service.create_album(&settings, &name));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("Couldn't create the album".into()));
            this.update(cx, |p, cx| {
                p.creating = false;
                match result {
                    Ok(album) => {
                        p.albums.retain(|a| a.uri != album.uri);
                        p.albums.insert(0, album.clone());
                        let (settings, albums) = (p.settings.clone(), p.albums.clone());
                        Runner::get(cx).spawn(move || cache_albums(&settings, &albums));
                        p.select_album(album.uri.clone(), cx);
                        p.status = format!("Created \u{201c}{}\u{201d} ✓", album.name);
                        p.new_album_cleared = true;
                    }
                    Err(e) => p.status = e,
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Publish: upload through the service, then record the publication (with the URL when
    /// it is a web address) under the module's marker in the catalog the photo came from.
    pub fn publish(&mut self, cx: &mut Context<Self>) {
        let Some(photo) = self.photo() else { return };
        let Some(catalog) = self.subject.catalog else {
            self.status = NO_ROWS.into();
            cx.notify();
            return;
        };
        if self.busy || (self.service.has_albums() && self.album.is_empty()) {
            return;
        }
        self.busy = true;
        self.status = "Publishing…".into();
        let request = PublishRequest {
            catalog,
            photo_id: photo,
            version_id: self.versions.chosen,
            title: self.title.read(cx).value().trim().to_string(),
            description: self.description.read(cx).value().trim().to_string(),
            album_uri: self.album.clone(),
            tags: if self.service.has_tags() { self.tags.read(cx).value().trim().to_string() } else { String::new() },
        };
        let (service, settings, app, marker) = (self.service.clone(), self.settings.clone(), self.app.clone(), self.marker.clone());
        let version = request.version_id;
        let rx = Runner::get(cx).run(move || {
            let answer = service.publish(&settings, request)?;
            record_publications(&app, catalog, &[(photo, version)], &marker, publication_url(&answer))
        });
        let (model, name) = (self.model.clone(), self.service.name());
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("The publish stopped unexpectedly".into()));
            if result.is_ok() {
                cx.update(|cx| model.update(cx, |m, cx| m.set_status(format!("Published to {name}."), cx)));
            }
            this.update(cx, |p, cx| {
                p.busy = false;
                p.status = match result {
                    Ok(()) => format!("Published to {name} ✓"),
                    Err(e) => e,
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }
}

fn cache_albums(settings: &ModuleSettings, albums: &[Album]) {
    if let Ok(json) = serde_json::to_string(albums) {
        if let Err(e) = settings.set(ALBUMS_CACHE, &json) {
            eprintln!("publish: couldn't cache the albums: {e}");
        }
    }
}

impl Render for PublishPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let body = ui::body().id("publish-panel");
        if self.photo().is_none() {
            return body.child(ui::empty("publish-panel-empty", "Select a photo", colors)).test_support();
        }
        if std::mem::take(&mut self.new_album_cleared) {
            self.new_album.update(cx, |i, cx| i.set_value("", window, cx));
        }
        let this = cx.entity().downgrade();
        let name = self.service.name();
        let mut body = body
            .child(ui::label("Version", colors))
            .child(div().child(self.versions.menu("publish-panel-version", this.clone(), |p: &mut Self, v, _| p.versions.chosen = v)))
            .child(ui::label("Title", colors))
            .child(div().id("publish-panel-title").child(Input::new(&self.title)).test_support())
            .child(ui::label("Description", colors))
            .child(div().id("publish-panel-description").child(Textarea::new(&self.description)).test_support());
        if self.service.has_tags() {
            body = body
                .child(ui::label("Tags", colors))
                .child(div().id("publish-panel-tags").child(Textarea::new(&self.tags)).test_support())
                .child(ui::sub(
                    format!(
                        "Prefilled from the photo's tags. Space-separated; quote multi-word tags (\"northern lights\"). \
                         Shown as real {name} tags, not #hashtags."
                    ),
                    colors,
                ));
        }
        if self.service.has_albums() {
            let chosen = self.albums.iter().find(|a| a.uri == self.album).map_or("(no albums yet)".to_string(), |a| a.name.clone());
            let albums = self.albums.clone();
            let current = self.album.clone();
            let pick_from = this.clone();
            let menu = Button::new("publish-panel-album").outline().small().label(chosen).dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for a in &albums {
                    let (this, uri) = (pick_from.clone(), a.uri.clone());
                    menu = menu.item(PopupMenuItem::new(a.name.clone()).checked(current == a.uri).on_click(
                        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                            this.update(cx, |p, cx| p.select_album(uri.clone(), cx)).ok();
                        },
                    ));
                }
                menu
            });
            body = body.child(ui::label("Album", colors)).child(
                ui::row().child(menu).child(ui::clickable(
                    ui::chip("publish-panel-refresh", "Refresh", true, colors),
                    true,
                    cx.listener(|p, _, _, cx| p.refresh_albums(cx)),
                )),
            );
            if self.service.can_create_album() {
                let can = !self.creating && !self.new_album.read(cx).value().trim().is_empty();
                body = body.child(
                    ui::row()
                        .child(div().w(px(260.)).child(Input::new(&self.new_album)))
                        .child(ui::clickable(
                            ui::chip("publish-panel-new-album", if self.creating { "Creating…" } else { "+ New" }, can, colors),
                            can,
                            cx.listener(|p, _, _, cx| p.create_album(cx)),
                        )),
                );
            }
        }
        let can = !self.busy && !(self.service.has_albums() && self.album.is_empty());
        let label = if self.busy { "Publishing…".to_string() } else { format!("Publish to {name}") };
        body.child(
            ui::row()
                .child(ui::clickable(ui::primary("publish-panel-publish", label, can, colors), can, cx.listener(|p, _, _, cx| p.publish(cx))))
                .child(div().id("publish-panel-status").child(ui::sub(self.status.clone(), colors)).test_support()),
        )
        .test_support()
    }
}
