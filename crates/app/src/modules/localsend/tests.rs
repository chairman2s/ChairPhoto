//! Headless tests of the LocalSend and Snapchat modules and `SendToDevicePanel`. The network
//! is faked at [`LocalSendBackend`]: no test discovers or sends on a real network (core's
//! `app::localsend` tests drive the real send against a loopback receiver).

use super::send::SendToDevicePanel;
use super::{LocalSendBackend, LocalSendModule, SendMode, SnapchatModule, LOCALSEND_ID, SNAPCHAT_ID};
use crate::modules::dialog::DialogHost;
use crate::modules::panel::{open_publish_dialog, PublishDialog};
use crate::modules::publishing::ActivePhoto;
use crate::modules::{Module, ModuleRegistry};
use crate::storage::Runner;
use crate::tests::{colliding_catalog, core_switch, deliver_switch, start, App, TempDir};
use chairphoto_core::app::localsend::{Device, SendOutcome, SendStopped, SEND_CANCELLED};
use chairphoto_core::app::{CoreEvent, EventSink as _, LocalSendProgress, CATALOG_CHANGED};
use chairphoto_core::catalog::Catalog;
use chairphoto_model::publishing::SNAPCHAT_WARNING;
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, Entity, SharedString, TestAppContext};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

/// A catalog in `dir` with `n` photos whose files exist; selects them all.
fn catalog_with_files(app: &App, dir: &TempDir, n: usize, cx: &mut TestAppContext) -> Vec<i64> {
    let db = dir.0.join("photos.chairphoto");
    let root = dir.0.join("photos");
    std::fs::create_dir_all(root.join("2026")).unwrap();
    let catalog = Catalog::open(&db, &root).unwrap();
    let ids = (0..n)
        .map(|i| {
            let p = root.join(format!("2026/p{i}.jpg"));
            std::fs::write(&p, format!("jpeg {i}")).unwrap();
            catalog.upsert_photo(&p, None, 0, 6).unwrap().id
        })
        .collect();
    *app.state.catalog.lock().unwrap() = Some(catalog);
    app.state.send(CoreEvent::CatalogSwitched(db.to_string_lossy().to_string()));
    cx.run_until_parked();
    work(cx);
    app.wired.shell.update(cx, |s, cx| {
        s.library.select_all();
        cx.notify();
    });
    ids
}

fn work(cx: &mut TestAppContext) -> usize {
    let mut total = 0;
    loop {
        let ran = cx.update(|cx| Runner::get(cx).run_pending());
        cx.run_until_parked();
        if ran == 0 {
            return total;
        }
        total += ran;
    }
}

fn step(cx: &mut TestAppContext) -> usize {
    let ran = cx.update(|cx| Runner::get(cx).run_pending());
    cx.run_until_parked();
    ran
}

/// What the fake network saw.
#[derive(Default)]
struct Net {
    discoveries: usize,
    /// (device, pin, photo ids) per send that ran.
    sends: Vec<(Device, Option<String>, Vec<i64>)>,
    /// Per send that ran: what the claimed job resolved each photo to (id, version name).
    sent_as: Vec<Vec<(i64, Option<String>)>>,
    /// Stop each send after this many photos, with this error (a rejection, a Cancel).
    stop_after: Option<(usize, String)>,
}

fn device(alias: &str, fp: &str, ip: &str) -> Device {
    Device {
        alias: alias.into(),
        device_model: Some("Phone".into()),
        device_type: None,
        ip: ip.into(),
        port: 53317,
        protocol: "https".into(),
        fingerprint: fp.into(),
    }
}

