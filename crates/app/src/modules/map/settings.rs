//! The Map module's settings panel (`MapSettings` in map.tsx) and the inspector's
//! "Geocode" panel (`GeocodePanelContent`).
//!
//! **Settings:** the tile URL (Save / Reset to default; an unusable template is refused with
//! the reason), the attribution note, this machine's per-host tile answers (allow, block,
//! forget — the "change it in Preferences" half of decision #118; the panel is the Map tab
//! of Preferences), and "Geocode all with GPS" with its
//! progress and Cancel. **Geocode panel:** fills the active photo's empty IPTC location fields.
//!
//! Both reverse-geocoding actions are user-initiated, as in React; that click is their
//! network opt-in (#118 left Nominatim's prompt undecided).

use super::state::MapState;
use crate::shell::style::Colors;
use crate::shell::ShellState;
use crate::storage::ui;
use chairphoto_core::plugins::map::tiles::source::{DEFAULT_TILE_URL, OSM_ATTRIBUTION};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, FontWeight, SharedString, Subscription, TestSupportExt as _, Window};

pub struct MapSettings {
    pub state: Entity<MapState>,
    pub url: Entity<InputState>,
    /// The stored URL the input was last filled from (refilled when it changes elsewhere).
    shown: Option<String>,
    pub note: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl MapSettings {
    pub fn new(state: Entity<MapState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let url = cx.new(|cx| InputState::new(window, cx).placeholder(DEFAULT_TILE_URL));
        let enter = cx.subscribe_in(&url, window, |this: &mut Self, _, e: &InputEvent, window, cx| {
            if matches!(e, InputEvent::PressEnter { .. }) {
                this.save(window, cx);
            }
        });
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        MapSettings { state, url, shown: None, note: None, _subscriptions: vec![enter, observe] }
    }

    pub fn save(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let url = self.url.read(cx).value().to_string();
        let saved = self.state.update(cx, |s, cx| s.set_tile_url(&url, cx));
        self.note = Some(match saved {
            Ok(()) => "Saved.".into(),
            Err(e) => e,
        });
        cx.notify();
    }

    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.url.update(cx, |i, cx| i.set_value(DEFAULT_TILE_URL, window, cx));
        self.save(window, cx);
    }
}

