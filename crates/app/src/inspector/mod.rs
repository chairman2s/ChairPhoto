//! The Photo inspector (#108): the inspector column's tab bodies — `PhotoInspector.tsx` and
//! the panels it hosts (`SignalsPanel`, `IptcPanel`, `MetadataPanel`, `VersionsPanel`,
//! `PublishedPanel`). `docs/plans/gpui/parity.md` § "Inspector and tags" is the acceptance
//! list. The column's chrome (header, tab row) is `shell::inspector`.
//!
//! | Tab | Body |
//! |---|---|
//! | details | EXIF line; stars, Pick/Reject/None, colour label; culling signals; the collapsible Stack, Orientation, Edit in, Storage, IPTC and Metadata sections |
//! | tags | the Tag panel's tagging block ([`crate::tags::photo_tags::PhotoTags`], #107), then the enabled modules' inspector panels |
//! | versions | Original + named versions: choose the one the loupe shows, rename, duplicate, delete, add |
//! | publish | where the photo was published (and which version), "Mark as published", "Publish…" |
//!
//! **The photo shown** is the one the loupes follow (`ShellState::loupe_target`,
//! `shellTarget.ts`): Compare's focused pane while Compare is open, else the Library's active
//! photo. The Versions tab's choice belongs to the active photo
//! (`ShellState::set_active_version` refuses another photo's version), as React's
//! `activeVersion` did.
//!
//! **Data flow.** Everything the inspector shows about the photo beyond its row — signals,
//! the stack, IPTC, metadata, versions, publications — is read off the UI thread, lazily (only
//! what the open tab and the expanded sections show, as React mounted a section's body only
//! when open), and owned by a *generation*: a new active photo or a catalog switch bumps it,
//! and a read from an older generation is dropped on arrival. Each slot also numbers its own
//! reads, so of two reloads of one slot only the newer lands. Mutations run off the UI thread
//! too (catalog writes on GPUI's background executor, sidecar/disk/process work on the
//! storage [`Runner`]); a mutation that a catalog switch overtook is dropped, and one that
//! lands re-reads its slot and the catalog-derived state (`AppModel::refresh`, which the
//! shell follows) — api.ts's conservative rule: a mutation's effects count as "everything".
//!
//! **Culling marks** go through the Library's single write path
//! (`ShellState::apply_mark_to`), on the photo shown — as `PhotoInspector.tsx` wrote
//! `photo.id`, not the selection.
//!
//! **Catalog identity.** The photo shown comes from the Library's rows, so the inspector is
//! bound to the catalog they were read from (`ShellState::rows_from`, held as `from`). Every
//! read of the photo's data and every write keyed by its id or by ids read with it (rotate,
//! unstack, back up / queue, offload, restore, IPTC, versions, publications, marks) goes
//! through `with_catalog_as` or a core `_as` function, so after a switch it fails closed
//! (`CATALOG_CHANGED`) — also in the window before `catalog:switched` arrives — instead of
//! landing on the new catalog's photo with the same id. The storage actions run on a
//! connection of their own to that catalog (`storage::backup_photo_as` and its siblings).
//!
//! **IPTC saves** are serialized per photo: Save is disabled while the photo's fields are
//! still loading, and a Save while that photo's previous save is in flight is queued with
//! its photo, catalog and the form's values at that press ([`IptcSave`]; a newer press
//! replaces it), and runs after the running one — also once the selection has moved on (so a
//! catalog row and its sidecar never interleave, and no save is silently dropped). A save
//! whose photo is no longer shown reports its outcome on the status line.
//!
//! **External editors** (darktable / RawTherapee / ART) and **RapidRAW** run on the storage
//! [`Runner`] (the core runtime's blocking pool), bound to the photo's catalog like every
//! write (`external_edit::develop_in_editor_as` / `import_developed_as`,
//! `rapidraw::edit_in_rapidraw_as`): a run whose worker starts after a switch fails closed
//! under the catalog lock, before anything is launched or imported. A sidecar-editor run is
//! owned by a sequence number per photo; its result lands only if no newer run for that photo started and the
//! catalog did not switch. A RapidRAW round-trip is queued under a core job id
//! (`rapidraw::queue_job`) before its worker starts — cancellable from then on, so a Cancel
//! before the worker runs means RapidRAW is never launched: its `rapidraw:progress` events (routed here
//! through `AppModel`) and its result update the photo's entry only while that job still owns
//! it, and Cancel cancels exactly that job (`cancel_rapidraw_job`). A catalog switch drops
//! every entry; round-trips already running keep running and import into the catalog they
//! started on (the core captured its file under the identity check), but this inspector no
//! longer follows them.
//!
//! **Section state is per machine**, as React kept it in localStorage: each section's open
//! state is read from [`crate::machine_prefs::MachinePrefs`] at `inspector.section.<id>`
//! (`"1"` open, anything else collapsed; collapsed when unset) and written back on a toggle.

pub mod render;
pub mod signals;
#[cfg(test)]
mod tests;

use crate::image_store::ImageStore;
use crate::model::{AppModel, AppModelEvent, EditorsChanged};
use crate::shell::state::{InspectorTab, Mark, ShellState};
use crate::storage::Runner;
use chairphoto_core::app::{with_catalog_as, AppState, CatalogIdentity, CoreEvent};
use chairphoto_core::catalog::{IptcFields, MetadataEntry, Photo, PhotoVersion, PickState, Publication};
use chairphoto_core::external_edit::AvailableEditor;
use chairphoto_core::photo_signals::PhotoSignals;
use chairphoto_core::rapidraw::RapidRawProgress;
use gpui_kit::component::input::{InputEvent, InputState, TextareaState};
use gpui_kit::{AppContext as _, Context, Entity, Subscription, Window};
use std::collections::{HashMap, HashSet};

/// One per-photo read: nothing asked yet, in flight, or its answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Load<T> {
    Idle,
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Load::Ready(v) => Some(v),
            _ => None,
        }
    }
}

/// A [`Load`] plus the number of its newest read: a reload keeps the old answer on screen
/// until the new one lands, and an older read that lands late is dropped.
#[derive(Debug, Clone)]
pub struct Slot<T> {
    pub load: Load<T>,
    seq: u64,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot { load: Load::Idle, seq: 0 }
    }
}