/// A fake network: discovery answers `found`; a send reports every reachable photo sent
/// unless its job was tripped first (then it answers cancelled, as core's does).
fn fake(found: Vec<Device>, net: Arc<Mutex<Net>>) -> LocalSendBackend {
    let n = net.clone();
    LocalSendBackend {
        discover: Arc::new(move || {
            n.lock().unwrap().discoveries += 1;
            Ok(found.clone())
        }),
        send: Arc::new(move |job, device, pin| {
            if job.abort_handle().load(Ordering::Relaxed) {
                return Err(String::from(SEND_CANCELLED).into());
            }
            let ids = job.photo_ids();
            let mut net = net.lock().unwrap();
            net.sends.push((device.clone(), pin.map(str::to_string), ids.clone()));
            net.sent_as.push(job.sent_as());
            if let Some((n, error)) = net.stop_after.clone() {
                return Err(SendStopped { sent: ids[..n].to_vec(), error });
            }
            Ok(SendOutcome { sent: ids, failed: 0 })
        }),
    }
}

fn open(app: &App, backend: LocalSendBackend, mode: SendMode, cx: &mut TestAppContext) -> Entity<SendToDevicePanel> {
    let view = cx
        .update_window(app.window(), |_, window, cx| {
            let host = DialogHost::new(&app.wired.model, &app.wired.shell, None, cx);
            let view = cx.new(|cx| SendToDevicePanel::new(host, backend, mode, window, cx));
            crate::modules::dialog::open("Publish", 560., true, view.clone(), window, cx);
            view
        })
        .unwrap();
    cx.run_until_parked();
    view
}

fn type_into(app: &App, view: &Entity<SendToDevicePanel>, field: &str, text: &str, cx: &mut TestAppContext) {
    let text = text.to_string();
    let field = field.to_string();
    cx.update_window(app.window(), |_, window, cx| {
        view.update(cx, |p, cx| {
            let input = match field.as_str() {
                "ip" => p.manual_ip.clone(),
                "port" => p.manual_port.clone(),
                _ => p.pin.clone(),
            };
            input.update(cx, |i, cx| i.set_value(text, window, cx));
        })
    })
    .unwrap();
    cx.run_until_parked();
}

