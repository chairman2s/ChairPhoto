//! Preferences → Editors: the external editors (`EditorsSection`: darktable / RawTherapee /
//! ART with GUI and CLI path overrides, RapidRAW with its binary and output format) and the
//! Darkroom (`DarkroomSection`: the `.rawf` decode cache's size, use and Clear, neighbour
//! preload, white balance for new edits, the export-parity line, and the dev render-timing
//! switch with the last drag summary). The GlSpike probe is dropped (parity.md).
//!
//! Every key is React's: `editor.<key>.gui` / `.cli`, `editor.rapidraw.bin` / `.format`,
//! `develop.decodeCacheGb`, `develop.preloadNeighbours`, `develop.wbSlider`,
//! `metrics.exportParity`, `editor.renderTiming`, `editor.renderTiming.lastSummary`.
//!
//! A saved editor path, RapidRAW binary or format tells the model ([`AppModel::editors_changed`])
//! once its worker has run, and the inspector re-checks its "Edit in" list. The notice does
//! not depend on this section (`Ctx::write_setting_landed`): saves happen on blur, so the
//! section is often gone by then — its tab switched, Preferences closed.
//!
//! [`AppModel::editors_changed`]: crate::model::AppModel::editors_changed

use super::{section, status, Ctx, Scope};
use crate::shell::style::Colors;
use crate::storage::ui;
use chairphoto_core::external_edit::{available_editors, AvailableEditor};
use chairphoto_core::rapidraw::rapidraw_available;
use chairphoto_model::darkroom::kelvin::{WbPrefer, WB_SLIDER_KEY};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::Disableable as _;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, Context, Entity, SharedString, Subscription, Window};

/// The decode cache's size limit in GB (`develop::cache::BUDGET_KEY`, only compiled with
/// `raw` + `edit`; the setting is read by `develop_open` either way).
pub const DECODE_CACHE_GB_KEY: &str = "develop.decodeCacheGb";
pub const DEFAULT_DECODE_CACHE_GB: f64 = 20.;
/// Neighbour preload, default on (`develop::session::PRELOAD_KEY`).
pub const PRELOAD_KEY: &str = "develop.preloadNeighbours";
/// The "export equals view" total the export commands keep (`commands::export`).
pub const EXPORT_PARITY_KEY: &str = "metrics.exportParity";
/// The dev render-timing switch and its last drag summary (`renderTiming.ts`).
pub const RENDER_TIMING_KEY: &str = "editor.renderTiming";
pub const RENDER_TIMING_SUMMARY_KEY: &str = "editor.renderTiming.lastSummary";
pub const RAPIDRAW_BIN_KEY: &str = "editor.rapidraw.bin";
pub const RAPIDRAW_FORMAT_KEY: &str = "editor.rapidraw.format";
/// RapidRAW's output formats and their labels.
pub const RAPIDRAW_FORMATS: [(&str, &str); 3] = [("tiff", "TIFF (16-bit)"), ("png", "PNG (16-bit)"), ("jpg", "JPEG")];

/// Bytes as "3.4 GB" / "512 MB" (`formatCacheBytes`).
pub fn format_cache_bytes(bytes: u64) -> String {
    let gb = bytes as f64 / 1024f64.powi(3);
    if gb >= 1. {
        format!("{gb:.1} GB")
    } else {
        format!("{} MB", (bytes as f64 / 1024f64.powi(2)).round())
    }
}

/// The decode-cache size a setting means: a non-negative number of GB, else the default
/// (`parseCacheGb`).
pub fn parse_cache_gb(v: Option<&str>) -> f64 {
    match v.map(str::trim).filter(|v| !v.is_empty()).and_then(|v| v.parse::<f64>().ok()) {
        Some(n) if n.is_finite() && n >= 0. => n,
        _ => DEFAULT_DECODE_CACHE_GB,
    }
}

/// The export-parity metric in words, or `None` before any RAW-engine export was checked
/// (`formatExportParity`).
pub fn format_export_parity(v: Option<&str>) -> Option<String> {
    let t: serde_json::Value = serde_json::from_str(v?).ok()?;
    let checked = t.get("checked").and_then(|c| c.as_f64()).unwrap_or(0.);
    if checked <= 0. {
        return None;
    }
    let differing = t.get("differing").and_then(|c| c.as_f64()).unwrap_or(0.);
    let noun = if checked == 1. { "export" } else { "exports" };
    Some(if differing == 0. {
        format!("What you see is what you export: {checked} RAW {noun} checked, none differed from the view.")
    } else {
        format!("What you see is what you export: {checked} RAW {noun} checked, {differing} differed from the view.")
    })
}

