//! [`PeopleView`]: the Faces module's main view, "People" (`PeopleView` in faces.tsx), drawn
//! from [`People`].
//!
//! - **Tabs** instead of React's one long page: People (the named wall; a card filters the
//!   Library by that person), Clusters (unnamed clusters; a card names it, "Pick" picks it for
//!   "Name together…" — a merge — and "Faces…" opens its face sheet, where picked faces are
//!   named apart — a split — or ignored), and Review suggestions (the queue: the "Confirm all
//!   ≥ X%" slider and per-row ✓ / ✕). Each tab is one virtualised `uniform_list` of uniform
//!   rows, so a catalog with thousands of clusters or suggestions builds only what shows.
//! - **Avatars through the image layer.** A face is cropped from its photo's thumbnail (the
//!   `Thumb` tier, as React's `thumb://`), turned by the photo's user rotation. A cover
//!   version's thumbnail is not the original's frame (#152), so for a photo whose thumbnail
//!   is one the face is cut from its `Preview` tier instead (rv151 L4: the original's frame,
//!   right for a cropped cover and a tone-only one alike; a larger decode, on the pool). The
//!   list's decoration reports the rows on screen; the view's [`ClaimId`] holds exactly their
//!   thumbnails (and those previews) plus an overscan, so what scrolls away — or the whole
//!   view, when the stage leaves it or it is released — is released (queued renders
//!   cancelled).
//! - **The naming dialog** (`NameClusterModal`): a field with a type-ahead over the people
//!   root's tags; Enter saves, Esc cancels (taken before any binding while the field has
//!   focus), a suggestion's click fills the field.

use super::logic::{avatar_placement, name_suggestions, reaches, rotate_box};
use super::people::{NameTarget, People, Tab};
use crate::image_store::{ClaimId, ImageState, ImageStore};
use crate::shell::style::Colors;
use crate::storage::ui;
use chairphoto_core::app::faces::{FaceBboxJson, Verdict};
use chairphoto_core::image_pool::ImageKind;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::prelude::*;
use gpui_kit::{
    div, img, px, AnyElement, App, Bounds, Context, Entity, Focusable as _, FontWeight, Pixels, Point, SharedString,
    Subscription, TestSupportExt as _, UniformListDecoration, UniformListScrollHandle, WeakEntity, Window,
};
use std::ops::Range;

const CARD_W: f32 = 108.;
const CARD_H: f32 = 140.;
const GAP: f32 = 10.;
const ROW_H: f32 = 64.;
/// Rows each side of the visible ones whose thumbnails are held too.
const OVERSCAN: usize = 2;

/// What one list row holds: the photos whose thumbnails it draws, by item.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rows {
    People,
    Clusters,
    Sheet,
    Suggestions,
}

pub struct PeopleView {
    pub people: Entity<People>,
    images: Option<Entity<ImageStore>>,
    claim: Option<ClaimId>,
    scroll: UniformListScrollHandle,
    pub name_input: Entity<InputState>,
    slider: Entity<SliderState>,
    /// Columns the last frame used.
    cols: usize,
    /// The list shown last frame and its photo ids per row (for the decoration).
    rows: (Option<Rows>, Vec<Vec<i64>>),
    /// The naming dialog the field was last cleared for.
    naming_open: bool,
    _subscriptions: Vec<Subscription>,
}

