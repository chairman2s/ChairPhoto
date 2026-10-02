//! Headless tests of the AI Tagging module (#126) through the real wiring: the settings panel,
//! the cloud opt-in gate (no request without consent), the bulk cloud cost confirm, the
//! suggestion list's accept/reject (single, all, groups) and catalog identity across a switch.
//!
//! No provider is ever called: [`FakeAi`] stands in for the provider side
//! ([`AiBackend`] — the only path a photo leaves by) and counts every call, so "nothing was
//! sent" is an assertion, not an assumption. The estimate is the real core clustering (local,
//! no provider). Catalog work runs on `Runner::manual` ([`work`]).

use super::*;
use crate::modules::ai_tagging::panel::AiPanel;
use crate::modules::ai_tagging::settings::AiSettings;
use crate::modules::ai_tagging::state::{AiBackend, AiBackendGlobal, AiState};
use crate::modules::ai_tagging::AI_MODULE_ID;
use crate::modules::{ModuleRegistry, PanelSlot};
use crate::shell::state::InspectorTab;
use crate::storage::Runner;
use chairphoto_core::app::ai::{self as core_ai, AiSuggestion, Confirmed, GroupedDispatchResult, Region};
use chairphoto_core::app::{CatalogIdentity, CATALOG_CHANGED};
use chairphoto_core::plugins::ai as plugin_ai;
use gpui_kit::SharedString;
use std::sync::Mutex;

type SuggestCall = (CatalogIdentity, i64, Option<String>, Option<Region>);

/// The provider side, recorded. Never touches the network.
#[derive(Default)]
struct FakeAi {
    suggests: Mutex<Vec<SuggestCall>>,
    grouped: Mutex<Vec<(CatalogIdentity, Vec<i64>)>>,
    ollama: Mutex<Vec<String>>,
    /// What `suggest` answers.
    answer: Mutex<Vec<AiSuggestion>>,
}

impl AiBackend for FakeAi {
    fn suggest(
        &self,
        _: &AppState,
        from: CatalogIdentity,
        _: Confirmed,
        photo: i64,
        question: Option<String>,
        region: Option<Region>,
    ) -> Result<Vec<AiSuggestion>, String> {
        self.suggests.lock().unwrap().push((from, photo, question, region));
        Ok(self.answer.lock().unwrap().clone())
    }

    fn suggest_grouped(&self, _: &AppState, from: CatalogIdentity, _: Confirmed, photos: Vec<i64>) -> Result<GroupedDispatchResult, String> {
        let n = photos.len();
        self.grouped.lock().unwrap().push((from, photos));
        Ok(GroupedDispatchResult { total: n, representatives: 1, dispatched: 1, propagated: n })
    }

    fn ollama_models(&self, url: &str) -> Result<Vec<String>, String> {
        self.ollama.lock().unwrap().push(url.to_string());
        Ok(vec!["llava:latest".into(), "moondream".into()])
    }
}

impl FakeAi {
    fn sent(&self) -> usize {
        self.suggests.lock().unwrap().len() + self.grouped.lock().unwrap().len()
    }
}

/// The provider under the real core bodies, counted: no preview is read from disk and nothing
/// is sent. Each call records the engine it was asked to send with.
#[derive(Default)]
struct CountingProvider {
    calls: Mutex<Vec<String>>,
}

impl core_ai::Provider for CountingProvider {
    fn image(&self, _: &std::path::Path, _: Option<Region>) -> Result<String, String> {
        Ok("aW1n".into())
    }

    async fn suggest(
        &self,
        config: &plugin_ai::Config,
        _: &str,
        _: &str,
        _: &[String],
        _: Option<&str>,
    ) -> Result<Vec<plugin_ai::Raw>, String> {
        self.calls.lock().unwrap().push(format!("{}/{}", config.provider, config.model()));
        Ok(Vec::new())
    }
}

/// The real core run bodies (settings read, consent check, clustering, storing) over a
/// [`CountingProvider`].
struct ViaCore(Arc<CountingProvider>);

