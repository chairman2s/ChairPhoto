//! A module's card in the pop-out loupe (#110): `LoupeCard` in `registry.ts`,
//! `LoupeCardView.tsx`, and the owned card in `host.ts` (`showInLoupe`).
//!
//! A module puts a card up with [`ModuleHost::show_in_loupe`](crate::modules::ModuleHost::show_in_loupe)
//! (the Tag graph mirrors its inspector there); while one is up the pop-out shows it instead
//! of the photo. The card is pure data and names a photo scope rather than photos, so it stays
//! small; [`CardView`] pages the scope itself.
//!
//! **Ownership** ([`ShellState::show_loupe_card`]): only the module that put a card up takes
//! it down (`None` from another module leaves it alone, as host.ts did), disabling that module
//! takes it down, and so does a catalog switch, since the scope's ids belong to the catalog
//! that closed. The wall's reads are bound to the catalog the card was shown under
//! (`with_catalog_as`), so a read that a switch overtakes fails closed.
//!
//! **The wall** shows "N photos" and 48 tiles a page ("Show more (N left)"); a tile opens the
//! photo full-size (a [`ZoomImage`]) with "← {title}" (or Esc) back to the wall. The full-size
//! photo's tiers are held by the view's own image claim and given back when it leaves the
//! photo or the card goes.

use crate::image_store::{ClaimId, ImageState, ImageStore};
use crate::keymap::contexts;
use crate::loupe::zoom::ZoomImage;
use crate::model::AppModel;
use crate::shell::state::ShellState;
use crate::shell::style::Colors;
use crate::storage::ui;
use chairphoto_core::app::{with_catalog_as, AppState, CatalogIdentity};
use chairphoto_core::catalog::{Photo, PhotoQuery, PhotoWindow};
use chairphoto_core::image_pool::ImageKind;
use gpui_kit::prelude::*;
use gpui_kit::{
    actions, div, img, px, App, Context, Entity, FocusHandle, Hsla, ObjectFit, SharedString, Subscription,
    TestSupportExt as _, Window,
};

/// The wall's page size (`PAGE` in LoupeCardView.tsx).
pub const PAGE: usize = 48;

actions!(
    loupe_card,
    [
        /// Esc on a card's full-size photo: back to the wall.
        BackToWall,
    ]
);

/// What a module can show in the pop-out loupe in place of the photo (`LoupeCard`).
#[derive(Debug, Clone, PartialEq)]
pub struct LoupeCard {
    /// The headline: a tag's leaf name, a camera model.
    pub title: SharedString,
    /// Under the title: a tag's full path, say.
    pub subtitle: Option<SharedString>,
    /// The title dot's colour.
    pub color: Option<Hsla>,
    pub chips: Vec<SharedString>,
    /// `(label, value)`.
    pub stats: Vec<(SharedString, SharedString)>,
    /// Related entries (co-occurring tags), strongest first.
    pub related: Vec<Related>,
    /// The photos to show as a wall.
    pub photos: Option<CardScope>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Related {
    pub label: SharedString,
    pub detail: Option<SharedString>,
    pub color: Option<Hsla>,
}

/// A card's photo scope: filters `list_photos` understands (`LoupeCardScope`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CardScope {
    /// A tag and its descendants.
    Tag(i64),
    /// An exact camera model.
    Camera(String),
}

impl CardScope {
    fn query(&self, offset: usize) -> PhotoQuery {
        let mut q = PhotoQuery { window: Some(PhotoWindow::new(offset, PAGE)), ..Default::default() };
        match self {
            CardScope::Tag(id) => q.tag_id = Some(*id),
            CardScope::Camera(model) => q.camera = Some(model.clone()),
        }
        q
    }
}

/// The card up in the pop-out, with its owner and the catalog it was shown under.
#[derive(Debug, Clone, PartialEq)]
pub struct ShownCard {
    pub module: SharedString,
    pub card: LoupeCard,
    pub from: Option<CatalogIdentity>,
}

/// The wall loaded for one scope.
#[derive(Default)]
struct Wall {
    key: Option<(CardScope, Option<CatalogIdentity>)>,
    photos: Vec<Photo>,
    total: usize,
    loading: bool,
    /// Bumped by every new scope: a page read for an older one is dropped.
    generation: u64,
}

/// Renders the shell's card (see the module docs).
pub struct CardView {
    app: AppState,
    shell: Entity<ShellState>,
    images: Entity<ImageStore>,
    wall: Wall,
    viewing: Option<i64>,
    zoom: Entity<ZoomImage>,
    claim: ClaimId,
    focus: FocusHandle,
    released: bool,
    _observers: Vec<Subscription>,
}

