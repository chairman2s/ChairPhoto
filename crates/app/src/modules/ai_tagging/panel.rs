//! The inspector's "AI tags" panel (`AiPanel` in aiTagging.tsx): engine and model pickers
//! (Ollama's models live, the curated cloud lists, a saved custom model kept), Suggest tags,
//! ▭ Region (drag a box on the preview; a tap under 2 % is discarded), "Suggest for N
//! selected" with the bulk cloud cost confirm, the suggestions grouped by provenance (✓ add,
//! ✓ all (N), ✗ reject, ↳ more specific; accept or reject a propagated group; Re-run
//! directly), and the follow-up question (Enter asks). Every run and write is
//! [`AiState`]'s.

use super::logic::{self, group_suggestions, is_tap, model_options, provider_short, region_between};
use super::state::{AiState, PhotoView};
use crate::image_store::{ClaimId, ImageState, ImageStore};
use crate::shell::style::Colors;
use crate::storage::ui;
use chairphoto_core::app::ai::{AiSuggestion, Region};
use chairphoto_core::image_pool::ImageKind;
use gpui_kit::component::button::Button;
use gpui_kit::component::{Disableable as _, Sizable as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::prelude::*;
use gpui_kit::{
    canvas, div, img, px, relative, AnyElement, Bounds, Context, Entity, FontWeight, MouseButton, MouseDownEvent,
    MouseMoveEvent, ObjectFit, Pixels, Point, SharedString, Subscription, TestSupportExt as _, Window,
};
use std::cell::Cell;
use std::rc::Rc;

pub struct AiPanel {
    pub state: Entity<AiState>,
    images: Option<Entity<ImageStore>>,
    claim: Option<ClaimId>,
    pub question: Entity<InputState>,
    /// The region picker's image bounds, measured at paint.
    region_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    drag_start: Option<(f32, f32)>,
    asked_seen: u64,
    /// A follow-up was answered: the next render clears the input (it needs the window).
    clear_question: bool,
    _subscriptions: Vec<Subscription>,
}

impl AiPanel {
    pub fn new(state: Entity<AiState>, images: Option<Entity<ImageStore>>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let question = cx.new(|cx| InputState::new(window, cx).placeholder("Ask a follow-up… e.g. what kind of boat?"));
        let shell = state.read(cx).shell().clone();
        let asked_seen = state.read(cx).asked;
        let claim = images.as_ref().map(|i| i.update(cx, |s, _| s.new_claim()));
        let _subscriptions = vec![
            cx.observe(&state, |this, state, cx| {
                this.sync_claim(cx);
                let asked = state.read(cx).asked;
                if asked != this.asked_seen {
                    this.asked_seen = asked;
                    // The question was answered: clear it (React `setQuestion("")`). Deferred:
                    // the input needs a window.
                    this.clear_question = true;
                }
                cx.notify();
            }),
            cx.observe(&shell, |this, _, cx| {
                this.sync_claim(cx);
                cx.notify();
            }),
            cx.subscribe_in(&question, window, |this, _, e: &InputEvent, _, cx| {
                if matches!(e, InputEvent::PressEnter { .. }) {
                    this.ask(cx);
                }
            }),
        ];
        AiPanel {
            state,
            images,
            claim,
            question,
            region_bounds: Rc::default(),
            drag_start: None,
            asked_seen,
            clear_question: false,
            _subscriptions,
        }
    }

    /// The region picker holds the active photo's preview while it is open.
    fn sync_claim(&mut self, cx: &mut Context<Self>) {
        let (Some(images), Some(claim)) = (self.images.clone(), self.claim) else { return };
        let s = self.state.read(cx);
        let shell = s.shell().read(cx);
        let want = s.region_mode.then(|| shell.library.selection().active_id).flatten();
        let from = shell.rows_from();
        images.update(cx, |st, cx| match want {
            Some(id) => {
                st.set_claim(claim, [(id, ImageKind::Preview)]);
                // Asked for the row's catalog (#258): a preview cached from another (before a
                // re-root) is rendered again rather than left undrawn.
                st.request_batch_in(from, &[(id, ImageKind::Preview)], cx);
            }
            None => st.set_claim(claim, []),
        });
    }

    pub fn ask(&mut self, cx: &mut Context<Self>) {
        let q = self.question.read(cx).value().trim().to_string();
        if q.is_empty() {
            return;
        }
        self.state.update(cx, |s, cx| s.run(Some(q), true, cx));
    }

    fn norm(&self, p: Point<Pixels>) -> Option<(f32, f32)> {
        let b = self.region_bounds.get()?;
        let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
        if w <= 0. || h <= 0. {
            return None;
        }
        Some(((f32::from(p.x - b.origin.x) / w).clamp(0., 1.), (f32::from(p.y - b.origin.y) / h).clamp(0., 1.)))
    }

    pub fn region_down(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(p) = self.norm(at) else { return };
        self.drag_start = Some(p);
        self.state.update(cx, |s, cx| s.set_region(Some(Region { x: p.0, y: p.1, w: 0., h: 0. }), cx));
    }

    pub fn region_move(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(start), Some(p)) = (self.drag_start, self.norm(at)) else { return };
        self.state.update(cx, |s, cx| s.set_region(Some(region_between(start, p)), cx));
    }

    pub fn region_up(&mut self, cx: &mut Context<Self>) {
        if self.drag_start.take().is_none() {
            return;
        }
        self.state.update(cx, |s, cx| {
            let r = s.region.filter(|r| !is_tap(r));
            s.set_region(r, cx)
        });
    }

    fn pickers(&self, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let s = self.state.read(cx);
        let Some(stored) = s.stored.clone() else { return ui::sub("Loading settings…", colors).into_any_element() };
        let (provider, model) = (stored.provider(), stored.raw(logic::model_key(&stored.provider())).to_string());
        let choices: Vec<String> = if logic::is_cloud(&provider) {
            logic::curated_models(&provider).iter().map(|m| m.to_string()).collect()
        } else {
            s.ollama_models()
        };
        let options = model_options(&model, &choices);
        let locked = s.confirm.is_some();
        let state = self.state.clone();
        let engine = Button::new("ai-engine").outline().small().label(provider_short(&provider)).disabled(locked).dropdown_menu(
            move |menu: PopupMenu, _, _| {
                logic::PROVIDERS.iter().fold(menu, |menu, (id, _)| {
                    let state = state.clone();
                    menu.item(PopupMenuItem::new(provider_short(id)).on_click(move |_, _, cx| {
                        state.update(cx, |s, cx| s.set_provider(id, cx));
                    }))
                })
            },
        );
        let state = self.state.clone();
        let label = if model.is_empty() { "Default model…".to_string() } else { model.clone() };
        let model_menu = Button::new("ai-model").outline().small().label(label).disabled(locked || options.is_empty()).dropdown_menu(
            move |menu: PopupMenu, _, _| {
                options.iter().fold(menu, |menu, m| {
                    let (state, m) = (state.clone(), m.clone());
                    menu.item(PopupMenuItem::new(m.clone()).on_click(move |_, _, cx| {
                        state.update(cx, |s, cx| s.set_model(m.clone(), cx));
                    }))
                })
            },
        );
        ui::row().flex_wrap().gap(px(6.)).child(engine).child(model_menu).into_any_element()
    }

    fn render_region(&self, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let s = self.state.read(cx);
        let shell = s.shell().read(cx);
        let (active, from) = (shell.library.selection().active_id, shell.rows_from());
        let region = s.region;
        // The active photo's preview, only if rendered in the catalog its row came from (#258).
        let image = match (&self.images, active) {
            (Some(images), Some(id)) => match images.read(cx).peek_in(id, ImageKind::Preview, from) {
                ImageState::Ready(l) => Some(l.image),
                _ => None,
            },
            _ => None,
        };
        let hint = if region.is_some() { "Suggesting for the boxed area." } else { "Drag on the photo to box a detail." };
        let picker: AnyElement = match image {
            Some(image) => {
                let bounds = self.region_bounds.clone();
                let measure = canvas(move |b, _, _| bounds.set(Some(b)), |_, _, _, _| {}).absolute().size_full();
                let size = image.size(0);
                let ratio = (size.width.0 as f32).max(1.) / (size.height.0 as f32).max(1.);
                div()
                    .id("ai-region-wrap")
                    .relative()
                    .w_full()
                    .aspect_ratio(ratio)
                    .cursor_crosshair()
                    .child(img(image).size_full().object_fit(ObjectFit::Fill))
                    .child(measure)
                    .when_some(region.filter(|r| r.w > 0. || r.h > 0.), |d, r| {
                        d.child(
                            div()
                                .id("ai-region-box")
                                .absolute()
                                .left(relative(r.x))
                                .top(relative(r.y))
                                .w(relative(r.w))
                                .h(relative(r.h))
                                .border_2()
                                .border_color(colors.accent)
                                .test_support(),
                        )
                    })
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.region_down(e.position, cx)
                    }))
                    .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| this.region_move(e.position, cx)))
                    .on_mouse_up(MouseButton::Left, cx.listener(|this, _, _, cx| this.region_up(cx)))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(|this, _, _, cx| this.region_up(cx)))
                    .test_support()
                    .into_any_element()
            }
            None => ui::empty("ai-region-loading", "Loading preview…", colors),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(picker)
            .child(
                ui::row()
                    .gap(px(6.))
                    .child(ui::sub(hint, colors))
                    .when(region.is_some(), |d| {
                        let state = self.state.clone();
                        d.child(ui::clickable(ui::chip("ai-region-clear", "clear", true, colors), true, move |_, _, cx| {
                            state.update(cx, |s, cx| s.set_region(None, cx))
                        }))
                    }),
            )
            .into_any_element()
    }

    fn render_confirm(&self, colors: Colors, cx: &mut Context<Self>) -> Option<AnyElement> {
        let s = self.state.read(cx);
        let c = s.confirm.clone()?;
        let busy = s.busy;
        let (p, x) = (self.state.clone(), self.state.clone());
        Some(
            div()
                .id("ai-bulk-confirm")
                .flex()
                .flex_col()
                .gap(px(6.))
                .p(px(8.))
                .border_1()
                .border_color(colors.accent)
                .rounded(px(6.))
                .child(div().font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child(format!(
                    "Send {} photos to cloud AI?",
                    c.representatives
                )))
                .child({
                    let line = format!("Grouping: {} photos → {} representatives → {}", c.count, c.representatives, c.display);
                    div().id("ai-bulk-estimate").text_color(colors.txt).child(line.clone()).aria_label(line).test_support()
                })
                .when(c.unknown, |d| d.child(ui::sub("Unknown model — check provider pricing.", colors)))
                .child(ui::sub(format!("Engine: {} · Model: {}", provider_short(&c.provider), c.model), colors))
                .child(ui::sub(
                    "Burst grouping sends only one representative frame per cluster; its suggestions are propagated to \
                     the rest for review. Rough estimate only — each image is ~1 500 input tokens + ~300 output tokens. \
                     Check your provider for exact billing.",
                    colors,
                ))
                .child(
                    ui::row()
                        .gap(px(6.))
                        .child(ui::clickable(ui::primary("ai-bulk-proceed", "Proceed", !busy, colors), !busy, move |_, _, cx| {
                            p.update(cx, |s, cx| s.proceed(cx))
                        }))
                        .child(ui::clickable(ui::danger_chip("ai-bulk-cancel", "Cancel", !busy, colors), !busy, move |_, _, cx| {
                            x.update(cx, |s, cx| s.cancel_confirm(cx))
                        })),
                )
                .test_support()
                .into_any_element(),
        )
    }

    fn suggestion_row(&self, s: &AiSuggestion, selected: usize, busy: bool, colors: Colors) -> AnyElement {
        let key = s.path.replace('/', "_");
        let chip = |name: &str, label: String, enabled: bool| ui::chip(SharedString::from(format!("ai-{name}-{key}")), label, enabled, colors);
        let mut actions = ui::row().flex_wrap().gap(px(4.));
        let (st, path) = (self.state.clone(), s.path.clone());
        actions = actions.child(ui::clickable(chip("add", "✓ add".into(), true), true, move |_, _, cx| {
            st.update(cx, |st, cx| st.accept(path.clone(), cx))
        }));
        if selected > 1 {
            let (st, path) = (self.state.clone(), s.path.clone());
            actions = actions.child(ui::clickable(chip("all", format!("✓ all ({selected})"), true), true, move |_, _, cx| {
                st.update(cx, |st, cx| st.accept_for_all(path.clone(), cx))
            }));
        }
        let (st, path) = (self.state.clone(), s.path.clone());
        actions = actions.child(ui::clickable(chip("reject", "✗ reject".into(), true), true, move |_, _, cx| {
            st.update(cx, |st, cx| st.reject(path.clone(), cx))
        }));
        let (st, q) = (self.state.clone(), logic::refine_question(&s.path));
        actions = actions.child(ui::clickable(chip("refine", "↳ more specific".into(), !busy), !busy, move |_, _, cx| {
            st.update(cx, |st, cx| st.run(Some(q.clone()), true, cx))
        }));
        let conf = format!("{}%", (s.confidence * 100.).round() as i32);
        div()
            .id(SharedString::from(format!("ai-sug-{key}")))
            .flex()
            .flex_col()
            .gap(px(3.))
            .py(px(4.))
            .child(
                ui::row()
                    .gap(px(6.))
                    .child(div().text_color(colors.txt).child(s.path.clone()))
                    .when(s.is_new, |d| d.child(div().text_size(px(10.)).text_color(colors.accent).child("new")))
                    .child(div().text_size(px(11.)).text_color(colors.dim).child(conf)),
            )
            .when(!s.reason.is_empty(), |d| d.child(ui::sub(s.reason.clone(), colors)))
            .when(s.is_new && !s.description.is_empty(), |d| d.child(ui::sub(format!("\u{201c}{}\u{201d}", s.description), colors)))
            .when(s.is_new && !s.synonyms.is_empty(), |d| d.child(ui::sub(format!("synonyms: {}", s.synonyms.join(", ")), colors)))
            .child(actions)
            .aria_label(s.path.clone())
            .test_support()
            .into_any_element()
    }
}

