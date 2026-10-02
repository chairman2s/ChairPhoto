//! `SendToDevicePanel` (SendToDevicePanel.tsx): sends the selection (or the active photo) to a
//! LocalSend device — the version picker (the active photo's; default its active version),
//! the discovered devices (scanned once when the form opens, then on Refresh) with alias,
//! model and IP, a manual IP + port (53317), an optional PIN, the Snapchat pre-flight warning,
//! "Sending d/t…", Send, Cancel, the status, and "Sent N to X, M skipped." on the status line.
//!
//! **The send** is core's job in two worker steps, never on the UI thread: [`SendToDevicePanel::send`]
//! queues the claim (identity check, resolve); when it lands the panel keeps the job's id and
//! abort flag and queues the run. Cancel reaches exactly this send: after the claim it trips
//! the job's flag; before, the claimed job is dropped unrun. A newer Send supersedes the older
//! (core trips it) and the older one's answers are dropped.
//!
//! **A typed address wins over a scan in flight** (6d1066e): a discovery pass that lands after
//! the user typed a manual IP does not auto-select a discovered device.

use super::{LocalSendBackend, SendMode};
use crate::model::AppModelEvent;
use crate::modules::dialog::{self, DialogHost};
use crate::modules::publishing::{record_publications, PublishSubject, VersionPicker, NO_ROWS};
use crate::shell::style::Colors;
use crate::storage::{ui, Runner};
use chairphoto_core::app::localsend::{claim_send, Device, LocalSendJob, SendOutcome, SEND_CANCELLED};
use chairphoto_core::app::CoreEvent;
use chairphoto_model::publishing::{device_label, manual_port, progress_line, sent_line, snapchat_preflight, DEFAULT_PORT};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::*;
use gpui_kit::{div, px, App, ClickEvent, Context, Entity, SharedString, Subscription, TestSupportExt as _, Window};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What the panel says when a worker step died (a panic) without an answer.
const STOPPED: &str = "The send stopped unexpectedly";

/// What a landed claim does next.
enum Go {
    Run,
    Cancelled,
    Superseded,
}

/// The running send: its claim, once it has landed.
#[derive(Default)]
struct Running {
    cancelled: bool,
    job: Option<u64>,
    abort: Option<Arc<AtomicBool>>,
}

pub struct SendToDevicePanel {
    host: DialogHost,
    backend: LocalSendBackend,
    pub mode: SendMode,
    pub subject: PublishSubject,
    pub versions: VersionPicker,
    pub devices: Vec<Device>,
    /// The chosen discovered device, by fingerprint.
    pub selected: Option<String>,
    pub manual_ip: Entity<InputState>,
    pub manual_port: Entity<InputState>,
    pub pin: Entity<InputState>,
    pub scanning: bool,
    /// Bumped by every discovery pass; an older pass's answer is dropped.
    scan_attempt: u64,
    pub busy: bool,
    /// The newest `localsend:progress` of this panel's job.
    pub progress: Option<(usize, usize)>,
    pub status: String,
    running: Option<Running>,
    /// Bumped by every Send; a superseded send's answer is dropped.
    attempt: u64,
    _subscriptions: Vec<Subscription>,
}

