//! "Import published from Flickr" (`ImportPublishedPanel` in flickr.tsx): Preview fetches the
//! photostream and matches it against the catalog (counts matched / ambiguous / not in catalog,
//! up to 50 matches with both thumbnails); an ambiguous photo is resolved by clicking the
//! catalog file that matches (Undo takes it back); "Import N publications" records them with
//! Flickr's historical upload dates. Never writes to Flickr.
//!
//! The preview is bound to the catalog it matched against, and so is the import: a catalog
//! switch clears the panel and drops any answer in flight, and the import of a preview from
//! the catalog before fails closed (`CATALOG_CHANGED`) — its ids mean other photos now. Every
//! fetch and write runs on a worker; Flickr's thumbnails are fetched only after a Preview,
//! from Flickr's image hosts.

use crate::image_store::{ClaimId, ImageState, ImageStore};
use crate::model::{AppModel, AppModelEvent};
use crate::modules::{ModuleHost, ModuleSettings, SETTINGS_NOT_READY};
use crate::shell::style::Colors;
use crate::storage::{ui, Runner};
use chairphoto_core::app::flickr::{import_apply, import_preview, FlickrApi, FlickrImportMatch, FlickrImportResult, ImportApplied};
use chairphoto_core::app::{AppState, CatalogIdentity, CoreEvent};
use chairphoto_core::image_pool::ImageKind;
use gpui_kit::prelude::*;
use gpui_kit::{div, img, px, AnyElement, Context, Entity, Image, ImageFormat, ObjectFit, SharedString, Subscription, TestSupportExt as _, Window};
use std::collections::HashMap;
use std::sync::Arc;

/// What the panel is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    Fetching,
    Importing,
}

pub struct ImportPublishedPanel {
    app: AppState,
    model: Entity<AppModel>,
    settings: ModuleSettings,
    api: Arc<dyn FlickrApi>,
    marker: String,
    images: Option<Entity<ImageStore>>,
    claim: Option<ClaimId>,
    pub busy: Option<Busy>,
    /// The last preview and the catalog it matched against.
    pub preview: Option<(CatalogIdentity, FlickrImportResult)>,
    /// Ambiguous photos resolved by hand, in the order they were resolved.
    pub resolved: Vec<FlickrImportMatch>,
    pub status: String,
    /// Flickr's small thumbnails, by URL.
    remote: HashMap<String, Arc<Image>>,
    /// Bumped by a catalog switch and a new Preview: older answers are dropped.
    generation: u64,
    _subscriptions: Vec<Subscription>,
}

impl ImportPublishedPanel {
    pub fn new(host: &ModuleHost, api: Arc<dyn FlickrApi>, cx: &mut Context<Self>) -> Self {
        let model = host.model().clone();
        let images = host.images().cloned();
        let mut subs = vec![cx.subscribe(&model, |this: &mut Self, model, event: &AppModelEvent, cx| match event {
            AppModelEvent::Core(CoreEvent::CatalogSwitched(_)) => this.catalog_switched(cx),
            AppModelEvent::CatalogRead => {
                if let Some(open) = model.read(cx).catalog_identity().filter(|&o| Some(o) != this.settings.catalog()) {
                    this.settings = this.settings.rebound(open);
                }
            }
            _ => {}
        })];
        let claim = images.as_ref().map(|i| {
            subs.push(cx.observe(i, |_, _, cx| cx.notify()));
            i.update(cx, |s, _| s.new_claim())
        });
        ImportPublishedPanel {
            app: model.read(cx).state().clone(),
            model,
            settings: host.settings(),
            api,
            marker: host.meta().marker().to_string(),
            images,
            claim,
            busy: None,
            preview: None,
            resolved: Vec::new(),
            status: String::new(),
            remote: HashMap::new(),
            generation: 0,
            _subscriptions: subs,
        }
    }

    fn catalog_switched(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.busy = None;
        self.clear(cx);
        self.status.clear();
        cx.notify();
    }

    /// Forget the preview, the resolutions and the thumbnails they held.
    fn clear(&mut self, cx: &mut Context<Self>) {
        self.preview = None;
        self.resolved.clear();
        self.remote.clear();
        self.hold_thumbs(cx);
    }