impl Render for AiPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.clear_question) {
            self.question.update(cx, |i, cx| i.set_value("", window, cx));
        }
        let colors = Colors::get(cx);
        let s = self.state.read(cx);
        let active = s.shell().read(cx).library.selection().active_id;
        if active.is_none() {
            return ui::empty("ai-none", "Select a photo", colors);
        }
        let list = match &s.photo {
            PhotoView::Ready(p) if Some(p.photo_id) == active => Some(p.list.clone()),
            PhotoView::Failed(_, e) => return ui::error("ai-error-load", format!("AI suggestions unavailable: {e}"), colors),
            _ => None,
        };
        let (busy, region_mode, has_region) = (s.busy, s.region_mode, s.region.is_some());
        let selected = s.selected(cx).len();
        let error = s.error.clone();
        let batch_msg = s.batch_msg.clone();
        let st = self.state.clone();

        let suggest_label = if busy { "Thinking…" } else if has_region { "Suggest for region" } else { "Suggest tags" };
        let mut actions = ui::row()
            .flex_wrap()
            .gap(px(6.))
            .child(ui::clickable(ui::primary("ai-suggest", suggest_label, !busy, colors), !busy, {
                let st = st.clone();
                move |_, _, cx| st.update(cx, |s, cx| s.run(None, true, cx))
            }))
            .child(ui::clickable(
                ui::chip("ai-region", "▭ Region", !busy, colors).when(region_mode, |c| c.border_color(colors.accent).text_color(colors.txt)),
                !busy,
                {
                    let st = st.clone();
                    move |_, _, cx| st.update(cx, |s, cx| s.toggle_region_mode(cx))
                },
            ));
        if selected > 1 {
            actions = actions.child(ui::clickable(ui::chip("ai-batch", format!("Suggest for {selected} selected"), !busy, colors), !busy, {
                let st = st.clone();
                move |_, _, cx| st.update(cx, |s, cx| s.run_batch(cx))
            }));
        }

        if s.batch_running() {
            actions = actions.child(ui::clickable(ui::danger_chip("ai-batch-cancel", "Cancel batch", true, colors), true, {
                let st = st.clone();
                move |_, _, cx| st.update(cx, |s, cx| s.cancel_batch(cx))
            }));
        }

        let mut body = div().id("ai-panel").flex().flex_col().gap(px(8.)).text_size(px(12.)).child(self.pickers(colors, cx)).child(actions);
        if region_mode {
            body = body.child(self.render_region(colors, cx));
        }
        if let Some(m) = batch_msg {
            body = body.child(div().id("ai-batch-msg").text_color(colors.dim).child(m.clone()).aria_label(m).test_support());
        }
        if let Some(c) = self.render_confirm(colors, cx) {
            body = body.child(c);
        }
        if let Some(e) = error {
            body = body.child(ui::error("ai-error", e, colors));
        }
        match &list {
            None => body = body.child(ui::sub("Loading…", colors)),
            Some(list) => {
                if list.iter().any(|s| s.source_photo_id.is_some()) {
                    body = body.child(
                        ui::row()
                            .gap(px(6.))
                            .child(ui::sub("These suggestions were propagated from a burst representative.", colors))
                            .child(ui::clickable(ui::chip("ai-rerun", "Re-run directly", !busy, colors), !busy, {
                                let st = st.clone();
                                move |_, _, cx| st.update(cx, |s, cx| s.run(None, false, cx))
                            })),
                    );
                }
                for group in group_suggestions(list) {
                    let mut g = div().flex().flex_col();
                    if let Some(src) = group.source {
                        let from = group.source_filename.clone().unwrap_or_else(|| format!("photo #{src}"));
                        let paths: Vec<String> = group.items.iter().map(|s| s.path.clone()).collect();
                        let (a, r) = (st.clone(), st.clone());
                        let (pa, pr, name) = (paths.clone(), paths, group.source_filename.clone());
                        g = g.child(
                            ui::row()
                                .flex_wrap()
                                .gap(px(6.))
                                .child(div().text_color(colors.dim).child(format!("from {from}")))
                                .child(ui::clickable(
                                    ui::chip(SharedString::from(format!("ai-group-accept-{src}")), format!("✓ accept group ({})", group.items.len()), !busy, colors),
                                    !busy,
                                    move |_, _, cx| a.update(cx, |s, cx| s.accept_group(pa.clone(), name.clone(), cx)),
                                ))
                                .child(ui::clickable(
                                    ui::chip(SharedString::from(format!("ai-group-reject-{src}")), "✗ reject group", !busy, colors),
                                    !busy,
                                    move |_, _, cx| r.update(cx, |s, cx| s.reject_group(pr.clone(), cx)),
                                )),
                        );
                    }
                    for s in &group.items {
                        g = g.child(self.suggestion_row(s, selected, busy, colors));
                    }
                    body = body.child(g);
                }
            }
        }
        let can_ask = !busy && !self.question.read(cx).value().trim().is_empty();
        body = body.child(
            ui::row()
                .gap(px(6.))
                .child(div().flex_1().child(Input::new(&self.question).small().disabled(busy)))
                .child(ui::clickable(ui::chip("ai-ask", "Ask", can_ask, colors), can_ask, cx.listener(|this, _, _, cx| this.ask(cx)))),
        );
        body.test_support().into_any_element()
    }
}