impl AiBackend for ViaCore {
    fn suggest(
        &self,
        app: &AppState,
        from: CatalogIdentity,
        confirmed: Confirmed,
        photo: i64,
        question: Option<String>,
        region: Option<Region>,
    ) -> Result<Vec<AiSuggestion>, String> {
        let run = core_ai::suggest_tags_via(app, Some(from), Some(confirmed), photo, question, region, self.0.clone());
        chairphoto_core::app::runtime().block_on(run)
    }

    fn suggest_grouped(&self, app: &AppState, from: CatalogIdentity, confirmed: Confirmed, photos: Vec<i64>) -> Result<GroupedDispatchResult, String> {
        let run = core_ai::suggest_tags_grouped_via(app, Some(from), Some(confirmed), photos, self.0.clone());
        chairphoto_core::app::runtime().block_on(run)
    }

    fn ollama_models(&self, _: &str) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}

/// Run queued catalog work and repaint until nothing more happens.
fn work(app: &App, cx: &mut TestAppContext) {
    for _ in 0..30 {
        let ran = cx.update(|cx| Runner::get(cx).run_pending());
        cx.run_until_parked();
        cx.update_window(app.window(), |_, window, cx| window.render_frame(cx)).unwrap();
        cx.run_until_parked();
        if ran == 0 && cx.update(|cx| Runner::get(cx).pending()) == 0 {
            return;
        }
    }
}

fn with_cat<R>(app: &App, f: impl FnOnce(&Catalog) -> R) -> R {
    f(app.state.catalog.lock().unwrap().as_ref().unwrap())
}

struct Ai {
    app: App,
    fake: Arc<FakeAi>,
    ids: Vec<i64>,
    dir: TempDir,
}

/// The app with `n` photos, `settings` stored (`ai.<key>`), the fake provider side and the
/// module enabled.
fn open_ai(n: usize, settings: &[(&str, &str)], tag: &str, cx: &mut TestAppContext) -> Ai {
    let fake = Arc::new(FakeAi::default());
    cx.update(|cx| cx.set_global(AiBackendGlobal(fake.clone())));
    let dir = TempDir::new(tag);
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, n, cx);
    with_cat(&app, |c| {
        plugin_ai::ensure_schema(c.conn()).unwrap();
        for (k, v) in settings {
            c.set_setting(&format!("ai.{k}"), v).unwrap();
        }
    });
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, AI_MODULE_ID, cx));
    app.wired.shell.update(cx, |s, cx| s.set_inspector_tab(InspectorTab::Tags, cx));
    work(&app, cx);
    Ai { app, fake, ids, dir }
}

fn raw(path: &str) -> plugin_ai::Raw {
    plugin_ai::Raw { path: path.into(), confidence: 0.8, reason: "seen".into(), description: "new one".into(), synonyms: vec![] }
}

impl Ai {
    fn window(&self) -> AnyWindowHandle {
        self.app.window()
    }

    fn settings(&self, cx: &mut TestAppContext) -> Entity<AiSettings> {
        let modules = self.app.wired.modules.clone();
        cx.update_window(self.window(), |_, window, cx| {
            ModuleRegistry::settings_views(&modules, &AI_MODULE_ID.into(), window, cx)
                .into_iter()
                .next()
                .expect("the AI settings panel")
                .downcast::<AiSettings>()
                .expect("an AiSettings")
        })
        .unwrap()
    }

    fn panel(&self, cx: &mut TestAppContext) -> Entity<AiPanel> {
        let modules = self.app.wired.modules.clone();
        cx.update_window(self.window(), |_, window, cx| {
            ModuleRegistry::panel_views(&modules, PanelSlot::Inspector, window, cx)
                .into_iter()
                .find(|p| p.id.as_ref() == "ai-tags")
                .expect("the AI tags panel")
                .view
                .downcast::<AiPanel>()
                .ok()
                .expect("an AiPanel")
        })
        .unwrap()
    }

    fn state(&self, cx: &mut TestAppContext) -> Entity<AiState> {
        let panel = self.panel(cx);
        panel.read_with(cx, |p, _| p.state.clone())
    }

