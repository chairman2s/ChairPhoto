//! [`ObsidianState`]: the Obsidian module's live state, shared by its three views — the
//! inspector's "Note" panel (the active photo's note), the tag editor's "Obsidian note"
//! section (the edited tag's note) and the settings panel (vault and notes folder).
//!
//! **ChairPhoto never writes into the vault.** "Create note" hands Obsidian an
//! `obsidian://new` URI with the note's path and initial text, and Obsidian creates it; "Open
//! note" hands it `obsidian://open`. Both go to the system opener (`App::open_url`: the
//! desktop's URI handler, e.g. `xdg-open` with the URI as one argument — no shell), as React's
//! `openExternal` did. What the module keeps is a record per subject in its own settings
//! (`obsidian.note.<photo uuid>`, `obsidian.tagnote.<tag uuid>`); "Forget" blanks that record
//! and leaves the note alone.
//!
//! **Off the UI thread.** Every catalog read and write runs on the storage [`Runner`]; a
//! result carries the generation it started under, and a catalog switch (or unloading the
//! module) bumps it, so a late answer from the old catalog is dropped.
//!
//! **Catalog identity** (map #92). The photo is read bound to the catalog the Library rows
//! came from ([`ShellState::rows_from`]), the tag to the one the tag editor opened over
//! ([`ShellState::editing_tag_from`]); Create and Forget run under that same identity, so
//! after a switch — even one whose `catalog:switched` has not arrived — they fail closed
//! instead of writing a record keyed by another catalog's photo or tag. Create reads the
//! settings, builds the note and stores its record under one catalog lock, and only then
//! opens Obsidian: a refused write opens nothing.
//!
//! [`ShellState::rows_from`]: crate::shell::ShellState::rows_from
//! [`ShellState::editing_tag_from`]: crate::shell::ShellState::editing_tag_from

use crate::model::{AppModel, AppModelEvent};
use crate::modules::ModuleSettings;
use crate::shell::ShellState;
use crate::storage::Runner;
use chairphoto_core::app::{with_catalog_as, with_catalog_identified, AppState, CatalogIdentity, CoreEvent};
use chairphoto_core::catalog::Catalog;
use chairphoto_model::obsidian::{self as ob, NoteRecord};
use gpui_kit::{Context, Entity, SharedString, Subscription};

/// What a note belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The active photo (inspector "Note").
    Photo,
    /// The tag being edited (tag editor "Obsidian note").
    Tag,
}

impl Kind {
    /// The record's settings key (without the module prefix) for a subject's uuid.
    fn record_key(self, uuid: &str) -> String {
        match self {
            Kind::Photo => ob::note_key(uuid),
            Kind::Tag => ob::tag_note_key(uuid),
        }
    }

    /// The subject's uuid, read from `c`.
    fn uuid(self, c: &Catalog, id: i64) -> chairphoto_core::catalog::Result<String> {
        Ok(match self {
            Kind::Photo => c.get_photo(id)?.uuid,
            Kind::Tag => c.get_tag(id)?.uuid,
        })
    }

    /// The note's vault-relative file and initial text, from the catalog.
    fn note(self, c: &Catalog, id: i64, folder: &str) -> chairphoto_core::catalog::Result<(String, String, String)> {
        Ok(match self {
            Kind::Photo => {
                let photo = c.get_photo(id)?;
                let tags = c.get_photo_tags(id)?;
                (photo.uuid.clone(), ob::note_file(folder, &photo), ob::note_content(&photo, &tags))
            }
            Kind::Tag => {
                let tag = c.get_tag(id)?;
                let aliases = ob::aliases(&c.list_terms(id)?);
                (tag.uuid.clone(), ob::tag_note_file(folder, &tag), ob::tag_note_content(&tag, &aliases))
            }
        })
    }
}

