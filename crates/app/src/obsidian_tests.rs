//! Headless tests of the Obsidian module (#128) through the real wiring: the settings (saved
//! trimmed, a path or an escaping folder refused), the inspector's "Note" panel and the tag
//! editor's "Obsidian note" section (Create without a vault, Create, Open, Forget — the URIs
//! and records the React module produced), the note's links opening back into the app, and
//! catalog identity (a switch with colliding ids, announced or not).
//!
//! Nothing here launches Obsidian or touches a vault: `App::open_url` on GPUI's test platform
//! only records the URI (`TestAppContext::opened_url`), and ChairPhoto itself never writes a
//! vault file — the expected URIs are built with `chairphoto_model::obsidian`, whose own tests
//! pin them to the React functions' output.

use super::*;
use crate::modules::obsidian::state::{Kind, NoteView, ObsidianState};
use crate::modules::obsidian::views::{NotePanel, ObsidianSettings};
use crate::modules::obsidian::OBSIDIAN_MODULE_ID;
use crate::modules::{ModuleRegistry, PanelSlot};
use crate::shell::state::InspectorTab;
use crate::storage::Runner;
use chairphoto_core::app::CATALOG_CHANGED;
use chairphoto_model::deep_link::{self, DeepLink};
use chairphoto_model::obsidian::{self as ob, NoteRecord};
use gpui_kit::SharedString;

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

/// The module's stored key for `key` (`obsidian.<key>`). Built, not written out, so the
/// host-namespace scan (`modules::tests::every_host_settings_prefix_is_reserved`) does not
/// take the module's own keys for host keys.
fn ob_setting(key: &str) -> String {
    format!("{OBSIDIAN_MODULE_ID}.{key}")
}

fn with_cat<R>(app: &App, f: impl FnOnce(&Catalog) -> R) -> R {
    f(app.state.catalog.lock().unwrap().as_ref().unwrap())
}

struct Ob {
    app: App,
    ids: Vec<i64>,
    dir: TempDir,
}

fn open_ob(n: usize, tag: &str, cx: &mut TestAppContext) -> Ob {
    let dir = TempDir::new(tag);
    let app = start(cx);
    let ids = open_catalog_with_photos(&app, &dir, n, cx);
    work(&app, cx);
    cx.update(|cx| ModuleRegistry::enable(&app.wired.modules, OBSIDIAN_MODULE_ID, cx));
    app.wired.shell.update(cx, |s, cx| s.set_inspector_tab(InspectorTab::Tags, cx));
    work(&app, cx);
    Ob { app, ids, dir }
}

impl Ob {
    fn window(&self) -> AnyWindowHandle {
        self.app.window()
    }

    fn state(&self, cx: &mut TestAppContext) -> Entity<ObsidianState> {
        let modules = self.app.wired.modules.clone();
        let panel = cx
            .update_window(self.window(), |_, window, cx| {
                ModuleRegistry::panel_views(&modules, PanelSlot::Inspector, window, cx)
                    .into_iter()
                    .find(|p| p.id.as_ref() == "obsidian-note")
                    .expect("the Note panel")
                    .view
                    .downcast::<NotePanel>()
                    .ok()
                    .expect("a NotePanel")
            })
            .unwrap();
        panel.read_with(cx, |p, _| p.state.clone())
    }

    fn settings(&self, cx: &mut TestAppContext) -> Entity<ObsidianSettings> {
        let modules = self.app.wired.modules.clone();
        cx.update_window(self.window(), |_, window, cx| {
            ModuleRegistry::settings_views(&modules, &OBSIDIAN_MODULE_ID.into(), window, cx)
                .into_iter()
                .next()
                .expect("the settings panel")
                .downcast::<ObsidianSettings>()
                .expect("an ObsidianSettings")
        })
        .unwrap()
    }

    fn select(&self, id: i64, cx: &mut TestAppContext) {
        self.app.wired.shell.update(cx, |s, cx| s.select_with(cx, |l| l.select_single(id)));
        work(&self.app, cx);
    }

    fn click(&self, id: &str, cx: &mut TestAppContext) {
        let id = SharedString::from(id.to_string());
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .unwrap();
        work(&self.app, cx);
    }

