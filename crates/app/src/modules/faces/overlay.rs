//! The loupe face overlay (`FaceOverlay` in faces.tsx), mounted by the loupe's panel slot
//! over its zoomable image: a box per face coloured by state, following zoom and pan; a name
//! chip with ✓ ✕ ⇄ – 🗑; the person picker under a box; "＋ face" draw mode (one drag = one
//! box, at least 8 px); "hide faces" / "show faces", remembered on this machine
//! (`faces.showBoxes`, the per-machine store parity.md asks for).
//!
//! **Keys** (taken by a keystroke interceptor, before the loupe's bindings, and only while the
//! loupe has focus and no text field does): **F** toggles the boxes (no modifiers); **Esc**
//! leaves draw mode instead of closing the loupe.
//!
//! **Geometry.** A box is drawn through the very transform the loupe draws its picture with
//! ([`ZoomView::placement`] of the drawn picture's size in the image's container), so it stays
//! glued to the face at any zoom, pan or window size. Face boxes are stored in the canonical
//! frame — the photo as its metadata orients it, **without** the non-destructive user rotation
//! (the indexer detects on the unrotated preview; the MWG export writes that frame) — while the
//! loupe's tiers are rendered turned by the user rotation. So each box is turned by the
//! rotation read with the faces before it is placed ([`rotate_box`]), and a box drawn on the
//! turned picture is turned back before it is stored ([`unrotate_box`]). The boxes are drawn
//! only over a tier of the image version the faces were read with: a rotation invalidates the
//! photo's images and re-reads its rotation, so a turned box never lands on stale pixels (or
//! fresh pixels with a stale angle). React placed unturned boxes on the turned picture.
//!
//! Over an edited version's render the boxes are hidden: a version may be cropped or rotated,
//! and its frame is no longer the one the boxes were measured in (React drew them there anyway,
//! misplaced). The same holds for the Darkroom's **print** in the pop-out (#110): it is drawn
//! from the edit's own render (`Drawn::OverrideLo`/`Hi`), so the boxes are hidden there too,
//! with the same note. And for the photo's thumbnail when it is its **cover** version's render
//! (#152, `Loaded::cover`), which the loupe draws as a placeholder until the preview lands:
//! hidden, with the same note. Drawing them when the print's geometry equals the original's
//! (no crop, straighten or rotation in the edit) was considered and not done: the overlay
//! would have to read the edit's geometry, which lives in the Darkroom and reaches the loupe
//! only as pixels, and a rule that shows boxes on some prints and not others would read as a
//! bug. Hidden with the note is the decision, until the edit carries a box transform the
//! overlay can apply.
//!
//! **Where the transform comes from.** The slot hands a panel only a window, so the overlay
//! asks the host which loupe image that window shows ([`loupe_zoom`], i.e.
//! [`crate::loupe::view::loupe_image`]: the inline loupe's in the main window, the pop-out's in
//! the pop-out, #110) and observes it. That reads the loupe's public accessors and changes
//! nothing in it.

use super::logic::{bbox_to_screen, chip_name, drag_to_bbox, rotate_box, state_color, unrotate_box};
use super::picker::{PersonPicker, PickerEvent};
use super::state::FacesState;
use crate::image_store::{ImageState, ImageStore};
use crate::keymap::contexts;
use crate::loupe::zoom::{Drawn, ZoomImage, ZoomView};
use crate::machine_prefs::MachinePrefs;
use crate::shell::style::Colors;
use chairphoto_core::app::faces::FaceBboxJson;
use chairphoto_core::image_pool::ImageKind;
use gpui_kit::prelude::*;
use gpui_kit::{
    div, px, AnyElement, App, Context, CursorStyle, Entity, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Point, SharedString, Subscription, TestSupportExt as _, WeakEntity, Window,
};

/// The per-machine preference: `"0"` hides the boxes.
pub const SHOW_BOXES_PREF: &str = "faces.showBoxes";

/// The loupe image `window` shows — the main window's inline loupe, or the pop-out's (#110) —
/// as every loupe registers it before building its loupe-slot panels.
pub fn loupe_zoom(window: &Window, cx: &App) -> Option<Entity<ZoomImage>> {
    crate::loupe::view::loupe_image(window, cx)
}