// --- external editors ------------------------------------------------------------------------

/// One editor's path fields.
pub struct EditorPaths {
    pub key: String,
    pub gui: Entity<InputState>,
    pub cli: Entity<InputState>,
}

/// What the section reads when it opens: the editors with their stored paths, RapidRAW's
/// status and binary.
struct EditorsRead {
    editors: Vec<(AvailableEditor, String, String)>,
    rapidraw: Option<(bool, String)>,
    rapidraw_bin: String,
}

fn read_editors(scope: &Scope) -> Result<EditorsRead, String> {
    let list = available_editors(scope.state())?;
    let mut editors = Vec::new();
    for e in list {
        let (gui, cli) = scope.catalog(|c| {
            Ok((c.get_setting(&format!("editor.{}.gui", e.key))?, c.get_setting(&format!("editor.{}.cli", e.key))?))
        })?;
        editors.push((e, gui.unwrap_or_default(), cli.unwrap_or_default()));
    }
    let rapidraw = rapidraw_available(scope.state()).ok().map(|s| (s.available, s.format));
    let rapidraw_bin = scope.catalog(|c| c.get_setting(RAPIDRAW_BIN_KEY))?.unwrap_or_default();
    Ok(EditorsRead { editors, rapidraw, rapidraw_bin })
}

/// What an editor save does once its worker has run, whether or not the section is still
/// shown: on success, tell the model ([`AppModel::editors_changed`]), so the inspector re-reads
/// its "Edit in" list.
fn announce<T>(ctx: &Ctx) -> impl FnOnce(&Result<T, String>, &mut gpui_kit::App) + 'static {
    let model = ctx.model.clone();
    move |result, cx| {
        if result.is_ok() {
            model.update(cx, |m, cx| m.editors_changed(cx));
        }
    }
}

pub struct EditorsSection {
    ctx: Ctx,
    /// The editors and whether their GUI / CLI is found.
    pub editors: Vec<AvailableEditor>,
    pub paths: Vec<EditorPaths>,
    pub rapidraw_found: bool,
    pub rapidraw_bin: Entity<InputState>,
    pub rapidraw_format: String,
    pub status: Option<String>,
    /// Whether the stored values are in the fields. Until then a blur must not save: it
    /// would overwrite the stored binary with the still-empty field (and with a failed read,
    /// any blur would).
    pub loaded: bool,
    subscriptions: Vec<Subscription>,
}

impl EditorsSection {
    pub fn new(ctx: Ctx, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let rapidraw_bin = cx.new(|cx| InputState::new(window, cx).placeholder("RapidRAW (binary command / path)"));
        let blur = cx.subscribe_in(&rapidraw_bin, window, |this, input, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Blur) && this.loaded {
                let value = input.read(cx).value().to_string();
                this.save_rapidraw(RAPIDRAW_BIN_KEY, value, cx);
            }
        });
        ctx.run_in(window, cx, read_editors, |s: &mut Self, result, window, cx| match result {
            Ok(read) => s.loaded(read, window, cx),
            Err(e) => s.status = Some(e),
        });
        EditorsSection {
            ctx,
            editors: Vec::new(),
            paths: Vec::new(),
            rapidraw_found: false,
            rapidraw_bin,
            rapidraw_format: "tiff".into(),
            status: None,
            loaded: false,
            subscriptions: vec![blur],
        }
    }

    fn loaded(&mut self, read: EditorsRead, window: &mut Window, cx: &mut Context<Self>) {
        for (e, gui_path, cli_path) in &read.editors {
            let gui = cx.new(|cx| InputState::new(window, cx).placeholder(format!("{} (GUI command / path)", e.key)));
            let cli = cx.new(|cx| InputState::new(window, cx).placeholder(format!("{}-cli (CLI command / path)", e.key)));
            gui.update(cx, |i, cx| i.set_value(gui_path.clone(), window, cx));
            cli.update(cx, |i, cx| i.set_value(cli_path.clone(), window, cx));
            for (input, which) in [(&gui, "gui"), (&cli, "cli")] {
                let key = e.key.clone();
                self.subscriptions.push(cx.subscribe_in(input, window, move |this, input, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Blur) {
                        let value = input.read(cx).value().to_string();
                        this.save(&key, which, value, cx);
                    }
                }));
            }
            self.paths.push(EditorPaths { key: e.key.clone(), gui, cli });
        }
        self.editors = read.editors.into_iter().map(|(e, ..)| e).collect();
        if let Some((found, format)) = read.rapidraw {
            self.rapidraw_found = found;
            self.rapidraw_format = format;
        }
        self.rapidraw_bin.update(cx, |i, cx| i.set_value(read.rapidraw_bin, window, cx));
        self.loaded = true;
    }

    /// Save one path (blank = the auto-detected command on PATH), then re-check availability.
    pub fn save(&mut self, key: &str, which: &str, value: String, cx: &mut Context<Self>) {
        let ctx = self.ctx.clone();
        ctx.write_setting_landed(
            cx,
            format!("editor.{key}.{which}"),
            value.trim().to_string(),
            |scope| available_editors(scope.state()),
            announce(&ctx),
            |s: &mut Self, result, _| match result {
                Ok((_, editors)) => {
                    s.editors = editors;
                    s.status = Some("Saved.".into());
                }
                Err(e) => s.status = Some(e),
            },
        );
    }

    /// Save RapidRAW's binary or format, then re-check it. The format chip follows what was
    /// stored only on a format write: a binary write's re-check may run before a newer format
    /// write has persisted.
    pub fn save_rapidraw(&mut self, key: &'static str, value: String, cx: &mut Context<Self>) {
        let ctx = self.ctx.clone();
        ctx.write_setting_landed(
            cx,
            key,
            value.trim().to_string(),
            |scope| rapidraw_available(scope.state()),
            announce(&ctx),
            move |s: &mut Self, result, _| match result {
                Ok((_, st)) => {
                    s.rapidraw_found = st.available;
                    if key == RAPIDRAW_FORMAT_KEY {
                        s.rapidraw_format = st.format;
                    }
                    s.status = Some("Saved.".into());
                }
                Err(e) => s.status = Some(e),
            },
        );
    }

    pub fn set_rapidraw_format(&mut self, format: &str, cx: &mut Context<Self>) {
        self.rapidraw_format = format.into();
        cx.notify();
        self.save_rapidraw(RAPIDRAW_FORMAT_KEY, format.into(), cx);
    }
}