    fn select(&self, id: i64, cx: &mut TestAppContext) {
        self.app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_single(id)));
        work(&self.app, cx);
    }

    fn select_all(&self, cx: &mut TestAppContext) {
        self.app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_all()));
        work(&self.app, cx);
    }

    /// Click `id` in the main window (the inspector's tags tab shows the panel).
    fn click(&self, id: &str, cx: &mut TestAppContext) {
        let id = SharedString::from(id.to_string());
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .unwrap();
        work(&self.app, cx);
    }

    fn label(&self, id: &str, cx: &mut TestAppContext) -> Option<String> {
        let id = SharedString::from(id.to_string());
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).and_then(|e| e.label().map(str::to_string))
        })
        .unwrap()
    }

    fn present(&self, id: &str, cx: &mut TestAppContext) -> bool {
        let id = SharedString::from(id.to_string());
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .unwrap()
    }

    fn photo_tags(&self, photo: i64) -> Vec<String> {
        with_cat(&self.app, |c| {
            let mut stmt = c
                .conn()
                .prepare("SELECT t.full_path FROM photo_tags pt JOIN tags t ON t.id = pt.tag_id WHERE pt.photo_id = ?1 ORDER BY 1")
                .unwrap();
            stmt.query_map([photo], |r| r.get(0)).unwrap().collect::<Result<Vec<String>, _>>().unwrap()
        })
    }
}

// --- settings ------------------------------------------------------------------------------

/// The settings read from the catalog (blank = the default), the engine and the selected
/// provider's key save into it, the key field is masked; a save after a switch the UI has not
/// heard of is refused and touches neither catalog.
#[gpui_kit::test]
fn settings_save_into_the_catalog_they_were_read_from(cx: &mut TestAppContext) {
    let a = open_ai(1, &[("ollama_model", "moondream")], "ai-settings", cx);
    let settings = a.settings(cx);
    // Preferences → AI Tagging: the panel renders, and its inputs fill from the catalog.
    cx.update_window(a.window(), |_, window, cx| window.dispatch_action(Box::new(crate::shell::actions::OpenPreferences), cx))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(450)); // the dialog's open animation
    cx.run_until_parked();
    a.click("prefs-tab-module-ai", cx);
    assert!(a.present("ai-settings", cx), "the module's tab shows its settings panel");
    settings.read_with(cx, |s, cx| {
        assert_eq!(s.provider, "ollama");
        assert_eq!(s.inputs["ollama_model"].read(cx).value().as_ref(), "moondream");
        assert_eq!(s.inputs["ollama_url"].read(cx).value().as_ref(), "http://localhost:11434", "blank shows the default");
    });
    assert!(!a.fake.ollama.lock().unwrap().is_empty(), "the local model list is asked of Ollama");

    cx.update_window(a.window(), |_, window, cx| {
        settings.update(cx, |s, cx| {
            s.set_provider("claude", cx);
            s.set_input("cloud_api_key", "sk-test-not-real", window, cx);
            s.existing_only = true;
            s.save(cx);
        })
    })
    .unwrap();
    work(&a.app, cx);
    with_cat(&a.app, |c| {
        assert_eq!(c.get_setting("ai.provider").unwrap().as_deref(), Some("claude"));
        assert_eq!(c.get_setting("ai.cloud_api_key").unwrap().as_deref(), Some("sk-test-not-real"));
        assert_eq!(c.get_setting("ai.existing_only").unwrap().as_deref(), Some("true"));
    });
    let masked = settings.read_with(cx, |s, cx| s.inputs["cloud_api_key"].read(cx).presentation().is_masked());
    assert!(masked, "the API key field is masked");
    assert!(a.present("ai-cloud-note", cx), "a cloud engine says photos are uploaded");
    assert_eq!(a.fake.sent(), 0, "saving settings sends nothing");

    // Another catalog is open, `catalog:switched` not delivered: the save is refused.
    let (other, _) = colliding_catalog(&a.dir, "other", 1);
    core_switch(&a.app, other);
    settings.update(cx, |s, cx| {
        s.provider = "gemini".into();
        s.save(cx)
    });
    work(&a.app, cx);
    assert!(status(&a.app, cx).contains(CATALOG_CHANGED), "{}", status(&a.app, cx));
    with_cat(&a.app, |c| assert_eq!(c.get_setting("ai.provider").unwrap(), None, "the new catalog is untouched"));
}

// --- privacy: the cloud opt-in ------------------------------------------------------------