/// What the overlay draws against this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub photo: i64,
    /// The drawn picture's size.
    pub natural: (f32, f32),
    /// The image container's top-left in the window, and its size.
    pub origin: Point<Pixels>,
    pub container: (f32, f32),
    pub view: ZoomView,
    /// How far the drawn picture is turned (clockwise) from the boxes' canonical frame.
    pub rotation: i64,
}

pub struct FaceOverlay {
    state: Entity<FacesState>,
    images: Option<Entity<ImageStore>>,
    zoom: Option<WeakEntity<ZoomImage>>,
    pub show_boxes: bool,
    pub draw_mode: bool,
    /// The drag in progress: its start and current point, container-local.
    draft: Option<((f32, f32), (f32, f32))>,
    picker: Option<(i64, Entity<PersonPicker>, Subscription)>,
    /// A drawn face whose picker opens once its row is read.
    pending_pick: Option<i64>,
    _subscriptions: Vec<Subscription>,
    _zoom_subscriptions: Vec<Subscription>,
}

impl FaceOverlay {
    pub fn new(state: Entity<FacesState>, images: Option<Entity<ImageStore>>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let show_boxes = MachinePrefs::read(cx, SHOW_BOXES_PREF).as_deref() != Some("0");
        let window_handle = window.window_handle();
        let mut subscriptions = vec![cx.observe(&state, |this, state, cx| {
            let faces = state.read(cx).faces().map(|p| p.faces.iter().map(|f| f.id).collect::<Vec<_>>()).unwrap_or_default();
            if this.picker.as_ref().is_some_and(|(f, ..)| !faces.contains(f)) {
                this.picker = None;
            }
            cx.notify();
        })];
        if let Some(images) = &images {
            subscriptions.push(cx.observe(images, |_, _, cx| cx.notify()));
        }
        let this = cx.entity().downgrade();
        subscriptions.push(cx.intercept_keystrokes({
            move |event, window, cx| {
                if window.window_handle() != window_handle {
                    return;
                }
                let k = &event.keystroke;
                if k.modifiers.control || k.modifiers.alt || k.modifiers.platform || k.modifiers.shift {
                    return;
                }
                let in_loupe = event.context_stack.iter().any(|c| c.contains(contexts::LOUPE));
                let in_input = event.context_stack.iter().any(|c| c.contains(contexts::INPUT));
                if !in_loupe || in_input {
                    return;
                }
                let Some(this) = this.upgrade() else { return };
                let handled = match k.key.as_str() {
                    "f" if this.read(cx).frame(cx).is_some() => {
                        this.update(cx, |o, cx| o.toggle_boxes(cx));
                        true
                    }
                    "escape" if this.read(cx).draw_mode => {
                        this.update(cx, |o, cx| o.set_draw_mode(false, cx));
                        true
                    }
                    _ => false,
                };
                if handled {
                    cx.stop_propagation();
                }
            }
        }));
        FaceOverlay {
            state,
            images,
            zoom: None,
            show_boxes,
            draw_mode: false,
            draft: None,
            picker: None,
            pending_pick: None,
            _subscriptions: subscriptions,
            _zoom_subscriptions: Vec::new(),
        }
    }

    pub fn toggle_boxes(&mut self, cx: &mut Context<Self>) {
        self.show_boxes = !self.show_boxes;
        MachinePrefs::set(cx, SHOW_BOXES_PREF, if self.show_boxes { "1" } else { "0" });
        cx.notify();
    }