fn click(app: &App, id: &'static str, cx: &mut TestAppContext) {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn present(app: &App, id: &'static str, cx: &mut TestAppContext) -> bool {
    cx.update_window(app.window(), |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
    .unwrap()
}

fn has_dialog(app: &App, cx: &mut TestAppContext) -> bool {
    cx.update_window(app.window(), |_, window, cx| window.has_active_dialog(cx)).unwrap()
}

/// The version names recorded for `id` under `platform` (`None` = the Original).
fn recorded_version_names(app: &App, id: i64, platform: &str) -> Vec<Option<String>> {
    let guard = app.state.catalog.lock().unwrap();
    let pubs = guard.as_ref().unwrap().list_publications(id).unwrap();
    pubs.into_iter().filter(|p| p.platform == platform).map(|p| p.version_name).collect()
}

fn publications(app: &App, id: i64) -> Vec<(String, Option<i64>)> {
    let guard = app.state.catalog.lock().unwrap();
    guard.as_ref().unwrap().list_publications(id).unwrap().into_iter().map(|p| (p.platform, p.version_id)).collect()
}

/// The two modules: their publish targets, Snapchat's marker and requirement — and the
/// Publish dialog builds (and so scans for) a target's form only when its chip is chosen.
#[gpui_kit::test]
fn the_targets_build_their_form_only_when_chosen(cx: &mut TestAppContext) {
    let dir = TempDir::new("localsend-targets");
    let app = start(cx);
    catalog_with_files(&app, &dir, 1, cx);
    let net = Arc::new(Mutex::new(Net::default()));
    let backend = fake(Vec::new(), net.clone());
    let modules: Vec<Rc<dyn Module>> =
        vec![Rc::new(LocalSendModule { backend: backend.clone() }), Rc::new(SnapchatModule { backend })];
    let snap = modules[1].meta();
    assert_eq!(snap.marker(), "snapchat");
    assert_eq!(snap.requires, [SharedString::from(LOCALSEND_ID)]);
    assert_eq!(snap.backend_feature.as_deref(), Some("localsend"));
    let registry = cx.update(|cx| {
        ModuleRegistry::install_with(modules, vec!["localsend".into()], &app.wired.model, &app.wired.shell, cx)
    });
    app.wired.model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    work(cx);
    cx.update(|cx| ModuleRegistry::enable(&registry, SNAPCHAT_ID, cx));
    cx.run_until_parked();
    work(cx);
    let enabled = registry.read_with(cx, |r, _| r.enabled_ids());
    assert!(enabled.contains(&LOCALSEND_ID.into()), "Snapchat enabled LocalSend first: {enabled:?}");
    let labels: Vec<String> = registry.read_with(cx, |r, _| r.publish_targets().into_iter().map(|(_, t)| t.label.to_string()).collect());
    assert_eq!(labels, ["Device (LocalSend)", "Snapchat"]);

    let dialog: Entity<PublishDialog> =
        cx.update_window(app.window(), |_, window, cx| open_publish_dialog(&registry, window, cx)).unwrap();
    cx.run_until_parked();
    assert!(present(&app, "send-to-device", cx));
    work(cx);
    assert_eq!(net.lock().unwrap().discoveries, 1, "only the shown form scanned");
    dialog.read_with(cx, |d, _| {
        assert!(d.is_built(LOCALSEND_ID));
        assert!(!d.is_built(SNAPCHAT_ID), "an unchosen target's form is not built");
    });
    click(&app, "publish-target-snapchat", cx);
    work(cx);
    dialog.read_with(cx, |d, _| assert!(d.is_built(SNAPCHAT_ID)));
    assert_eq!(net.lock().unwrap().discoveries, 2);
}

/// Discovery lists the devices and selects the first; Send goes to it with the trimmed PIN,
/// through two worker steps (claim, then run), and reports on the panel and the status line.
/// LocalSend records nothing.
#[gpui_kit::test]
fn discovery_then_send_to_the_chosen_device(cx: &mut TestAppContext) {
    let dir = TempDir::new("localsend-send");
    let app = start(cx);
    let ids = catalog_with_files(&app, &dir, 2, cx);
    let net = Arc::new(Mutex::new(Net::default()));
    let phone = device("Kind Carrot", "fp-phone", "192.168.1.128");
    let view = open(&app, fake(vec![phone.clone(), device("Desk", "fp-desk", "192.168.1.2")], net.clone()), SendMode::Transfer, cx);
    view.read_with(cx, |p, _| assert_eq!(p.status, "Scanning the network…"));
    work(cx);
    view.read_with(cx, |p, _| {
        assert_eq!(p.devices.len(), 2);
        assert_eq!(p.selected.as_deref(), Some("fp-phone"));
        assert_eq!(p.status, "");
    });
    type_into(&app, &view, "pin", " 1234 ", cx);
    click(&app, "send-send", cx);
    view.read_with(cx, |p, _| assert!(p.busy && p.job().is_none(), "queued, not claimed on the UI thread"));
    assert!(present(&app, "send-progress", cx), "Sending 0/2…");
    assert_eq!(step(cx), 1, "the claim");
    assert!(view.read_with(cx, |p, _| p.job()).is_some());
    assert_eq!(step(cx), 1, "the run");
    let sends = &net.lock().unwrap().sends;
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0, phone);
    assert_eq!(sends[0].1.as_deref(), Some("1234"));
    assert_eq!(sends[0].2, ids);
    view.read_with(cx, |p, _| {
        assert!(!p.busy);
        assert_eq!(p.status, "Sent 2 ✓");
    });
    assert_eq!(crate::tests::status(&app, cx), "Sent 2 to Kind Carrot.");
    assert!(publications(&app, ids[0]).is_empty(), "a transfer, not a publication");
}