impl Render for EditorsSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let mut body = section("prefs-editors", "External editors", colors).child(ui::sub(
            "Send a photo to darktable, RawTherapee, or ART to develop it; when the editor closes, the result is \
             rendered via its command-line tool and stacked under the original. Leave a field blank to use the \
             auto-detected command on your PATH.",
            colors,
        ));
        for (e, p) in self.editors.iter().zip(&self.paths) {
            let line = format!(
                "{} — GUI {} · CLI {}",
                e.label,
                if e.gui { "✓" } else { "✗ not found" },
                if e.cli { "✓ (auto-render)" } else { "✗ (launch only)" }
            );
            body = body.child(
                div()
                    .id(SharedString::from(format!("editor-{}", p.key)))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(ui::sub(line, colors))
                    .child(
                        ui::row()
                            .child(div().flex_1().child(Input::new(&p.gui)))
                            .child(div().flex_1().child(Input::new(&p.cli))),
                    ),
            );
        }
        let mut formats = ui::row();
        for (value, label) in RAPIDRAW_FORMATS {
            let on = self.rapidraw_format == value;
            let chip = ui::chip(SharedString::from(format!("rapidraw-format-{value}")), label, true, colors)
                .when(on, |c| c.border_color(colors.accent).text_color(colors.accent));
            formats = formats.child(ui::clickable(chip, true, cx.listener(move |s, _, _, cx| s.set_rapidraw_format(value, cx))));
        }
        body.child(
            div()
                .id("editor-rapidraw")
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(ui::sub(
                    format!(
                        "RapidRAW — {} · round-trip (opens the photo, stacks the exported result on Done)",
                        if self.rapidraw_found { "✓ found" } else { "✗ not found" }
                    ),
                    colors,
                ))
                .child(ui::row().child(div().flex_1().child(Input::new(&self.rapidraw_bin).id("rapidraw-bin"))).child(formats)),
        )
        .children(self.status.clone().map(|t| status("editors-status", t, colors)))
    }
}

// --- the Darkroom ------------------------------------------------------------------------

/// What the Darkroom section reads when it opens.
struct DarkroomRead {
    gb: f64,
    preload: bool,
    wb: WbPrefer,
    parity: Option<String>,
    timing: bool,
    last_summary: String,
}

pub struct DarkroomSection {
    ctx: Ctx,
    pub gb: Entity<InputState>,
    /// `None` until read (the control is disabled meanwhile).
    pub preload: Option<bool>,
    pub wb: Option<WbPrefer>,
    /// Bytes the decode cache holds; `None` until known.
    pub usage: Option<u64>,
    pub parity: Option<String>,
    pub timing: Option<bool>,
    pub last_summary: String,
    /// Whether the stored cache size is in its field: until then Enter or a blur saves
    /// nothing (it would store the empty field's default over the user's size).
    pub gb_loaded: bool,
    /// Clearing the cache.
    pub busy: bool,
    _subscriptions: Vec<Subscription>,
}