impl Render for MapSettings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let s = self.state.read(cx);
        let template = s.source.template().to_string();
        let known = s.settings_known();
        let hosts: Vec<(String, bool)> = s.host_consent().hosts().map(|(h, a)| (h.to_string(), a)).collect();
        let consent_write_error = s.consent_write_error().map(str::to_string);
        let geocode = s.geocode.clone();
        if known && self.shown.as_deref() != Some(template.as_str()) {
            self.shown = Some(template.clone());
            self.url.update(cx, |i, cx| i.set_value(template.clone(), window, cx));
        }
        let mut body = ui::body()
            .id("map-settings")
            .text_size(px(12.))
            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child("Map"))
            .child(ui::label("Tile source URL", colors))
            .child(Input::new(&self.url))
            .child(ui::sub(
                "A tile URL with {z}, {x} and {y} placeholders. Default: OpenStreetMap. Point it at a \
                 self-hosted tile server or a commercial provider for heavy use (OpenStreetMap's tile policy \
                 limits high-volume access). A new server is asked about before its first tile loads.",
                colors,
            ))
            .child(
                ui::row()
                    .child(ui::clickable(
                        ui::primary("map-url-save", "Save", true, colors),
                        true,
                        cx.listener(|this, _, window, cx| this.save(window, cx)),
                    ))
                    .child(ui::clickable(
                        ui::chip("map-url-reset", "Reset to default", true, colors),
                        true,
                        cx.listener(|this, _, window, cx| this.reset(window, cx)),
                    )),
            )
            .when_some(self.note.clone(), |d, n| d.child(div().id("map-url-note").text_color(colors.dim).child(n).test_support()))
            .child(ui::sub(
                format!(
                    "Attribution: OpenStreetMap tiles are provided under the ODbL licence. The map always shows \
                     \u{201c}Tiles {OSM_ATTRIBUTION}\u{201d} while tiles are shown."
                ),
                colors,
            ))
            .child(ui::label("Tile servers", colors))
            .child(ui::sub("Answers are remembered on this computer, for every catalog.", colors));
        if let Some(e) = consent_write_error {
            body = body.child(ui::error(
                "map-consent-write-error",
                format!(
                    "This machine's tile-server preferences could not be saved ({e}). The change you just made \
                     here applies only for this session and may be asked again after a restart; any answer \
                     saved earlier is unaffected."
                ),
                colors,
            ));
        }
        if hosts.is_empty() {
            body = body.child(ui::sub("No tile server has been allowed or blocked yet.", colors));
        }
        for (host, allowed) in hosts {
            let (h1, h2, h3) = (host.clone(), host.clone(), host.clone());
            body = body.child(
                ui::row()
                    .id(SharedString::from(format!("map-host-{host}")))
                    .child(div().text_color(colors.txt).child(host.clone()))
                    .child(div().text_color(if allowed { colors.ok } else { colors.danger }).child(if allowed { "allowed" } else { "blocked" }))
                    .child(ui::clickable(
                        ui::chip(SharedString::from(format!("map-host-toggle-{host}")), if allowed { "Block" } else { "Allow" }, true, colors),
                        true,
                        cx.listener(move |this, _, _, cx| {
                            let h = h1.clone();
                            this.state.update(cx, |s, cx| s.set_consent(&h, Some(!allowed), cx));
                        }),
                    ))
                    .child(ui::clickable(
                        ui::chip(SharedString::from(format!("map-host-forget-{h2}")), "Ask again", true, colors),
                        true,
                        cx.listener(move |this, _, _, cx| {
                            let h = h3.clone();
                            this.state.update(cx, |s, cx| s.set_consent(&h, None, cx));
                        }),
                    ))
                    .test_support(),
            );
        }
        let pct = (geocode.total > 0).then(|| geocode.done * 100 / geocode.total);
        body = body
            .child(div().pt(px(10.)).font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child("Reverse geocoding"))
            .child(ui::sub(
                "Fill empty IPTC location fields (City, State/Province, Country, Country code) for every photo \
                 that has GPS coordinates but missing location metadata. Uses OSM Nominatim (max 1 request/s). \
                 Existing values are never overwritten.",
                colors,
            ))
            .child(
                ui::row()
                    .child(ui::clickable(
                        ui::primary("map-geocode-all", if geocode.busy { "Geocoding…" } else { "Geocode all with GPS" }, !geocode.busy, colors),
                        !geocode.busy,
                        cx.listener(|this, _, _, cx| this.state.update(cx, |s, cx| s.geocode_all(cx))),
                    ))
                    .when(geocode.busy, |d| {
                        d.child(ui::clickable(
                            ui::chip("map-geocode-cancel", "Cancel", true, colors),
                            true,
                            cx.listener(|this, _, _, cx| this.state.update(cx, |s, cx| s.cancel_geocode(cx))),
                        ))
                    })
                    .when_some(pct, |d, p| d.child(div().text_color(colors.dim).child(format!("{p}%")))),
            )
            .when(!geocode.status.is_empty(), |d| {
                d.child(div().id("map-geocode-status").text_color(colors.dim).child(geocode.status.clone()).test_support())
            });
        body.test_support()
    }
}

/// The inspector's "Geocode" block: the active photo's "Geocode location".
pub struct GeocodePanel {
    state: Entity<MapState>,
    shell: Entity<ShellState>,
    busy: bool,
    pub status: String,
    _observe: Subscription,
}