/// SendToDevicePanel.test.tsx, first case: a typed address and port are what the send gets,
/// with the scheme left for core to probe.
#[gpui_kit::test]
fn sends_to_the_typed_address_and_port(cx: &mut TestAppContext) {
    let dir = TempDir::new("localsend-manual");
    let app = start(cx);
    catalog_with_files(&app, &dir, 1, cx);
    let net = Arc::new(Mutex::new(Net::default()));
    let view = open(&app, fake(Vec::new(), net.clone()), SendMode::Transfer, cx);
    work(cx);
    view.read_with(cx, |p, _| assert_eq!(p.status, "No devices found — enter an IP below."));
    type_into(&app, &view, "ip", "10.0.0.55", cx);
    type_into(&app, &view, "port", "9000", cx);
    click(&app, "send-send", cx);
    work(cx);
    let sends = &net.lock().unwrap().sends;
    let d = &sends[0].0;
    assert_eq!((d.ip.as_str(), d.port, d.protocol.as_str(), d.fingerprint.as_str()), ("10.0.0.55", 9000, "", ""));
    assert_eq!(sends[0].1, None, "no PIN");
    view.read_with(cx, |p, _| assert_eq!(p.status, "Sent 1 ✓"));
}

/// SendToDevicePanel.test.tsx, the 6d1066e race: an address typed while the opening scan is in
/// flight is not overridden by the device the scan finds.
#[gpui_kit::test]
fn a_landing_scan_does_not_steal_a_typed_address(cx: &mut TestAppContext) {
    let dir = TempDir::new("localsend-race");
    let app = start(cx);
    catalog_with_files(&app, &dir, 1, cx);
    let net = Arc::new(Mutex::new(Net::default()));
    let view = open(&app, fake(vec![device("Living Room TV", "abc123", "192.168.1.50")], net.clone()), SendMode::Transfer, cx);
    view.read_with(cx, |p, _| assert!(p.scanning, "the scan is in flight"));
    type_into(&app, &view, "ip", "10.0.0.55", cx);
    work(cx); // the scan lands
    view.read_with(cx, |p, _| {
        assert_eq!(p.devices.len(), 1);
        assert_eq!(p.selected, None, "the typed address keeps the choice");
    });
    click(&app, "send-send", cx);
    work(cx);
    assert_eq!(net.lock().unwrap().sends[0].0.ip, "10.0.0.55");
}

/// Progress moves only for this panel's job; Cancel after the claim trips that job (the send
/// answers cancelled); Cancel before the claim keeps the claimed job from running.
#[gpui_kit::test]
fn progress_is_this_jobs_and_cancel_stops_the_send(cx: &mut TestAppContext) {
    let dir = TempDir::new("localsend-cancel");
    let app = start(cx);
    catalog_with_files(&app, &dir, 2, cx);
    let net = Arc::new(Mutex::new(Net::default()));
    let view = open(&app, fake(vec![device("Phone", "fp", "192.168.1.9")], net.clone()), SendMode::Transfer, cx);
    work(cx);
    click(&app, "send-send", cx);
    step(cx); // the claim
    let job = view.read_with(cx, |p, _| p.job()).unwrap();
    app.state.send(CoreEvent::LocalSendProgress(LocalSendProgress { done: 1, total: 2, job: job + 100 }));
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |p, _| p.progress), Some((0, 2)), "another job's progress is not ours");
    app.state.send(CoreEvent::LocalSendProgress(LocalSendProgress { done: 1, total: 2, job }));
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |p, _| p.progress), Some((1, 2)));
    click(&app, "send-cancel", cx);
    work(cx);
    view.read_with(cx, |p, _| {
        assert!(!p.busy);
        assert_eq!(p.status, SEND_CANCELLED);
        assert_eq!(p.progress, None);
    });
    assert!(net.lock().unwrap().sends.is_empty(), "the tripped job sent nothing");

    // Cancel before the claim lands.
    click(&app, "send-send", cx);
    view.update(cx, |p, cx| p.cancel(cx));
    assert_eq!(step(cx), 1, "the claim ran");
    assert_eq!(step(cx), 0, "and nothing after it");
    view.read_with(cx, |p, _| assert_eq!(p.status, SEND_CANCELLED));
    assert!(net.lock().unwrap().sends.is_empty());
}