    pub fn set_draw_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        self.draw_mode = on;
        self.draft = None;
        cx.notify();
    }

    /// Find (once) and follow the loupe image this overlay lies over.
    fn attach(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.zoom.as_ref().is_some_and(|z| z.upgrade().is_some()) {
            return;
        }
        let Some(zoom) = loupe_zoom(window, cx) else { return };
        let shared = zoom.read(cx).shared_view().clone();
        self._zoom_subscriptions = vec![cx.observe(&zoom, |_, _, cx| cx.notify()), cx.observe(&shared, |_, _, cx| cx.notify())];
        self.zoom = Some(zoom.downgrade());
    }

    /// The loupe's picture this frame, when the boxes can be drawn on it: the photo whose
    /// faces are shown, drawn from one of its own tiers (not an edited version's render),
    /// with its container measured.
    pub fn frame(&self, cx: &App) -> Option<Frame> {
        let zoom = self.zoom.as_ref()?.upgrade()?;
        let z = zoom.read(cx);
        let (photo, drawn) = z.drawn()?;
        let faces = self.state.read(cx).faces()?;
        if z.photo() != Some(photo) || faces.photo_id != photo {
            return None;
        }
        let kind = match drawn {
            Drawn::Thumb => ImageKind::Thumb,
            Drawn::Preview => ImageKind::Preview,
            Drawn::Zoom => ImageKind::Zoom,
            Drawn::OverrideLo | Drawn::OverrideHi => return None,
        };
        let images = self.images.as_ref()?.read(cx);
        // The faces' rotation is the one these pixels were rendered with only at the version
        // it was read for (a rotation bumps it and re-reads). That is the Preview tier's
        // version: an invalidate bumps every tier together, so it says whether the faces were
        // read after the last one, whichever tier is drawn — the thumbnail's alone also moves
        // with the Darkroom strip's cover looks (#134), which turn nothing.
        if images.key(photo, ImageKind::Preview).version != faces.image_version {
            return None;
        }
        let ImageState::Ready(loaded) = images.peek(photo, kind) else { return None };
        if loaded.cover {
            // The cover version's thumbnail (the placeholder while the preview loads): not the
            // original's frame (#152).
            return None;
        }
        let size = loaded.image.size(0);
        let bounds = z.bounds()?;
        Some(Frame {
            photo,
            natural: (size.width.0 as f32, size.height.0 as f32),
            origin: bounds.origin,
            container: (f32::from(bounds.size.width), f32::from(bounds.size.height)),
            view: z.view(cx),
            rotation: faces.rotation,
        })
    }

    /// Whether the loupe shows an edited version's render of the faces' photo: the version
    /// shown, or the cover version's thumbnail standing in for the preview (#152).
    fn on_version(&self, cx: &App) -> bool {
        let Some(zoom) = self.zoom.as_ref().and_then(|z| z.upgrade()) else { return false };
        match zoom.read(cx).drawn() {
            Some((_, Drawn::OverrideLo | Drawn::OverrideHi)) => true,
            Some((photo, Drawn::Thumb)) => self.images.as_ref().is_some_and(|images| {
                matches!(images.read(cx).peek(photo, ImageKind::Thumb), ImageState::Ready(l) if l.cover)
            }),
            _ => false,
        }
    }

    fn local(frame: &Frame, p: Point<Pixels>) -> (f32, f32) {
        (f32::from(p.x - frame.origin.x), f32::from(p.y - frame.origin.y))
    }

    fn draw_down(&mut self, e: &MouseDownEvent, cx: &mut Context<Self>) {
        let Some(frame) = self.frame(cx) else { return };
        cx.stop_propagation();
        let p = Self::local(&frame, e.position);
        self.draft = Some((p, p));
        cx.notify();
    }

    fn draw_move(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        let (Some(frame), Some((a, _))) = (self.frame(cx), self.draft) else { return };
        cx.stop_propagation();
        self.draft = Some((a, Self::local(&frame, e.position)));
        cx.notify();
    }

    fn draw_up(&mut self, e: &MouseUpEvent, cx: &mut Context<Self>) {
        let (Some(frame), Some((a, _))) = (self.frame(cx), self.draft.take()) else { return };
        cx.stop_propagation();
        self.draw_mode = false;
        let b = Self::local(&frame, e.position);
        cx.notify();
        let Some((x, y, w, h)) = drag_to_bbox(a, b, frame.natural, frame.container, frame.view) else { return };
        // Drawn on the turned picture: stored in the canonical frame.
        let (x, y, w, h) = unrotate_box((x as f32, y as f32, w as f32, h as f32), frame.rotation);
        let bbox = (x as f64, y as f64, w as f64, h as f64);
        let this = cx.entity().downgrade();
        self.state.update(cx, |s, cx| {
            s.add_manual(
                frame.photo,
                bbox,
                move |id, cx| {
                    this.update(cx, |o, cx| {
                        o.pending_pick = Some(id);
                        cx.notify();
                    })
                    .ok();
                },
                cx,
            )
        });
    }

    pub fn open_picker(&mut self, face_id: i64, current: Option<i64>, window: &mut Window, cx: &mut Context<Self>) {
        let tags = self.state.read(cx).faces().map(|p| p.people.tags.clone()).unwrap_or_default();
        let picker = cx.new(|cx| PersonPicker::new(format!("faces-picker-{face_id}"), tags, current, window, cx));
        let sub = cx.subscribe(&picker, move |this, _, event: &PickerEvent, cx| {
            match event {
                PickerEvent::Pick { tag_id, .. } => this.state.update(cx, |s, cx| s.assign(face_id, *tag_id, cx)),
                PickerEvent::Create(name) => this.state.update(cx, |s, cx| s.create_person(face_id, name.clone(), cx)),
                PickerEvent::Cancel => {}
            }
            this.picker = None;
            cx.notify();
        });
        self.picker = Some((face_id, picker, sub));
        cx.notify();
    }

    pub fn picker(&self) -> Option<&Entity<PersonPicker>> {
        self.picker.as_ref().map(|(_, p, _)| p)
    }

    fn chip_button(
        &self,
        id: String,
        label: &'static str,
        danger: bool,
        colors: Colors,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(SharedString::from(id))
            .px(px(3.))
            .rounded(px(3.))
            .text_color(if danger { colors.danger } else { colors.txt })
            .cursor_pointer()
            .hover(|s| s.bg(colors.sel))
            .child(label)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                f(this, window, cx);
            }))
            .test_support()
            .into_any_element()
    }
}

