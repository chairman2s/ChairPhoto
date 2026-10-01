//! [`CompareView`]: two to four frames side by side, sharing one pan/zoom
//! (`CompareView.tsx`, and the Compare branch of App.tsx's key handler). Its state is the
//! shell's [`CompareSession`](crate::loupe::compare::CompareSession); this view draws it and
//! turns keys and clicks into the shell's Compare verbs.
//!
//! - **One transform.** Every pane's [`ZoomImage`] drives the same [`ZoomShared`]; it resets
//!   to fit when the compared set changes, since holding a deep zoom across a swap would show
//!   an arbitrary corner of the new frames.
//! - **Images.** The panes' previews are asked for together, the focused pane first; the
//!   zoom tier follows the first zoom-in per pane.
//! - **Mixed sizes.** When the frames' pixel dimensions differ, the same zoom shows different
//!   crops, and the bar says so.
//!
//! Keys ([`contexts::COMPARE`]): Esc/C close; ←/→ are the duel's verdicts, else move the
//! focus (as ↑/↓ do); Page Down/Up page the grid; K keeps the focused pane; the culling keys
//! mark the focused pane only.

use crate::image_store::ImageStore;
use crate::keymap::contexts;
use crate::loupe::compare::{CompareMode, MAX_PANES, MODE_PREF};
use crate::loupe::view::{file_name, with_culling_actions};
use crate::loupe::zoom::{ZoomImage, ZoomShared, ZoomView};
use crate::loupe::*;
use crate::machine_prefs::MachinePrefs;
use crate::shell::state::ShellState;
use crate::shell::style::{Colors, COLOR_LABELS};
use crate::storage::ui;
use chairphoto_core::catalog::{Photo, PickState};
use chairphoto_core::image_pool::ImageKind;
use chairphoto_model::compare_duel::DuelSide;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, AnyElement, Context, Entity, FocusHandle, SharedString, Subscription, TestSupportExt as _, Window};

/// See the module docs.
pub struct CompareView {
    shell: Entity<ShellState>,
    images: Entity<ImageStore>,
    shared: Entity<ZoomShared>,
    panes: Vec<Entity<ZoomImage>>,
    focus: FocusHandle,
    /// The ids on screen at the last render: a different set resets the view.
    shown: Vec<i64>,
    _observers: [Subscription; 2],
}

impl CompareView {
    pub fn new(shell: Entity<ShellState>, images: Entity<ImageStore>, cx: &mut Context<Self>) -> Self {
        let shared = cx.new(|_| ZoomShared::new());
        let panes = (0..MAX_PANES)
            .map(|i| {
                let (images, shared) = (images.clone(), shared.clone());
                cx.new(|cx| ZoomImage::shared(images, shared, format!("compare-image-{i}"), cx))
            })
            .collect();
        let _observers = [
            cx.observe(&shell, |this, shell, cx| {
                // Closed: reset here, not in render — a closed Compare is not on the stage, so
                // it is not rendered, and reopening on the same frames would find the old zoom.
                if shell.read(cx).compare().is_none() {
                    this.reset(cx);
                }
                cx.notify()
            }),
            cx.observe(&shared, |_, _, cx| cx.notify()),
        ];
        CompareView { shell, images, shared, panes, focus: cx.focus_handle(), shown: Vec::new(), _observers }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn shared_view(&self) -> &Entity<ZoomShared> {
        &self.shared
    }

    /// The panes' images, in pane order.
    pub fn panes(&self) -> &[Entity<ZoomImage>] {
        &self.panes
    }

    /// Compare closed: the next one starts fresh, whatever its frames — fit, the preview
    /// tier, no photo held.
    fn reset(&mut self, cx: &mut Context<Self>) {
        if self.shown.is_empty() {
            return;
        }
        self.shown.clear();
        self.shared.update(cx, |s, cx| s.set(ZoomView::FIT, cx));
        for pane in &self.panes {
            pane.update(cx, |z, cx| z.set_photo(None, cx));
        }
    }

    fn duel(&self, cx: &Context<Self>) -> bool {
        self.shell.read(cx).compare().is_some_and(|c| c.mode() == CompareMode::Duel)
    }

    fn left(&mut self, cx: &mut Context<Self>) {
        if self.duel(cx) {
            self.shell.update(cx, |s, cx| s.compare_verdict(DuelSide::Left, cx));
        } else {
            self.cycle(-1, cx);
        }
    }

    fn right(&mut self, cx: &mut Context<Self>) {
        if self.duel(cx) {
            self.shell.update(cx, |s, cx| s.compare_verdict(DuelSide::Right, cx));
        } else {
            self.cycle(1, cx);
        }
    }

    fn cycle(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| s.update_compare(cx, |c, ids| c.cycle_focus(ids, delta)));
    }