/// Snapchat: the 9:16 warning for a landscape photo (gone for a 9:16 crop version), and after
/// the send a `snapchat` publication with the version it was sent as.
#[gpui_kit::test]
fn snapchat_warns_and_records_what_was_sent(cx: &mut TestAppContext) {
    let dir = TempDir::new("localsend-snapchat");
    let app = start(cx);
    let ids = catalog_with_files(&app, &dir, 2, cx);
    let version = {
        let guard = app.state.catalog.lock().unwrap();
        let c = guard.as_ref().unwrap();
        let v = c.create_version(ids[0], "Story crop").unwrap();
        c.set_version_edit(v, r#"{"crop":{"x":0,"y":0,"w":1,"h":1,"aspect":"9:16"}}"#).unwrap();
        v
    };
    app.wired.shell.update(cx, |s, cx| {
        s.library.select(ids[0], Default::default());
        cx.notify();
    });
    let net = Arc::new(Mutex::new(Net::default()));
    let view = open(&app, fake(vec![device("Phone", "fp", "192.168.1.9")], net.clone()), SendMode::Snapchat { marker: "snapchat".into() }, cx);
    work(cx);
    view.update(cx, |p, _| p.subject.active = Some(ActivePhoto { id: ids[0], width: Some(6000), height: Some(4000) }));
    assert_eq!(view.read_with(cx, |p, _| p.warning()), Some(SNAPCHAT_WARNING));
    assert!(present(&app, "send-warning", cx));
    view.update(cx, |p, cx| {
        p.versions.chosen = Some(version);
        cx.notify();
    });
    view.read_with(cx, |p, _| {
        assert_eq!(p.versions.versions.len(), 1, "the active photo's versions were read");
        assert_eq!(p.warning(), None, "the 9:16 crop passes");
    });
    click(&app, "send-send", cx);
    work(cx);
    assert_eq!(net.lock().unwrap().sends[0].2, [ids[0]]);
    // What the job resolved and sent is what was recorded: the chosen version, by id and name.
    let sent_as = net.lock().unwrap().sent_as[0].clone();
    for (id, name) in &sent_as {
        assert_eq!(recorded_version_names(&app, *id, "snapchat"), [name.clone()], "recorded as the version that was sent");
    }
    assert_eq!(sent_as, [(ids[0], Some("Story crop".to_string()))], "the claimed job rendered the chosen version");
    assert_eq!(publications(&app, ids[0]), [("snapchat".to_string(), Some(version))]);
    assert!(publications(&app, ids[1]).is_empty(), "not selected, not sent, not recorded");
}

/// A Snapchat send that stops part-way — the receiver rejects the 4th of 5, or Cancel lands
/// after 3 — records a publication for exactly the three delivered photos (the active one with
/// the version it was sent as), and the status says how many went and why it stopped.
fn stops_part_way(case: &str, why: &str, cx: &mut TestAppContext) {
    {
        let dir = TempDir::new(&format!("localsend-partial-{case}"));
        let app = start(cx);
        let ids = catalog_with_files(&app, &dir, 5, cx);
        let version = app.state.catalog.lock().unwrap().as_ref().unwrap().create_version(ids[0], "Story").unwrap();
        let net = Arc::new(Mutex::new(Net { stop_after: Some((3, why.to_string())), ..Net::default() }));
        let view = open(&app, fake(vec![device("Phone", "fp", "192.168.1.9")], net.clone()), SendMode::Snapchat { marker: "snapchat".into() }, cx);
        work(cx);
        view.update(cx, |p, _| {
            p.subject.active = Some(ActivePhoto { id: ids[0], width: Some(1080), height: Some(1920) });
            p.versions.chosen = Some(version);
        });
        click(&app, "send-send", cx);
        work(cx);
        assert_eq!(net.lock().unwrap().sends[0].2, ids, "{case}: all five were asked for");
        assert_eq!(publications(&app, ids[0]), [("snapchat".to_string(), Some(version))], "{case}");
        for &id in &ids[1..3] {
            assert_eq!(publications(&app, id), [("snapchat".to_string(), None)], "{case}: delivered, recorded");
        }
        for &id in &ids[3..] {
            assert!(publications(&app, id).is_empty(), "{case}: never reached the device, not recorded");
        }
        view.read_with(cx, |p, _| {
            assert!(!p.busy);
            assert_eq!(p.status, format!("Sent 3 of 5 to Phone, then stopped: {why}"), "{case}");
        });
    }
}

#[gpui_kit::test]
fn a_rejection_of_the_4th_of_5_records_the_3_delivered(cx: &mut TestAppContext) {
    stops_part_way("reject", "LocalSend: upload of p3.jpg rejected (500): disk full", cx);
}

#[gpui_kit::test]
fn a_cancel_after_3_records_the_3_delivered(cx: &mut TestAppContext) {
    stops_part_way("cancel", SEND_CANCELLED, cx);
}

/// A switch to a catalog whose ids collide: with `catalog:switched` not yet delivered, Send is
/// refused (nothing claimed, sent or recorded in the new catalog); once it is delivered the
/// dialog closes.
#[gpui_kit::test]
fn a_catalog_switch_refuses_the_send_and_closes_the_dialog(cx: &mut TestAppContext) {
    let dir = TempDir::new("localsend-switch");
    let app = start(cx);
    let ids = catalog_with_files(&app, &dir, 2, cx);
    let net = Arc::new(Mutex::new(Net::default()));
    let view = open(&app, fake(vec![device("Phone", "fp", "192.168.1.9")], net.clone()), SendMode::Snapchat { marker: "snapchat".into() }, cx);
    work(cx);
    let (b, b_ids) = colliding_catalog(&dir, "b", 2);
    assert_eq!(b_ids, ids);
    core_switch(&app, b);
    let before = app.state.jobs.localsend.job_ids_issued();
    click(&app, "send-send", cx);
    work(cx);
    view.read_with(cx, |p, _| assert_eq!(p.status, CATALOG_CHANGED));
    assert_eq!(app.state.jobs.localsend.job_ids_issued(), before, "nothing claimed");
    assert!(net.lock().unwrap().sends.is_empty());
    assert!(publications(&app, ids[0]).is_empty(), "nothing recorded in B");
    assert!(has_dialog(&app, cx));
    deliver_switch(&app, cx);
    assert!(!has_dialog(&app, cx), "the dialog closed on catalog:switched");
}

/// A send claimed before the switch: core's switch trips it, so it never runs.
#[gpui_kit::test]
fn a_switch_after_the_claim_stops_the_send(cx: &mut TestAppContext) {
    let dir = TempDir::new("localsend-switch-run");
    let app = start(cx);
    catalog_with_files(&app, &dir, 2, cx);
    let net = Arc::new(Mutex::new(Net::default()));
    let view = open(&app, fake(vec![device("Phone", "fp", "192.168.1.9")], net.clone()), SendMode::Transfer, cx);
    work(cx);
    click(&app, "send-send", cx);
    step(cx); // the claim
    let (b, _) = colliding_catalog(&dir, "b", 2);
    core_switch(&app, b);
    work(cx);
    view.read_with(cx, |p, _| assert_eq!(p.status, SEND_CANCELLED));
    assert!(net.lock().unwrap().sends.is_empty());
}

/// No selection: "Select a photo", no scan.
#[gpui_kit::test]
fn without_a_selection_the_form_says_so(cx: &mut TestAppContext) {
    let dir = TempDir::new("localsend-empty");
    let app = start(cx);
    catalog_with_files(&app, &dir, 1, cx);
    app.wired.shell.update(cx, |s, cx| {
        s.library.clear_selection();
        cx.notify();
    });
    let net = Arc::new(Mutex::new(Net::default()));
    open(&app, fake(Vec::new(), net.clone()), SendMode::Transfer, cx);
    work(cx);
    assert!(present(&app, "send-empty", cx));
    assert_eq!(net.lock().unwrap().discoveries, 0);
}
