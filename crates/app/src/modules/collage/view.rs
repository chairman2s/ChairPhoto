//! "Make collage" (`CollageDialog.tsx`): a freeform canvas (≤ 520 × 460) the selection is
//! laid onto — auto-arranged as the justified mosaic when it opens — with seven templates
//! (which lock the layout), Auto-arrange, Front/Back and Lock layout; drag to move, the corner
//! handle to resize, Shift-drag to pan the photo in its frame, the wheel to zoom it (1–6×),
//! and, locked, drag onto another tile to swap. Then aspect, width (with the output size),
//! background, border, corner radius, JPEG/PNG, and Save to Library (tagged `Collage/<kind>`)
//! or Folder (Browse…), Reveal, errors. No backdrop close.
//!
//! The canvas state and gesture maths are `chairphoto_model::collage::CollageCanvas`; this
//! view only feeds it pointer events. A tile's photo is its thumbnail, cover-filled exactly
//! as core composites it (`collage::cover_rect`). Moves and releases are taken at window
//! level while a gesture runs (React's window `pointermove`/`pointerup`), so a drag that
//! leaves the canvas still ends.
//!
//! Every backend call (auto-arrange, render, library save) runs on a worker, bound to the
//! catalog the selection was read from; a newer call of the same kind drops the older one's
//! answer. `catalog:switched` closes the dialog.

use super::CollageBackend;
use crate::image_store::ImageState;
use crate::modules::dialog::{self, DialogHost, Picked, SelectionSnapshot, NO_ROWS};
use crate::shell::style::Colors;
use crate::storage::{ui, Runner};
use crate::tags::toggle;
use chairphoto_core::app::collage::{auto_arrange, make_freeform, parse_color, save_to_catalog, CollageOptionsDto, FreeformOptionsDto, PlacementDto};
use chairphoto_core::image_pool::ImageKind;
use chairphoto_model::collage::{
    aspect_ratio, body_gesture, cover_rect, display_size, output_height, CanvasRect, CollageCanvas, GestureMode, Placement,
    ASPECTS, TEMPLATES, WIDTH_PRESETS,
};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::prelude::*;
use gpui_kit::{
    canvas, div, img, px, rgba, AnyElement, Bounds, Context, DispatchPhase, Entity, Hsla, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ObjectFit, Pixels, ScrollWheelEvent, SharedString, Subscription, TestSupportExt as _, Window,
};
use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

/// Where the collage goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveTo {
    Library,
    Folder,
}

/// The output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Jpeg,
    Png,
}

impl Format {
    fn wire(self) -> &'static str {
        match self {
            Format::Jpeg => "jpeg",
            Format::Png => "png",
        }
    }
}

/// Background swatches beside the hex field.
const SWATCHES: [&str; 4] = ["#ffffff", "#000000", "#808080", "#f2ede4"];

pub struct CollageDialog {
    host: DialogHost,
    backend: CollageBackend,
    snapshot: SelectionSnapshot,
    pub canvas: CollageCanvas,
    pub aspect: &'static str,
    pub width: u32,
    pub background: Entity<InputState>,
    pub border: u32,
    pub corner: u32,
    pub format: Format,
    pub save_to: SaveTo,
    pub dest: Entity<InputState>,
    border_slider: Entity<SliderState>,
    corner_slider: Entity<SliderState>,
    pub busy: bool,
    pub arranging: bool,
    pub output: Option<PathBuf>,
    pub saved_to_library: bool,
    pub error: Option<String>,
    /// Bumped by each Auto-arrange / Render; a superseded call's answer is dropped.
    arrange_attempt: u64,
    render_attempt: u64,
    /// Where the canvas is on screen (from its last prepaint), for the gesture maths.
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Each photo's displayed aspect, once its thumbnail has loaded.
    aspects: HashMap<i64, f64>,
    _subscriptions: Vec<Subscription>,
}