/// A subject with its note record, read bound to `from`.
#[derive(Debug, Clone, PartialEq)]
pub struct Linked {
    pub id: i64,
    pub from: CatalogIdentity,
    pub uuid: String,
    /// The note, when the module has one on record.
    pub record: Option<NoteRecord>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum NoteView {
    /// No subject (no active photo, no tag editor open).
    #[default]
    None,
    Loading(i64),
    Ready(Linked),
    Failed(i64, String),
}

/// One kind's subject and the work on it.
#[derive(Default)]
pub struct Slot {
    key: Option<(i64, CatalogIdentity)>,
    seq: u64,
    pub view: NoteView,
    /// "Create note" is running.
    pub creating: bool,
}

impl Slot {
    pub fn linked(&self) -> Option<&Linked> {
        match &self.view {
            NoteView::Ready(l) => Some(l),
            _ => None,
        }
    }
}

/// What Create found under the catalog lock.
enum Prepared {
    NoVault,
    /// The record is stored; open `uri`.
    Stored { uuid: String, record: NoteRecord, uri: String },
}

pub struct ObsidianState {
    app: AppState,
    settings: ModuleSettings,
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    /// `obsidian.vault` / `obsidian.folder` as stored; `None` until read.
    pub vault: Option<String>,
    pub folder: Option<String>,
    settings_from: Option<CatalogIdentity>,
    /// Bumped by every successful save (the settings view's "Saved").
    pub saves: u64,
    pub photo: Slot,
    pub tag: Slot,
    generation: u64,
    live: bool,
    _subscriptions: Vec<Subscription>,
}

impl ObsidianState {
    pub fn new(
        app: AppState,
        settings: ModuleSettings,
        model: Entity<AppModel>,
        shell: Entity<ShellState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe(&model, |this, _, event: &AppModelEvent, cx| match event {
                AppModelEvent::CatalogRead => this.catalog_read(cx),
                AppModelEvent::Core(CoreEvent::CatalogSwitched(_)) => this.catalog_switched(cx),
                _ => {}
            }),
            cx.observe(&shell, |this, _, cx| this.follow(false, cx)),
        ];
        let mut this = ObsidianState {
            app,
            settings,
            model: model.clone(),
            shell,
            vault: None,
            folder: None,
            settings_from: None,
            saves: 0,
            photo: Slot::default(),
            tag: Slot::default(),
            generation: 0,
            live: true,
            _subscriptions: subscriptions,
        };
        if model.read(cx).catalog.is_some() {
            this.catalog_read(cx);
        }
        this
    }

    pub fn shell(&self) -> &Entity<ShellState> {
        &self.shell
    }

    pub fn slot(&self, kind: Kind) -> &Slot {
        match kind {
            Kind::Photo => &self.photo,
            Kind::Tag => &self.tag,
        }
    }

    fn slot_mut(&mut self, kind: Kind) -> &mut Slot {
        match kind {
            Kind::Photo => &mut self.photo,
            Kind::Tag => &mut self.tag,
        }
    }

    /// Whether the settings have been read (Save needs the catalog they came from).
    pub fn settings_ready(&self) -> bool {
        self.settings_from.is_some()
    }