impl DarkroomSection {
    pub fn new(ctx: Ctx, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let gb = cx.new(|cx| InputState::new(window, cx));
        let save = cx.subscribe_in(&gb, window, |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. }) {
                this.save_gb(window, cx);
            }
        });
        ctx.run_in(
            window,
            cx,
            |scope| {
                let get = |key: &str| scope.catalog(|c| c.get_setting(key)).ok().flatten();
                DarkroomRead {
                    gb: parse_cache_gb(get(DECODE_CACHE_GB_KEY).as_deref()),
                    preload: get(PRELOAD_KEY).as_deref() != Some("0"),
                    wb: WbPrefer::from_setting(get(WB_SLIDER_KEY).as_deref()),
                    parity: format_export_parity(get(EXPORT_PARITY_KEY).as_deref()),
                    timing: get(RENDER_TIMING_KEY).as_deref() == Some("1"),
                    last_summary: get(RENDER_TIMING_SUMMARY_KEY).unwrap_or_default(),
                }
            },
            |s: &mut Self, r, window, cx| {
                s.gb.update(cx, |i, cx| i.set_value(r.gb.to_string(), window, cx));
                s.gb_loaded = true;
                s.preload = Some(r.preload);
                s.wb = Some(r.wb);
                s.parity = r.parity;
                s.timing = Some(r.timing);
                s.last_summary = r.last_summary;
            },
        );
        let mut this = DarkroomSection {
            ctx,
            gb,
            preload: None,
            wb: None,
            usage: None,
            parity: None,
            timing: None,
            last_summary: String::new(),
            gb_loaded: false,
            busy: false,
            _subscriptions: vec![save],
        };
        this.refresh_usage(cx);
        this
    }

    pub fn refresh_usage(&mut self, cx: &mut Context<Self>) {
        self.ctx.run(cx, |_| chairphoto_core::app::decode_cache_usage(), |s: &mut Self, bytes, _| s.usage = Some(bytes));
    }

    /// Enter or leaving the field: normalise the value and store it.
    pub fn save_gb(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.gb_loaded {
            return;
        }
        let n = parse_cache_gb(Some(&self.gb.read(cx).value()));
        let text = n.to_string();
        if self.gb.read(cx).value() != text.as_str() {
            self.gb.update(cx, |i, cx| i.set_value(text.clone(), window, cx));
        }
        let ctx = self.ctx.clone();
        ctx.write_setting(cx, DECODE_CACHE_GB_KEY, text, |_| Ok(()), |_: &mut Self, _, _| {});
    }

    pub fn clear_cache(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        cx.notify();
        self.ctx.run(cx, |_| chairphoto_core::app::decode_cache_clear(), |s: &mut Self, _, cx| {
            s.busy = false;
            s.refresh_usage(cx);
        });
    }

    /// A boolean setting shown at once; once the newest write of it completes, the control
    /// shows what is stored (`read` turns the stored value into the flag), and is put back if
    /// that write failed.
    fn store_flag(
        &mut self,
        key: &'static str,
        next: bool,
        field: fn(&mut Self) -> &mut Option<bool>,
        read: fn(Option<&str>) -> bool,
        cx: &mut Context<Self>,
    ) {
        *field(self) = Some(next);
        cx.notify();
        let value = if next { "1" } else { "0" };
        let ctx = self.ctx.clone();
        ctx.write_setting(cx, key, value.into(), |_| Ok(()), move |s: &mut Self, result, _| {
            *field(s) = Some(match result {
                Ok((stored, ())) => read(stored.as_deref()),
                Err(_) => !next,
            });
        });
    }

    pub fn set_preload(&mut self, on: bool, cx: &mut Context<Self>) {
        self.store_flag(PRELOAD_KEY, on, |s| &mut s.preload, |v| v != Some("0"), cx);
    }

    pub fn set_timing(&mut self, on: bool, cx: &mut Context<Self>) {
        self.store_flag(RENDER_TIMING_KEY, on, |s| &mut s.timing, |v| v == Some("1"), cx);
    }

    pub fn set_wb(&mut self, next: WbPrefer, cx: &mut Context<Self>) {
        let before = self.wb;
        self.wb = Some(next);
        cx.notify();
        let value = match next {
            WbPrefer::Kelvin => "kelvin",
            WbPrefer::Relative => "relative",
        };
        let ctx = self.ctx.clone();
        ctx.write_setting(cx, WB_SLIDER_KEY, value.into(), |_| Ok(()), move |s: &mut Self, result, _| {
            s.wb = match result {
                Ok((stored, ())) => Some(WbPrefer::from_setting(stored.as_deref())),
                Err(_) => before,
            };
        });
    }
}