impl PeopleView {
    pub fn new(people: Entity<People>, images: Option<Entity<ImageStore>>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. People/Jane"));
        let threshold = people.read(cx).threshold;
        let slider = cx.new(|_| SliderState::new().min(0.).max(1.).step(0.05).default_value(threshold as f32));
        let claim = images.as_ref().map(|i| i.update(cx, |s, _| s.new_claim()));
        let window_handle = window.window_handle();
        let this = cx.entity().downgrade();
        let subscriptions = vec![
            cx.observe(&people, |this, people, cx| {
                // Off stage: hold no thumbnails.
                if !people.read(cx).visible() {
                    this.release(cx);
                }
                cx.notify();
            }),
            cx.subscribe(&name_input, |this, _, e: &InputEvent, cx| match e {
                InputEvent::PressEnter { .. } => this.confirm_naming(cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            }),
            cx.subscribe_in(&slider, window, |this: &mut Self, _, e: &SliderEvent, _, cx| {
                if let SliderEvent::Change(v) = e {
                    let t = (v.start() as f64 * 20.).round() / 20.;
                    this.people.update(cx, |p, cx| p.set_threshold(t, cx));
                }
            }),
            // Esc in the naming field cancels the dialog, before any binding.
            cx.intercept_keystrokes(move |event, window, cx| {
                if window.window_handle() != window_handle || event.keystroke.key != "escape" {
                    return;
                }
                let Some(this) = this.upgrade() else { return };
                let focused = this.read(cx).name_input.read(cx).focus_handle(cx).is_focused(window);
                if focused && this.read(cx).people.read(cx).naming.is_some() {
                    this.update(cx, |v, cx| v.people.update(cx, |p, cx| p.cancel_naming(cx)));
                    cx.stop_propagation();
                }
            }),
        ];
        cx.on_release(|this: &mut Self, cx| {
            if let (Some(images), Some(claim)) = (this.images.clone(), this.claim) {
                images.update(cx, |s, _| s.drop_claim(claim));
            }
        })
        .detach();
        PeopleView {
            people,
            images,
            claim,
            scroll: UniformListScrollHandle::new(),
            name_input,
            slider,
            cols: 1,
            rows: (None, Vec::new()),
            naming_open: false,
            _subscriptions: subscriptions,
        }
    }

    /// The thumbnails this view holds now (tests).
    pub fn held(&self, cx: &App) -> Vec<i64> {
        let (Some(images), Some(claim)) = (&self.images, self.claim) else { return Vec::new() };
        let mut ids: Vec<i64> = images.read(cx).claim(claim).into_iter().map(|(p, _)| p).collect();
        ids.sort();
        ids
    }

    fn release(&mut self, cx: &mut Context<Self>) {
        if let (Some(images), Some(claim)) = (&self.images, self.claim) {
            images.update(cx, |s, _| s.set_claim(claim, []));
        }
    }

    /// The rows on screen (from the list's decoration, once per frame): hold their
    /// thumbnails and the overscan's, most urgent first, and let go of the rest.
    fn on_visible(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let (Some(images), Some(claim)) = (self.images.clone(), self.claim) else { return };
        let rows = &self.rows.1;
        let n = rows.len();
        let mut order: Vec<usize> = range.clone().filter(|&r| r < n).collect();
        for d in 1..=OVERSCAN {
            if range.end + d - 1 < n {
                order.push(range.end + d - 1);
            }
            if range.start >= d {
                order.push(range.start - d);
            }
        }
        let mut wanted: Vec<(i64, ImageKind)> = Vec::new();
        for r in order {
            for &p in &rows[r] {
                if !wanted.contains(&(p, ImageKind::Thumb)) {
                    wanted.push((p, ImageKind::Thumb));
                }
            }
        }
        images.update(cx, |s, _| {
            // A cover thumbnail is not the original's frame: its avatar is cut from the
            // preview instead (rv151 L4), decoded on the pool like any tier.
            let previews: Vec<(i64, ImageKind)> = wanted
                .iter()
                .filter(|&&(p, _)| matches!(s.peek(p, ImageKind::Thumb), ImageState::Ready(l) if l.cover))
                .map(|&(p, _)| (p, ImageKind::Preview))
                .collect();
            wanted.extend(previews);
            s.set_claim(claim, wanted.iter().copied());
            s.request_batch(&wanted);
        });
    }

    /// What an avatar of `photo` is cut from: its thumbnail, or — when that is the cover
    /// version's render — its preview, the original's frame (rv151 L4). Empty until it lands.
    fn thumb(&self, photo: i64, cx: &mut Context<Self>) -> ImageState {
        match &self.images {
            Some(i) => i.update(cx, |s, _| match s.get(photo, ImageKind::Thumb) {
                ImageState::Ready(l) if l.cover => s.get(photo, ImageKind::Preview),
                other => other,
            }),
            None => ImageState::Absent,
        }
    }

    fn confirm_naming(&mut self, cx: &mut Context<Self>) {
        let typed = self.name_input.read(cx).value().to_string();
        self.people.update(cx, |p, cx| p.confirm_naming(typed, cx));
    }

    fn columns(&self, window: &Window) -> usize {
        let width = self
            .scroll
            .0
            .borrow()
            .last_item_size
            .map(|s| f32::from(s.item.width))
            .unwrap_or_else(|| f32::from(window.viewport_size().width) - 120.);
        (((width + GAP) / (CARD_W + GAP)).floor() as usize).max(1)
    }
}

/// A face cropped from its photo's thumbnail (or preview, [`PeopleView::thumb`]), in a round
/// `size` square. Never from a cover version's render (#152): the version may be cropped or
/// turned, so the face's box, in the original's frame, would cut out something else — the
/// circle stays empty until the original's frame is there.
fn avatar(photo: i64, state: ImageState, bbox: FaceBboxJson, rotation: i64, size: f32, colors: Colors) -> AnyElement {
    let mut d = div().flex_none().size(px(size)).rounded_full().overflow_hidden().relative().bg(colors.elev);
    if let ImageState::Ready(loaded) = state.filter(|l| !l.cover) {
        let s = loaded.image.size(0);
        let natural = (s.width.0 as f32, s.height.0 as f32);
        let b = rotate_box((bbox.x, bbox.y, bbox.w, bbox.h), rotation);
        let (l, t, w, h) = avatar_placement(b, natural, size);
        d = d.child(
            img(loaded.image)
                .id(SharedString::from(format!("faces-avatar-{photo}")))
                .absolute()
                .left(px(l))
                .top(px(t))
                .w(px(w))
                .h(px(h))
                .test_support(),
        );
    }
    d.into_any_element()
}

fn card(id: impl Into<SharedString>, label: impl Into<SharedString>, dashed: bool, picked: bool, colors: Colors) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id.into())
        .flex()
        .flex_col()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .w(px(CARD_W))
        .h(px(CARD_H - GAP))
        .pt(px(10.))
        .px(px(6.))
        .border_1()
        .rounded(px(8.))
        .border_color(if picked { colors.accent } else { colors.border })
        .when(dashed, |d| d.border_dashed())
        .when(picked, |d| d.bg(colors.sel))
        .cursor_pointer()
        .hover(|s| s.bg(colors.elev))
        .aria_label(label.into())
}