impl CardView {
    pub fn new(
        model: &Entity<AppModel>,
        shell: Entity<ShellState>,
        images: Entity<ImageStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let zoom = cx.new(|cx| ZoomImage::new(images.clone(), "loupe-card-image", cx));
        let claim = images.update(cx, |s, _| s.new_claim());
        let _observers =
            vec![cx.observe(&shell, |this, _, cx| this.sync(cx)), cx.observe(&images, |_, _, cx| cx.notify())];
        let mut view = CardView {
            app: model.read(cx).state().clone(),
            shell,
            images,
            wall: Wall::default(),
            viewing: None,
            zoom,
            claim,
            focus: cx.focus_handle(),
            released: false,
            _observers,
        };
        view.sync(cx);
        view
    }

    /// The scope's photos loaded so far and the total (tests).
    pub fn wall(&self) -> (Vec<i64>, usize) {
        (self.wall.photos.iter().map(|p| p.id).collect(), self.wall.total)
    }

    /// The handle a full-size card photo takes key focus with.
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn viewing(&self) -> Option<i64> {
        self.viewing
    }

    /// Follow the shell's card: a new scope (or catalog) starts its wall over.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let key = if self.released {
            None
        } else {
            self.shell.read(cx).loupe_card().and_then(|c| c.card.photos.clone().map(|s| (s, c.from)))
        };
        if key == self.wall.key {
            cx.notify();
            return;
        }
        self.view(None, cx);
        let generation = self.wall.generation + 1;
        self.wall = Wall { key: key.clone(), generation, ..Wall::default() };
        if key.is_some() {
            self.load_more(cx);
        }
        cx.notify();
    }

    /// Read the next page of the scope, off the UI thread, bound to the card's catalog.
    pub fn load_more(&mut self, cx: &mut Context<Self>) {
        let Some((scope, from)) = self.wall.key.clone() else { return };
        if self.wall.loading {
            return;
        }
        let Some(from) = from else {
            self.wall.total = 0;
            return;
        };
        self.wall.loading = true;
        let (generation, offset, app) = (self.wall.generation, self.wall.photos.len(), self.app.clone());
        let read = cx.background_executor().spawn(async move {
            let query = scope.query(offset);
            with_catalog_as(&app, from, |c| Ok((c.list_photos(&query)?, c.count_photos(&query)?)))
        });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |v, cx| {
                if v.wall.generation != generation {
                    return;
                }
                v.wall.loading = false;
                match result {
                    Ok((page, total)) => {
                        v.wall.photos.extend(page);
                        v.wall.total = total;
                    }
                    Err(e) => eprintln!("loupe card: {e}"),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open `photo` full-size, or go back to the wall (`None`). The photo left gives back its
    /// tiers unless another view holds them.
    pub fn view(&mut self, photo: Option<i64>, cx: &mut Context<Self>) {
        if self.viewing == photo {
            return;
        }
        let left = std::mem::replace(&mut self.viewing, photo);
        let claim = self.claim;
        self.images.update(cx, |s, cx| {
            match photo {
                Some(id) => {
                    s.set_claim(claim, [(id, ImageKind::Preview), (id, ImageKind::Zoom)]);
                    s.request(id, ImageKind::Preview);
                }
                None => s.set_claim(claim, []),
            }
            if let Some(left) = left {
                s.evict(|k| k.kind == ImageKind::Zoom && k.photo == left, cx);
            }
        });
        self.zoom.update(cx, |z, cx| z.set_photo(photo, cx));
        cx.notify();
    }

    /// The window closed: give back the full-size photo and the claim.
    pub fn release(&mut self, cx: &mut Context<Self>) {
        if self.released {
            return;
        }
        self.released = true;
        self.sync(cx);
        let claim = self.claim;
        self.images.update(cx, |s, _| s.drop_claim(claim));
    }

    fn render_viewing(&mut self, id: i64, card: &LoupeCard, colors: Colors, window: &mut Window, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        if !self.focus.contains_focused(window, cx) {
            self.focus.focus(window, cx);
        }
        let name = self
            .wall
            .photos
            .iter()
            .find(|p| p.id == id)
            .map(|p| crate::loupe::view::file_name(&p.path))
            .unwrap_or_default();
        div()
            .id("loupe-card-viewing")
            .key_context(contexts::LOUPE_CARD)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &BackToWall, _, cx| this.view(None, cx)))
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(8.))
                    .h(px(40.))
                    .px(px(12.))
                    .border_b_1()
                    .border_color(colors.border)
                    .child(ui::clickable(
                        ui::chip("loupe-card-back", format!("← {}", card.title), true, colors),
                        true,
                        cx.listener(|this, _, _, cx| this.view(None, cx)),
                    ))
                    .child(div().text_size(px(12.)).text_color(colors.dim).child(name)),
            )
            .child(div().flex_1().min_h_0().child(self.zoom.clone()))
            .into_any_element()
    }
}