    fn present(&self, id: &str, cx: &mut TestAppContext) -> bool {
        let id = SharedString::from(id.to_string());
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).is_some()
        })
        .unwrap()
    }

    fn label(&self, id: &str, cx: &mut TestAppContext) -> Option<String> {
        let id = SharedString::from(id.to_string());
        cx.update_window(self.window(), |_, window, cx| {
            window.render_frame(cx);
            window.try_find(id).and_then(|e| e.label().map(str::to_string))
        })
        .unwrap()
    }

    fn setting(&self, key: &str) -> Option<String> {
        with_cat(&self.app, |c| c.get_setting(key).unwrap())
    }

    /// Every `obsidian.*` setting of the open catalog.
    fn obsidian_settings(&self) -> Vec<(String, String)> {
        with_cat(&self.app, |c| {
            let mut stmt = c.conn().prepare("SELECT key, value FROM settings WHERE key LIKE 'obsidian.%' ORDER BY key").unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
        })
    }

    fn set_vault(&self, vault: &str) {
        with_cat(&self.app, |c| c.set_setting(&ob_setting("vault"), vault).unwrap());
    }

    fn set_input(&self, input: &Entity<gpui_kit::component::input::InputState>, text: &str, cx: &mut TestAppContext) {
        let (input, text) = (input.clone(), text.to_string());
        cx.update_window(self.window(), |_, window, cx| input.update(cx, |i, cx| i.set_value(text, window, cx))).unwrap();
        cx.run_until_parked();
    }

    /// Open the tag editor on `tag` (the tag panel's edit button).
    fn edit_tag(&self, tag: i64, cx: &mut TestAppContext) {
        self.click(&format!("tag-edit-{tag}"), cx);
        std::thread::sleep(std::time::Duration::from_millis(300)); // the dialog's open animation
        work(&self.app, cx);
    }
}

/// The `chairphoto://` links in a note's text.
fn links(content: &str) -> Vec<String> {
    content.split(['(', ')']).filter(|s| s.starts_with("chairphoto://")).map(str::to_string).collect()
}