    /// Run `work` on a worker; `land` its answer unless a switch or a newer Preview came first.
    fn run<R: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce() -> R + Send + 'static,
        land: impl FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
    ) {
        let generation = self.generation;
        let rx = Runner::get(cx).run(work);
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |p, cx| {
                if p.generation == generation {
                    land(p, result, cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Preview: fetch the photostream and match it, read-only, against the catalog this
    /// panel's settings are bound to.
    pub fn preview(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let Some(from) = self.settings.catalog() else {
            self.status = SETTINGS_NOT_READY.into();
            cx.notify();
            return;
        };
        self.generation += 1;
        self.clear(cx);
        self.busy = Some(Busy::Fetching);
        self.status = "Fetching photostream…".into();
        let (api, settings, app, marker) = (self.api.clone(), self.settings.clone(), self.app.clone(), self.marker.clone());
        self.run(
            cx,
            move || import_preview(&*api, &settings, &app, Some(from), &marker),
            move |p, result, cx| {
                p.busy = None;
                match result {
                    Ok(preview) => {
                        p.status = format!(
                            "{} matched · {} ambiguous · {} not in catalog",
                            preview.matched_count, preview.ambiguous_count, preview.unmatched_count
                        );
                        p.preview = Some((from, preview));
                        p.hold_thumbs(cx);
                        p.fetch_remote(cx);
                    }
                    Err(e) => p.status = e,
                }
            },
        );
    }

    /// The catalog thumbnails the preview shows, held while it is up.
    fn hold_thumbs(&mut self, cx: &mut Context<Self>) {
        let (Some(images), Some(claim)) = (self.images.clone(), self.claim) else { return };
        let mut wanted: Vec<(i64, ImageKind)> = Vec::new();
        if let Some((_, preview)) = &self.preview {
            let ids = preview
                .matches
                .iter()
                .map(|m| m.catalog_id)
                .chain(preview.ambiguous.iter().flat_map(|a| a.candidates.iter().map(|c| c.catalog_id)));
            for id in ids {
                if !wanted.contains(&(id, ImageKind::Thumb)) {
                    wanted.push((id, ImageKind::Thumb));
                }
            }
        }
        images.update(cx, |s, _| {
            s.set_claim(claim, wanted.iter().copied());
            s.request_batch(&wanted);
        });
    }

    /// Flickr's small thumbnails for what the preview shows.
    fn fetch_remote(&mut self, cx: &mut Context<Self>) {
        let Some((_, preview)) = &self.preview else { return };
        let mut urls: Vec<String> = Vec::new();
        let shown = preview.matches.iter().map(|m| &m.thumb_url).chain(preview.ambiguous.iter().map(|a| &a.thumb_url));
        for url in shown.flatten() {
            if !urls.contains(url) {
                urls.push(url.clone());
            }
        }
        if urls.is_empty() {
            return;
        }
        let api = self.api.clone();
        self.run(
            cx,
            move || urls.into_iter().filter_map(|u| api.thumbnail(&u).ok().map(|b| (u, b))).collect::<Vec<_>>(),
            |p, fetched, _| {
                for (url, bytes) in fetched {
                    p.remote.insert(url, Arc::new(Image::from_bytes(ImageFormat::Jpeg, bytes)));
                }
            },
        );
    }

    /// Resolve the ambiguous Flickr photo `flickr_id` to catalog photo `catalog_id` (one of
    /// its candidates), replacing an earlier choice for it.
    pub fn resolve(&mut self, flickr_id: &str, catalog_id: i64, cx: &mut Context<Self>) {
        let Some((_, preview)) = &self.preview else { return };
        let Some(a) = preview.ambiguous.iter().find(|a| a.flickr_id == flickr_id) else { return };
        let Some(c) = a.candidates.iter().find(|c| c.catalog_id == catalog_id) else { return };
        let entry = a.resolve(c);
        self.resolved.retain(|m| m.flickr_id != flickr_id);
        self.resolved.push(entry);
        cx.notify();
    }

    /// Undo: take a resolution back.
    pub fn unresolve(&mut self, flickr_id: &str, cx: &mut Context<Self>) {
        self.resolved.retain(|m| m.flickr_id != flickr_id);
        cx.notify();
    }

    /// The full plan an Import records: every match (uncapped) plus the resolved ones.
    pub fn full_plan(&self) -> Vec<FlickrImportMatch> {
        let matched = self.preview.as_ref().map(|(_, p)| p.plan.clone()).unwrap_or_default();
        matched.into_iter().chain(self.resolved.iter().cloned()).collect()
    }

    /// Import: record the plan into the catalog the preview matched against.
    pub fn apply(&mut self, cx: &mut Context<Self>) {
        let Some((from, _)) = &self.preview else { return };
        let from = *from;
        let plan = self.full_plan();
        if plan.is_empty() || self.busy.is_some() {
            return;
        }
        self.busy = Some(Busy::Importing);
        self.status = "Importing…".into();
        let (app, marker) = (self.app.clone(), self.marker.clone());
        self.run(
            cx,
            move || import_apply(&app, Some(from), &marker, &plan),
            |p, result: Result<ImportApplied, String>, cx| {
                p.busy = None;
                match result {
                    Ok(done) => {
                        p.clear(cx);
                        let toast = format!("Imported {} Flickr {}.", done.applied, if done.applied == 1 { "publication" } else { "publications" });
                        p.status = match done.failed.first() {
                            None => format!("Done — {} recorded.", ui::plural(done.applied, "publication", "publications")),
                            Some(first) => format!(
                                "Done — {} recorded; {} could not be ({first}).",
                                ui::plural(done.applied, "publication", "publications"),
                                done.failed.len()
                            ),
                        };
                        p.model.update(cx, |m, cx| {
                            m.set_status(toast, cx);
                            // The inspector's Published-to panel and the facets re-read.
                            m.refresh(cx);
                        });
                    }
                    Err(e) => p.status = e,
                }
            },
        );
    }

    fn thumb(&self, catalog_id: i64, size: f32, colors: Colors, cx: &Context<Self>) -> AnyElement {
        let state = self.images.as_ref().map_or(ImageState::Absent, |i| i.read(cx).peek(catalog_id, ImageKind::Thumb));
        let inner = match state {
            ImageState::Ready(l) => img(l.image.clone()).size_full().object_fit(ObjectFit::Cover).into_any_element(),
            _ => div().size_full().bg(colors.well).into_any_element(),
        };
        div().size(px(size)).flex_none().overflow_hidden().rounded(px(4.)).child(inner).into_any_element()
    }

    fn flickr_thumb(&self, url: &Option<String>, title: &str, size: f32, colors: Colors) -> AnyElement {
        let d = div().size(px(size)).flex_none().overflow_hidden().rounded(px(4.)).bg(colors.well);
        match url.as_ref().and_then(|u| self.remote.get(u)) {
            Some(image) => d.child(img(image.clone()).size_full().object_fit(ObjectFit::Cover)).into_any_element(),
            None => d
                .flex()
                .items_center()
                .justify_center()
                .p(px(4.))
                .text_size(px(9.))
                .text_color(colors.mute)
                .child(if title.is_empty() { "no preview".to_string() } else { title.to_string() })
                .into_any_element(),
        }
    }
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

const THUMB: f32 = 120.;
const MATCH_THUMB: f32 = 56.;

impl Render for ImportPublishedPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let count = self.full_plan().len();
        let fetching = self.busy == Some(Busy::Fetching);
        let can_preview = self.busy.is_none();
        let can_apply = self.busy.is_none() && self.preview.is_some() && count > 0;
        let apply_label = match (self.busy, &self.preview) {
            (Some(Busy::Importing), _) => "Importing…".to_string(),
            (_, Some(_)) => format!("Import {}", ui::plural(count, "publication", "publications")),
            _ => "Import".to_string(),
        };
        let mut body = ui::body()
            .id("flickr-import")
            .child(ui::label("Import published from Flickr", colors))
            .child(ui::sub(
                "Matches photos in your Flickr photostream to the local catalog by capture time and title, then records \
                 them as publications with their real historical upload dates. Never writes to Flickr.",
                colors,
            ))
            .child(
                ui::row()
                    .child(ui::clickable(
                        ui::chip("flickr-import-preview", if fetching { "Fetching…" } else { "Preview" }, can_preview, colors),
                        can_preview,
                        cx.listener(|p, _, _, cx| p.preview(cx)),
                    ))
                    .child(ui::clickable(
                        ui::primary("flickr-import-apply", apply_label, can_apply, colors),
                        can_apply,
                        cx.listener(|p, _, _, cx| p.apply(cx)),
                    )),
            );
        if !self.status.is_empty() {
            body = body.child(div().id("flickr-import-status").child(ui::sub(self.status.clone(), colors)).test_support());
        }
        let Some((_, preview)) = &self.preview else { return body.test_support() };

        if !preview.matches.is_empty() {
            let mut list = div().id("flickr-import-matches").flex().flex_col().gap(px(4.)).max_h(px(340.)).overflow_y_scroll();
            for m in &preview.matches {
                let url = m.flickr_url.clone();
                list = list.child(
                    ui::row()
                        .child(self.flickr_thumb(&m.thumb_url, "", MATCH_THUMB, colors))
                        .child(div().text_color(colors.mute).child("→"))
                        .child(self.thumb(m.catalog_id, MATCH_THUMB, colors, cx))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .min_w_0()
                                .child(div().text_size(px(10.)).text_color(colors.mute).child(crate::inspector::publication_date(m.published_at)))
                                .child(
                                    div()
                                        .id(SharedString::from(format!("flickr-import-match-{}", m.flickr_id)))
                                        .text_size(px(11.))
                                        .text_color(colors.accent)
                                        .cursor_pointer()
                                        .child(file_name(&m.catalog_path))
                                        .on_click(move |_, _, cx| cx.open_url(&url)),
                                ),
                        ),
                );
            }
            body = body.child(ui::sub("Matches — Flickr photo → your catalog file (up to 50 shown):", colors)).child(list);
        }

        if !self.resolved.is_empty() {
            let mut list = div().flex().flex_col().gap(px(6.));
            for m in &self.resolved {
                let flickr_id = m.flickr_id.clone();
                list = list.child(
                    ui::row()
                        .child(self.flickr_thumb(&m.thumb_url, "", MATCH_THUMB, colors))
                        .child(div().text_color(colors.mute).child("→"))
                        .child(self.thumb(m.catalog_id, MATCH_THUMB, colors, cx))
                        .child(div().flex_1().min_w_0().text_size(px(11.)).child(file_name(&m.catalog_path)))
                        .child(ui::clickable(
                            ui::chip(SharedString::from(format!("flickr-import-undo-{}", m.flickr_id)), "Undo", true, colors),
                            true,
                            cx.listener(move |p, _, _, cx| p.unresolve(&flickr_id, cx)),
                        )),
                );
            }
            body = body
                .child(div().text_size(px(11.5)).text_color(colors.ok).child(format!("Manually resolved ({}) — Flickr photo → your catalog file:", self.resolved.len())))
                .child(list);
        }

        if !preview.ambiguous.is_empty() {
            let mut list = div().id("flickr-import-ambiguous").flex().flex_col().gap(px(12.)).max_h(px(560.)).overflow_y_scroll();
            for a in &preview.ambiguous {
                let chosen = self.resolved.iter().find(|m| m.flickr_id == a.flickr_id);
                let url = a.flickr_url.clone();
                let mut item = div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .pl(px(8.))
                    .border_l_2()
                    .border_color(colors.rating)
                    .child(div().text_size(px(9.)).text_color(colors.accent).child("ON FLICKR"))
                    .child(
                        ui::row()
                            .child(self.flickr_thumb(&a.thumb_url, &a.title, THUMB, colors))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .min_w_0()
                                    .gap(px(2.))
                                    .child(div().text_size(px(12.)).child(if a.title.is_empty() { "untitled".to_string() } else { a.title.clone() }))
                                    .child(div().text_size(px(10.)).text_color(colors.mute).child(crate::inspector::publication_date(a.published_at)))
                                    .child(div().text_size(px(10.)).text_color(colors.mute).child(a.reason.clone()))
                                    .child(
                                        div()
                                            .id(SharedString::from(format!("flickr-import-open-{}", a.flickr_id)))
                                            .text_size(px(11.))
                                            .text_color(colors.accent)
                                            .cursor_pointer()
                                            .child("Open on Flickr")
                                            .on_click(move |_, _, cx| cx.open_url(&url)),
                                    ),
                            ),
                    );
                if let Some(m) = chosen {
                    let flickr_id = a.flickr_id.clone();
                    item = item.child(
                        ui::row()
                            .child(div().text_size(px(11.)).text_color(colors.ok).child(format!("✓ {}", file_name(&m.catalog_path))))
                            .child(ui::clickable(
                                ui::chip(SharedString::from(format!("flickr-import-reopen-{}", a.flickr_id)), "Undo", true, colors),
                                true,
                                cx.listener(move |p, _, _, cx| p.unresolve(&flickr_id, cx)),
                            )),
                    );
                } else if !a.candidates.is_empty() {
                    let mut cards = ui::row().items_start();
                    for c in &a.candidates {
                        let (flickr_id, catalog_id) = (a.flickr_id.clone(), c.catalog_id);
                        let when = c.capture_time.clone().map_or("no time".to_string(), |t| t.replace('T', " "));
                        cards = cards.child(
                            div()
                                .id(SharedString::from(format!("flickr-import-candidate-{}-{}", a.flickr_id, c.catalog_id)))
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                .p(px(4.))
                                .w(px(THUMB + 8.))
                                .rounded(px(6.))
                                .border_1()
                                .border_color(colors.border)
                                .cursor_pointer()
                                .hover(|s| s.border_color(colors.accent))
                                .child(self.thumb(c.catalog_id, THUMB, colors, cx))
                                .child(div().text_size(px(9.)).text_color(colors.dim).overflow_hidden().whitespace_nowrap().text_ellipsis().child(file_name(&c.catalog_path)))
                                .child(div().text_size(px(9.)).text_color(colors.mute).child(when))
                                .on_click(cx.listener(move |p, _, _, cx| p.resolve(&flickr_id, catalog_id, cx)))
                                .test_support(),
                        );
                    }
                    item = item.child(div().text_size(px(9.)).text_color(colors.mute).child("IN YOUR CATALOG — CLICK THE FILE THAT MATCHES")).child(cards);
                }
                list = list.child(item);
            }
            body = body.child(div().text_size(px(11.5)).text_color(colors.rating).child("Ambiguous — pick the right file or skip (up to 50 shown):")).child(list);
        }
        body.test_support()
    }
}