fn small(text: impl Into<SharedString>, colors: Colors) -> gpui_kit::Div {
    div().text_size(px(11.)).text_color(colors.dim).text_center().child(text.into())
}

fn plural(n: impl Into<i64>, one: &str, many: &str) -> String {
    let n = n.into();
    format!("{n} {}", if n == 1 { one } else { many })
}

impl PeopleView {
    fn render_tabs(&self, colors: Colors, cx: &mut Context<Self>) -> gpui_kit::Div {
        let p = self.people.read(cx);
        let counts = p.data.as_ref().map(|d| (d.people.len(), d.clusters.len(), d.suggestions.len()));
        let tab = |id: &'static str, label: String, t: Tab| {
            let on = p.tab == t;
            let people = self.people.clone();
            ui::clickable(
                ui::chip(id, label, true, colors).when(on, |c| c.border_color(colors.accent).text_color(colors.txt)),
                true,
                move |_, _, cx| people.update(cx, |p, cx| p.set_tab(t, cx)),
            )
        };
        let n = |i: Option<usize>| i.map(|n| format!(" ({n})")).unwrap_or_default();
        ui::row()
            .px(px(16.))
            .py(px(10.))
            .border_b_1()
            .border_color(colors.border)
            .child(div().font_weight(FontWeight::SEMIBOLD).text_size(px(15.)).text_color(colors.txt).mr(px(8.)).child("People"))
            .child(tab("faces-tab-people", format!("People{}", n(counts.map(|c| c.0))), Tab::People))
            .child(tab("faces-tab-clusters", format!("Unnamed clusters{}", n(counts.map(|c| c.1))), Tab::Clusters))
            .child(tab("faces-tab-suggestions", format!("Review suggestions{}", n(counts.map(|c| c.2))), Tab::Suggestions))
            .child(div().flex_1())
            .when(p.matching(cx), |d| d.child(ui::sub("Face matching is running…", colors)))
            .when(p.loading, |d| d.child(ui::sub("Loading…", colors)))
            .child(ui::clickable(ui::chip("faces-people-refresh", "Refresh", true, colors), true, {
                let people = self.people.clone();
                move |_, _, cx| people.update(cx, |p, cx| p.refresh(cx))
            }))
    }

    /// The rows of the list shown now, and its toolbar.
    fn content(&mut self, window: &mut Window, colors: Colors, cx: &mut Context<Self>) -> (Option<AnyElement>, Option<(Rows, Vec<Vec<i64>>, f32)>, Option<AnyElement>) {
        let cols = self.columns(window);
        if cols != self.cols {
            self.cols = cols;
            cx.notify();
        }
        let p = self.people.read(cx);
        let Some(data) = &p.data else { return (None, None, None) };
        let chunk = |ids: Vec<i64>| -> Vec<Vec<i64>> { ids.chunks(cols).map(|c| c.to_vec()).collect() };
        if let Some(sheet) = &p.sheet {
            let picked = sheet.picked.len();
            let free = !p.busy && !p.matching(cx);
            let people = self.people.clone();
            let bar = ui::row()
                .px(px(16.))
                .py(px(8.))
                .child(ui::clickable(ui::chip("faces-sheet-back", "← Clusters", true, colors), true, {
                    let people = people.clone();
                    move |_, _, cx| people.update(cx, |p, cx| p.close_cluster(cx))
                }))
                .child(ui::sub(
                    match &sheet.faces {
                        None => "Loading…".to_string(),
                        Some(f) => format!("{} — pick faces to name them apart (a split), or ignore them.", plural(f.len() as i64, "face", "faces")),
                    },
                    colors,
                ))
                .child(ui::clickable(
                    ui::primary("faces-sheet-name", if picked == 0 { "Name all…".to_string() } else { format!("Name selected ({picked})…") }, free, colors),
                    free,
                    {
                        let people = people.clone();
                        move |_, _, cx| people.update(cx, |p, cx| p.name_sheet(cx))
                    },
                ))
                .child(ui::clickable(ui::chip("faces-sheet-ignore", format!("Ignore selected ({picked})"), free && picked > 0, colors), free && picked > 0, {
                    move |_, _, cx| people.update(cx, |p, cx| p.ignore_picked(cx))
                }))
                .into_any_element();
            let ids = sheet.faces.as_ref().map(|f| f.iter().map(|f| f.photo_id).collect()).unwrap_or_default();
            return (Some(bar), Some((Rows::Sheet, chunk(ids), CARD_H)), None);
        }
        match p.tab {
            Tab::People => {
                if data.people.is_empty() {
                    let e = ui::empty("faces-people-empty", "No named people yet. Index faces and run matching to populate this view, or name a cluster.", colors);
                    return (None, None, Some(e));
                }
                (None, Some((Rows::People, chunk(data.people.iter().map(|x| x.avatar_photo_id).collect()), CARD_H)), None)
            }
            Tab::Clusters => {
                let picked = p.picked_clusters.len();
                let free = !p.matching(cx);
                let people = self.people.clone();
                let bar = ui::row()
                    .px(px(16.))
                    .py(px(8.))
                    .child(ui::sub("Click a cluster to name the person. Pick several to name them together as one person.", colors))
                    .child(ui::clickable(
                        ui::primary("faces-name-together", format!("Name {picked} together…"), free && picked >= 2, colors),
                        free && picked >= 2,
                        move |_, _, cx| people.update(cx, |p, cx| p.name_picked_clusters(cx)),
                    ))
                    .into_any_element();
                if data.clusters.is_empty() {
                    return (Some(bar), None, Some(ui::empty("faces-clusters-empty", "No unnamed clusters.", colors)));
                }
                (Some(bar), Some((Rows::Clusters, chunk(data.clusters.iter().map(|c| c.avatar_photo_id).collect()), CARD_H)), None)
            }
            Tab::Suggestions => {
                let above = p.above_threshold().len();
                let pct = (p.threshold * 100.).round() as i64;
                let free = !p.busy && !p.matching(cx);
                let people = self.people.clone();
                let bar = ui::row()
                    .px(px(16.))
                    .py(px(8.))
                    .child(ui::label("Confidence threshold:", colors))
                    .child(div().w(px(140.)).child(Slider::new(&self.slider)))
                    .child(div().id("faces-threshold").text_size(px(12.)).text_color(colors.dim).child(format!("{pct}%")).aria_label(format!("{pct}%")).test_support())
                    .child(ui::clickable(
                        ui::primary("faces-confirm-all", format!("Confirm all ≥{pct}% ({above})"), free && above > 0, colors),
                        free && above > 0,
                        move |_, _, cx| people.update(cx, |p, cx| p.confirm_all(cx)),
                    ))
                    .into_any_element();
                if data.suggestions.is_empty() {
                    return (Some(bar), None, Some(ui::empty("faces-suggestions-empty", "No pending suggestions.", colors)));
                }
                (Some(bar), Some((Rows::Suggestions, data.suggestions.iter().map(|s| vec![s.photo_id]).collect(), ROW_H)), None)
            }
        }
    }

    fn render_rows(&mut self, range: Range<usize>, colors: Colors, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(kind) = self.rows.0 else { return Vec::new() };
        let cols = self.cols.max(1);
        let mut out = Vec::new();
        for row in range {
            let mut el = div().id(("faces-row", row as u64)).flex().flex_row().gap(px(GAP)).px(px(16.)).w_full();
            match kind {
                Rows::People | Rows::Clusters | Rows::Sheet => {
                    el = el.h(px(CARD_H));
                    for i in row * cols..(row + 1) * cols {
                        let Some(c) = self.cell(kind, i, colors, cx) else { break };
                        el = el.child(c);
                    }
                }
                Rows::Suggestions => {
                    el = el.h(px(ROW_H));
                    if let Some(c) = self.suggestion(row, colors, cx) {
                        el = el.child(c);
                    }
                }
            }
            out.push(el.into_any_element());
        }
        out
    }

    fn cell(&mut self, kind: Rows, i: usize, colors: Colors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let people = self.people.clone();
        let p = people.read(cx);
        let data = p.data.as_ref()?;
        match kind {
            Rows::People => {
                let x = data.people.get(i)?.clone();
                let thumb = self.thumb(x.avatar_photo_id, cx);
                let tag = x.tag_id;
                Some(
                    card(format!("faces-person-{tag}"), format!("Filter Library to {}", x.full_path), false, false, colors)
                        .child(avatar(x.avatar_photo_id, thumb, x.avatar_bbox, x.avatar_rotation, 72., colors))
                        .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).max_w(px(96.)).truncate().child(x.name.clone()))
                        .child(small(format!("{} · {}", plural(x.photo_count, "photo", "photos"), plural(x.face_count, "face", "faces")), colors))
                        .on_click(move |_, _, cx| people.update(cx, |p, cx| p.filter_by_person(tag, cx)))
                        .test_support()
                        .into_any_element(),
                )
            }
            Rows::Clusters => {
                let c = data.clusters.get(i)?.clone();
                let picked = p.picked_clusters.contains(&c.cluster_id);
                let free = !p.matching(cx);
                let thumb = self.thumb(c.avatar_photo_id, cx);
                let id = c.cluster_id;
                let toggle = {
                    let people = people.clone();
                    div()
                        .id(SharedString::from(format!("faces-cluster-pick-{id}")))
                        .text_size(px(10.5))
                        .text_color(if picked { colors.accent } else { colors.mute })
                        .child(if picked { "✓ Picked" } else { "Pick" })
                        .aria_label(if picked { "Picked" } else { "Pick" })
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            people.update(cx, |p, cx| p.toggle_cluster(id, cx));
                        })
                        .test_support()
                };
                let open = {
                    let people = people.clone();
                    div()
                        .id(SharedString::from(format!("faces-cluster-faces-{id}")))
                        .text_size(px(10.5))
                        .text_color(colors.mute)
                        .child("Faces…")
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            people.update(cx, |p, cx| p.open_cluster(id, cx));
                        })
                        .test_support()
                };
                Some(
                    card(format!("faces-cluster-{id}"), format!("Name this cluster ({})", plural(c.member_count, "face", "faces")), true, picked, colors)
                        .child(avatar(c.avatar_photo_id, thumb, c.avatar_bbox, c.avatar_rotation, 64., colors))
                        .child(small(plural(c.member_count, "face", "faces"), colors))
                        .child(ui::row().gap(px(6.)).child(toggle).child(open))
                        .when(free, |d| d.on_click(move |_, _, cx| people.update(cx, |p, cx| p.name_cluster(id, cx))))
                        .test_support()
                        .into_any_element(),
                )
            }
            Rows::Sheet => {
                let sheet = p.sheet.as_ref()?;
                let f = sheet.faces.as_ref()?.get(i)?.clone();
                let picked = sheet.picked.contains(&f.face_id);
                let thumb = self.thumb(f.photo_id, cx);
                let id = f.face_id;
                Some(
                    card(format!("faces-face-{id}"), if picked { "Picked face" } else { "Face" }, false, picked, colors)
                        .child(avatar(f.photo_id, thumb, f.bbox, f.rotation, 72., colors))
                        .child(small(if picked { "✓ picked" } else { "click to pick" }, colors))
                        .on_click(move |_, _, cx| people.update(cx, |p, cx| p.toggle_face(id, cx)))
                        .test_support()
                        .into_any_element(),
                )
            }
            Rows::Suggestions => None,
        }
    }

    fn suggestion(&mut self, i: usize, colors: Colors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let people = self.people.clone();
        let p = people.read(cx);
        let e = p.data.as_ref()?.suggestions.get(i)?.clone();
        let below = !reaches(e.confidence, p.threshold);
        let free = !p.busy && !p.matching(cx);
        let thumb = self.thumb(e.photo_id, cx);
        let face = e.face_id;
        let pct = (e.confidence * 100.).round() as i64;
        let verdict = |id: String, label: &'static str, color, v: Verdict| {
            let people = people.clone();
            ui::clickable(
                ui::chip(id, label, free, colors).text_color(color).border_color(color),
                free,
                move |_, _, cx| people.update(cx, |p, cx| p.review_one(face, v, cx)),
            )
        };
        Some(
            div()
                .id(SharedString::from(format!("faces-sugg-{face}")))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.))
                .w_full()
                .h(px(ROW_H))
                .border_b_1()
                .border_color(colors.line)
                .when(below, |d| d.opacity(0.55))
                .child(avatar(e.photo_id, thumb, e.bbox, e.rotation, 52., colors))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(colors.ok).truncate().child(e.person_full_path.clone()))
                        .child(small(format!("Confidence: {pct}%"), colors).text_left()),
                )
                .child(verdict(format!("faces-sugg-confirm-{face}"), "✓", colors.ok, Verdict::Confirm))
                .child(verdict(format!("faces-sugg-reject-{face}"), "✕", colors.danger, Verdict::Reject))
                .aria_label(format!("{} {pct}%", e.person_full_path))
                .test_support()
                .into_any_element(),
        )
    }

    fn render_naming(&mut self, window: &mut Window, colors: Colors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = self.people.read(cx);
        let Some(naming) = p.naming.clone() else {
            self.naming_open = false;
            return None;
        };
        let (root, tags) = p.data.as_ref().map(|d| (d.root.clone(), d.people_tags.clone())).unwrap_or_default();
        if !self.naming_open {
            // A fresh dialog: an empty, focused field.
            self.naming_open = true;
            self.name_input.update(cx, |i, cx| {
                i.set_value("", window, cx);
                i.focus(window, cx);
            });
        }
        let typed = self.name_input.read(cx).value().to_string();
        let suggestions = name_suggestions(&tags, &typed);
        let title = match &naming.target {
            NameTarget::Clusters(ids) if ids.len() > 1 => format!("Name {} clusters as one person", ids.len()),
            NameTarget::Clusters(_) => "Name this cluster".to_string(),
            NameTarget::Faces(_) => "Name the selected faces".to_string(),
        };
        let mut list = div().flex().flex_col().border_1().border_color(colors.border).rounded(px(6.));
        for (i, path) in suggestions.iter().enumerate() {
            let path = path.clone();
            let fill = path.clone();
            list = list.child(
                div()
                    .id(SharedString::from(format!("faces-name-suggestion-{i}")))
                    .px(px(10.))
                    .py(px(5.))
                    .text_size(px(12.))
                    .text_color(colors.txt)
                    .cursor_pointer()
                    .hover(|s| s.bg(colors.sel))
                    .child(path.clone())
                    .aria_label(path)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.name_input.update(cx, |i, cx| i.set_value(fill.clone(), window, cx));
                    }))
                    .test_support(),
            );
        }
        let busy = naming.busy;
        let dialog = div()
            .id("faces-name-dialog")
            .flex()
            .flex_col()
            .gap(px(8.))
            .w(px(380.))
            .p(px(20.))
            .bg(colors.panel)
            .border_1()
            .border_color(colors.border)
            .rounded(px(10.))
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(div().font_weight(FontWeight::BOLD).text_size(px(15.)).text_color(colors.txt).child(title))
            .child(ui::sub(
                format!(
                    "{} — type a person name to create or assign an existing tag{}.",
                    plural(naming.faces as i64, "face", "faces"),
                    if root.is_empty() { String::new() } else { format!(" under {root}") }
                ),
                colors,
            ))
            .child(Input::new(&self.name_input).id("faces-name-input"))
            .when(!suggestions.is_empty(), |d| d.child(list))
            .when_some(naming.error.clone(), |d, e| d.child(ui::error("faces-name-error", e, colors)))
            .child(
                ui::row()
                    .child(ui::clickable(ui::primary("faces-name-confirm", if busy { "Saving…" } else { "Confirm" }, !busy, colors), !busy, cx.listener(|this, _, _, cx| this.confirm_naming(cx))))
                    .child(ui::clickable(ui::chip("faces-name-cancel", "Cancel", true, colors), true, {
                        let people = self.people.clone();
                        move |_, _, cx| people.update(cx, |p, cx| p.cancel_naming(cx))
                    })),
            )
            .test_support();
        let people = self.people.clone();
        Some(
            div()
                .id("faces-name-scrim")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(colors.scrim)
                .on_mouse_down(gpui_kit::MouseButton::Left, move |_, _, cx| people.update(cx, |p, cx| p.cancel_naming(cx)))
                .child(dialog)
                .into_any_element(),
        )
    }
}