impl GeocodePanel {
    pub fn new(state: Entity<MapState>, shell: Entity<ShellState>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&shell, |_, _, cx| cx.notify());
        GeocodePanel { state, shell, busy: false, status: String::new(), _observe }
    }

    fn geocode(&mut self, photo: i64, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        // The photo id is the shown catalog's: the fill is bound to it (fails closed after a
        // switch, writing neither the new catalog's row nor any sidecar).
        let Some(from) = self.state.read(cx).catalog() else {
            self.status = "The catalog is still loading; try again.".into();
            cx.notify();
            return;
        };
        self.busy = true;
        self.status = "Geocoding…".into();
        cx.notify();
        let generation = self.state.read(cx).generation();
        let app = self.state.read(cx).app().clone();
        // Network (Nominatim): the core runtime, never the UI thread.
        let task = chairphoto_core::app::runtime()
            .spawn(async move { chairphoto_core::plugins::map::geocode::geocode_photo_to_iptc(&app, Some(from), photo).await });
        cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
            this.update(cx, |p, cx| {
                p.busy = false;
                if p.state.read(cx).generation() != generation {
                    p.status.clear(); // another catalog now: this photo id means something else
                    cx.notify();
                    return;
                }
                let (status, changed) = geocode_status(&result);
                if changed {
                    p.state.update(cx, |s, cx| s.catalog_changed(cx));
                }
                p.status = status;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// The status line for a single-photo geocode, and whether the catalog changed (so the
/// catalog view re-reads). A location stored in the catalog with its sidecar pending is not
/// an error, and the catalog did change: the core types it (`GeocodeOutcome`, review of
/// #153, M2), so nothing here reads a message's wording (React's `singleGeocodeOutcome`).
pub fn geocode_status(
    result: &Result<chairphoto_core::plugins::map::geocode::GeocodeOutcome, String>,
) -> (String, bool) {
    match result {
        Ok(outcome) => (outcome.status(), outcome.filled),
        Err(e) => (format!("Error: {e}"), false),
    }
}

impl Render for GeocodePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let Some(photo) = self.shell.read(cx).library.selection().active_id else {
            return ui::empty("map-geocode-none", "No photo selected", colors);
        };
        div()
            .id("map-geocode-panel")
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(ui::clickable(
                ui::chip("map-geocode-one", if self.busy { "Geocoding…" } else { "Geocode location" }, !self.busy, colors),
                !self.busy,
                cx.listener(move |this, _, _, cx| this.geocode(photo, cx)),
            ))
            .when(!self.status.is_empty(), |d| d.child(ui::sub(self.status.clone(), colors)))
            .test_support()
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::geocode_status;
    use chairphoto_core::catalog::IptcSidecarState;
    use chairphoto_core::plugins::map::geocode::GeocodeOutcome;

    fn outcome(filled: bool, sidecar: IptcSidecarState, reason: Option<&str>) -> Result<GeocodeOutcome, String> {
        Ok(GeocodeOutcome { filled, sidecar, reason: reason.map(Into::into) })
    }

    /// #153 (review of #148, and of #153, M2): a location stored in the catalog with its
    /// sidecar pending is shown without an "Error: " prefix and re-reads the catalog view; a
    /// real failure keeps the prefix and changes nothing; a fill re-reads, a no-op does not.
    #[test]
    fn a_pending_geocode_is_not_an_error_and_refreshes_the_catalog() {
        let (line, changed) = geocode_status(&outcome(true, IptcSidecarState::Pending, Some("read-only")));
        assert!(changed);
        assert_eq!(
            line,
            "Geocoded location stored in the catalog, but not yet in the sidecar (read-only); the repair pass will write it."
        );
        assert_eq!(geocode_status(&Err("geocode: HTTP 500".into())), ("Error: geocode: HTTP 500".into(), false));
        assert_eq!(geocode_status(&outcome(true, IptcSidecarState::Written, None)), ("Location fields filled.".into(), true));
        assert!(!geocode_status(&outcome(false, IptcSidecarState::Unchanged, None)).1);
    }
}