/// A cloud engine without its saved API key gets nothing: neither "Suggest tags" nor a batch
/// reaches the provider side, and the panel says how to opt in. Ollama (local) runs at once.
///
/// Mutation-checked: removing the `may_send` check from `AiState::run` makes the first
/// `sent() == 0` fail.
#[gpui_kit::test]
fn no_photo_reaches_a_cloud_engine_without_its_key(cx: &mut TestAppContext) {
    let a = open_ai(3, &[("provider", "claude")], "ai-optin", cx);
    a.select(a.ids[0], cx);
    a.click("ai-suggest", cx);
    assert_eq!(a.fake.sent(), 0, "a photo went to a cloud engine with no key saved");
    let state = a.state(cx);
    state.read_with(cx, |s, _| {
        let e = s.error.as_deref().unwrap();
        assert!(e.contains("API key"), "{e}");
    });

    a.select_all(cx);
    a.click("ai-batch", cx);
    assert_eq!(a.fake.sent(), 0, "a batch went to a cloud engine with no key saved");
    state.read_with(cx, |s, _| assert!(s.confirm.is_none(), "no estimate is even offered"));

    // The local engine needs no opt-in.
    state.update(cx, |s, cx| s.set_provider("ollama", cx));
    work(&a.app, cx);
    a.select(a.ids[0], cx);
    a.click("ai-suggest", cx);
    assert_eq!(a.fake.suggests.lock().unwrap().len(), 1);
}

/// An Ollama URL that is not a loopback literal is remote: neither Suggest nor a batch reaches
/// the provider side until the user allows that server in the settings (which saves its URL as
/// `ai.ollama_remote_url`); the settings say photos leave the machine. Pointing the URL at
/// another host needs a new opt-in. Loopback Ollama needs none (the test above).
///
/// Mutation-checked: `Stored::opt_in` always Ok makes the first `sent() == 0` fail.
#[gpui_kit::test]
fn a_remote_ollama_gets_nothing_until_allowed(cx: &mut TestAppContext) {
    let remote = "http://192.168.1.20:11434";
    let a = open_ai(3, &[("ollama_url", remote)], "ai-remote-ollama", cx);
    a.select(a.ids[0], cx);
    a.click("ai-suggest", cx);
    assert_eq!(a.fake.sent(), 0, "a photo went to an Ollama server elsewhere without its opt-in");
    let state = a.state(cx);
    state.read_with(cx, |s, _| assert!(s.error.as_deref().unwrap().contains("not on this machine")));
    a.select_all(cx);
    a.click("ai-batch", cx);
    assert_eq!(a.fake.sent(), 0, "a batch went to an Ollama server elsewhere without its opt-in");

    // Preferences: the server is flagged remote; allowing it saves its URL.
    cx.update_window(a.window(), |_, window, cx| window.dispatch_action(Box::new(crate::shell::actions::OpenPreferences), cx))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(450)); // the dialog's open animation
    cx.run_until_parked();
    a.click("prefs-tab-module-ai", cx);
    assert!(a.present("ai-remote-note", cx), "the settings say photos leave the machine");
    a.click("ai-set-ollama-remote", cx);
    a.click("ai-set-save", cx);
    with_cat(&a.app, |c| assert_eq!(c.get_setting("ai.ollama_remote_url").unwrap().as_deref(), Some(remote)));
    // (Preferences is still open over the panel: run as its Suggest button does.)
    a.select(a.ids[0], cx);
    state.update(cx, |s, cx| s.run(None, false, cx));
    work(&a.app, cx);
    state.read_with(cx, |s, _| assert_eq!(s.error, None));
    assert_eq!(a.fake.suggests.lock().unwrap().len(), 1, "allowed, the server is asked");

    // Another host is another opt-in.
    state.update(cx, |s, cx| s.save(vec![("ollama_url", "http://192.168.1.21:11434".into())], cx));
    work(&a.app, cx);
    state.update(cx, |s, cx| s.run(None, false, cx));
    work(&a.app, cx);
    assert_eq!(a.fake.suggests.lock().unwrap().len(), 1, "the opt-in carried to another server");
}

// --- the bulk cloud confirm ---------------------------------------------------------------