impl Render for DarkroomSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let can_clear = !self.busy && self.usage.is_some_and(|u| u > 0);
        let used = self.usage.map(|u| format!("· {} used", format_cache_bytes(u))).unwrap_or_default();
        let mut wb = ui::row().child(ui::sub("White balance for new edits", colors));
        for (value, id, label) in [
            (WbPrefer::Kelvin, "darkroom-wb-kelvin", "Kelvin — the scene's light (5200 K)"),
            (WbPrefer::Relative, "darkroom-wb-relative", "Warmer / cooler than as shot"),
        ] {
            let enabled = self.wb.is_some();
            let on = self.wb == Some(value);
            let chip = ui::chip(id, label, enabled, colors).when(on, |c| c.border_color(colors.accent).text_color(colors.accent));
            wb = wb.child(ui::clickable(chip, enabled, cx.listener(move |s, _, _, cx| s.set_wb(value, cx))));
        }
        let mut body = section("prefs-darkroom", "Darkroom", colors)
            .child(ui::sub(
                "The Darkroom develops the RAW itself — all the bits the sensor recorded, in linear light — and every \
                 slider, proof and print renders from that.",
                colors,
            ))
            .child(
                ui::row()
                    .child("RAW decode cache")
                    .child(div().w(px(70.)).child(Input::new(&self.gb).id("darkroom-gb")))
                    .child("GB")
                    .child(ui::sub(used, colors))
                    .child(ui::clickable(ui::chip("darkroom-clear", "Clear", can_clear, colors), can_clear, cx.listener(|s, _, _, cx| s.clear_cache(cx)))),
            )
            .child(ui::sub(
                "Each RAW decoded in the Darkroom is kept on disk so opening it again is near-instant (about 200–400 MB \
                 per photo). Oldest entries go first when the limit is reached; 0 keeps nothing.",
                colors,
            ))
            .child(
                ui::checkbox("darkroom-preload", "Prepare the next and previous photo in the background")
                    // Preferences' rows inherit the 13 px body text.
                    .text_size(px(13.))
                    .checked(self.preload.unwrap_or(true))
                    .disabled(self.preload.is_none())
                    .on_change(cx.listener(|s, on: &bool, _, cx| s.set_preload(*on, cx))),
            )
            .child(wb);
        if let Some(p) = &self.parity {
            body = body.child(status("darkroom-parity", p.clone(), colors));
        }
        body = body.child(
            ui::checkbox("darkroom-timing", "Log render timings to the console (dev)")
                .text_size(px(13.))
                .checked(self.timing.unwrap_or(false))
                .disabled(self.timing.is_none())
                .on_change(cx.listener(|s, on: &bool, _, cx| s.set_timing(*on, cx))),
        );
        if self.timing == Some(true) && !self.last_summary.is_empty() {
            body = body.child(status("darkroom-last-summary", format!("Last drag summary: {}", self.last_summary), colors));
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_helpers_match_reacts() {
        assert_eq!(format_cache_bytes(512 * 1024 * 1024), "512 MB");
        assert_eq!(format_cache_bytes(3_650_722_201), "3.4 GB");
        assert_eq!(parse_cache_gb(None), 20.);
        assert_eq!(parse_cache_gb(Some(" ")), 20.);
        assert_eq!(parse_cache_gb(Some("-1")), 20.);
        assert_eq!(parse_cache_gb(Some("abc")), 20.);
        assert_eq!(parse_cache_gb(Some("0")), 0.);
        assert_eq!(parse_cache_gb(Some("2.5")), 2.5);
        assert_eq!(2.5f64.to_string(), "2.5");
        assert_eq!(20f64.to_string(), "20", "as JS String(20)");
        assert_eq!(format_export_parity(None), None);
        assert_eq!(format_export_parity(Some("{\"checked\":0}")), None);
        assert_eq!(format_export_parity(Some("not json")), None);
        assert_eq!(
            format_export_parity(Some("{\"checked\":1,\"differing\":0}")).unwrap(),
            "What you see is what you export: 1 RAW export checked, none differed from the view."
        );
        assert_eq!(
            format_export_parity(Some("{\"checked\":4,\"differing\":2}")).unwrap(),
            "What you see is what you export: 4 RAW exports checked, 2 differed from the view."
        );
    }
}