/// The photo's stack (`StackSection`): the master ("Original") and what is stacked under it.
#[derive(Debug, Clone)]
pub struct StackGroup {
    pub master: Photo,
    pub children: Vec<Photo>,
}

/// What the inspector has read about the photo it shows.
#[derive(Debug, Default)]
pub struct PhotoData {
    pub signals: Slot<PhotoSignals>,
    /// `None` inside: the photo is in no stack.
    pub stack: Slot<Option<StackGroup>>,
    pub iptc: Slot<IptcFields>,
    pub metadata: Slot<Vec<MetadataEntry>>,
    pub versions: Slot<Vec<PhotoVersion>>,
    pub publications: Slot<Vec<Publication>>,
}

/// The details tab's collapsible sections, collapsed by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Stack,
    Orientation,
    Develop,
    Storage,
    Iptc,
    Metadata,
}

impl Section {
    pub fn id(self) -> &'static str {
        match self {
            Section::Stack => "stack",
            Section::Orientation => "orientation",
            Section::Develop => "develop",
            Section::Storage => "storage",
            Section::Iptc => "iptc",
            Section::Metadata => "metadata",
        }
    }

    pub const ALL: [Section; 6] =
        [Section::Stack, Section::Orientation, Section::Develop, Section::Storage, Section::Iptc, Section::Metadata];

    /// Its per-machine key, `inspector.section.<id>` (PhotoInspector.tsx's `Section`).
    pub fn pref_key(self) -> String {
        format!("inspector.section.{}", self.id())
    }
}

/// The metadata groups shown expanded until toggled (`MetadataPanel`'s `DEFAULT_OPEN`).
pub const META_DEFAULT_OPEN: [&str; 4] = ["EXIF", "Composite", "IPTC", "XMP"];

/// The IPTC fields as the form edits them, in `IptcPanel`'s order after the caption.
pub const IPTC_FIELDS: [(&str, &str); 10] = [
    ("headline", "Headline"),
    ("title", "Title"),
    ("creator", "Creator"),
    ("copyright", "Copyright"),
    ("credit", "Credit"),
    ("source", "Source"),
    ("city", "City"),
    ("state", "State/Province"),
    ("country", "Country"),
    ("countryCode", "Country code"),
];

fn iptc_get(f: &IptcFields, key: &str) -> String {
    match key {
        "headline" => f.headline.clone(),
        "title" => f.title.clone(),
        "creator" => f.creator.clone(),
        "copyright" => f.copyright.clone(),
        "credit" => f.credit.clone(),
        "source" => f.source.clone(),
        "city" => f.city.clone(),
        "state" => f.state.clone(),
        "country" => f.country.clone(),
        "countryCode" => f.country_code.clone(),
        _ => String::new(),
    }
}

fn iptc_set(f: &mut IptcFields, key: &str, value: String) {
    match key {
        "headline" => f.headline = value,
        "title" => f.title = value,
        "creator" => f.creator = value,
        "copyright" => f.copyright = value,
        "credit" => f.credit = value,
        "source" => f.source = value,
        "city" => f.city = value,
        "state" => f.state = value,
        "country" => f.country = value,
        "countryCode" => f.country_code = value,
        _ => {}
    }
}

/// The IPTC form (`IptcPanel`): one input per field, the values last loaded or saved, and
/// the status line. Loaded values are put into the inputs at the next render (setting an
/// input's text needs the window).
pub struct IptcForm {
    pub description: Entity<TextareaState>,
    pub fields: Vec<Entity<InputState>>,
    /// What the catalog holds, as last read or saved: "Save IPTC" is enabled only when the
    /// form differs from it.
    pub saved: IptcFields,
    pub status: String,
    fill: Option<IptcFields>,
}

impl IptcForm {
    /// The form's current values.
    pub fn values(&self, cx: &gpui_kit::App) -> IptcFields {
        let mut f = IptcFields { description: self.description.read(cx).value().to_string(), ..Default::default() };
        for ((key, _), input) in IPTC_FIELDS.iter().zip(&self.fields) {
            iptc_set(&mut f, key, input.read(cx).value().to_string());
        }
        f
    }

    pub fn dirty(&self, cx: &gpui_kit::App) -> bool {
        self.values(cx) != self.saved
    }

    /// Put loaded values into the inputs (render time, where the window is at hand).
    pub(crate) fn apply_fill(&mut self, window: &mut Window, cx: &mut gpui_kit::App) {
        let Some(f) = self.fill.take() else { return };
        let description = f.description.clone();
        self.description.update(cx, |i, cx| i.set_value(description, window, cx));
        for ((key, _), input) in IPTC_FIELDS.iter().zip(&self.fields) {
            let v = iptc_get(&f, key);
            input.update(cx, |i, cx| i.set_value(v, window, cx));
        }
    }
}

/// An IPTC save as Save captured it: the photo's catalog, the form's values then, and the
/// photo's file name for the status line. A save queued behind a running one carries all of
/// it, so it runs — and reports — after the inspector has moved to another photo.
/// `generation` is the inspector's generation when Save was clicked: if it no longer matches
/// when the save lands, the photo was navigated away from and back (or otherwise reset) in
/// between, so the live form was already reset and re-read for the view as it is now — this
/// save's captured values must not be adopted as its baseline (#201).
#[derive(Debug, Clone)]
pub(crate) struct IptcSave {
    from: CatalogIdentity,
    generation: u64,
    fields: IptcFields,
    name: String,
}

/// The last component of a catalog-relative path, for messages.
fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Which editors this machine has (`availableEditors` filtered to those with a GUI, and
/// `rapidrawAvailable`), read once per catalog.
#[derive(Debug, Clone, Default)]
pub struct Editors {
    pub editors: Vec<AvailableEditor>,
    pub rapidraw: bool,
}

/// A sidecar-editor run (darktable / RawTherapee / ART) for one photo: which editor, and
/// the sequence number that owns the photo's entry.
#[derive(Debug, Clone, PartialEq)]
pub struct SidecarRun {
    pub editor: String,
    seq: u64,
}

/// A RapidRAW round-trip's live phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RapidPhase {
    Editing,
    Waiting,
    Importing,
}