    fn page(&mut self, dir: isize, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| {
            s.update_compare(cx, |c, _| {
                c.page(dir);
            })
        });
    }

    fn set_mode(&mut self, mode: CompareMode, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| s.set_compare_mode(mode, cx));
        MachinePrefs::set(cx, MODE_PREF, mode.pref());
    }

    /// The panes' previews, focused first, as one batch; the previous set's queued
    /// previews are superseded.
    fn request(&mut self, ids: &[i64], focused: Option<i64>, cx: &mut Context<Self>) {
        let mut order: Vec<i64> = focused.into_iter().collect();
        order.extend(ids.iter().copied().filter(|&id| Some(id) != focused));
        self.images.update(cx, |store, _| {
            store.release_pending(|k| k.kind != ImageKind::Preview || order.contains(&k.photo));
            let batch: Vec<(i64, ImageKind)> = order.iter().map(|&id| (id, ImageKind::Preview)).collect();
            store.request_batch(&batch);
        });
    }

    fn render_pane(
        &self,
        i: usize,
        photo: &Photo,
        focused: bool,
        champion: Option<bool>,
        show_keep: bool,
        duel: bool,
        soft: f64,
        colors: Colors,
    ) -> AnyElement {
        let id = photo.id;
        let mut head = div().flex().flex_none().items_center().gap(px(6.)).h(px(28.)).px(px(8.)).overflow_hidden();
        head = head.child(
            div().text_size(px(12.)).text_color(colors.txt).whitespace_nowrap().child(file_name(&photo.path)),
        );
        let tag = |text: String, color| div().text_size(px(11.)).text_color(color).child(text);
        if let Some(done) = champion {
            head = head.child(
                div()
                    .id(("compare-champion", id as u64))
                    .text_size(px(11.))
                    .text_color(colors.accent)
                    .child(if done { "♛ winner" } else { "champion" })
                    .test_support(),
            );
        }
        if photo.rating > 0 {
            head = head.child(tag("★".repeat(photo.rating as usize), colors.rating));
        }
        match photo.pick_state {
            PickState::Pick => head = head.child(tag("pick".into(), colors.ok)),
            PickState::Reject => head = head.child(tag("rejected".into(), colors.danger)),
            PickState::None => {}
        }
        if let Some(c) = COLOR_LABELS.iter().find(|l| l.name == photo.label) {
            head = head.child(div().size(px(9.)).rounded_full().bg(c.color()));
        }
        if let Some(s) = photo.sharpness {
            head = head.child(tag(format!("⌖ {s:.0}"), if s < soft { colors.danger } else { colors.dim }));
        }
        if photo.burst_flag.as_deref() == Some("sharpest-of-burst") {
            head = head.child(tag("♛".into(), colors.rating));
        }
        let shell = self.shell.clone();
        let keep = show_keep.then(|| {
            let label = if duel { "This one wins" } else { "Keep this" };
            let chip = ui::chip(SharedString::from(format!("compare-keep-{id}")), label, true, colors)
                .when(focused, |c| c.border_color(colors.accent).text_color(colors.txt));
            ui::clickable(chip, true, move |_, _, cx| shell.update(cx, |s, cx| s.compare_keep(Some(id), cx)))
        });
        let shell = self.shell.clone();
        div()
            .id(("compare-pane", id as u64))
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .rounded(px(4.))
            .border_2()
            .border_color(if focused { colors.accent } else { gpui_kit::transparent_black() })
            .when(photo.pick_state == PickState::Reject, |d| d.opacity(0.55))
            // Focus follows the press, so starting a pan in a pane also focuses it.
            .capture_any_mouse_down(move |_, _, cx| shell.update(cx, |s, cx| s.update_compare(cx, |c, _| c.set_focus(id))))
            .child(head)
            .child(div().relative().flex_1().min_h_0().bg(colors.well).child(self.panes[i].clone()))
            .child(div().flex().flex_none().h(px(36.)).items_center().justify_center().children(keep))
            .test_support()
            .into_any_element()
    }
}