/// A cloud batch of several photos waits for Proceed: the estimate (per burst
/// representative) is shown, Cancel sends nothing, a changed model voids the confirm, and only
/// Proceed sends — the confirmed photos, bound to their catalog. A local batch runs at once.
///
/// Mutation-checked: making `run_batch` dispatch cloud batches directly fails the first
/// `sent() == 0`.
#[gpui_kit::test]
fn a_cloud_batch_waits_for_the_cost_confirm(cx: &mut TestAppContext) {
    let a = open_ai(3, &[("provider", "claude"), ("cloud_api_key", "sk-test-not-real")], "ai-bulk", cx);
    a.select(a.ids[0], cx);
    a.select_all(cx);
    a.click("ai-batch", cx);
    assert_eq!(a.fake.sent(), 0, "the batch was sent before the confirm");
    let state = a.state(cx);
    let (reps, count) = state.read_with(cx, |s, _| {
        let c = s.confirm.as_ref().expect("the confirm is open");
        assert_eq!(c.model, "claude-sonnet-4-6");
        (c.representatives, c.count)
    });
    assert_eq!(count, 3);
    let expected = format!(
        "Grouping: 3 photos → {reps} representatives → {}",
        crate::modules::ai_tagging::logic::estimate_bulk_cost("claude-sonnet-4-6", reps).unwrap()
    );
    assert_eq!(a.label("ai-bulk-estimate", cx).as_deref(), Some(expected.as_str()));

    a.click("ai-bulk-cancel", cx);
    assert_eq!(a.fake.sent(), 0, "Cancel sent the batch");
    state.read_with(cx, |s, _| assert!(s.confirm.is_none()));

    // Open again, then the model changes (Preferences): the estimate is void.
    a.click("ai-batch", cx);
    state.read_with(cx, |s, _| assert!(s.confirm.is_some()));
    state.update(cx, |s, cx| s.save(vec![("cloud_model", "claude-haiku-4-5-20251001".into())], cx));
    work(&a.app, cx);
    state.read_with(cx, |s, _| assert!(s.confirm.is_none(), "a changed model must close the confirm"));
    assert_eq!(a.fake.sent(), 0);

    a.click("ai-batch", cx);
    a.click("ai-bulk-proceed", cx);
    let grouped = a.fake.grouped.lock().unwrap().clone();
    assert_eq!(grouped.len(), 1, "Proceed sends once");
    let mut sent = grouped[0].1.clone();
    sent.sort();
    assert_eq!(sent, a.ids);
    assert_eq!(Some(grouped[0].0), a.app.wired.shell.read_with(cx, |s, _| s.rows_from()));
    assert!(a.label("ai-batch-msg", cx).unwrap().starts_with("Done — 3 photos"));

    // Local: no confirm.
    state.update(cx, |s, cx| s.set_provider("ollama", cx));
    work(&a.app, cx);
    a.click("ai-batch", cx);
    assert_eq!(a.fake.grouped.lock().unwrap().len(), 2, "a local batch runs at once");
}