fn chip(text: SharedString, colors: Colors) -> gpui_kit::Div {
    div()
        .px(px(8.))
        .py(px(2.))
        .rounded(px(10.))
        .bg(colors.panel)
        .text_size(px(11.))
        .text_color(colors.dim)
        .child(text)
}

impl Render for CardView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let Some(card) = self.shell.read(cx).loupe_card().map(|c| c.card.clone()) else {
            return div().id("loupe-card").into_any_element();
        };
        if let Some(id) = self.viewing {
            return self.render_viewing(id, &card, colors, window, cx);
        }
        let mut head = div().flex().flex_col().gap(px(8.)).child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(18.))
                .text_color(colors.txt)
                .children(card.color.map(|c| div().size(px(10.)).rounded_full().bg(c)))
                .child(
                    div().id("loupe-card-title").child(card.title.clone()).aria_label(card.title.clone()).test_support(),
                ),
        );
        if let Some(sub) = &card.subtitle {
            head = head.child(div().text_size(px(12.)).text_color(colors.mute).child(sub.clone()));
        }
        if !card.chips.is_empty() {
            head = head.child(div().flex().flex_wrap().gap(px(6.)).children(card.chips.iter().map(|c| chip(c.clone(), colors))));
        }
        if !card.stats.is_empty() {
            head = head.child(div().flex().gap(px(16.)).children(card.stats.iter().map(|(label, value)| {
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_size(px(16.)).text_color(colors.txt).child(value.clone()))
                    .child(div().text_size(px(10.)).text_color(colors.mute).child(label.clone()))
            })));
        }
        if !card.related.is_empty() {
            head = head
                .child(div().text_size(px(10.)).text_color(colors.mute).child("CONNECTED"))
                .child(div().flex().flex_wrap().gap(px(6.)).children(card.related.iter().map(|r| {
                    let mut c = div().flex().items_center().gap(px(4.));
                    if let Some(color) = r.color {
                        c = c.child(div().size(px(7.)).rounded_full().bg(color));
                    }
                    c = c.child(r.label.clone());
                    if let Some(d) = &r.detail {
                        c = c.child(div().text_color(colors.mute).child(d.clone()));
                    }
                    chip("".into(), colors).child(c)
                })));
        }
        let mut root = div()
            .id("loupe-card")
            .size_full()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(16.))
            .overflow_y_scroll()
            .child(head);
        if card.photos.is_some() {
            let (count, total, loading) = (self.wall.photos.len(), self.wall.total, self.wall.loading);
            let wall_head = if total > 0 {
                format!("{total} photo{}", if total == 1 { "" } else { "s" })
            } else if loading {
                "Loading…".into()
            } else {
                "No photos".into()
            };
            let ids: Vec<i64> = self.wall.photos.iter().map(|p| p.id).collect();
            let cells = self.images.update(cx, |s, _| {
                let wanted: Vec<_> = ids.iter().map(|&id| (id, ImageKind::Thumb)).collect();
                s.request_batch(&wanted);
                ids.iter().map(|&id| s.peek(id, ImageKind::Thumb)).collect::<Vec<_>>()
            });
            let mut wall = div().flex().flex_wrap().gap(px(6.));
            for (&id, state) in ids.iter().zip(cells) {
                let tile = div()
                    .id(SharedString::from(format!("loupe-card-tile-{id}")))
                    .w(px(120.))
                    .h(px(120.))
                    .rounded(px(6.))
                    .overflow_hidden()
                    .bg(colors.panel)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.view(Some(id), cx)))
                    .test_support();
                wall = wall.child(match state {
                    ImageState::Ready(l) => tile.child(img(l.image).size_full().object_fit(ObjectFit::Cover)),
                    _ => tile,
                });
            }
            root = root
                .child(
                    div()
                        .id("loupe-card-wall-head")
                        .text_size(px(10.))
                        .text_color(colors.mute)
                        .child(wall_head.clone())
                        .aria_label(wall_head)
                        .test_support(),
                )
                .child(wall);
            if count < total {
                let label = if loading { "Loading…".to_string() } else { format!("Show more ({} left)", total - count) };
                root = root.child(ui::clickable(
                    ui::chip("loupe-card-more", label, !loading, colors),
                    !loading,
                    cx.listener(|this, _, _, cx| this.load_more(cx)),
                ));
            }
        }
        root.into_any_element()
    }
}

/// Whether the shell's card should show: one is up and its module is still enabled.
pub fn shown(shell: &Entity<ShellState>, modules: &Entity<crate::modules::ModuleRegistry>, cx: &App) -> bool {
    shell.read(cx).loupe_card().is_some_and(|c| modules.read(cx).is_enabled(&c.module))
}