impl CollageDialog {
    pub fn new(host: DialogHost, backend: CollageBackend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let snapshot = SelectionSnapshot::take(&host.shell, cx);
        let input = |value: &str, window: &mut Window, cx: &mut Context<Self>| {
            let value = value.to_string();
            cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(value.clone());
                s.set_value(value, window, cx);
                s
            })
        };
        let background = input("#ffffff", window, cx);
        let dest = input("~/Pictures/Export", window, cx);
        let slider = |max: f32, cx: &mut Context<Self>| cx.new(|_| SliderState::new().max(max).min(0.).step(1.).default_value(0.));
        let border_slider = slider(64., cx);
        let corner_slider = slider(128., cx);
        let mut subs = vec![
            dialog::close_on_switch(&host.model, window, cx),
            cx.subscribe(&background, |_, _, _: &InputEvent, cx| cx.notify()),
            cx.subscribe_in(&border_slider, window, |this: &mut Self, _, e: &SliderEvent, _, cx| {
                if let SliderEvent::Change(v) = e {
                    this.border = v.start().round().clamp(0., 64.) as u32;
                    cx.notify();
                }
            }),
            cx.subscribe_in(&corner_slider, window, |this: &mut Self, _, e: &SliderEvent, _, cx| {
                if let SliderEvent::Change(v) = e {
                    this.corner = v.start().round().clamp(0., 128.) as u32;
                    cx.notify();
                }
            }),
        ];
        if let Some(images) = &host.images {
            subs.push(cx.observe(images, |_, _, cx| cx.notify()));
        }
        let mut this = CollageDialog {
            host,
            backend,
            snapshot,
            canvas: CollageCanvas::new(),
            aspect: "1:1",
            width: 2048,
            background,
            border: 0,
            corner: 0,
            format: Format::Jpeg,
            save_to: SaveTo::Library,
            dest,
            border_slider,
            corner_slider,
            busy: false,
            arranging: false,
            output: None,
            saved_to_library: false,
            error: None,
            arrange_attempt: 0,
            render_attempt: 0,
            bounds: Rc::default(),
            aspects: HashMap::new(),
            _subscriptions: subs,
        };
        // Seed the canvas with the mosaic on open.
        if this.photos().len() >= 2 {
            this.auto_arrange(cx);
        }
        this
    }

    pub fn photos(&self) -> &[Picked] {
        &self.snapshot.photos
    }

    fn background_text(&self, cx: &gpui_kit::App) -> String {
        self.background.read(cx).value().trim().to_string()
    }

    /// Auto-arrange: the justified mosaic, unlocked (`Mosaic`).
    pub fn auto_arrange(&mut self, cx: &mut Context<Self>) {
        let Some(catalog) = self.snapshot.catalog else {
            self.error = Some(NO_ROWS.into());
            cx.notify();
            return;
        };
        self.arranging = true;
        self.error = None;
        self.arrange_attempt += 1;
        let attempt = self.arrange_attempt;
        let opts = CollageOptionsDto {
            width: 2000,
            aspect: Some(self.aspect.to_string()),
            row_height: 460,
            gap: 10,
            background: self.background_text(cx),
            fit: "contain".into(),
            border_width: self.border,
            corner_radius: self.corner,
        };
        let (state, ids, previews) = (self.host.app.clone(), self.snapshot.ids(), self.backend.previews.clone());
        let rx = Runner::get(cx).run(move || auto_arrange(&state, Some(catalog), &ids, &opts, &previews));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("Auto-arrange stopped unexpectedly".into()));
            this.update(cx, |d, cx| {
                if d.arrange_attempt != attempt {
                    return;
                }
                d.arranging = false;
                match result {
                    Ok(placements) => d.canvas.set_arranged(placements.iter().map(from_dto).collect()),
                    Err(e) => d.error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Apply template `id` (fixed slots, locked).
    pub fn apply_template(&mut self, id: &str, cx: &mut Context<Self>) {
        let ids = self.snapshot.ids();
        self.error = self.canvas.apply_template(id, &ids).err();
        cx.notify();
    }

    /// Render or Save to library.
    pub fn run(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.output = None;
        self.saved_to_library = false;
        if self.canvas.placements.is_empty() {
            self.error = Some("Auto-arrange or place at least one photo first.".into());
            cx.notify();
            return;
        }
        let dest = self.dest.read(cx).value().trim().to_string();
        if self.save_to == SaveTo::Folder && dest.is_empty() {
            self.error = Some("Choose an output folder.".into());
            cx.notify();
            return;
        }
        let Some(catalog) = self.snapshot.catalog else {
            self.error = Some(NO_ROWS.into());
            cx.notify();
            return;
        };
        let opts = FreeformOptionsDto {
            width: self.width,
            height: output_height(self.width, aspect_ratio(self.aspect)),
            background: self.background_text(cx),
            border_width: self.border,
            corner_radius: self.corner,
        };
        let placements: Vec<PlacementDto> = self.canvas.placements.iter().map(to_dto).collect();
        let (state, previews, format, kind, save_to) =
            (self.host.app.clone(), self.backend.previews.clone(), self.format.wire(), self.canvas.layout_kind.clone(), self.save_to);
        self.busy = true;
        self.render_attempt += 1;
        let attempt = self.render_attempt;
        let rx = Runner::get(cx).run(move || match save_to {
            SaveTo::Library => save_to_catalog(&state, Some(catalog), &placements, &opts, format, &kind, &previews).map(|_| None),
            SaveTo::Folder => make_freeform(&state, Some(catalog), &placements, &opts, format, &dest, &previews).map(Some),
        });
        let host = self.host.clone();
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("The collage render stopped unexpectedly".into()));
            if result.as_ref().is_ok_and(Option::is_none) {
                // Saved into the library: the new photo appears in the grid.
                cx.update(|cx| {
                    host.status("Collage saved to your library.", cx);
                    host.shell.update(cx, |s, cx| s.refresh_rows(cx));
                });
            }
            let landed = this.update(cx, |d, cx| {
                if d.render_attempt != attempt {
                    return;
                }
                d.busy = false;
                match result.clone() {
                    Ok(Some(path)) => d.output = Some(path),
                    Ok(None) => d.saved_to_library = true,
                    Err(e) => d.error = Some(e),
                }
                cx.notify();
            });
            if landed.is_err() {
                match result {
                    Ok(Some(path)) => cx.update(|cx| host.status(format!("Collage saved to {}", path.display()), cx)),
                    Err(e) => cx.update(|cx| host.status(format!("Collage failed: {e}"), cx)),
                    Ok(None) => {}
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        dialog::pick_folder(
            "Choose the output folder",
            window,
            cx,
            |d, path, window, cx| d.dest.update(cx, |i, cx| i.set_value(path, window, cx)),
            |d, e, cx| {
                d.error = Some(e);
                cx.notify();
            },
        );
    }

    fn rect(&self) -> Option<CanvasRect> {
        let b = self.bounds.get()?;
        Some(CanvasRect {
            left: f64::from(b.origin.x),
            top: f64::from(b.origin.y),
            width: f64::from(b.size.width),
            height: f64::from(b.size.height),
        })
    }

    fn begin(&mut self, id: i64, mode: GestureMode, e: &MouseDownEvent, cx: &mut Context<Self>) {
        let aspect = self.aspects.get(&id).copied().unwrap_or(1.0);
        self.canvas.begin(id, mode, f64::from(e.position.x), f64::from(e.position.y), aspect);
        cx.notify();
    }

    fn chip(&self, id: SharedString, label: impl Into<SharedString>, on: bool, enabled: bool, colors: Colors, cx: &mut Context<Self>, f: impl Fn(&mut Self, &mut Context<Self>) + 'static) -> AnyElement {
        let chip = ui::chip(id, label, enabled, colors);
        let chip = if on { chip.border_color(colors.accent).text_color(colors.txt) } else { chip };
        ui::clickable(chip, enabled, cx.listener(move |d, _, _, cx| {
            f(d, cx);
            cx.notify();
        }))
    }

    fn render_canvas(&mut self, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let (disp_w, disp_h) = display_size(aspect_ratio(self.aspect));
        let bg = to_hsla(parse_color(&self.background_text(cx)));
        let sorted = self.canvas.sorted();
        let images: HashMap<i64, ImageState> = match &self.host.images {
            Some(images) => images.update(cx, |store, _| {
                let wanted: Vec<_> = sorted.iter().map(|p| (p.photo_id, ImageKind::Thumb)).collect();
                store.request_batch(&wanted);
                sorted.iter().map(|p| (p.photo_id, store.get(p.photo_id, ImageKind::Thumb))).collect()
            }),
            None => HashMap::new(),
        };
        for (id, state) in &images {
            if let ImageState::Ready(l) = state {
                let s = l.image.size(0);
                if s.height.0 > 0 {
                    self.aspects.insert(*id, s.width.0 as f64 / s.height.0 as f64);
                }
            }
        }
        let locked = self.canvas.locked;
        let tiles = sorted.iter().map(|p| {
            let (bw, bh) = (p.w * disp_w, p.h * disp_h);
            let id = p.photo_id;
            let selected = self.canvas.selected == Some(id);
            let swap = self.canvas.swap_target == Some(id);
            let tile = div()
                .id(("collage-tile", id as u64))
                .absolute()
                .left(px((p.x * disp_w) as f32))
                .top(px((p.y * disp_h) as f32))
                .w(px(bw as f32))
                .h(px(bh as f32))
                .overflow_hidden()
                .bg(colors.panel)
                .when(selected || swap, |t| t.border_2().border_color(colors.accent))
                .on_mouse_down(MouseButton::Left, cx.listener(move |d, e: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let mode = body_gesture(e.modifiers.shift, d.canvas.locked);
                    d.begin(id, mode, e, cx);
                }))
                .on_scroll_wheel(cx.listener(move |d, e: &ScrollWheelEvent, _, cx| {
                    cx.stop_propagation();
                    // Browser deltaY: positive scrolls down, which zooms out.
                    let dy = -f64::from(e.delta.pixel_delta(px(16.)).y);
                    d.canvas.wheel(id, dy);
                    cx.notify();
                }));
            let tile = match images.get(&id) {
                Some(ImageState::Ready(l)) => {
                    let aspect = self.aspects.get(&id).copied().unwrap_or(1.0);
                    let (l_, t_, w, h) = cover_rect(bw, bh, aspect, p.zoom, p.ox, p.oy);
                    tile.child(
                        img(l.image.clone())
                            .absolute()
                            .left(px(l_ as f32))
                            .top(px(t_ as f32))
                            .w(px(w as f32))
                            .h(px(h as f32))
                            .object_fit(ObjectFit::Fill),
                    )
                }
                _ => tile,
            };
            let tile = if !locked && selected {
                tile.child(
                    div()
                        .id(("collage-resize", id as u64))
                        .absolute()
                        .right_0()
                        .bottom_0()
                        .w(px(12.))
                        .h(px(12.))
                        .bg(colors.accent)
                        .cursor_nwse_resize()
                        .on_mouse_down(MouseButton::Left, cx.listener(move |d, e: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            d.begin(id, GestureMode::Resize, e, cx);
                        }))
                        .test_support(),
                )
            } else {
                tile
            };
            tile.test_support().into_any_element()
        });
        let bounds = self.bounds.clone();
        let this = cx.weak_entity();
        let dragging = self.canvas.dragging();
        let capture = canvas(
            move |b, _, _| bounds.set(Some(b)),
            move |_, _, window, _| {
                if !dragging {
                    return;
                }
                let this2 = this.clone();
                window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        this2
                            .update(cx, |d, cx| {
                                if let Some(rect) = d.rect() {
                                    d.canvas.pointer_move(f64::from(e.position.x), f64::from(e.position.y), rect);
                                    cx.notify();
                                }
                            })
                            .ok();
                    }
                });
                let this2 = this.clone();
                window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble && e.button == MouseButton::Left {
                        this2
                            .update(cx, |d, cx| {
                                d.canvas.pointer_up();
                                cx.notify();
                            })
                            .ok();
                    }
                });
            },
        )
        .absolute()
        .size_full();
        div()
            .flex()
            .justify_center()
            .child(
                div()
                    .id("collage-canvas")
                    .relative()
                    .w(px(disp_w as f32))
                    .h(px(disp_h as f32))
                    .overflow_hidden()
                    .bg(bg)
                    .border_1()
                    .border_color(colors.border)
                    .on_mouse_down(MouseButton::Left, cx.listener(|d, _, _, cx| {
                        d.canvas.deselect();
                        cx.notify();
                    }))
                    .child(capture)
                    .children(tiles)
                    .test_support(),
            )
            .into_any_element()
    }
}