impl SendToDevicePanel {
    pub fn new(host: DialogHost, backend: LocalSendBackend, mode: SendMode, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subject = PublishSubject::take(&host.shell, cx);
        let manual_ip = cx.new(|cx| InputState::new(window, cx).placeholder("Manual IP (e.g. 192.168.1.42)"));
        let manual_port = cx.new(|cx| InputState::new(window, cx).placeholder("Port").default_value(DEFAULT_PORT.to_string()));
        let pin = cx.new(|cx| InputState::new(window, cx).placeholder("If the receiver requires a PIN"));
        let subscriptions = vec![
            dialog::close_on_switch(&host.model, window, cx),
            cx.subscribe(&host.model, |this: &mut Self, _, event: &AppModelEvent, cx| {
                if let AppModelEvent::Core(CoreEvent::LocalSendProgress(p)) = event {
                    this.on_progress(p.job, p.done, p.total, cx);
                }
            }),
            // Typing an address deselects the discovered device: the two are alternatives.
            cx.subscribe(&manual_ip, |this: &mut Self, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) && !input.read(cx).value().trim().is_empty() {
                    this.selected = None;
                    cx.notify();
                }
            }),
        ];
        let mut panel = SendToDevicePanel {
            versions: VersionPicker::new(&subject),
            host,
            backend,
            mode,
            subject,
            devices: Vec::new(),
            selected: None,
            manual_ip,
            manual_port,
            pin,
            scanning: false,
            scan_attempt: 0,
            busy: false,
            progress: None,
            status: String::new(),
            running: None,
            attempt: 0,
            _subscriptions: subscriptions,
        };
        VersionPicker::load(&panel.host.app, &panel.subject, cx, |p: &mut Self, versions, cx| {
            p.versions.versions = versions;
            cx.notify();
        });
        // Scan once when the form opens (an empty list reads as "discovery is broken");
        // Refresh scans again.
        if !panel.subject.targets.is_empty() {
            panel.discover(cx);
        }
        panel
    }

    /// Refresh: one discovery pass on a worker.
    pub fn discover(&mut self, cx: &mut Context<Self>) {
        self.scan_attempt += 1;
        let attempt = self.scan_attempt;
        self.scanning = true;
        self.status = "Scanning the network…".into();
        let discover = self.backend.discover.clone();
        let rx = Runner::get(cx).run(move || discover());
        cx.spawn(async move |this, cx| {
            let found = rx.await.unwrap_or_else(|_| Err("Discovery stopped unexpectedly".into()));
            this.update(cx, |p, cx| p.discovered(attempt, found, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    fn discovered(&mut self, attempt: u64, found: Result<Vec<Device>, String>, cx: &mut Context<Self>) {
        if attempt != self.scan_attempt {
            return;
        }
        self.scanning = false;
        match found {
            Ok(devices) => {
                let typed = !self.manual_ip.read(cx).value().trim().is_empty();
                let still_there = self.selected.as_ref().is_some_and(|fp| devices.iter().any(|d| &d.fingerprint == fp));
                if !typed && !still_there {
                    self.selected = devices.first().map(|d| d.fingerprint.clone());
                }
                self.status = if devices.is_empty() { "No devices found — enter an IP below.".into() } else { String::new() };
                self.devices = devices;
            }
            Err(e) => self.status = e,
        }
        cx.notify();
    }

    /// The device a Send goes to: the chosen discovered one, else the typed address (its
    /// scheme left empty: core probes it, `localsend::probe_protocol`).
    pub fn chosen_device(&self, cx: &App) -> Option<Device> {
        if let Some(d) = self.selected.as_ref().and_then(|fp| self.devices.iter().find(|d| &d.fingerprint == fp)) {
            return Some(d.clone());
        }
        let ip = self.manual_ip.read(cx).value().trim().to_string();
        if ip.is_empty() {
            return None;
        }
        Some(Device {
            alias: "Manual".into(),
            device_model: None,
            device_type: None,
            ip,
            port: manual_port(&self.manual_port.read(cx).value()),
            protocol: String::new(),
            fingerprint: String::new(),
        })
    }

    /// The Snapchat pre-flight on the active photo and the chosen version.
    pub fn warning(&self) -> Option<&'static str> {
        if !matches!(self.mode, SendMode::Snapchat { .. }) {
            return None;
        }
        let active = self.subject.active?;
        snapchat_preflight(active.width, active.height, self.versions.chosen_version().map(|v| v.edit_json.as_str()))
    }

    /// The running send's job id, once its claim has landed.
    pub fn job(&self) -> Option<u64> {
        self.running.as_ref().and_then(|r| r.job)
    }

    fn on_progress(&mut self, job: u64, done: usize, total: usize, cx: &mut Context<Self>) {
        if self.busy && self.job() == Some(job) {
            self.progress = Some((done, total));
            cx.notify();
        }
    }

    /// Send: claim core's send job on a worker, then run it there.
    pub fn send(&mut self, cx: &mut Context<Self>) {
        let Some(device) = self.chosen_device(cx) else { return };
        if self.busy || self.subject.targets.is_empty() {
            return;
        }
        let Some(catalog) = self.subject.catalog else {
            self.status = NO_ROWS.into();
            cx.notify();
            return;
        };
        self.attempt += 1;
        let attempt = self.attempt;
        self.running = Some(Running::default());
        self.busy = true;
        self.progress = Some((0, self.subject.targets.len()));
        self.status = "Sending…".into();
        let (app, ids, version) = (self.host.app.clone(), self.subject.targets.clone(), self.versions.chosen);
        let pin = Some(self.pin.read(cx).value().trim().to_string()).filter(|p| !p.is_empty());
        let active = self.subject.active.map(|a| a.id);
        let (backend, mode, host) = (self.backend.clone(), self.mode.clone(), self.host.clone());
        let claim = Runner::get(cx).run(move || claim_send(&app, Some(catalog), &ids, version));
        let alias = device.alias.clone();
        cx.spawn(async move |this, cx| {
            let job: LocalSendJob = match claim.await.unwrap_or_else(|_| Err(STOPPED.into())) {
                Ok(job) => job,
                Err(e) => {
                    let result = Err(e);
                    if this.update(cx, |p, cx| p.land(attempt, &alias, result.clone(), cx)).is_err() {
                        cx.update(|cx| host.status(gone(&alias, &result), cx));
                    }
                    return;
                }
            };
            // A closed dialog does not stop the send (its answer goes to the status line).
            match this.update(cx, |p, cx| p.claimed(attempt, &job, cx)).unwrap_or(Go::Run) {
                Go::Run => {}
                Go::Cancelled => {
                    this.update(cx, |p, cx| p.land(attempt, &alias, Err(SEND_CANCELLED.into()), cx)).ok();
                    return;
                }
                Go::Superseded => return,
            }
            let app = host.app.clone();
            let run = cx.update(|cx| {
                Runner::get(cx).run(move || {
                    let outcome = (backend.send)(job, &device, pin.as_deref())?;
                    if let SendMode::Snapchat { marker } = &mode {
                        // Recorded for the photos that reached the device, each with the
                        // version it was sent as (the chosen one applies to the active photo).
                        let published: Vec<(i64, Option<i64>)> = outcome
                            .sent
                            .iter()
                            .map(|&id| (id, if Some(id) == active { version } else { None }))
                            .collect();
                        if let Err(e) = record_publications(&app, catalog, &published, marker, None) {
                            return Err(format!("Sent {}, but couldn't record it as published: {e}", outcome.sent.len()));
                        }
                    }
                    Ok(outcome)
                })
            });
            let result = run.await.unwrap_or_else(|_| Err(STOPPED.into()));
            if this.update(cx, |p, cx| p.land(attempt, &alias, result.clone(), cx)).is_err() {
                cx.update(|cx| host.status(gone(&alias, &result), cx));
            }
        })
        .detach();
        cx.notify();
    }

    /// The claim of send `attempt` landed: keep its id and abort flag, unless Cancel came
    /// first (then it never runs) or a newer Send superseded it.
    fn claimed(&mut self, attempt: u64, job: &LocalSendJob, cx: &mut Context<Self>) -> Go {
        if attempt != self.attempt {
            return Go::Superseded;
        }
        let Some(running) = self.running.as_mut() else { return Go::Superseded };
        running.job = Some(job.job);
        running.abort = Some(job.abort_handle());
        cx.notify();
        if running.cancelled {
            Go::Cancelled
        } else {
            Go::Run
        }
    }

    fn land(&mut self, attempt: u64, alias: &str, result: Result<SendOutcome, String>, cx: &mut Context<Self>) {
        if attempt != self.attempt {
            return; // a superseded send
        }
        self.busy = false;
        self.running = None;
        self.progress = None;
        match result {
            Ok(outcome) => {
                let line = sent_line(outcome.sent.len(), outcome.failed, alias);
                self.host.status(line, cx);
                let skipped = if outcome.failed > 0 { format!(", {} skipped", outcome.failed) } else { String::new() };
                self.status = format!("Sent {}{skipped} ✓", outcome.sent.len());
            }
            Err(e) => self.status = e,
        }
        cx.notify();
    }

    /// Cancel the running send: trip its job once claimed, else keep it from running.
    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(r) = self.running.as_mut() {
            r.cancelled = true;
            if let Some(abort) = &r.abort {
                abort.store(true, Ordering::Relaxed);
            }
        }
        cx.notify();
    }
}

/// The status line for a send whose panel is gone.
fn gone(alias: &str, result: &Result<SendOutcome, String>) -> String {
    match result {
        Ok(o) => sent_line(o.sent.len(), o.failed, alias),
        Err(e) => format!("Send to {alias} failed: {e}"),
    }
}

impl Render for SendToDevicePanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let body = ui::body().id("send-to-device");
        if self.subject.targets.is_empty() {
            return body.child(ui::empty("send-empty", "Select a photo", colors)).test_support();
        }
        let this = cx.entity().downgrade();
        let versions = self.versions.menu("send-version", this.clone(), |p: &mut Self, v, _| p.versions.chosen = v);
        let chosen = self.selected.as_ref().and_then(|fp| self.devices.iter().find(|d| &d.fingerprint == fp));
        let label: SharedString = match chosen {
            Some(d) => device_label(&d.alias, d.device_model.as_deref(), &d.ip).into(),
            None if self.devices.is_empty() => "(none found — use manual IP)".into(),
            None => "(choose a device)".into(),
        };
        let devices = self.devices.clone();
        let current = self.selected.clone();
        let device_menu = Button::new("send-device").outline().small().label(label).dropdown_menu(move |menu, _, _| {
            let mut menu = menu;
            for d in &devices {
                let (this, fp) = (this.clone(), d.fingerprint.clone());
                menu = menu.item(
                    PopupMenuItem::new(device_label(&d.alias, d.device_model.as_deref(), &d.ip))
                        .checked(current.as_deref() == Some(d.fingerprint.as_str()))
                        .on_click(move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                            this.update(cx, |p, cx| {
                                p.selected = Some(fp.clone());
                                cx.notify();
                            })
                            .ok();
                        }),
                );
            }
            menu
        });
        let mut body = body
            .child(ui::label("Version", colors))
            .child(div().child(versions))
            .child(ui::label("Device", colors))
            .child(
                ui::row().child(device_menu).child(ui::clickable(
                    ui::chip("send-refresh", if self.scanning { "Scanning…" } else { "Refresh" }, !self.scanning, colors),
                    !self.scanning,
                    cx.listener(|p, _, _, cx| p.discover(cx)),
                )),
            )
            .child(
                ui::row()
                    .child(div().id("send-manual-ip").w(px(240.)).child(Input::new(&self.manual_ip)).test_support())
                    .child(div().id("send-manual-port").w(px(80.)).child(Input::new(&self.manual_port)).test_support()),
            )
            .child(ui::sub(
                "Devices are found over the local network. If discovery is blocked, type the device's IP shown in its \
                 LocalSend app.",
                colors,
            ))
            .child(ui::label("PIN (optional)", colors))
            .child(div().id("send-pin").w(px(240.)).child(Input::new(&self.pin)).test_support());
        if let Some(w) = self.warning() {
            body = body.child(div().id("send-warning").text_size(px(11.5)).text_color(colors.rating).child(w).test_support());
        }
        if let Some((done, total)) = self.progress {
            body = body.child(div().id("send-progress").child(ui::sub(progress_line(done, total), colors)).test_support());
        }
        let can = !self.busy && self.chosen_device(cx).is_some();
        let n = self.subject.targets.len();
        let label = if self.busy { "Sending…".to_string() } else if n > 1 { format!("Send {n} photos") } else { "Send photo".into() };
        let mut actions = ui::row().child(ui::clickable(ui::primary("send-send", label, can, colors), can, cx.listener(|p, _, _, cx| p.send(cx))));
        if self.busy {
            actions = actions.child(ui::clickable(ui::chip("send-cancel", "Cancel", true, colors), true, cx.listener(|p, _, _, cx| p.cancel(cx))));
        }
        actions = actions.child(div().id("send-status").child(ui::sub(self.status.clone(), colors)).test_support());
        body.child(actions).test_support()
    }
}
