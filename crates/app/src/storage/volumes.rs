//! Volumes (`VolumesPanel.tsx`, Preferences → Storage): each registered volume with its kind
//! ("NAS / backup" / "local"), reachable/offline and base path; Remove — behind a confirm —
//! on every volume but the library folder (`catalog-root`); Add with name, base path (`~`
//! expands) and kind, Enter in the path adds.
//!
//! A view entity Preferences mounts in its Storage tab ([`crate::preferences`], #113), the
//! one way in, as in React. Reachability is stated off the catalog
//! lock through the volume-health cache, which add and remove invalidate, as the Tauri
//! commands do.

use super::ui;
use super::{CloseDialog, Runner};
use crate::shell::style::Colors;
use chairphoto_core::app::{expand_home, with_catalog, with_catalog_as, with_catalog_identified, AppState, CatalogIdentity};
use chairphoto_core::catalog::{Volume, VolumeKind};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::TestSupportExt as _;
use gpui_kit::{div, px, Context, Entity, EventEmitter, SharedString, Subscription, Window};

/// The library folder's volume: never removable here.
pub const LIBRARY_VOLUME: &str = "catalog-root";

pub struct VolumesPanel {
    app: AppState,
    pub volumes: Vec<Volume>,
    /// The catalog `volumes` were read from: a Remove of one of them is bound to it.
    pub volumes_from: Option<CatalogIdentity>,
    pub name: Entity<InputState>,
    pub path: Entity<InputState>,
    pub kind: VolumeKind,
    pub error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for VolumesPanel {}

impl VolumesPanel {
    pub fn new(app: AppState, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Name (e.g. NAS)"));
        let path = cx.new(|cx| InputState::new(window, cx).placeholder("Base path (e.g. ~/ZimaCube/Gallery)"));
        let enter = cx.subscribe_in(&path, window, |this: &mut Self, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.add(window, cx);
            }
        });
        let mut this = VolumesPanel {
            app,
            volumes: Vec::new(),
            volumes_from: None,
            name,
            path,
            kind: VolumeKind::Backup,
            error: None,
            _subscriptions: vec![enter],
        };
        this.reload(cx);
        this
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let state = self.app.clone();
        let rx = Runner::get(cx).run(move || {
            let (from, mut vols) = with_catalog_identified(&state, |c| c.volume_rows())?;
            let pairs: Vec<(i64, String)> = vols.iter().map(|v| (v.id, v.base_path.clone())).collect();
            let reachable = state.volume_health.refresh(&pairs);
            for v in &mut vols {
                v.reachable = reachable.get(&v.id).copied().unwrap_or(false);
            }
            Ok::<_, String>((from, vols))
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                match result {
                    Ok((from, v)) => {
                        s.volumes = v;
                        s.volumes_from = Some(from);
                    }
                    Err(e) => s.error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn toggle_kind(&mut self, cx: &mut Context<Self>) {
        self.kind = match self.kind {
            VolumeKind::Backup => VolumeKind::Local,
            VolumeKind::Local => VolumeKind::Backup,
        };
        cx.notify();
    }

    pub fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let name = self.name.read(cx).value().trim().to_string();
        let path = self.path.read(cx).value().trim().to_string();
        if name.is_empty() || path.is_empty() {
            self.error = Some("Name and path are required.".into());
            cx.notify();
            return;
        }
        let (state, kind) = (self.app.clone(), self.kind);
        let rx = Runner::get(cx).run(move || {
            let id = with_catalog(&state, |c| c.add_volume(&name, &expand_home(&path), kind))?;
            state.volume_health.invalidate();
            Ok::<_, String>(id)
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update_in(cx, |s, window, cx| {
                match result {
                    Ok(_) => {
                        s.name.update(cx, |i, cx| i.set_value("", window, cx));
                        s.path.update(cx, |i, cx| i.set_value("", window, cx));
                        s.reload(cx);
                    }
                    Err(e) => s.error = Some(e),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Remove, after the confirm React showed with `window.confirm` — from the catalog the
    /// list was read from: volume ids are per catalog, so once another catalog is open (the
    /// confirm is asynchronous) it fails closed with `CATALOG_CHANGED` and removes nothing.
    pub fn remove(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(v) = self.volumes.get(index).cloned() else { return };
        let Some(from) = self.volumes_from else { return };
        if v.name == LIBRARY_VOLUME {
            return;
        }
        self.error = None;
        let answer = ui::confirm(
            window,
            cx,
            format!("Remove volume \"{}\"?", v.name).into(),
            "Files on disk are NOT deleted, but the catalog forgets where copies on this volume live. Any photo \
             whose only copy is here will be unreachable until the volume is re-added."
                .into(),
            "Remove",
        );
        let state = self.app.clone();
        let runner = Runner::get(cx);
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(true) {
                return;
            }
            let rx = runner.run(move || {
                with_catalog_as(&state, from, |c| c.remove_volume(v.id))?;
                state.volume_health.invalidate();
                Ok::<_, String>(())
            });
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                if let Err(e) = result {
                    s.error = Some(e);
                }
                s.reload(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Render for VolumesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let mut body = ui::body().id("volumes-panel");
        for (i, v) in self.volumes.iter().enumerate() {
            let kind = match v.kind {
                VolumeKind::Backup => "NAS / backup",
                VolumeKind::Local => "local",
            };
            let row = ui::row()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(
                            ui::row()
                                .child(div().text_size(px(12.5)).text_color(colors.txt).child(v.name.clone()))
                                .child(div().text_size(px(10.5)).text_color(colors.mute).child(kind))
                                .child(
                                    div()
                                        .id(SharedString::from(format!("volume-state-{i}")))
                                        .text_size(px(10.5))
                                        .text_color(if v.reachable { colors.ok } else { colors.mute })
                                        .child(if v.reachable { "reachable" } else { "offline" })
                                        .test_support(),
                                ),
                        )
                        .child(ui::sub(v.base_path.clone(), colors)),
                )
                .child(if v.name == LIBRARY_VOLUME {
                    ui::sub("library folder", colors).into_any_element()
                } else {
                    ui::clickable(
                        ui::danger_chip(SharedString::from(format!("volume-remove-{i}")), "Remove", true, colors),
                        true,
                        cx.listener(move |s, _, window, cx| s.remove(i, window, cx)),
                    )
                    .into_any_element()
                });
            body = body.child(row);
        }
        let kind = match self.kind {
            VolumeKind::Backup => "NAS / backup",
            VolumeKind::Local => "local",
        };
        body = body
            .child(ui::sub("Add a volume (e.g. a NAS mount). “~” expands to your home.", colors))
            .child(
                ui::row()
                    .child(div().w(px(140.)).child(Input::new(&self.name).id("volume-name")))
                    .child(div().flex_1().child(Input::new(&self.path).id("volume-path")))
                    .child(ui::clickable(ui::chip("volume-kind", kind, true, colors), true, cx.listener(|s, _, _, cx| s.toggle_kind(cx))))
                    .child(ui::clickable(ui::primary("volume-add", "Add", true, colors), true, cx.listener(|s, _, window, cx| s.add(window, cx)))),
            );
        if let Some(e) = &self.error {
            body = body.child(ui::error("volumes-error", e.clone(), colors));
        }
        body
    }
}