/// Give every photo an original on disk, so a run gets as far as its provider (the
/// [`CountingProvider`] never reads it).
fn reachable(a: &Ai) {
    for i in 0..a.ids.len() {
        let p = a.dir.0.join(format!("photos/2026/p{i}.ARW"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"raw").unwrap();
    }
}

/// A settings save that lands in the catalog between the user's consent and the run — the
/// confirm's Proceed, or Suggest — sends nothing: the run carries the engine the user saw, and
/// the core refuses because the catalog now names another (#126 review).
///
/// Forced interleaving: the save's catalog write runs on the manual runner, and Proceed /
/// Suggest are clicked before its answer reaches `AiState` (so the module's own checks still
/// see the old engine). Mutation-checked: dropping `admit` from the core's grouped body makes
/// the first `calls` assertion fail (the batch goes to the unconfirmed model).
#[gpui_kit::test]
fn a_save_in_the_consent_window_sends_nothing(cx: &mut TestAppContext) {
    let a = open_ai(3, &[("provider", "claude"), ("cloud_api_key", "sk-test-not-real")], "ai-consent-window", cx);
    let provider = Arc::new(CountingProvider::default());
    cx.update(|cx| cx.set_global(AiBackendGlobal(Arc::new(ViaCore(provider.clone())))));
    reachable(&a);
    let state = a.state(cx);
    a.select(a.ids[0], cx);
    a.select_all(cx);
    a.click("ai-batch", cx);
    state.read_with(cx, |s, _| assert_eq!(s.confirm.as_ref().expect("the confirm is open").model, "claude-sonnet-4-6"));

    // Preferences saves another model: its write lands, its answer has not reached the module.
    state.update(cx, |s, cx| s.save(vec![("cloud_model", "claude-opus-4-8".into())], cx));
    assert_eq!(cx.update(|cx| Runner::get(cx).run_pending()), 1, "the save's write ran");
    with_cat(&a.app, |c| assert_eq!(c.get_setting("ai.cloud_model").unwrap().as_deref(), Some("claude-opus-4-8")));
    state.update(cx, |s, cx| s.proceed(cx));
    work(&a.app, cx);
    assert!(provider.calls.lock().unwrap().is_empty(), "the batch went to a model the user did not confirm");
    state.read_with(cx, |s, _| assert_eq!(s.error.as_deref(), Some(core_ai::ENGINE_CHANGED)));

    // The same for one photo: Suggest asked with Claude, the catalog now says Ollama.
    a.select(a.ids[0], cx);
    state.update(cx, |s, cx| s.save(vec![("provider", "ollama".into())], cx));
    assert_eq!(cx.update(|cx| Runner::get(cx).run_pending()), 1);
    state.update(cx, |s, cx| s.run(None, false, cx));
    work(&a.app, cx);
    assert!(provider.calls.lock().unwrap().is_empty(), "the photo went to an engine the user did not ask");
    state.read_with(cx, |s, _| assert_eq!(s.error.as_deref(), Some(core_ai::ENGINE_CHANGED)));

    // Asked again, with the engine now shown, it sends — with exactly that engine.
    a.click("ai-suggest", cx);
    assert_eq!(*provider.calls.lock().unwrap(), vec!["ollama/llava:latest".to_string()]);
}

/// The confirm's answer is bound to the catalog it priced: after a switch it closes, and a
/// Proceed bound to the old catalog's photos sends nothing.
#[gpui_kit::test]
fn a_switch_closes_the_confirm_unsent(cx: &mut TestAppContext) {
    let a = open_ai(3, &[("provider", "claude"), ("cloud_api_key", "sk-test-not-real")], "ai-bulk-switch", cx);
    a.select(a.ids[0], cx);
    a.select_all(cx);
    a.click("ai-batch", cx);
    let state = a.state(cx);
    state.read_with(cx, |s, _| assert!(s.confirm.is_some()));
    let (b, _) = colliding_catalog(&a.dir, "b", 3);
    core_switch(&a.app, b);
    deliver_switch(&a.app, cx);
    work(&a.app, cx);
    state.update(cx, |s, cx| s.proceed(cx));
    work(&a.app, cx);
    assert_eq!(a.fake.sent(), 0, "the old catalog's confirm sent photos after a switch");
}

// --- suggestions --------------------------------------------------------------------------

/// A run's answer shows; ✓ add tags the photo (a new path is created), ✗ reject remembers;
/// a propagated group shows its provenance and accepts as a whole; ✓ all tags the selection.
#[gpui_kit::test]
fn suggestions_accept_reject_and_groups(cx: &mut TestAppContext) {
    let a = open_ai(3, &[], "ai-sugs", cx);
    let (p0, p1) = (a.ids[0], a.ids[1]);
    with_cat(&a.app, |c| {
        plugin_ai::upsert_pending(c.conn(), p0, &raw("Animals/Gull"), 1).unwrap();
        plugin_ai::upsert_pending(c.conn(), p0, &raw("Nature/Fog"), 1).unwrap();
        plugin_ai::upsert_pending_propagated(c.conn(), p0, &raw("Style/Moody"), p1, 1).unwrap();
        plugin_ai::upsert_pending_propagated(c.conn(), p0, &raw("Genre/Seascape"), p1, 1).unwrap();
    });
    a.select(p0, cx);
    assert!(a.present("ai-sug-Animals_Gull", cx));
    assert!(a.present("ai-rerun", cx), "propagated rows offer a direct re-run");

    a.click("ai-add-Animals_Gull", cx);
    assert_eq!(a.photo_tags(p0), vec!["Animals/Gull"]);
    assert_eq!(status(&a.app, cx), "Tagged: Animals/Gull");
    assert!(!a.present("ai-sug-Animals_Gull", cx));
    with_cat(&a.app, |c| {
        let id = c.find_tag_id_by_path("Animals/Gull").unwrap().unwrap();
        assert_eq!(c.get_tag(id).unwrap().description, "new one", "a new tag arrives documented");
    });

    a.click("ai-reject-Nature_Fog", cx);
    with_cat(&a.app, |c| assert_eq!(plugin_ai::rejected_paths(c.conn(), p0).unwrap(), vec!["Nature/Fog".to_string()]));

    a.click(&format!("ai-group-accept-{p1}"), cx);
    assert_eq!(a.photo_tags(p0), vec!["Animals/Gull", "Genre/Seascape", "Style/Moody"]);

    // A run's answer replaces the list; "✓ all (N)" tags every selected photo.
    *a.fake.answer.lock().unwrap() = vec![AiSuggestion {
        path: "Places/Harbour".into(),
        confidence: 0.9,
        reason: String::new(),
        existing_tag_id: None,
        is_new: true,
        description: String::new(),
        synonyms: vec![],
        source_photo_id: None,
        source_photo_filename: None,
    }];
    with_cat(&a.app, |c| plugin_ai::upsert_pending(c.conn(), p0, &raw("Places/Harbour"), 1).unwrap());
    a.click("ai-suggest", cx);
    assert!(a.present("ai-sug-Places_Harbour", cx));
    a.select_all(cx);
    a.click("ai-all-Places_Harbour", cx);
    for id in &a.ids {
        assert!(a.photo_tags(*id).contains(&"Places/Harbour".to_string()), "photo {id} untagged");
    }
}

/// The region box and the follow-up question go with the run; the box does not carry to the
/// next photo.
#[gpui_kit::test]
fn region_and_question_ride_with_the_run(cx: &mut TestAppContext) {
    let a = open_ai(2, &[], "ai-region", cx);
    a.select(a.ids[0], cx);
    let state = a.state(cx);
    let r = Region { x: 0.1, y: 0.2, w: 0.3, h: 0.4 };
    state.update(cx, |s, cx| {
        s.toggle_region_mode(cx);
        s.set_region(Some(r), cx);
    });
    let panel = a.panel(cx);
    cx.update_window(a.window(), |_, window, cx| {
        panel.update(cx, |p, cx| p.question.update(cx, |i, cx| i.set_value("what kind of boat?", window, cx)))
    })
    .unwrap();
    a.click("ai-ask", cx);
    let call = a.fake.suggests.lock().unwrap()[0].clone();
    assert_eq!((call.1, call.2.as_deref(), call.3), (a.ids[0], Some("what kind of boat?"), Some(r)));

    a.select(a.ids[1], cx);
    state.read_with(cx, |s, _| assert_eq!(s.region, None, "the box carried to the next photo"));
}

/// Catalog identity: an accept keyed by the old catalog's photo id, after a switch the UI has
/// not heard of, is refused and tags neither catalog; a run in flight across a delivered
/// switch lands nowhere.
#[gpui_kit::test]
fn writes_and_runs_stay_with_their_catalog(cx: &mut TestAppContext) {
    let a = open_ai(1, &[], "ai-identity", cx);
    let p0 = a.ids[0];
    with_cat(&a.app, |c| plugin_ai::upsert_pending(c.conn(), p0, &raw("Animals/Gull"), 1).unwrap());
    a.select(p0, cx);
    let (b, b_ids) = colliding_catalog(&a.dir, "b", 1);
    assert_eq!(b_ids[0], p0, "the ids collide");
    core_switch(&a.app, b);
    a.click("ai-add-Animals_Gull", cx);
    let state = a.state(cx);
    state.read_with(cx, |s, _| assert!(s.error.as_deref().unwrap().contains(CATALOG_CHANGED)));
    assert!(a.photo_tags(p0).is_empty(), "the new catalog's photo was tagged");

    // A run started now is bound to the old catalog; the switch's delivery drops its answer.
    state.update(cx, |s, cx| s.run(None, false, cx));
    deliver_switch(&a.app, cx);
    work(&a.app, cx);
    state.read_with(cx, |s, _| assert!(!s.busy && s.suggestions().is_none_or(|p| p.list.is_empty())));
}