impl Render for FaceOverlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.attach(window, cx);
        let colors = Colors::get(cx);
        let frame = self.frame(cx);
        let faces = self.state.read(cx).faces().map(|p| (p.photo_id, p.faces.clone()));
        // A drawn box's picker opens once its row has been read.
        if let Some(id) = self.pending_pick {
            if faces.as_ref().is_some_and(|(_, f)| f.iter().any(|f| f.id == id)) {
                self.pending_pick = None;
                self.open_picker(id, None, window, cx);
            }
        }
        let root = div().id("faces-overlay").absolute().top_0().left_0().size_full().overflow_hidden().test_support();
        let (Some(frame), Some((_, faces))) = (frame, faces) else {
            let note = self.on_version(cx).then(|| {
                div()
                    .id("faces-overlay-version")
                    .absolute()
                    .top(px(8.))
                    .left(px(8.))
                    .px(px(8.))
                    .py(px(3.))
                    .rounded(px(6.))
                    .bg(colors.panel.opacity(0.85))
                    .text_size(px(11.))
                    .text_color(colors.dim)
                    .child("Faces are shown on the original, not on an edited version")
                    .test_support()
            });
            return root.children(note).into_any_element();
        };

        let mut root = root;
        if self.draw_mode {
            root = root
                .occlude()
                .cursor(CursorStyle::Crosshair)
                .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, _, cx| this.draw_down(e, cx)))
                .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| this.draw_move(e, cx)))
                .on_mouse_up(MouseButton::Left, cx.listener(|this, e: &MouseUpEvent, _, cx| this.draw_up(e, cx)));
        }

        // The toolbar: "＋ face" / "✕ cancel", and hide/show when there are faces.
        let toolbar_button = |id: &'static str, label: &'static str, on: bool| {
            div()
                .id(id)
                .px(px(8.))
                .py(px(3.))
                .rounded(px(6.))
                .border_1()
                .border_color(colors.border)
                .bg(if on { colors.accent.opacity(0.9) } else { colors.panel.opacity(0.85) })
                .text_size(px(11.5))
                .text_color(if on { colors.onaccent } else { colors.txt })
                .cursor_pointer()
                .child(label)
                .aria_label(label)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        };
        let toolbar = div()
            .absolute()
            .top(px(8.))
            .left(px(8.))
            .flex()
            .gap(px(6.))
            .child(
                toolbar_button("faces-draw", if self.draw_mode { "✕ cancel" } else { "＋ face" }, self.draw_mode)
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        let on = !this.draw_mode;
                        this.set_draw_mode(on, cx);
                    }))
                    .test_support(),
            )
            .when(!faces.is_empty(), |d| {
                d.child(
                    toolbar_button("faces-toggle", if self.show_boxes { "hide faces" } else { "show faces" }, false)
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_boxes(cx);
                        }))
                        .test_support(),
                )
            });

        let draft = self.draft.map(|(a, b)| {
            div()
                .id("faces-draft")
                .absolute()
                .left(px(a.0.min(b.0)))
                .top(px(a.1.min(b.1)))
                .w(px((a.0 - b.0).abs()))
                .h(px((a.1 - b.1).abs()))
                .border_2()
                .border_dashed()
                .border_color(colors.accent)
                .bg(colors.sel)
                .rounded(px(4.))
                .test_support()
        });

        let mut layers: Vec<AnyElement> = Vec::new();
        if self.show_boxes {
            for face in &faces {
                let b = face.bbox;
                let (x, y, w, h) = rotate_box((b.x, b.y, b.w, b.h), frame.rotation);
                let r = bbox_to_screen(FaceBboxJson { x, y, w, h }, frame.natural, frame.container, frame.view);
                let color = state_color(&face.state, colors);
                let id = face.id;
                let state = face.state.as_str();
                let (suggested, ignored, rejected) = (state == "suggested", state == "ignored", state == "rejected");
                layers.push(
                    div()
                        .id(SharedString::from(format!("faces-box-{id}")))
                        .absolute()
                        .left(px(r.left))
                        .top(px(r.top))
                        .w(px(r.width))
                        .h(px(r.height))
                        .border_2()
                        .border_color(color)
                        .rounded(px(4.))
                        .when(ignored, |d| d.opacity(0.4))
                        .test_support()
                        .into_any_element(),
                );
                let mut chip = div()
                    .id(SharedString::from(format!("faces-chip-{id}")))
                    .absolute()
                    .left(px(r.left))
                    .top(px(r.top - 24.))
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .px(px(5.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(color)
                    .bg(colors.panel.opacity(0.88))
                    .text_size(px(11.))
                    .text_color(colors.txt)
                    .whitespace_nowrap()
                    .when(ignored, |d| d.opacity(0.6))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child({
                        let name = chip_name(face.person_name.as_deref(), state);
                        div()
                            .id(SharedString::from(format!("faces-chip-name-{id}")))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .text_color(color)
                            .max_w(px(120.))
                            .overflow_hidden()
                            .child(name.clone())
                            .aria_label(name)
                            .test_support()
                    })
                    .when_some(suggested.then_some(face.match_confidence).flatten(), |d, c| {
                        d.child(div().text_size(px(10.)).opacity(0.75).child(format!("{}%", (c * 100.).round() as i32)))
                    });
                if !ignored && !rejected {
                    if suggested {
                        chip = chip.child(self.chip_button(format!("faces-confirm-{id}"), "✓", false, colors, move |o, _, cx| {
                            o.state.update(cx, |s, cx| s.accept(id, cx))
                        }, cx));
                    }
                    if suggested || state == "unassigned" {
                        chip = chip.child(self.chip_button(format!("faces-reject-{id}"), "✕", true, colors, move |o, _, cx| {
                            o.state.update(cx, |s, cx| s.reject(id, cx))
                        }, cx));
                    }
                    let current = face.person_tag_id;
                    chip = chip.child(self.chip_button(format!("faces-reassign-{id}"), "⇄", false, colors, move |o, window, cx| {
                        if o.picker.as_ref().is_some_and(|(f, ..)| *f == id) {
                            o.picker = None;
                            cx.notify();
                        } else {
                            o.open_picker(id, current, window, cx);
                        }
                    }, cx));
                    chip = chip.child(self.chip_button(format!("faces-ignore-{id}"), "–", false, colors, move |o, _, cx| {
                        o.state.update(cx, |s, cx| s.ignore(id, cx))
                    }, cx));
                    if face.source == "drawn" {
                        chip = chip.child(self.chip_button(format!("faces-delete-{id}"), "🗑", true, colors, move |o, _, cx| {
                            o.state.update(cx, |s, cx| s.delete_drawn(id, cx))
                        }, cx));
                    }
                }
                layers.push(chip.test_support().into_any_element());
                if let Some((_, picker, _)) = self.picker.as_ref().filter(|(f, ..)| *f == id) {
                    layers.push(
                        div()
                            .id(SharedString::from(format!("faces-reassign-drop-{id}")))
                            .absolute()
                            .left(px(r.left))
                            .top(px(r.top + r.height + 2.))
                            .p(px(6.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(colors.border)
                            .bg(colors.panel.opacity(0.97))
                            .occlude()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.picker = None;
                                cx.notify();
                            }))
                            .child(div().text_size(px(10.)).text_color(colors.mute).pb(px(4.)).child("REASSIGN TO…"))
                            .child(picker.clone())
                            .test_support()
                            .into_any_element(),
                    );
                }
            }
        }
        root.children(layers).children(draft).child(toolbar).into_any_element()
    }
}