impl RapidPhase {
    /// React's per-phase note.
    pub fn note(self) -> &'static str {
        match self {
            RapidPhase::Editing => {
                "Editing in RapidRAW… click Done there to import the result. If it opened in an existing RapidRAW \
                 window, edit there and click Done — or Cancel to stop waiting."
            }
            RapidPhase::Waiting => {
                "Waiting for RapidRAW's exported result… click Done in RapidRAW, or Cancel to stop waiting."
            }
            RapidPhase::Importing => "Importing the result…",
        }
    }
}

/// The RapidRAW round-trip a photo has in flight: its core job id and phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RapidRun {
    pub job: u64,
    pub phase: RapidPhase,
}

/// The inspector's state. See the module docs.
pub struct PhotoInspector {
    app: AppState,
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    pub(crate) images: Entity<ImageStore>,
    /// The photo shown, and its stack shape (master id, child count) — a change of the
    /// latter re-reads the Stack section.
    pub photo_id: Option<i64>,
    /// The catalog the photo shown (and everything read about it) came from: every read and
    /// write is bound to it.
    pub from: Option<CatalogIdentity>,
    stack_key: Option<(Option<i64>, i64)>,
    /// Bumped by every change of the photo shown and every catalog switch.
    generation: u64,
    /// Bumped by every catalog switch.
    epoch: u64,
    tab: Option<InspectorTab>,
    /// The shell's active version as last seen: "Mark as published" follows it.
    seen_version: Option<i64>,
    sections: HashSet<Section>,
    pub data: PhotoData,
    pub iptc: IptcForm,
    /// Metadata groups currently expanded.
    pub meta_open: HashSet<String>,
    /// "New version" name.
    pub version_name: Entity<InputState>,
    /// The version being renamed inline, its input, and the input's Enter/blur subscription.
    pub renaming: Option<(i64, Entity<InputState>, Subscription)>,
    /// "Mark as published": the platform, and the version (`None` = Original).
    pub platform: Entity<InputState>,
    pub publish_version: Option<i64>,
    /// The Storage section's last action message for the photo shown.
    pub storage_msg: Option<String>,
    pub editors: Option<Editors>,
    editors_reading: bool,
    /// Bumped by every editors read started; only the newest one's result lands.
    editors_seq: u64,
    /// Per photo: the sidecar-editor run in flight, the RapidRAW round-trip in flight, and
    /// the last terminal note.
    pub sidecar: HashMap<i64, SidecarRun>,
    pub rapid: HashMap<i64, RapidRun>,
    pub notes: HashMap<i64, String>,
    seq: u64,
    /// Photos with an IPTC save in flight, and the save queued behind each.
    pub iptc_saving: HashSet<i64>,
    iptc_queued: HashMap<i64, IptcSave>,
    _subscriptions: Vec<Subscription>,
}