impl Render for CompareView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let shell = self.shell.read(cx);
        let panes: Vec<Photo> = shell.compare_panes().into_iter().cloned().collect();
        let Some(session) = shell.compare().cloned() else {
            self.reset(cx);
            return div().id("compare").into_any_element();
        };
        let soft = shell.soft_threshold;
        let ids: Vec<i64> = panes.iter().map(|p| p.id).collect();
        let focused = session.focused(&ids);
        if ids != self.shown {
            self.shown = ids.clone();
            self.shared.update(cx, |s, cx| s.set(ZoomView::FIT, cx));
            self.request(&ids, focused, cx);
        }
        for (i, pane) in self.panes.iter().enumerate() {
            let id = ids.get(i).copied();
            pane.update(cx, |z, cx| z.set_photo(id, cx));
        }
        let view = self.shared.read(cx).view;
        let duel = session.mode() == CompareMode::Duel;
        let done = duel && session.duel().done;
        let pool = session.pool().len();

        let mut bar = div()
            .id("compare-bar")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .h(px(40.))
            .px(px(12.))
            .border_b_1()
            .border_color(colors.border)
            .child(ui::clickable(ui::chip("compare-back", "‹ Back to grid (Esc)", true, colors), true, {
                let shell = self.shell.clone();
                move |_, _, cx| shell.update(cx, |s, cx| s.close_compare(cx))
            }));
        if pool > 2 {
            let mode_chip = |id: &'static str, label: &'static str, on: bool, mode: CompareMode, cx: &mut Context<Self>| {
                let chip = ui::chip(id, label, true, colors).when(on, |c| c.bg(colors.sel).text_color(colors.txt));
                ui::clickable(chip, true, cx.listener(move |this, _, _, cx| this.set_mode(mode, cx)))
            };
            bar = bar
                .child(mode_chip("compare-mode-duel", "Duel", duel, CompareMode::Duel, cx))
                .child(mode_chip("compare-mode-grid", "Grid", !duel, CompareMode::Grid, cx));
        }
        let text = |id: &'static str, s: String, color| {
            div().id(id).text_size(px(12.)).text_color(color).child(s.clone()).aria_label(s).test_support()
        };
        if duel {
            if done {
                let name = panes.first().map(|p| file_name(&p.path)).unwrap_or_default();
                bar = bar.child(text(
                    "compare-status",
                    format!("Champion — {name} · picked, rivals rejected · Esc to finish"),
                    colors.txt,
                ));
            } else {
                let (round, total) = session.duel_progress();
                bar = bar.child(text("compare-status", format!("Duel {round} of {total}"), colors.txt)).child(
                    div().text_size(px(11.)).text_color(colors.mute).child(
                        "← left wins · → right wins · loser is rejected · 0–5 rate the focused pane",
                    ),
                );
            }
        } else if pool > panes.len() || session.start() > 0 {
            let start = session.start();
            let prev = start > 0;
            let next = start + MAX_PANES < pool;
            bar = bar
                .child(ui::clickable(
                    ui::chip("compare-prev", "‹", prev, colors),
                    prev,
                    cx.listener(|this, _, _, cx| this.page(-1, cx)),
                ))
                .child(text(
                    "compare-status",
                    format!("Comparing {}–{} of {pool}", start + 1, start + panes.len()),
                    colors.txt,
                ))
                .child(ui::clickable(
                    ui::chip("compare-next", "›", next, colors),
                    next,
                    cx.listener(|this, _, _, cx| this.page(1, cx)),
                ))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(colors.mute)
                        .child("←/→ focus · 0–5 rate · P/X pick or reject · K keep & next batch"),
                );
        } else {
            bar = bar.child(text(
                "compare-status",
                format!("Comparing {} — ←/→ focus, 0–5 rate, P/X pick or reject, K keep", panes.len()),
                colors.txt,
            ));
        }
        if view.zoomed() {
            let shared = self.shared.clone();
            bar = bar.child(ui::clickable(
                ui::chip("compare-fit", format!("Fit {}%", view.percent()), true, colors),
                true,
                move |_, _, cx| shared.update(cx, |s, cx| s.set(ZoomView::FIT, cx)),
            ));
        }
        let mut dims: Vec<String> =
            panes.iter().map(|p| format!("{}x{}", p.width.unwrap_or(0), p.height.unwrap_or(0))).collect();
        dims.dedup();
        dims.sort();
        dims.dedup();
        if dims.len() > 1 {
            bar = bar.child(
                div()
                    .id("compare-mixed")
                    .text_size(px(11.5))
                    .text_color(colors.danger)
                    .child("⚠ mixed sizes")
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(format!(
                            "These frames have different pixel dimensions ({}), so at the same zoom the panes do not show the same crop.",
                            dims.join(", ")
                        ))
                        .build(window, cx)
                    })
                    .test_support(),
            );
        }

        let champion = session.champion();
        let panes_el = div().flex().flex_row().gap(px(6.)).flex_1().min_h_0().p(px(6.)).children(
            panes.iter().enumerate().map(|(i, p)| {
                let is_champion = (champion == Some(p.id)).then_some(done);
                self.render_pane(i, p, focused == Some(p.id), is_champion, !done, duel, soft, colors)
            }),
        );

        let root = div()
            .id("compare")
            .key_context(contexts::COMPARE)
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .on_mouse_down(gpui_kit::MouseButton::Left, cx.listener(|this, _, window, cx| this.focus.focus(window, cx)))
            .on_action(cx.listener(|this, _: &CloseCompare, _, cx| this.shell.update(cx, |s, cx| s.close_compare(cx))))
            .on_action(cx.listener(|this, _: &CompareLeft, _, cx| this.left(cx)))
            .on_action(cx.listener(|this, _: &CompareRight, _, cx| this.right(cx)))
            .on_action(cx.listener(|this, _: &ComparePrevious, _, cx| this.cycle(-1, cx)))
            .on_action(cx.listener(|this, _: &CompareNext, _, cx| this.cycle(1, cx)))
            .on_action(cx.listener(|this, _: &ComparePageDown, _, cx| this.page(1, cx)))
            .on_action(cx.listener(|this, _: &ComparePageUp, _, cx| this.page(-1, cx)))
            .on_action(cx.listener(|this, _: &CompareKeep, _, cx| this.shell.update(cx, |s, cx| s.compare_keep(None, cx))))
            .test_support();
        with_culling_actions(root, &self.shell).child(bar).child(panes_el).into_any_element()
    }
}