/// The `content` parameter of an `obsidian://new` URI, decoded.
fn content_of(uri: &str) -> String {
    let encoded = uri.split("&content=").nth(1).expect("a content parameter");
    let bytes = encoded.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            out.push(u8::from_str_radix(&encoded[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

// --- settings -------------------------------------------------------------------------------

/// The settings panel shows the stored values; Save stores them trimmed (the folder without
/// its surrounding slashes) and says so; a path for the vault or a folder that leaves the
/// vault is refused with its reason and stores nothing; after an unannounced switch to another
/// catalog the save is refused and writes neither.
///
/// Mutation-checked: saving without `ob::vault_name`'s check stores the path and fails here.
#[gpui_kit::test]
fn settings_are_validated_saved_and_bound_to_their_catalog(cx: &mut TestAppContext) {
    let s = open_ob(1, "ob-settings", cx);
    s.set_vault("Old Vault");
    s.app.wired.model.update(cx, |m, cx| m.refresh(cx));
    work(&s.app, cx);
    let settings = s.settings(cx);
    cx.update_window(s.window(), |_, window, cx| window.dispatch_action(Box::new(crate::shell::actions::OpenPreferences), cx))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(450)); // the dialog's open animation
    cx.run_until_parked();
    s.click("prefs-tab-module-obsidian", cx);
    assert!(s.present("obsidian-settings", cx), "the module's Preferences tab shows its settings");
    let (vault, folder) = settings.read_with(cx, |v, _| (v.vault.clone(), v.folder.clone()));
    assert_eq!(vault.read_with(cx, |i, _| i.value().to_string()), "Old Vault", "the stored vault is shown");

    s.set_input(&vault, "/home/me/Vaults/Photos", cx);
    s.click("obsidian-save", cx);
    assert!(s.present("obsidian-settings-error", cx), "a path is refused");
    settings.read_with(cx, |v, _| assert!(v.error.as_deref().is_some_and(|e| e.contains("not its path")), "{:?}", v.error));
    assert_eq!(s.setting("obsidian.vault").as_deref(), Some("Old Vault"));

    s.set_input(&vault, "  My Vault ", cx);
    s.set_input(&folder, "../elsewhere", cx);
    s.click("obsidian-save", cx);
    assert!(s.present("obsidian-settings-error", cx), "a folder outside the vault is refused");
    assert_eq!(s.setting("obsidian.vault").as_deref(), Some("Old Vault"), "a refused save stores nothing");

    s.set_input(&folder, " /Photo Notes/ ", cx);
    s.click("obsidian-save", cx);
    assert!(!s.present("obsidian-settings-error", cx));
    assert_eq!(s.setting("obsidian.vault").as_deref(), Some("My Vault"));
    assert_eq!(s.setting("obsidian.folder").as_deref(), Some("Photo Notes"));
    assert_eq!(status(&s.app, cx), "Obsidian settings saved");

    let (other, _) = colliding_catalog(&s.dir, "other", 1);
    core_switch(&s.app, other);
    s.set_input(&vault, "Elsewhere", cx);
    settings.update(cx, |v, cx| v.save(cx));
    work(&s.app, cx);
    assert!(status(&s.app, cx).contains(CATALOG_CHANGED), "{}", status(&s.app, cx));
    assert!(s.obsidian_settings().is_empty(), "the save reached the new catalog");
}

// --- photo notes ----------------------------------------------------------------------------

/// The inspector's "Note": without a vault, Create only says where to set one (nothing opens,
/// nothing is stored). With one, Create hands Obsidian React's `obsidian://new` URI and stores
/// React's record; the panel then shows the note with Open (React's `obsidian://open`) and
/// Forget, which blanks the record and opens nothing. The note's links open the photo and
/// its loupe through the app's own deep-link handling.
///
/// Mutation-checked: keying the photo's record `tagnote.` fails it (and the React-record
/// test); dropping the empty-vault refusal opens a URI without a vault and fails it.
#[gpui_kit::test]
fn photo_note_create_open_forget(cx: &mut TestAppContext) {
    let s = open_ob(2, "ob-photo", cx);
    let p = s.ids[1];
    let (photo, tags) = with_cat(&s.app, |c| {
        let t = c.create_tag("Street Photography/Old Town").unwrap();
        c.assign_tag(p, t).unwrap();
        (c.get_photo(p).unwrap(), c.get_photo_tags(p).unwrap())
    });
    let state = s.state(cx);
    state.read_with(cx, |st, _| assert_eq!(st.slot(Kind::Photo).view, NoteView::None, "no active photo"));
    s.select(p, cx);
    assert!(s.present("obsidian-create", cx));

    s.click("obsidian-create", cx);
    assert_eq!(status(&s.app, cx), ob::NO_VAULT);
    assert_eq!(cx.opened_url(), None, "nothing opens without a vault");
    assert!(s.obsidian_settings().is_empty(), "nothing is stored without a vault");

    s.set_vault("My Vault"); // read at click time, as React did
    s.click("obsidian-create", cx);
    let file = ob::note_file(ob::DEFAULT_FOLDER, &photo);
    let content = ob::note_content(&photo, &tags);
    let uri = cx.opened_url().expect("Obsidian was asked to create the note");
    assert_eq!(uri, ob::new_uri("My Vault", &file, &content));
    assert!(content.contains("  - StreetPhotography/OldTown"), "{content}");
    let raw = s.setting(&format!("obsidian.note.{}", photo.uuid)).expect("the record");
    let record = NoteRecord::parse(&raw).unwrap();
    assert_eq!((record.vault.as_str(), record.file.as_str()), ("My Vault", file.as_str()));
    assert!(raw.starts_with(r#"{"vault":"My Vault","file":"#) && raw.contains(r#""createdAt":"#), "{raw}");
    assert_eq!(s.label("obsidian-file", cx).as_deref(), Some(ob::note_name(&photo).as_str()));

    s.click("obsidian-open", cx);
    assert_eq!(cx.opened_url().as_deref(), Some(ob::open_uri("My Vault", &file).as_str()));

    s.click("obsidian-forget", cx);
    assert_eq!(s.setting(&format!("obsidian.note.{}", photo.uuid)).as_deref(), Some(""), "Forget blanks the record");
    assert!(s.present("obsidian-create", cx), "the panel offers Create again");
    assert_eq!(cx.opened_url().as_deref(), Some(ob::open_uri("My Vault", &file).as_str()), "Forget opens nothing");

    // The note's links come back into the app: the photo, then its loupe.
    s.select(s.ids[0], cx);
    let links = links(&content_of(&uri));
    assert_eq!(links.len(), 2);
    for (link, loupe) in links.iter().zip([false, true]) {
        assert!(matches!(deep_link::parse(link), Some(DeepLink::Photo { .. })));
        s.app.wired.model.update(cx, |m, cx| m.open_url(link, cx));
        work(&s.app, cx);
        s.app.wired.shell.read_with(cx, |sh, _| {
            assert_eq!(sh.library.selection().active_id, Some(p), "{link} selects the photo");
            assert_eq!(sh.loupe_open, loupe, "{link}");
        });
    }
}

/// A record written by the React app reads in the port, and the panel opens it.
#[gpui_kit::test]
fn a_react_record_is_read(cx: &mut TestAppContext) {
    let s = open_ob(1, "ob-react", cx);
    let uuid = with_cat(&s.app, |c| c.get_photo(s.ids[0]).unwrap().uuid);
    with_cat(&s.app, |c| {
        c.set_setting(&format!("obsidian.note.{uuid}"), r#"{"vault":"V","file":"ChairPhoto/undated p0 abc","createdAt":1767225600000}"#)
            .unwrap()
    });
    s.select(s.ids[0], cx);
    assert_eq!(s.label("obsidian-file", cx).as_deref(), Some("undated p0 abc"));
    s.click("obsidian-open", cx);
    assert_eq!(cx.opened_url().as_deref(), Some("obsidian://open?vault=V&file=ChairPhoto%2Fundated%20p0%20abc"));
}

/// Catalog identity: the panel shows a photo of catalog A; catalog B (same ids) is opened
/// without `catalog:switched` reaching the UI. Create and Forget are refused, open nothing and
/// write neither catalog. Once the switch is announced, the panel re-reads from B.
///
/// Mutation-checked: running Create, or Forget, under `with_catalog` instead of
/// `with_catalog_as` (whatever catalog is open) fails this test.
#[gpui_kit::test]
fn create_and_forget_fail_closed_after_a_switch(cx: &mut TestAppContext) {
    let s = open_ob(1, "ob-switch", cx);
    let p = s.ids[0];
    s.set_vault("A");
    s.select(p, cx);
    s.click("obsidian-create", cx);
    let a_uuid = with_cat(&s.app, |c| c.get_photo(p).unwrap().uuid);
    let a_record = s.setting(&format!("obsidian.note.{a_uuid}")).expect("A's record");
    let opened = cx.opened_url();

    let (b, b_ids) = colliding_catalog(&s.dir, "b", 1);
    assert_eq!(b_ids[0], p);
    b.set_setting(&ob_setting("vault"), "B").unwrap();
    let b_uuid = b.get_photo(p).unwrap().uuid;
    core_switch(&s.app, b);

    s.click("obsidian-forget", cx);
    assert!(status(&s.app, cx).contains(CATALOG_CHANGED), "{}", status(&s.app, cx));
    assert_eq!(reopen_a(&s.dir).get_setting(&ob_setting(&format!("note.{a_uuid}"))).unwrap().as_deref(), Some(a_record.as_str()), "A kept its record");

    // Forget was refused, so the panel still offers Open, not Create; make it Create again by
    // reading the photo as having no note, then try Create against the switched catalog.
    let state = s.state(cx);
    state.update(cx, |st, cx| {
        st.photo.view = match st.photo.view.clone() {
            NoteView::Ready(mut l) => {
                l.record = None;
                NoteView::Ready(l)
            }
            other => other,
        };
        cx.notify();
    });
    s.click("obsidian-create", cx);
    assert!(status(&s.app, cx).contains(CATALOG_CHANGED), "{}", status(&s.app, cx));
    assert_eq!(cx.opened_url(), opened, "a refused Create opens nothing");
    assert_eq!(s.obsidian_settings(), vec![("obsidian.vault".to_string(), "B".to_string())], "B got no record");
    state.read_with(cx, |st, _| assert!(!st.photo.creating, "the refusal ends Creating…"));

    deliver_switch(&s.app, cx);
    work(&s.app, cx);
    s.select(p, cx);
    state.read_with(cx, |st, _| {
        let l = st.slot(Kind::Photo).linked().expect("re-read from B");
        assert_eq!(l.uuid, b_uuid, "the panel names B's photo");
        assert_eq!(l.record, None);
    });
}

/// Run `held` Runner work (from `hold_pending`) on this thread, without letting its results
/// land: a worker that has read and answered, whose answer the UI has not taken yet.
fn run_held(held: Vec<Box<dyn FnOnce() + Send>>) -> usize {
    let n = held.len();
    for w in held {
        w();
    }
    n
}

/// Hold everything queued on the Runner (work started but not yet picked up by a worker).
fn hold(cx: &mut TestAppContext) -> Vec<Box<dyn FnOnce() + Send>> {
    cx.update(|cx| Runner::get(cx).hold_pending())
}

/// A refresh (`CatalogRead`) while a Create, then a Forget, is in flight forces a re-read of
/// the record. On a pool of workers that re-read can read the record before the write
/// commits and land after the write has: its answer is stale and must not undo the write on
/// the panel (Create offered again for a stored note, or Open for a forgotten one).
///
/// The test scheduler orders two ready landings at random, so the re-read's stale snapshot is
/// forced instead: the write commits and lands first, then the held re-read runs against the
/// catalog as it was before the write (the key put back as it was), then lands.
///
/// Mutation-checked: without the sequence bump in `set_record` the panel shows "Create note"
/// after Create and this test fails; bumping only when a record is set (Create, not Forget)
/// fails the Forget half.
#[gpui_kit::test]
fn a_stale_reread_does_not_undo_create_or_forget(cx: &mut TestAppContext) {
    let s = open_ob(1, "ob-reread", cx);
    let p = s.ids[0];
    s.set_vault("V");
    s.select(p, cx);
    let state = s.state(cx);
    let key = format!("obsidian.note.{}", with_cat(&s.app, |c| c.get_photo(p).unwrap().uuid));
    let set_raw = |value: Option<&str>| {
        with_cat(&s.app, |c| match value {
            Some(v) => c.set_setting(&key, v).unwrap(),
            None => {
                c.conn().execute("DELETE FROM settings WHERE key = ?1", [&key]).unwrap();
            }
        })
    };

    // Create starts; a refresh then forces a re-read of the record while Create is held.
    state.update(cx, |st, cx| st.create(Kind::Photo, cx));
    let create = hold(cx);
    assert_eq!(create.len(), 1, "Create's job");
    s.app.wired.model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    let reread = hold(cx);
    assert!(!reread.is_empty(), "the CatalogRead forced a re-read");
    // Create commits and lands.
    run_held(create);
    cx.run_until_parked();
    assert!(s.present("obsidian-open", cx), "Create landed");
    // The re-read, as read before Create committed (no record), lands after it.
    let stored = s.setting(&key).expect("Create's record");
    set_raw(None);
    run_held(reread);
    set_raw(Some(&stored));
    cx.run_until_parked();
    assert!(s.present("obsidian-open", cx), "a stale re-read does not take back Create's record");
    state.read_with(cx, |st, _| assert!(st.photo.linked().unwrap().record.is_some()));

    // Forget, the same way: the re-read read the record before Forget blanked it.
    state.update(cx, |st, cx| st.forget(Kind::Photo, cx));
    let forget = hold(cx);
    assert_eq!(forget.len(), 1, "Forget's job");
    s.app.wired.model.update(cx, |m, cx| m.refresh(cx));
    cx.run_until_parked();
    let reread = hold(cx);
    assert!(!reread.is_empty(), "the CatalogRead forced a re-read");
    run_held(forget);
    cx.run_until_parked();
    assert!(s.present("obsidian-create", cx), "Forget landed");
    set_raw(Some(&stored));
    run_held(reread);
    set_raw(Some(""));
    cx.run_until_parked();
    assert!(s.present("obsidian-create", cx), "a stale re-read does not bring back a forgotten record");
    state.read_with(cx, |st, _| assert!(st.photo.linked().unwrap().record.is_none()));
    work(&s.app, cx);
    assert!(s.present("obsidian-create", cx), "a later re-read agrees");
}

/// Catalog A ([`open_catalog_with_photos`]'s file), reopened after a switch away from it, to
/// check what it holds.
fn reopen_a(dir: &TempDir) -> Catalog {
    Catalog::open(&dir.0.join("photos.chairphoto"), &dir.0.join("photos")).unwrap()
}

// --- tag notes ------------------------------------------------------------------------------

/// The tag editor's "Obsidian note": Create hands Obsidian React's tag note (aliases from the
/// tag's terms, its description as the body) under `<folder>/Tags/`, stores React's record
/// under `tagnote.<uuid>`; the note's link filters the Library to the tag; Open and Forget act
/// on the tag's record. After an unannounced switch to a catalog with colliding tag ids,
/// Create is refused and writes neither catalog.
///
/// Mutation-checked: running Create under `with_catalog` instead of `with_catalog_as` fails
/// the identity half.
#[gpui_kit::test]
fn tag_note_create_open_forget_and_identity(cx: &mut TestAppContext) {
    let s = open_ob(2, "ob-tag", cx);
    s.set_vault("My Vault");
    with_cat(&s.app, |c| c.set_setting(&ob_setting("folder"), "Notes").unwrap());
    let tag_id = with_cat(&s.app, |c| {
        let id = c.create_tag("Places/Vestfold/Tønsberg").unwrap();
        c.set_tag_description(id, "  The old town.  ").unwrap();
        c.add_term(id, "Tunsberg", None, false, true).unwrap();
        c.add_term(id, "Tønsberg", Some("nb"), true, true).unwrap();
        c.assign_tag(s.ids[0], id).unwrap();
        id
    });
    s.app.wired.model.update(cx, |m, cx| m.refresh(cx));
    work(&s.app, cx);
    s.edit_tag(tag_id, cx);
    assert!(s.present("obsidian-tag-create", cx), "the tag editor shows the module's section");

    s.click("obsidian-tag-create", cx);
    let (tag, terms) = with_cat(&s.app, |c| (c.get_tag(tag_id).unwrap(), c.list_terms(tag_id).unwrap()));
    let file = ob::tag_note_file("Notes", &tag);
    let content = ob::tag_note_content(&tag, &ob::aliases(&terms));
    assert!(content.contains("aliases:\n") && content.ends_with("The old town.\n"), "{content}");
    let uri = cx.opened_url().expect("Obsidian was asked to create the tag note");
    assert_eq!(uri, ob::new_uri("My Vault", &file, &content));
    let record = NoteRecord::parse(&s.setting(&format!("obsidian.tagnote.{}", tag.uuid)).expect("the record")).unwrap();
    assert_eq!(record.file, file);
    assert!(file.starts_with("Notes/Tags/Tønsberg "), "{file}");

    s.click("obsidian-tag-open", cx);
    assert_eq!(cx.opened_url().as_deref(), Some(ob::open_uri("My Vault", &file).as_str()));
    s.click("obsidian-tag-forget", cx);
    assert_eq!(s.setting(&format!("obsidian.tagnote.{}", tag.uuid)).as_deref(), Some(""));
    assert!(s.present("obsidian-tag-create", cx));

    let link = links(&content_of(&uri)).pop().expect("the tag link");
    s.app.wired.model.update(cx, |m, cx| m.open_url(&link, cx));
    work(&s.app, cx);
    s.app.wired.shell.read_with(cx, |sh, _| assert_eq!(sh.library.scope().tag_id, Some(tag_id), "{link} filters by the tag"));

    // An unannounced switch to a catalog whose tag has the same id.
    let (b, _) = colliding_catalog(&s.dir, "b", 1);
    let b_tag = b.create_tag("Other/Places/Here").unwrap();
    assert_eq!(b_tag, tag_id);
    b.set_setting(&ob_setting("vault"), "B").unwrap();
    let opened = cx.opened_url();
    core_switch(&s.app, b);
    s.click("obsidian-tag-create", cx);
    assert!(status(&s.app, cx).contains(CATALOG_CHANGED), "{}", status(&s.app, cx));
    assert_eq!(cx.opened_url(), opened, "a refused Create opens nothing");
    assert_eq!(s.obsidian_settings(), vec![("obsidian.vault".to_string(), "B".to_string())], "B got no record");
    assert_eq!(reopen_a(&s.dir).get_setting(&ob_setting(&format!("tagnote.{}", tag.uuid))).unwrap().as_deref(), Some(""), "A unchanged");
}