    /// Run `work` on the [`Runner`]; `land` with its result unless the catalog was switched or
    /// the module unloaded meanwhile.
    fn run<R: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce(&AppState) -> R + Send + 'static,
        land: impl FnOnce(&mut Self, R, &mut Context<Self>) + 'static,
    ) {
        let generation = self.generation;
        let app = self.app.clone();
        let rx = Runner::get(cx).run(move || work(&app));
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update(cx, |s, cx| {
                if s.generation != generation || !s.live {
                    return;
                }
                land(s, result, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn status(&self, line: impl Into<SharedString>, cx: &mut Context<Self>) {
        let line = line.into();
        self.model.update(cx, |m, cx| m.set_status(line, cx));
    }

    // --- catalog lifecycle ----------------------------------------------------------------

    fn catalog_read(&mut self, cx: &mut Context<Self>) {
        self.reload_settings(cx);
        self.follow(true, cx);
    }

    fn catalog_switched(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.vault = None;
        self.folder = None;
        self.settings_from = None;
        self.photo = Slot::default();
        self.tag = Slot::default();
        cx.notify();
    }

    pub fn unload(&mut self, cx: &mut Context<Self>) {
        self.live = false;
        self.generation += 1;
        cx.notify();
    }

    // --- settings -------------------------------------------------------------------------

    pub fn reload_settings(&mut self, cx: &mut Context<Self>) {
        let (vault_key, folder_key) = (self.settings.key(ob::VAULT_KEY), self.settings.key(ob::FOLDER_KEY));
        self.run(
            cx,
            move |app| {
                with_catalog_identified(app, |c| {
                    Ok((c.get_setting(&vault_key)?.unwrap_or_default(), c.get_setting(&folder_key)?.unwrap_or_default()))
                })
            },
            |s, result, _| match result {
                Ok((from, (vault, folder))) => {
                    s.settings_from = Some(from);
                    s.vault = Some(vault);
                    s.folder = Some(folder);
                }
                Err(e) => eprintln!("obsidian: settings unavailable: {e}"),
            },
        );
    }

    /// "Save": the vault name and notes folder, validated ([`ob::vault_name`],
    /// [`ob::notes_folder`]), into the catalog they were read from. `Err` = why nothing was
    /// saved (the settings view shows it).
    pub fn save_settings(&mut self, vault: &str, folder: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let vault = ob::vault_name(vault)?;
        let folder = ob::notes_folder(folder)?;
        let Some(from) = self.settings_from else {
            return Err("The Obsidian settings are still loading; try again.".into());
        };
        let (vault_key, folder_key) = (self.settings.key(ob::VAULT_KEY), self.settings.key(ob::FOLDER_KEY));
        let stored = (vault.clone(), folder.clone());
        self.run(
            cx,
            move |app| {
                with_catalog_as(app, from, |c| {
                    c.set_setting(&vault_key, &vault)?;
                    c.set_setting(&folder_key, &folder)
                })
            },
            move |s, result, cx| match result {
                Ok(()) => {
                    (s.vault, s.folder) = (Some(stored.0), Some(stored.1));
                    s.saves += 1;
                    s.status("Obsidian settings saved", cx);
                }
                Err(e) => s.status(format!("Obsidian: could not save the settings: {e}"), cx),
            },
        );
        Ok(())
    }

    // --- the subjects ---------------------------------------------------------------------

    /// Follow the active photo and the edited tag; `force` re-reads both.
    pub fn follow(&mut self, force: bool, cx: &mut Context<Self>) {
        if !self.live {
            return;
        }
        let (photo, tag) = {
            let shell = self.shell.read(cx);
            (
                shell.library.selection().active_id.zip(shell.rows_from()),
                shell.editing_tag.zip(shell.editing_tag_from),
            )
        };
        self.follow_kind(Kind::Photo, photo, force, cx);
        self.follow_kind(Kind::Tag, tag, force, cx);
    }

    fn follow_kind(&mut self, kind: Kind, key: Option<(i64, CatalogIdentity)>, force: bool, cx: &mut Context<Self>) {
        let slot = self.slot_mut(kind);
        if key == slot.key && !force {
            return;
        }
        if slot.key != key {
            slot.creating = false;
        }
        let Some((id, from)) = key else {
            *slot = Slot { seq: slot.seq + 1, ..Slot::default() };
            cx.notify();
            return;
        };
        if slot.key != key {
            slot.view = NoteView::Loading(id);
        }
        slot.key = key;
        slot.seq += 1;
        let seq = slot.seq;
        let prefix_key = self.settings.key("");
        self.run(
            cx,
            move |app| {
                with_catalog_as(app, from, |c| {
                    let uuid = kind.uuid(c, id)?;
                    let raw = c.get_setting(&format!("{prefix_key}{}", kind.record_key(&uuid)))?;
                    Ok((uuid, raw))
                })
            },
            move |s, result, _| {
                let slot = s.slot_mut(kind);
                if slot.seq != seq {
                    return;
                }
                slot.view = match result {
                    Ok((uuid, raw)) => {
                        NoteView::Ready(Linked { id, from, uuid, record: raw.as_deref().and_then(NoteRecord::parse) })
                    }
                    Err(e) => NoteView::Failed(id, e),
                };
            },
        );
    }

    /// Set the shown subject's record, if `(id, from)` is still the one shown — a Create or
    /// Forget that has committed. It also bumps the slot's sequence: a re-read still in
    /// flight (every `CatalogRead` forces one) may have read the record before that write
    /// committed, and on a pool of workers it can land after it; its answer is stale. A
    /// re-read started after this one reads the committed record.
    fn set_record(&mut self, kind: Kind, id: i64, from: CatalogIdentity, record: Option<NoteRecord>) {
        let slot = self.slot_mut(kind);
        if let NoteView::Ready(l) = &mut slot.view {
            if l.id == id && l.from == from {
                l.record = record;
                slot.seq += 1;
            }
        }
    }

    /// "Create note in Obsidian": under one lock on the subject's catalog, read the settings
    /// (no vault → the reminder, nothing else), build the note from the catalog and store its
    /// record; then hand Obsidian the `obsidian://new` URI. A refused read or write (another
    /// catalog is open) opens nothing.
    pub fn create(&mut self, kind: Kind, cx: &mut Context<Self>) {
        let slot = self.slot(kind);
        let Some((id, from)) = slot.linked().map(|l| (l.id, l.from)) else { return };
        if slot.creating {
            return;
        }
        self.slot_mut(kind).creating = true;
        cx.notify();
        let prefix = self.settings.key("");
        let generation = self.generation;
        let app = self.app.clone();
        let rx = Runner::get(cx).run(move || {
            with_catalog_as(&app, from, |c| {
                let setting = |k: &str| c.get_setting(&format!("{prefix}{k}"));
                let vault = match ob::vault_name(&setting(ob::VAULT_KEY)?.unwrap_or_default()) {
                    Ok(v) if v.is_empty() => return Ok(Ok(Prepared::NoVault)),
                    Ok(v) => v,
                    Err(e) => return Ok(Err(e)),
                };
                let folder = match ob::folder_or_default(setting(ob::FOLDER_KEY)?.as_deref()) {
                    Ok(f) => f,
                    Err(e) => return Ok(Err(e)),
                };
                let (uuid, file, content) = kind.note(c, id, &folder)?;
                let uri = ob::new_uri(&vault, &file, &content);
                let record = NoteRecord { vault, file, created_at: now_ms() };
                c.set_setting(&format!("{prefix}{}", kind.record_key(&uuid)), &record.to_json())?;
                Ok(Ok(Prepared::Stored { uuid, record, uri }))
            })
            .and_then(|r| r)
        });
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the worker stopped".into()));
            this.update(cx, |s, cx| {
                // The record is stored in its catalog whatever happened since: open the note
                // the user asked for even if the panel has moved on; show it only if not.
                let current = s.generation == generation && s.live;
                if current {
                    s.slot_mut(kind).creating = false;
                }
                match result {
                    Ok(Prepared::NoVault) => s.status(ob::NO_VAULT, cx),
                    Ok(Prepared::Stored { uuid, record, uri }) => {
                        cx.open_url(&uri);
                        if current {
                            let shown = s.slot(kind).linked().is_some_and(|l| l.uuid == uuid);
                            if shown {
                                s.set_record(kind, id, from, Some(record));
                            }
                        }
                    }
                    Err(e) => s.status(format!("Couldn't create note: {e}"), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// "Open note": `obsidian://open` for the shown subject's note. No catalog work.
    pub fn open(&mut self, kind: Kind, cx: &mut Context<Self>) {
        if let Some(uri) = self.slot(kind).linked().and_then(|l| l.record.as_ref()).map(NoteRecord::open_uri) {
            cx.open_url(&uri);
        }
    }

    /// "Forget": blank the shown subject's record in its catalog (there is no delete, as in
    /// React). The note stays in the vault.
    pub fn forget(&mut self, kind: Kind, cx: &mut Context<Self>) {
        let Some(l) = self.slot(kind).linked().filter(|l| l.record.is_some()).cloned() else { return };
        let key = self.settings.key(&kind.record_key(&l.uuid));
        self.run(cx, move |app| with_catalog_as(app, l.from, |c| c.set_setting(&key, "")), move |s, result, cx| match result {
            Ok(()) => s.set_record(kind, l.id, l.from, None),
            Err(e) => s.status(format!("Couldn't forget the note: {e}"), cx),
        });
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}