impl PhotoInspector {
    pub fn new(
        model: Entity<AppModel>,
        shell: Entity<ShellState>,
        images: Entity<ImageStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let app = model.read(cx).state().clone();
        let description = cx.new(|cx| TextareaState::new(window, cx).rows(2));
        let fields = IPTC_FIELDS.iter().map(|_| cx.new(|cx| InputState::new(window, cx))).collect();
        let version_name = cx.new(|cx| InputState::new(window, cx).placeholder("New version (e.g. Instagram square)"));
        let platform = cx.new(|cx| {
            let mut i = InputState::new(window, cx).placeholder("Platform (e.g. flickr)");
            i.set_value(COMMON_PLATFORMS[0], window, cx);
            i
        });
        let subscriptions = vec![
            cx.observe(&shell, |this, _, cx| this.sync(cx)),
            cx.subscribe(&model, |this, _, event: &AppModelEvent, cx| match event {
                AppModelEvent::Core(event) => this.on_core_event(event, cx),
                AppModelEvent::CatalogRead => this.read_editors(cx),
                AppModelEvent::DeepLink(_) => {}
            }),
            // Preferences → Editors saved a path: re-check, without waiting for a switch.
            cx.subscribe(&model, |this, _, _: &EditorsChanged, cx| this.reread_editors(cx)),
            cx.subscribe_in(&version_name, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.add_version(window, cx);
                }
            }),
            cx.subscribe_in(&platform, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.mark_published(cx);
                }
            }),
        ];
        let mut this = PhotoInspector {
            app,
            model,
            shell,
            images,
            photo_id: None,
            from: None,
            stack_key: None,
            generation: 0,
            epoch: 0,
            tab: None,
            seen_version: None,
            // The sections this machine left open (React: `v === null ? false : v === "1"`).
            sections: Section::ALL
                .into_iter()
                .filter(|s| crate::machine_prefs::MachinePrefs::read(cx, &s.pref_key()).as_deref() == Some("1"))
                .collect(),
            data: PhotoData::default(),
            iptc: IptcForm { description, fields, saved: IptcFields::default(), status: String::new(), fill: None },
            meta_open: META_DEFAULT_OPEN.iter().map(|s| s.to_string()).collect(),
            version_name,
            renaming: None,
            platform,
            publish_version: None,
            storage_msg: None,
            editors: None,
            editors_reading: false,
            editors_seq: 0,
            sidecar: HashMap::new(),
            rapid: HashMap::new(),
            notes: HashMap::new(),
            seq: 0,
            iptc_saving: HashSet::new(),
            iptc_queued: HashMap::new(),
            _subscriptions: subscriptions,
        };
        this.sync(cx);
        this
    }

    /// The photo the inspector shows ([`ShellState::loupe_target`]: Compare's focused pane,
    /// else the Library's active photo), as the rows hold it.
    pub fn photo(&self, cx: &gpui_kit::App) -> Option<Photo> {
        self.shell.read(cx).loupe_target().cloned()
    }

    pub fn section_open(&self, section: Section) -> bool {
        self.sections.contains(&section)
    }

    pub fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        let open = !self.sections.remove(&section);
        if open {
            self.sections.insert(section);
        }
        crate::machine_prefs::MachinePrefs::set(cx, &section.pref_key(), if open { "1" } else { "0" });
        self.ensure_loaded(cx);
        cx.notify();
    }

    pub fn toggle_meta_group(&mut self, group: &str, cx: &mut Context<Self>) {
        if !self.meta_open.remove(group) {
            self.meta_open.insert(group.to_string());
        }
        cx.notify();
    }

    // --- following the shell ---------------------------------------------------------

    /// The shell changed: a new active photo resets everything per-photo; a new tab, or a
    /// new stack shape, reads what it shows.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let (photo, tab, active_version, from) = {
            let shell = self.shell.read(cx);
            let photo = shell.loupe_target().map(|p| (p.id, p.stack_parent_id, p.stack_count));
            (photo, shell.inspector_tab, shell.active_version().map(|v| v.id), shell.rows_from())
        };
        let id = photo.map(|p| p.0);
        if id != self.photo_id || from != self.from {
            self.photo_id = id;
            self.from = from;
            self.generation += 1;
            self.data = PhotoData::default();
            self.stack_key = photo.map(|p| (p.1, p.2));
            self.storage_msg = None;
            self.renaming = None;
            self.publish_version = active_version;
            self.iptc.saved = IptcFields::default();
            self.iptc.status.clear();
            self.iptc.fill = Some(IptcFields::default());
            cx.notify();
        } else if let Some(p) = photo {
            if self.stack_key != Some((p.1, p.2)) {
                self.stack_key = Some((p.1, p.2));
                self.data.stack.load = Load::Idle;
            }
        }
        // PublishedPanel defaulted "Mark as published" to the active version, and followed it.
        if self.seen_version != active_version {
            self.seen_version = active_version;
            self.publish_version = active_version;
            cx.notify();
        }
        self.tab = Some(tab);
        self.ensure_loaded(cx);
    }

    /// Ask for whatever the open tab and sections show and nobody has asked for yet.
    fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.photo_id else { return };
        match self.tab {
            Some(InspectorTab::Details) => {
                if matches!(self.data.signals.load, Load::Idle) {
                    self.read(|t| &mut t.data.signals, move |c| chairphoto_core::photo_signals::explain_photo_signals(c, id), cx);
                }
                if matches!(self.data.stack.load, Load::Idle) {
                    let (parent, count) = self.stack_key.unwrap_or((None, 0));
                    self.read(|t| &mut t.data.stack, move |c| read_stack(c, id, parent, count), cx);
                }
                if self.section_open(Section::Iptc) && matches!(self.data.iptc.load, Load::Idle) {
                    self.read_iptc(id, cx);
                }
                if self.section_open(Section::Metadata) && matches!(self.data.metadata.load, Load::Idle) {
                    self.read(|t| &mut t.data.metadata, move |c| c.get_photo_metadata(id), cx);
                }
            }
            Some(InspectorTab::Versions) => {
                if matches!(self.data.versions.load, Load::Idle) {
                    self.reload_versions(cx);
                }
            }
            Some(InspectorTab::Publish) => {
                if matches!(self.data.publications.load, Load::Idle) {
                    self.reload_publications(cx);
                }
                if matches!(self.data.versions.load, Load::Idle) {
                    self.reload_versions(cx);
                }
            }
            Some(InspectorTab::Tags) | None => {}
        }
    }

    /// Read one slot off the UI thread (a short catalog read: GPUI's background executor, as
    /// the shell's reads). The answer lands only if the photo and the slot's newest read are
    /// still the ones it was asked for.
    fn read<T: Send + 'static>(
        &mut self,
        slot: fn(&mut Self) -> &mut Slot<T>,
        read: impl FnOnce(&chairphoto_core::catalog::Catalog) -> chairphoto_core::catalog::Result<T> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(from) = self.from else { return };
        let generation = self.generation;
        let s = slot(self);
        s.seq += 1;
        let seq = s.seq;
        if !matches!(s.load, Load::Ready(_)) {
            s.load = Load::Loading;
        }
        let state = self.app.clone();
        let task = cx.background_executor().spawn(async move { with_catalog_as(&state, from, read) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                let s = slot(this);
                if s.seq != seq {
                    return;
                }
                s.load = match result {
                    Ok(v) => Load::Ready(v),
                    Err(e) => Load::Failed(e),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn read_iptc(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(from) = self.from else { return };
        let generation = self.generation;
        self.data.iptc.seq += 1;
        let seq = self.data.iptc.seq;
        self.data.iptc.load = Load::Loading;
        let state = self.app.clone();
        let task = cx.background_executor().spawn(async move { with_catalog_as(&state, from, |c| c.get_iptc(id)) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.generation != generation || this.data.iptc.seq != seq {
                    return;
                }
                // IptcPanel: a failed read shows an empty form.
                let fields = result.clone().unwrap_or_default();
                this.iptc.saved = fields.clone();
                this.iptc.fill = Some(fields);
                this.iptc.status.clear();
                this.data.iptc.load = match result {
                    Ok(v) => Load::Ready(v),
                    Err(e) => Load::Failed(e),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn reload_versions(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.photo_id else { return };
        self.read(|t| &mut t.data.versions, move |c| c.list_versions(id), cx);
    }

    fn reload_publications(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.photo_id else { return };
        self.read(|t| &mut t.data.publications, move |c| c.list_publications(id), cx);
    }

    fn on_core_event(&mut self, event: &CoreEvent, cx: &mut Context<Self>) {
        match event {
            CoreEvent::CatalogSwitched(_) => {
                self.epoch += 1;
                self.generation += 1;
                self.photo_id = None;
                self.from = None;
                self.iptc_saving.clear();
                self.iptc_queued.clear();
                self.data = PhotoData::default();
                self.editors = None;
                self.editors_reading = false;
                self.sidecar.clear();
                self.rapid.clear();
                self.notes.clear();
                self.storage_msg = None;
                self.renaming = None;
                cx.notify();
            }
            CoreEvent::RapidRawProgress(p) => self.on_rapidraw_progress(p, cx),
            _ => {}
        }
    }

    // --- mutations ---------------------------------------------------------------------

    /// Run a short catalog write off the UI thread, bound to the catalog the photo came from
    /// (`with_catalog_as`: after a switch it fails closed); `done` runs only if no catalog
    /// switch overtook it.
    fn write<T: Send + 'static>(
        &mut self,
        work: impl FnOnce(&chairphoto_core::catalog::Catalog) -> chairphoto_core::catalog::Result<T> + Send + 'static,
        done: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(from) = self.from else { return };
        let epoch = self.epoch;
        let state = self.app.clone();
        let task = cx.background_executor().spawn(async move { with_catalog_as(&state, from, work) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.epoch == epoch {
                    done(this, result, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// [`run_blocking`](Self::run_blocking), but `done` runs whatever the epoch (it checks
    /// itself): the IPTC save's bookkeeping must end even after a switch.
    fn run_blocking_always<T: Send + 'static>(
        &mut self,
        work: impl FnOnce(&AppState) -> Result<T, String> + Send + 'static,
        done: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let state = self.app.clone();
        let rx = Runner::get(cx).run(move || work(&state));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the worker stopped".into()));
            this.update(cx, |this, cx| done(this, result, cx)).ok();
        })
        .detach();
    }

    /// Run blocking disk or process work on the storage [`Runner`]; `done` as [`write`](Self::write).
    fn run_blocking<T: Send + 'static>(
        &mut self,
        work: impl FnOnce(&AppState) -> Result<T, String> + Send + 'static,
        done: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let epoch = self.epoch;
        let state = self.app.clone();
        let rx = Runner::get(cx).run(move || work(&state));
        cx.spawn(async move |this, cx| {
            let result = rx.await.unwrap_or_else(|_| Err("the worker stopped".into()));
            this.update(cx, |this, cx| {
                if this.epoch == epoch {
                    done(this, result, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// A mutation landed: re-read the catalog-derived state everything else shows (the
    /// model's refresh, which the shell follows with its lists, counts and rows).
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.refresh(cx));
    }

    fn status(&mut self, line: String, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.set_status(line, cx));
    }

    /// The stars: a click on the current rating clears it.
    pub fn rate(&mut self, stars: i64, cx: &mut Context<Self>) {
        let Some(p) = self.photo(cx) else { return };
        let Some(from) = self.from else { return };
        let rating = next_rating(p.rating, stars);
        self.shell.update(cx, |s, cx| s.apply_mark_to(Mark::Rating(rating), vec![p.id], from, cx));
    }

    pub fn pick(&mut self, pick: PickState, cx: &mut Context<Self>) {
        let Some(p) = self.photo(cx) else { return };
        let Some(from) = self.from else { return };
        self.shell.update(cx, |s, cx| s.apply_mark_to(Mark::Pick(pick), vec![p.id], from, cx));
    }

    /// A swatch: a click on the current label clears it; `""` is the Clear swatch, which
    /// does nothing on an unlabelled photo.
    pub fn label(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(p) = self.photo(cx) else { return };
        let Some(label) = next_label(&p.label, name) else { return };
        let Some(from) = self.from else { return };
        self.shell.update(cx, |s, cx| s.apply_mark_to(Mark::Label(label), vec![p.id], from, cx));
    }

    /// Orientation: turn the displayed image (non-destructive), then drop its cached images
    /// so the grid and loupe re-render it (App.tsx's `rotateSelected` + `bustThumb`).
    pub fn rotate(&mut self, delta: i64, cx: &mut Context<Self>) {
        let Some(id) = self.photo_id else { return };
        self.write(
            move |c| c.rotate_photo(id, delta),
            move |this, result, cx| match result {
                Ok(_) => this.images.update(cx, |s, cx| s.invalidate(id, cx)),
                Err(e) => this.status(format!("Rotate failed: {e}"), cx),
            },
            cx,
        );
    }

    /// Stack: "View" — show a member of the stack. A stacked child is not in the grid's
    /// rows, so the Library views it off-grid (`LibrarySession::view_photo`) and opens the
    /// loupe on it (`ShellState::view_in_loupe`), as React did.
    pub fn view_stack_member(&mut self, photo: Photo, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| s.view_in_loupe(photo, cx));
    }

    /// Stack: "Unstack" — the child returns to the grid as its own photo.
    pub fn unstack(&mut self, child: i64, cx: &mut Context<Self>) {
        self.write(
            move |c| c.unstack(child),
            |this, result, cx| {
                if let Err(e) = result {
                    this.status(format!("Unstack failed: {e}"), cx);
                }
                this.data.stack.load = Load::Idle;
                this.ensure_loaded(cx);
                this.changed(cx);
            },
            cx,
        );
    }

    // --- storage -----------------------------------------------------------------------

    /// Back up now if the NAS is reachable, else queue a backup (React's `onBackup`).
    pub fn back_up(&mut self, cx: &mut Context<Self>) {
        let (Some(id), Some(from)) = (self.photo_id, self.from) else { return };
        self.storage_msg = Some("Backing up…".into());
        cx.notify();
        self.storage_action(
            id,
            move |state| match chairphoto_core::app::storage::backup_photo_as(state, from, id) {
                Ok(()) => Ok("Backed up".to_string()),
                Err(e) if e == chairphoto_core::app::CATALOG_CHANGED => Err(e),
                Err(_) => {
                    chairphoto_core::app::storage::enqueue_backup_as(state, from, id)?;
                    Ok("Queued (NAS offline)".to_string())
                }
            },
            cx,
        );
    }

    pub fn offload(&mut self, cx: &mut Context<Self>) {
        let (Some(id), Some(from)) = (self.photo_id, self.from) else { return };
        self.storage_msg = Some("Offloading…".into());
        cx.notify();
        self.storage_action(
            id,
            move |state| {
                chairphoto_core::app::storage::offload_photo_as(state, from, id).map(|()| "Local copy freed".to_string())
            },
            cx,
        );
    }

    pub fn restore(&mut self, cx: &mut Context<Self>) {
        let (Some(id), Some(from)) = (self.photo_id, self.from) else { return };
        self.storage_msg = Some("Restoring…".into());
        cx.notify();
        self.storage_action(
            id,
            move |state| {
                chairphoto_core::app::storage::restore_photo_as(state, from, id).map(|()| "Restored to local".to_string())
            },
            cx,
        );
    }

    fn storage_action(
        &mut self,
        id: i64,
        work: impl FnOnce(&AppState) -> Result<String, String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let generation = self.generation;
        self.run_blocking(
            work,
            move |this, result, cx| {
                if this.generation == generation && this.photo_id == Some(id) {
                    this.storage_msg = Some(result.unwrap_or_else(|e| e));
                    cx.notify();
                }
                this.changed(cx);
            },
            cx,
        );
    }

    // --- IPTC --------------------------------------------------------------------------

    /// Whether "Save IPTC" can run: the photo's fields have loaded (a save before that would
    /// store the empty form over them).
    pub fn iptc_loaded(&self) -> bool {
        matches!(self.data.iptc.load, Load::Ready(_) | Load::Failed(_))
    }

    /// "Save IPTC": the catalog, then the sidecar (`app::iptc::save_iptc_as`, bound to the
    /// photo's catalog), on a worker. One save per photo at a time: a Save while one is in
    /// flight is queued with the form's values now and runs when it ends.
    pub fn save_iptc(&mut self, cx: &mut Context<Self>) {
        let (Some(id), Some(from)) = (self.photo_id, self.from) else { return };
        if !self.iptc_loaded() {
            return;
        }
        let name = self.photo(cx).map(|p| file_name(&p.path)).unwrap_or_else(|| format!("photo {id}"));
        let save = IptcSave { from, generation: self.generation, fields: self.iptc.values(cx), name };
        self.iptc.status = "Saving…".into();
        cx.notify();
        if self.iptc_saving.contains(&id) {
            // A newer Save replaces a still-queued one: its values are the form's now.
            self.iptc_queued.insert(id, save);
            return;
        }
        self.start_iptc_save(id, save, cx);
    }

    /// Run one IPTC save for `id`, then the save queued behind it — whatever the inspector
    /// shows by then: the queued save carries its own photo, catalog and values.
    fn start_iptc_save(&mut self, id: i64, save: IptcSave, cx: &mut Context<Self>) {
        self.iptc_saving.insert(id);
        let epoch = self.epoch;
        let IptcSave { from, generation, fields, name } = save;
        let saved = fields.clone();
        self.run_blocking_always(
            move |state| chairphoto_core::app::iptc::save_iptc_as(state, from, id, &fields),
            move |this, result, cx| {
                if this.epoch != epoch {
                    return; // a switch cleared the bookkeeping; the save was bound to the old catalog
                }
                this.iptc_saving.remove(&id);
                let shown = this.photo_id == Some(id) && this.from == Some(from);
                if let Some(next) = this.iptc_queued.remove(&id) {
                    if let Err(e) = result {
                        this.status(format!("IPTC save for {name} failed: {e}"), cx);
                    }
                    this.start_iptc_save(id, next, cx);
                    return;
                }
                if shown && this.generation == generation {
                    match result {
                        // The catalog has the values whatever became of the sidecar, so they
                        // are the form's new baseline; the status says whether the sidecar
                        // has them or still owes them (#148).
                        Ok(outcome) => {
                            this.iptc.saved = saved;
                            this.iptc.status = outcome.status();
                        }
                        Err(e) => this.iptc.status = format!("Failed: {e}"),
                    }
                    cx.notify();
                } else {
                    // The inspector moved on, or this photo was navigated away from and back
                    // (the generation changed) since Save was clicked: the live form was
                    // already reset and may have re-read the catalog before this save landed,
                    // so its captured values must not be adopted as the new baseline (#201).
                    // The outcome goes to the status line either way; if the photo is shown
                    // again, a fresh read replaces whatever the reset left on screen.
                    let line = match result {
                        Ok(outcome) if outcome.sidecar == chairphoto_core::catalog::IptcSidecarState::Written => {
                            format!("IPTC saved to sidecar for {name}")
                        }
                        Ok(outcome) => format!("IPTC for {name}: {}", outcome.status()),
                        Err(e) => format!("IPTC save for {name} failed: {e}"),
                    };
                    this.status(line, cx);
                    if shown {
                        this.read_iptc(id, cx);
                    }
                }
            },
            cx,
        );
    }

    // --- versions ----------------------------------------------------------------------

    /// Click a version (or Original, `None`): the loupe shows it.
    pub fn select_version(&mut self, version: Option<PhotoVersion>, cx: &mut Context<Self>) {
        self.shell.update(cx, |s, cx| s.set_active_version(version, cx));
    }

    /// "+ Add": the typed name, else "Version N".
    pub fn add_version(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.photo_id else { return };
        let count = self.data.versions.load.ready().map_or(0, |v| v.len());
        let typed = self.version_name.read(cx).value().trim().to_string();
        let name = if typed.is_empty() { format!("Version {}", count + 1) } else { typed };
        self.version_name.update(cx, |i, cx| i.set_value("", window, cx));
        self.write(move |c| c.create_version(id, &name), |this, result, cx| this.after_version_change(result.map(drop), cx), cx);
    }

    /// Double-click a version: rename it inline.
    pub fn start_rename(&mut self, version: &PhotoVersion, window: &mut Window, cx: &mut Context<Self>) {
        let name = version.name.clone();
        let input = cx.new(|cx| {
            let mut i = InputState::new(window, cx);
            i.set_value(name, window, cx);
            i
        });
        let vid = version.id;
        let sub = cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, _, cx| match event {
            InputEvent::PressEnter { .. } | InputEvent::Blur => this.commit_rename(vid, cx),
            _ => {}
        });
        input.update(cx, |i, cx| i.focus(window, cx));
        self.renaming = Some((vid, input, sub));
        cx.notify();
    }

    /// Esc while renaming.
    pub fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        self.renaming = None;
        cx.notify();
    }

    /// Enter or blur while renaming: an empty name keeps the old one.
    pub fn commit_rename(&mut self, version_id: i64, cx: &mut Context<Self>) {
        if self.renaming.as_ref().map(|r| r.0) != Some(version_id) {
            return;
        }
        let Some((vid, input, _sub)) = self.renaming.take() else { return };
        let name = input.read(cx).value().trim().to_string();
        cx.notify();
        if name.is_empty() {
            return;
        }
        self.write(move |c| c.rename_version(vid, &name), |this, result, cx| this.after_version_change(result, cx), cx);
    }

    pub fn duplicate_version(&mut self, version_id: i64, cx: &mut Context<Self>) {
        self.write(move |c| c.duplicate_version(version_id), |this, result, cx| this.after_version_change(result.map(drop), cx), cx);
    }

    /// ✕ (no confirm, as React): the loupe falls back to Original if it showed this one.
    pub fn delete_version(&mut self, version_id: i64, cx: &mut Context<Self>) {
        self.write(
            move |c| c.delete_version(version_id),
            move |this, result, cx| {
                if result.is_ok() && this.shell.read(cx).active_version().map(|v| v.id) == Some(version_id) {
                    this.shell.update(cx, |s, cx| s.set_active_version(None, cx));
                }
                this.after_version_change(result, cx);
            },
            cx,
        );
    }

    /// ✎ on a version: the Darkroom (#111) edits it — the version is chosen, then Develop
    /// opens on it, as React chose it and opened the develop view.
    pub fn edit_version(&mut self, version: PhotoVersion, cx: &mut Context<Self>) {
        self.select_version(Some(version), cx);
        self.shell.update(cx, |s, cx| s.open_develop(cx));
    }

    fn after_version_change(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        if let Err(e) = result {
            self.status(format!("Versions: {e}"), cx);
        }
        self.reload_versions(cx);
        self.changed(cx);
    }

    // --- publications ------------------------------------------------------------------

    /// "+ Mark" / Enter: record that the chosen version went to the typed platform.
    pub fn mark_published(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.photo_id else { return };
        let platform = self.platform.read(cx).value().trim().to_lowercase();
        if platform.is_empty() {
            return;
        }
        let version = self.publish_version;
        self.write(
            move |c| c.record_publication(id, version, &platform, None),
            |this, result, cx| this.after_publication_change(result.map(drop), cx),
            cx,
        );
    }

    pub fn set_platform(&mut self, platform: &str, window: &mut Window, cx: &mut Context<Self>) {
        let platform = platform.to_string();
        self.platform.update(cx, |i, cx| i.set_value(platform, window, cx));
    }

    pub fn set_publish_version(&mut self, version: Option<i64>, cx: &mut Context<Self>) {
        self.publish_version = version;
        cx.notify();
    }

    /// ✕ on a publication (no confirm, as React).
    pub fn delete_publication(&mut self, publication_id: i64, cx: &mut Context<Self>) {
        self.write(
            move |c| c.delete_publication(publication_id),
            |this, result, cx| this.after_publication_change(result, cx),
            cx,
        );
    }

    fn after_publication_change(&mut self, result: Result<(), String>, cx: &mut Context<Self>) {
        if let Err(e) = result {
            self.status(format!("Publications: {e}"), cx);
        }
        self.reload_publications(cx);
        self.changed(cx);
    }

    // --- external editors --------------------------------------------------------------

    /// Which editors this machine has: once per catalog (it runs `which`, so it is worker
    /// work), and again whenever Preferences saves an editor setting ([`Self::reread_editors`]).
    pub fn read_editors(&mut self, cx: &mut Context<Self>) {
        if self.editors.is_some() || self.editors_reading {
            return;
        }
        self.start_editors_read(cx);
    }

    /// An editor setting changed ([`EditorsChanged`]): read the list again, even if one is
    /// cached or a read is in flight — that read may have run before the setting was stored.
    /// The list shown stays until the new one lands.
    pub fn reread_editors(&mut self, cx: &mut Context<Self>) {
        self.start_editors_read(cx);
    }

    fn start_editors_read(&mut self, cx: &mut Context<Self>) {
        self.editors_seq += 1;
        let seq = self.editors_seq;
        self.editors_reading = true;
        self.run_blocking(
            |state| {
                let editors = chairphoto_core::external_edit::available_editors(state)
                    .map(|es| es.into_iter().filter(|e| e.gui).collect())
                    .unwrap_or_default();
                let rapidraw = chairphoto_core::rapidraw::rapidraw_available(state).map(|s| s.available).unwrap_or(false);
                Ok(Editors { editors, rapidraw })
            },
            move |this, result, cx| {
                // A newer read was started (a later save): this one may predate it.
                if this.editors_seq != seq {
                    return;
                }
                this.editors_reading = false;
                this.editors = result.ok();
                cx.notify();
            },
            cx,
        );
    }

    /// Whether any editor run (sidecar or RapidRAW) is in flight for `photo`.
    pub fn editing(&self, photo: i64) -> bool {
        self.sidecar.contains_key(&photo) || self.rapid.contains_key(&photo)
    }

    /// "Edit in <editor>": launch it on the original; when it closes, the core renders the
    /// sidecar's result and stacks it (`develop_in_editor`).
    pub fn develop(&mut self, editor: &str, label: &str, cx: &mut Context<Self>) {
        let (Some(id), Some(from)) = (self.photo_id, self.from) else { return };
        let note = format!("Editing in {label}… the result imports when you close it.");
        let label = label.to_string();
        let key = editor.to_string();
        let seq = self.begin_sidecar(id, editor, note, cx);
        self.run_blocking(
            move |state| {
                futures::executor::block_on(chairphoto_core::external_edit::develop_in_editor_as(state.clone(), from, id, key))
            },
            move |this, result, cx| {
                let note = match result {
                    Ok(Some(_)) => None,
                    Ok(None) => {
                        Some(format!("No changes detected. If {label} was already open, edit there, then \"Import result\"."))
                    }
                    Err(e) => Some(e),
                };
                this.end_sidecar(id, seq, note, cx);
            },
            cx,
        );
    }

    /// "Import result": render the current sidecar and stack it, without relaunching.
    pub fn import_result(&mut self, editor: &str, cx: &mut Context<Self>) {
        let (Some(id), Some(from)) = (self.photo_id, self.from) else { return };
        let key = editor.to_string();
        let note = self.notes.get(&id).cloned().unwrap_or_default();
        let seq = self.begin_sidecar(id, editor, note, cx);
        self.run_blocking(
            move |state| {
                futures::executor::block_on(chairphoto_core::external_edit::import_developed_as(state.clone(), from, id, key))
            },
            move |this, result, cx| this.end_sidecar(id, seq, result.err(), cx),
            cx,
        );
    }

    fn begin_sidecar(&mut self, id: i64, editor: &str, note: String, cx: &mut Context<Self>) -> u64 {
        self.seq += 1;
        self.sidecar.insert(id, SidecarRun { editor: editor.to_string(), seq: self.seq });
        if note.is_empty() {
            self.notes.remove(&id);
        } else {
            self.notes.insert(id, note);
        }
        cx.notify();
        self.seq
    }

    /// A sidecar run ended: only the run that still owns the photo's entry writes its note.
    pub(crate) fn end_sidecar(&mut self, id: i64, seq: u64, note: Option<String>, cx: &mut Context<Self>) {
        if self.sidecar.get(&id).map(|r| r.seq) != Some(seq) {
            return;
        }
        self.sidecar.remove(&id);
        match note {
            Some(n) => self.notes.insert(id, n),
            None => self.notes.remove(&id),
        };
        self.changed(cx);
        cx.notify();
    }

    /// "RapidRAW": the round-trip under a job id chosen before it starts, so its events
    /// match from the first one. The entry shows "editing" at once (React's optimistic
    /// entry); the events then drive it, and the result writes the terminal note.
    pub fn edit_in_rapidraw(&mut self, cx: &mut Context<Self>) {
        let (Some(id), Some(from)) = (self.photo_id, self.from) else { return };
        if self.rapid.contains_key(&id) {
            return;
        }
        // Queued under its job id now, so a Cancel before the worker starts still lands.
        let queued = chairphoto_core::rapidraw::queue_job();
        let job = queued.id();
        self.rapid.insert(id, RapidRun { job, phase: RapidPhase::Editing });
        self.notes.remove(&id);
        cx.notify();
        self.run_blocking(
            move |state| {
                futures::executor::block_on(chairphoto_core::rapidraw::edit_in_rapidraw_as(state.clone(), from, id, queued))
            },
            move |this, result, cx| this.on_rapidraw_result(id, job, result, cx),
            cx,
        );
    }

    /// Cancel: stop waiting for this photo's round-trip — exactly the job this inspector
    /// follows.
    pub fn cancel_rapidraw(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.photo_id else { return };
        let Some(from) = self.from else { return };
        let Some(run) = self.rapid.get(&id).copied() else { return };
        if let Err(e) = chairphoto_core::rapidraw::cancel_rapidraw_job(from, id, run.job) {
            self.status(format!("Cancel failed: {e}"), cx);
        }
    }

    /// A `rapidraw:progress` event: only the job that owns the photo's entry moves it; a
    /// terminal phase ends the entry (the result writes the note).
    pub(crate) fn on_rapidraw_progress(&mut self, p: &RapidRawProgress, cx: &mut Context<Self>) {
        let Some(run) = self.rapid.get_mut(&p.photo_id) else { return };
        if run.job != p.job_id {
            return;
        }
        match p.phase.as_str() {
            "editing" => run.phase = RapidPhase::Editing,
            "waiting" => run.phase = RapidPhase::Waiting,
            "importing" => run.phase = RapidPhase::Importing,
            "done" | "error" | "cancelled" => {
                self.rapid.remove(&p.photo_id);
            }
            _ => return,
        }
        cx.notify();
    }

    /// The round-trip's result: scoped to its job, like its events.
    pub(crate) fn on_rapidraw_result(
        &mut self,
        id: i64,
        job: u64,
        result: Result<Option<i64>, String>,
        cx: &mut Context<Self>,
    ) {
        let owned = self.rapid.get(&id).is_none_or(|r| r.job == job);
        if !owned {
            return; // a newer round-trip on this photo owns its entry and note
        }
        self.rapid.remove(&id);
        match result {
            Ok(Some(_)) => {
                self.notes.remove(&id);
                self.changed(cx);
            }
            Ok(None) => {
                self.notes.insert(id, "Cancelled — nothing was imported.".into());
            }
            Err(e) => {
                self.notes.insert(id, e);
            }
        }
        cx.notify();
    }
}

/// The platforms "Mark as published" suggests (`PublishedPanel`'s datalist).
pub const COMMON_PLATFORMS: [&str; 3] = ["instagram", "flickr", "smugmug"];

/// The stars: clicking the current rating clears it.
pub fn next_rating(current: i64, clicked: i64) -> i64 {
    if clicked == current {
        0
    } else {
        clicked
    }
}

/// A label swatch: clicking the current label clears it. `""` is the Clear swatch: `None`
/// (nothing to write) on a photo with no label.
pub fn next_label(current: &str, clicked: &str) -> Option<String> {
    if clicked.is_empty() {
        return (!current.is_empty()).then(String::new);
    }
    Some(if current == clicked { String::new() } else { clicked.to_string() })
}

/// The header's camera · lens · ƒ · shutter · ISO line (whatever the file carried).
pub fn exif_line(p: &Photo) -> String {
    let aperture = p.aperture.map(|a| {
        if a.fract() == 0.0 {
            format!("ƒ/{}", a as i64)
        } else {
            format!("ƒ/{a:.1}")
        }
    });
    [p.camera_model.clone(), p.lens.clone(), aperture, p.shutter_speed.clone(), p.iso.map(|i| format!("ISO {i}"))]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Metadata grouped by family, keeping the catalog's order of first appearance.
pub fn metadata_groups(entries: &[MetadataEntry]) -> Vec<(String, Vec<&MetadataEntry>)> {
    let mut groups: Vec<(String, Vec<&MetadataEntry>)> = Vec::new();
    for e in entries {
        match groups.iter_mut().find(|(g, _)| *g == e.group_name) {
            Some((_, items)) => items.push(e),
            None => groups.push((e.group_name.clone(), vec![e])),
        }
    }
    groups
}

/// A publication's date (`toLocaleDateString` in React): the UTC calendar date of a Unix
/// timestamp, `YYYY-MM-DD`.
pub fn publication_date(unix_seconds: i64) -> String {
    // Howard Hinnant's days-to-civil.
    let z = unix_seconds.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The photo's stack: its master (itself, or its parent) and the master's children; `None`
/// when it is in no stack. Blocking: background only.
fn read_stack(
    c: &chairphoto_core::catalog::Catalog,
    id: i64,
    parent: Option<i64>,
    child_count: i64,
) -> chairphoto_core::catalog::Result<Option<StackGroup>> {
    let master_id = match parent {
        Some(p) => p,
        None if child_count > 0 => id,
        None => return Ok(None),
    };
    let master = c.get_photo(master_id)?;
    let children = c.list_stack_children(master_id)?;
    Ok(Some(StackGroup { master, children }))
}