impl Render for PeopleView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let tabs = self.render_tabs(colors, cx);
        let (bar, list, empty) = self.content(window, colors, cx);
        let error = self.people.read(cx).error.clone();
        let naming = self.render_naming(window, colors, cx);
        let mut root = div().id("faces-people").relative().flex().flex_col().size_full().bg(colors.canvas).child(tabs).children(bar);
        if let Some(e) = error {
            root = root.child(div().px(px(16.)).child(ui::error("faces-people-error", e, colors)));
        }
        match list {
            Some((kind, rows, _)) => {
                if self.rows.0 != Some(kind) {
                    self.scroll.scroll_to_item(0, gpui_kit::ScrollStrategy::Top);
                }
                let count = rows.len();
                self.rows = (Some(kind), rows);
                let list = gpui_kit::uniform_list(
                    "faces-people-list",
                    count,
                    cx.processor(move |this, range: Range<usize>, _window, cx| this.render_rows(range, colors, cx)),
                )
                .track_scroll(&self.scroll)
                .with_decoration(Visible(cx.entity().downgrade()))
                .flex_1()
                .min_h_0()
                .w_full()
                .pt(px(8.));
                // A findable box around the list (headless scroll tests aim at it).
                root = root.child(
                    div().id("faces-people-scroll").flex().flex_col().flex_1().min_h_0().w_full().child(list).test_support(),
                );
            }
            None => {
                // No list draws, so no decoration reports: hold nothing.
                self.rows = (None, Vec::new());
                self.release(cx);
                root = root.children(empty.map(|e| div().px(px(16.)).child(e)));
            }
        }
        root.children(naming).test_support()
    }
}

/// Reports the list's visible rows to the view once per frame (see [`PeopleView::on_visible`]).
struct Visible(WeakEntity<PeopleView>);

impl UniformListDecoration for Visible {
    fn compute(
        &self,
        visible_range: Range<usize>,
        _bounds: Bounds<Pixels>,
        _scroll_offset: Point<Pixels>,
        _item_height: Pixels,
        _item_count: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        self.0.update(cx, |view, cx| view.on_visible(visible_range, cx)).ok();
        div().into_any_element()
    }
}