fn to_dto(p: &Placement) -> PlacementDto {
    PlacementDto {
        photo_id: p.photo_id,
        x: p.x as f32,
        y: p.y as f32,
        w: p.w as f32,
        h: p.h as f32,
        z: p.z,
        ox: p.ox as f32,
        oy: p.oy as f32,
        zoom: p.zoom as f32,
    }
}

fn from_dto(p: &PlacementDto) -> Placement {
    Placement {
        photo_id: p.photo_id,
        x: p.x as f64,
        y: p.y as f64,
        w: p.w as f64,
        h: p.h as f64,
        z: p.z,
        ox: p.ox as f64,
        oy: p.oy as f64,
        zoom: p.zoom as f64,
    }
}

fn to_hsla([r, g, b, a]: [u8; 4]) -> Hsla {
    rgba(u32::from_be_bytes([r, g, b, a])).into()
}

impl Render for CollageDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let body = ui::body().id("collage-dialog");
        if self.photos().len() < 2 {
            return body
                .child(div().id("collage-hint").child(ui::sub("Select at least 2 photos in the library, then open Make collage again.", colors)).test_support())
                .test_support();
        }
        let has_selection = self.canvas.selected.is_some();
        let mut templates = ui::row().child(ui::label("Template", colors));
        for t in &TEMPLATES {
            let id = t.id;
            templates = templates.child(self.chip(format!("collage-template-{id}").into(), t.label, false, true, colors, cx, move |d, cx| d.apply_template(id, cx)));
        }
        let arranging = self.arranging;
        let tools = ui::row()
            .child(self.chip("collage-arrange".into(), if arranging { "Arranging…" } else { "Auto-arrange" }, false, !arranging, colors, cx, |d, cx| d.auto_arrange(cx)))
            .child(self.chip("collage-front".into(), "Front", false, has_selection, colors, cx, |d, _| d.canvas.bring_to_front()))
            .child(self.chip("collage-back".into(), "Back", false, has_selection, colors, cx, |d, _| d.canvas.send_to_back()))
            .child(toggle("collage-lock", self.canvas.locked, "Lock layout", colors, cx.listener(|d, _, _, cx| {
                d.canvas.locked = !d.canvas.locked;
                cx.notify();
            })));
        let hint = if self.canvas.locked {
            "Locked: drag a photo onto another to swap (drop onto the big cell to set the feature) · Shift-drag to reposition · scroll to zoom. Uncheck Lock layout to move/resize freely."
        } else {
            "Drag to move · corner to resize · Shift-drag to reposition the photo in its frame · scroll to zoom · tiles can overlap."
        };
        let canvas = self.render_canvas(colors, cx);
        let mut body = body.child(templates).child(tools).child(ui::sub(hint, colors)).child(canvas);

        let mut aspects = ui::row().child(ui::label("Aspect", colors));
        for (a, _, _) in ASPECTS {
            aspects = aspects.child(self.chip(format!("collage-aspect-{a}").into(), a, self.aspect == a, true, colors, cx, move |d, _| d.aspect = a));
        }
        let mut widths = ui::row();
        for w in WIDTH_PRESETS {
            widths = widths.child(self.chip(format!("collage-width-{w}").into(), format!("{w}px wide"), self.width == w, true, colors, cx, move |d, _| d.width = w));
        }
        let out_h = output_height(self.width, aspect_ratio(self.aspect));
        body = body.child(aspects).child(widths.child(ui::sub(format!("Output {}×{out_h}.", self.width), colors)));

        let mut bg = ui::row().child(ui::label("Background", colors)).child(div().w(px(110.)).child(Input::new(&self.background)));
        for s in SWATCHES {
            let color = to_hsla(parse_color(s));
            bg = bg.child(
                div()
                    .id(SharedString::from(format!("collage-bg-{}", &s[1..])))
                    .w(px(18.))
                    .h(px(18.))
                    .rounded(px(3.))
                    .border_1()
                    .border_color(colors.border)
                    .bg(color)
                    .cursor_pointer()
                    .on_click(cx.listener(move |d, _, window, cx| d.background.update(cx, |i, cx| i.set_value(s, window, cx))))
                    .test_support(),
            );
        }
        body = body
            .child(bg)
            .child(
                ui::row()
                    .child(ui::label(format!("Border {} px", self.border), colors))
                    .child(div().w(px(140.)).child(Slider::new(&self.border_slider)))
                    .child(ui::label(format!("Corner radius {} px", self.corner), colors))
                    .child(div().w(px(140.)).child(Slider::new(&self.corner_slider))),
            )
            .child(
                ui::row()
                    .child(ui::label("Format", colors))
                    .child(self.chip("collage-format-jpeg".into(), "JPEG", self.format == Format::Jpeg, true, colors, cx, |d, _| d.format = Format::Jpeg))
                    .child(self.chip("collage-format-png".into(), "PNG (alpha)", self.format == Format::Png, true, colors, cx, |d, _| d.format = Format::Png)),
            );
        if self.corner > 0 && self.format != Format::Png {
            body = body.child(ui::sub("Rounded corners need PNG — JPEG flattens them onto the background color.", colors));
        }
        body = body.child(
            ui::row()
                .child(ui::label("Save to", colors))
                .child(self.chip("collage-save-library".into(), "Library (catalog)", self.save_to == SaveTo::Library, true, colors, cx, |d, _| d.save_to = SaveTo::Library))
                .child(self.chip("collage-save-folder".into(), "Folder", self.save_to == SaveTo::Folder, true, colors, cx, |d, _| d.save_to = SaveTo::Folder)),
        );
        if self.save_to == SaveTo::Folder {
            body = body.child(
                ui::row()
                    .child(div().w(px(380.)).child(Input::new(&self.dest)))
                    .child(ui::clickable(ui::chip("collage-browse", "Browse…", true, colors), true, cx.listener(|d, _, window, cx| d.browse(window, cx)))),
            );
        }
        let (busy, library) = (self.busy, self.save_to == SaveTo::Library);
        let label = match (busy, library) {
            (true, true) => "Saving…",
            (true, false) => "Rendering…",
            (false, true) => "Save to library",
            (false, false) => "Render",
        };
        let enabled = !busy && !self.canvas.placements.is_empty();
        body = body.child(ui::row().child(ui::clickable(ui::primary("collage-run", label, enabled, colors), enabled, cx.listener(|d, _, _, cx| d.run(cx)))));
        if self.saved_to_library {
            body = body.child(div().id("collage-saved").child(ui::sub("Saved to your library. Close to see it in the grid.", colors)).test_support());
        }
        if let Some(path) = self.output.clone() {
            body = body.child(
                ui::row()
                    .id("collage-output")
                    .child(ui::sub(format!("Saved to {}", path.display()), colors))
                    .child(ui::clickable(ui::chip("collage-reveal", "Reveal", true, colors), true, move |_, _, cx| cx.reveal_path(&path)))
                    .test_support(),
            );
        }
        if let Some(e) = &self.error {
            body = body.child(ui::error("collage-error", e.clone(), colors));
        }
        body.test_support()
    }
}
