//! Preferences (#113): the port of `src/components/Preferences.tsx` with `SafetyPanel.tsx`
//! and the Modules tab (`ModulesPanel.tsx` as [`crate::modules::panel::ModulesPanel`]);
//! `docs/plans/gpui/parity.md` § Preferences is the acceptance list.
//!
//! One dialog, the rail's gear and More ⋯ → Preferences… ([`OpenPreferences`]). Tabs:
//!
//! - **Storage** ([`storage`]): the library folder, Volumes (the storage ticket's
//!   [`VolumesPanel`]), Safety, Local / NAS tiering with "Index existing NAS photos", and
//!   Maintenance (remove unavailable / empty photos behind a confirm, compact the catalog).
//! - **Tags** ([`tags`]): tidy redundant tags, find duplicate and unused tags.
//! - **Editors** ([`editors`]): darktable / RawTherapee / ART and RapidRAW, and the Darkroom's
//!   decode cache, neighbour preload, white balance for new edits, export parity and the dev
//!   render-timing switch.
//! - **Modules**: enable and disable modules.
//! - **Appearance** ([`appearance`]): Follow Omarchy / ChairPhoto Standard, per machine.
//! - **One tab per enabled module with settings panels**, named after the module, showing
//!   its [`crate::modules::SettingsPanel`]s.
//!
//! Every catalog setting keeps the React app's key, so an existing catalog keeps its values.
//!
//! **Lifecycle.** As React mounted only the active tab, a tab's sections are built when it is
//! shown and dropped when another is; each reads what it shows as it is built. Every read
//! and write runs on the [`Runner`] ([`Ctx::run`]), never the UI thread, and its result
//! reaches the section through a weak handle, only while the catalog it was started against
//! is still open: a `catalog:switched` (or a re-root, which reopens the catalog) rebuilds the
//! open tab's sections, so a result from before it finds its section gone and is dropped
//! ([`Ctx::live`] checks the epoch and the identity too). Catalog-setting writes persist in
//! the order they were made, whatever order the Runner runs them in, and only the newest
//! write of a key reports back to the section ([`Ctx::write_setting`]).
//!
//! **Catalog identity.** A tab is built bound to the catalog the model last read
//! ([`AppModel::catalog_identity`]); its work gets a [`Scope`] whose catalog access goes
//! through `with_catalog_as`. So a read, a settings write or a delete queued in one catalog
//! fails closed (`CATALOG_CHANGED`) when it runs after the core switched to another — also
//! before `catalog:switched` reaches the dialog — instead of landing on the other catalog's
//! settings or on its rows with the same ids.
//!
//! **Dialogs over Preferences.** Tags → "Merge X away…" opens the Tag panel's merge preview
//! (`tags::merge::TagMerge`) on top of this dialog ([`Preferences::open_tag_merge`]). This
//! entity, not the section, owns its subscriptions: a rebuild drops the section, and the
//! preview must still close itself when its tree is superseded.
//!
//! **Dropped** (parity.md): the GlSpike probe ("Run WebGL probe", `editor.glSpike.lastReport`).

pub mod appearance;
pub mod editors;
pub mod storage;
pub mod tags;

use crate::model::AppModel;
use crate::modules::panel::ModulesPanel;
use crate::modules::ModuleRegistry;
use crate::shell::style::Colors;
use crate::shell::ShellState;
use crate::storage::volumes::VolumesPanel;
use crate::storage::{CloseDialog, Runner};
use crate::tags::merge::{TagMerge, TagMerged};
use crate::tags::state::TagDialog;
use crate::tags::TagsState;
use crate::view::RootView;
use chairphoto_core::app::{with_catalog_as, AppState, CatalogIdentity};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use gpui_kit::component::WindowExt as _;
use gpui_kit::prelude::*;
use gpui_kit::TestSupportExt as _;
use gpui_kit::{
    div, px, AnyElement, App, Context, Div, Entity, EventEmitter, FontWeight, Global, SharedString, Subscription,
    WeakEntity, Window,
};

pub use crate::shell::actions::OpenPreferences;

/// The dialog's width and the height of its tab area.
const DIALOG_W: f32 = 860.;
const BODY_H: f32 = 560.;

/// A Preferences tab (`validIds` in Preferences.tsx).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tab {
    Storage,
    Tags,
    Editors,
    Modules,
    Appearance,
    /// An enabled module's settings, by module id.
    Module(SharedString),
}

impl Tab {
    /// The tab button's element id.
    pub fn element_id(&self) -> SharedString {
        match self {
            Tab::Storage => "prefs-tab-storage".into(),
            Tab::Tags => "prefs-tab-tags".into(),
            Tab::Editors => "prefs-tab-editors".into(),
            Tab::Modules => "prefs-tab-modules".into(),
            Tab::Appearance => "prefs-tab-appearance".into(),
            Tab::Module(id) => format!("prefs-tab-module-{id}").into(),
        }
    }
}

/// What every section needs: the core state, the entities it reports to, and the catalog
/// epoch it was built in.
#[derive(Clone)]
pub struct Ctx {
    pub app: AppState,
    pub model: Entity<AppModel>,
    pub shell: Entity<ShellState>,
    /// [`AppModel::catalog_epoch`] when the section was built.
    pub epoch: u64,
    /// The catalog the section was built for ([`AppModel::catalog_identity`] then): every
    /// catalog access of its work is bound to it ([`Scope`]).
    pub identity: Option<CatalogIdentity>,
}

/// What a section's queued work runs against: the core state and the catalog the section
/// was built for. Catalog access goes through [`Scope::catalog`], which fails closed with
/// `CATALOG_CHANGED` once another catalog is open; core functions that touch the catalog
/// themselves get [`Scope::identity`].
#[derive(Clone)]
pub struct Scope {
    state: AppState,
    identity: Option<CatalogIdentity>,
}

impl Scope {
    /// For work that is not the catalog's (the decode cache, which editors are on `PATH`).
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// The catalog the work is bound to.
    pub fn identity(&self) -> Result<CatalogIdentity, String> {
        self.identity.ok_or_else(|| "No catalog is open".to_string())
    }

    /// `f` against the catalog the work is bound to (`with_catalog_as`).
    pub fn catalog<T>(
        &self,
        f: impl FnOnce(&chairphoto_core::catalog::Catalog) -> chairphoto_core::catalog::Result<T>,
    ) -> Result<T, String> {
        with_catalog_as(&self.state, self.identity()?, f)
    }
}

/// The order of the catalog-setting writes Preferences makes ([`Ctx::write_setting`]). A GPUI
/// global, so the order holds across a tab's rebuild and the dialog's reopening.
#[derive(Default)]
pub struct SettingWrites {
    /// The last write number handed out. UI thread only.
    seq: u64,
    /// Per key, the newest write made. UI thread only.
    issued: HashMap<String, u64>,
    /// Per key, the newest write persisted. Workers only, under the catalog lock.
    persisted: Arc<Mutex<HashMap<String, u64>>>,
}

impl Global for SettingWrites {}

impl Ctx {
    /// Still the catalog the section was built for.
    pub fn live(&self, cx: &App) -> bool {
        let model = self.model.read(cx);
        model.catalog_epoch == self.epoch && model.catalog_identity() == self.identity
    }

    /// The [`Scope`] this section's work runs in.
    pub fn scope(&self) -> Scope {
        Scope { state: self.app.clone(), identity: self.identity }
    }

    /// Run `work` on the [`Runner`] and hand its result to `apply` on the UI thread — only if
    /// the section still exists and its catalog is still open. `work`'s catalog access is
    /// bound to the section's catalog ([`Scope`]).
    pub fn run<V: 'static, R: Send + 'static>(
        &self,
        cx: &mut Context<V>,
        work: impl FnOnce(&Scope) -> R + Send + 'static,
        apply: impl FnOnce(&mut V, R, &mut Context<V>) + 'static,
    ) {
        self.run_landed(cx, work, |_, _| {}, apply);
    }

    /// [`run`](Self::run), with `landed` called first on the UI thread whenever the work
    /// finishes — **also when the section is gone** (its tab switched, Preferences closed) —
    /// for what must not depend on the section, such as telling the rest of the app.
    fn run_landed<V: 'static, R: Send + 'static>(
        &self,
        cx: &mut Context<V>,
        work: impl FnOnce(&Scope) -> R + Send + 'static,
        landed: impl FnOnce(&R, &mut App) + 'static,
        apply: impl FnOnce(&mut V, R, &mut Context<V>) + 'static,
    ) {
        let scope = self.scope();
        let rx = Runner::get(cx).run(move || work(&scope));
        let ctx = self.clone();
        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else { return };
            cx.update(|cx| landed(&result, cx));
            this.update(cx, |view, cx| {
                if ctx.live(cx) {
                    apply(view, result, cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// [`run`](Self::run) for an `apply` that needs the window (it sets an input's text).
    pub fn run_in<V: 'static, R: Send + 'static>(
        &self,
        window: &mut Window,
        cx: &mut Context<V>,
        work: impl FnOnce(&Scope) -> R + Send + 'static,
        apply: impl FnOnce(&mut V, R, &mut Window, &mut Context<V>) + 'static,
    ) {
        let scope = self.scope();
        let rx = Runner::get(cx).run(move || work(&scope));
        let ctx = self.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(result) = rx.await else { return };
            this.update_in(cx, |view, window, cx| {
                if ctx.live(cx) {
                    apply(view, result, window, cx);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Persist the catalog setting `key = value` on the [`Runner`], **in the order the writes
    /// were made**, then run `after` on the same worker, and hand `apply` the outcome with the
    /// value stored for `key` once the write ran — so the section shows what persisted.
    ///
    /// Each write is an independent Runner task, and those run in any order, so two quick
    /// changes of one setting (off → on) could otherwise persist as on → off while the UI shows
    /// on. Every write takes a number from [`SettingWrites`] as it is made; the worker writes
    /// only if no later write of `key` has been persisted yet (checked and recorded under the
    /// catalog lock the write holds, so two writers cannot interleave), else it leaves the
    /// newer value in place. `apply` runs only for the newest write made of `key`: an older
    /// one's completion would show a value the user has already changed.
    pub fn write_setting<V: 'static, R: Send + 'static>(
        &self,
        cx: &mut Context<V>,
        key: impl Into<String>,
        value: String,
        after: impl FnOnce(&Scope) -> Result<R, String> + Send + 'static,
        apply: impl FnOnce(&mut V, Result<(Option<String>, R), String>, &mut Context<V>) + 'static,
    ) {
        self.write_setting_landed(cx, key, value, after, |_, _| {}, apply);
    }

    /// [`write_setting`](Self::write_setting), with `landed` called with the outcome of every
    /// write of it once the worker has run — whether or not the section still exists, and
    /// whether or not a newer write of the key was made since ([`run_landed`](Self::run_landed)).
    pub fn write_setting_landed<V: 'static, R: Send + 'static>(
        &self,
        cx: &mut Context<V>,
        key: impl Into<String>,
        value: String,
        after: impl FnOnce(&Scope) -> Result<R, String> + Send + 'static,
        landed: impl FnOnce(&Result<(Option<String>, R), String>, &mut App) + 'static,
        apply: impl FnOnce(&mut V, Result<(Option<String>, R), String>, &mut Context<V>) + 'static,
    ) {
        let key: String = key.into();
        let writes = cx.default_global::<SettingWrites>();
        writes.seq += 1;
        let seq = writes.seq;
        writes.issued.insert(key.clone(), seq);
        let persisted = writes.persisted.clone();
        let k = key.clone();
        self.run_landed(
            cx,
            move |scope| {
                let stored = scope.catalog(|c| {
                    // A leaf under the catalog lock: nothing else is taken while it is held.
                    let mut persisted = persisted.lock().unwrap_or_else(|e| e.into_inner());
                    let last = persisted.entry(k.clone()).or_default();
                    if seq > *last {
                        c.set_setting(&k, &value)?;
                        *last = seq;
                    }
                    c.get_setting(&k)
                })?;
                Ok((stored, after(scope)?))
            },
            landed,
            move |view, result, cx| {
                let newest = cx.try_global::<SettingWrites>().and_then(|w| w.issued.get(&key).copied());
                if newest == Some(seq) {
                    apply(view, result, cx);
                }
            },
        );
    }

    /// Something catalog-derived changed (React's `onLibraryRootChanged` → `refresh()`): the
    /// model re-reads, and its `CatalogRead` makes the shell re-read its lists and counts.
    pub fn changed(&self, cx: &mut App) {
        self.model.update(cx, |m, cx| m.refresh(cx));
    }

    /// Set the app's status line.
    pub fn status(&self, line: impl Into<SharedString>, cx: &mut App) {
        let line = line.into();
        self.model.update(cx, |m, cx| {
            m.status = line;
            cx.notify();
        });
    }
}

/// The Storage tab's sections.
pub struct StorageTab {
    pub library: Entity<storage::LibrarySection>,
    pub volumes: Entity<VolumesPanel>,
    pub safety: Entity<storage::SafetySection>,
    pub tiering: Entity<storage::TieringSection>,
    pub maintenance: Entity<storage::MaintenanceSection>,
}

/// The active tab's sections.
pub enum Content {
    Storage(StorageTab),
    Tags(Entity<tags::TagMaintenance>),
    Editors(Entity<editors::EditorsSection>, Entity<editors::DarkroomSection>),
    Modules(Entity<ModulesPanel>),
    Appearance(Entity<appearance::AppearanceSection>),
    /// A module's settings panels: the registry's views, cached per window.
    Module(SharedString),
}

/// The Preferences dialog's content.
pub struct Preferences {
    app: AppState,
    model: Entity<AppModel>,
    shell: Entity<ShellState>,
    registry: Entity<ModuleRegistry>,
    /// The tag tree: Tags → "Merge X away…" runs its preview under it.
    tags: Entity<TagsState>,
    /// The tab shown.
    pub tab: Tab,
    pub content: Content,
    /// The catalog epoch and identity [`Self::content`] was built in.
    epoch: u64,
    identity: Option<CatalogIdentity>,
    /// The sections' own subscriptions (Safety's "Show me" closes the dialog).
    content_subscriptions: Vec<Subscription>,
    /// The merge preview opened from Tags: its close and its report. Kept across a rebuild.
    merge_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CloseDialog> for Preferences {}

/// The Preferences dialog opened last — what a test drives.
#[derive(Clone)]
pub struct LastPreferences(pub WeakEntity<Preferences>);

impl Global for LastPreferences {}

impl Preferences {
    pub fn new(
        model: Entity<AppModel>,
        shell: Entity<ShellState>,
        registry: Entity<ModuleRegistry>,
        tags: Entity<TagsState>,
        tab: Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let app = model.read(cx).state().clone();
        let epoch = model.read(cx).catalog_epoch;
        let _subscriptions = vec![
            // A catalog switch (or a re-root's reopen, or the first read): rebuild the open tab
            // against the catalog now open; whatever the old sections still had in flight
            // finds them gone.
            cx.observe_in(&model, window, |this, model, window, cx| {
                let m = model.read(cx);
                if m.catalog_epoch != this.epoch || m.catalog_identity() != this.identity {
                    this.rebuild(window, cx);
                }
            }),
            // A module enabled or disabled: its tab comes or goes; a vanished tab falls back
            // to Storage (React's `active`).
            cx.observe_in(&registry, window, |this, _, window, cx| {
                if let Tab::Module(id) = &this.tab {
                    if !this.module_tabs(cx).iter().any(|(m, _)| m == id) {
                        this.select(Tab::Storage, window, cx);
                        return;
                    }
                }
                cx.notify();
            }),
        ];
        let placeholder = Content::Module(SharedString::default());
        let mut this = Preferences {
            app,
            model,
            shell,
            registry,
            tags,
            tab,
            content: placeholder,
            epoch,
            identity: None,
            content_subscriptions: Vec::new(),
            merge_subscriptions: Vec::new(),
            _subscriptions,
        };
        if let Tab::Module(id) = &this.tab {
            if !this.module_tabs(cx).iter().any(|(m, _)| m == id) {
                this.tab = Tab::Storage;
            }
        }
        this.rebuild(window, cx);
        this
    }

    fn ctx(&self, cx: &App) -> Ctx {
        let model = self.model.read(cx);
        Ctx {
            app: self.app.clone(),
            model: self.model.clone(),
            shell: self.shell.clone(),
            epoch: model.catalog_epoch,
            identity: model.catalog_identity(),
        }
    }

    /// The enabled modules with settings panels, in registration order: `(id, name)`.
    pub fn module_tabs(&self, cx: &App) -> Vec<(SharedString, SharedString)> {
        let registry = self.registry.read(cx);
        registry
            .list()
            .into_iter()
            .filter(|m| m.enabled && !registry.settings_panels(&m.id).is_empty())
            .map(|m| (m.id, m.name))
            .collect()
    }

    /// Show `tab`, building its sections.
    pub fn select(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = tab;
        self.rebuild(window, cx);
    }

    fn rebuild(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ctx = self.ctx(cx);
        self.epoch = ctx.epoch;
        self.identity = ctx.identity;
        self.content_subscriptions.clear();
        self.content = match &self.tab {
            Tab::Storage => {
                let safety = cx.new(|cx| storage::SafetySection::new(ctx.clone(), cx));
                self.content_subscriptions.push(cx.subscribe(&safety, |_, _, _: &CloseDialog, cx| cx.emit(CloseDialog)));
                Content::Storage(StorageTab {
                    library: cx.new(|cx| storage::LibrarySection::new(ctx.clone(), window, cx)),
                    volumes: cx.new(|cx| VolumesPanel::new(ctx.scope(), window, cx)),
                    safety,
                    tiering: cx.new(|cx| storage::TieringSection::new(ctx.clone(), window, cx)),
                    maintenance: cx.new(|cx| storage::MaintenanceSection::new(ctx.clone(), cx)),
                })
            }
            Tab::Tags => {
                let tree = self.tags.clone();
                let section = cx.new(|cx| tags::TagMaintenance::new(ctx.clone(), tree, cx));
                self.content_subscriptions.push(cx.subscribe_in(
                    &section,
                    window,
                    |this, section, open: &tags::OpenTagMerge, window, cx| {
                        this.open_tag_merge(section.downgrade(), open.0.clone(), window, cx)
                    },
                ));
                Content::Tags(section)
            }
            Tab::Editors => Content::Editors(
                cx.new(|cx| editors::EditorsSection::new(ctx.clone(), window, cx)),
                cx.new(|cx| editors::DarkroomSection::new(ctx.clone(), window, cx)),
            ),
            Tab::Modules => Content::Modules(cx.new(|cx| ModulesPanel::new(self.registry.clone(), cx))),
            Tab::Appearance => Content::Appearance(cx.new(appearance::AppearanceSection::new)),
            Tab::Module(id) => Content::Module(id.clone()),
        };
        cx.notify();
    }

    /// Tags → "Merge X away…": the Tag panel's merge preview for `source`, over this dialog.
    /// Its report goes to the section that asked, if that section is still the one shown.
    pub fn open_tag_merge(
        &mut self,
        section: WeakEntity<tags::TagMaintenance>,
        source: chairphoto_core::catalog::TagWithCount,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tree = self.tags.clone();
        let view = cx.new(|cx| TagMerge::new(tree, source, window, cx));
        self.tags.update(cx, |t, _| t.last_dialog = Some(TagDialog::Merge(view.downgrade())));
        self.merge_subscriptions = vec![
            cx.subscribe_in(&view, window, |_, _, _: &CloseDialog, window, cx| window.close_dialog(cx)),
            cx.subscribe(&view, move |_, _, merged: &TagMerged, cx| {
                section.update(cx, |s, cx| s.merged(&merged.0, cx)).ok();
            }),
        ];
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.title("Merge tag").w(px(620.)).child(view.clone()).on_ok(|_, _, _| false)
        });
    }

    fn tab_button(&self, tab: Tab, label: SharedString, colors: Colors, cx: &mut Context<Self>) -> AnyElement {
        let on = self.tab == tab;
        div()
            .id(tab.element_id())
            .px(px(10.))
            .py(px(6.))
            .rounded(px(6.))
            .text_size(px(12.5))
            .cursor_pointer()
            .text_color(if on { colors.txt } else { colors.dim })
            .when(on, |d| d.bg(colors.sel).font_weight(FontWeight::SEMIBOLD))
            .when(!on, |d| d.hover(|s| s.text_color(colors.txt)))
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| this.select(tab.clone(), window, cx)))
            .test_support()
            .into_any_element()
    }
}

impl Render for Preferences {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::get(cx);
        let mut nav = div().id("prefs-tabs").flex().flex_col().flex_none().w(px(150.)).gap(px(2.));
        for (tab, label) in [
            (Tab::Storage, "Storage"),
            (Tab::Tags, "Tags"),
            (Tab::Editors, "Editors"),
            (Tab::Modules, "Modules"),
            (Tab::Appearance, "Appearance"),
        ] {
            nav = nav.child(self.tab_button(tab, label.into(), colors, cx));
        }
        for (id, name) in self.module_tabs(cx) {
            nav = nav.child(self.tab_button(Tab::Module(id), name, colors, cx));
        }
        let content: AnyElement = match &self.content {
            Content::Storage(s) => div()
                .flex()
                .flex_col()
                .gap(px(22.))
                .child(s.library.clone())
                .child(section("prefs-volumes", "Volumes", colors).child(s.volumes.clone()))
                .child(s.safety.clone())
                .child(s.tiering.clone())
                .child(s.maintenance.clone())
                .into_any_element(),
            Content::Tags(t) => t.clone().into_any_element(),
            Content::Editors(e, d) => div().flex().flex_col().gap(px(22.)).child(e.clone()).child(d.clone()).into_any_element(),
            Content::Modules(m) => m.clone().into_any_element(),
            Content::Appearance(a) => a.clone().into_any_element(),
            Content::Module(id) => {
                let views = ModuleRegistry::settings_views(&self.registry, id, window, cx);
                if views.is_empty() {
                    crate::storage::ui::empty("prefs-module-empty", "This module has no settings.", colors)
                } else {
                    div()
                        .id("prefs-module-settings")
                        .flex()
                        .flex_col()
                        .gap(px(18.))
                        .children(views.into_iter().map(|v| div().pb(px(12.)).border_b_1().border_color(colors.line).child(v)))
                        .into_any_element()
                }
            }
        };
        div()
            .id("preferences")
            .flex()
            .flex_row()
            .gap(px(18.))
            .h(px(BODY_H))
            .text_size(px(12.5))
            .text_color(colors.txt)
            .child(nav)
            .child(div().id("prefs-content").flex_1().min_w_0().h_full().overflow_y_scroll().pr(px(8.)).child(content))
    }
}

/// `.prefs-section` with its `<h3>`.
pub fn section(id: &'static str, title: &'static str, colors: Colors) -> gpui_kit::Stateful<Div> {
    div().id(id).flex().flex_col().gap(px(8.)).child(heading(title, colors))
}

/// `.prefs-section h3`.
pub fn heading(title: impl Into<SharedString>, colors: Colors) -> Div {
    div().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).text_color(colors.txt).child(title.into())
}

/// A status line (`.modal-sub` under a section), findable in tests.
pub fn status(id: &'static str, text: impl Into<SharedString>, colors: Colors) -> AnyElement {
    div().id(id).text_size(px(11.5)).text_color(colors.dim).child(text.into()).test_support().into_any_element()
}

/// `toLocaleString()` for a count: thousands grouped with commas.
pub fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        out.insert(0, '-');
    }
    out
}

impl RootView {
    /// Open Preferences on `tab` (the rail's gear and More ⋯ → Preferences…: Storage).
    pub(crate) fn open_preferences(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        let (model, shell, registry, tags) = (self.model.clone(), self.shell.clone(), self.modules.clone(), self.tags.clone());
        let view = cx.new(|cx| Preferences::new(model, shell, registry, tags, tab, window, cx));
        cx.set_global(LastPreferences(view.downgrade()));
        self.dialog_close = Some(cx.subscribe_in(&view, window, |_, _, _: &CloseDialog, window, cx| {
            window.close_dialog(cx);
        }));
        window.open_dialog(cx, move |dialog, _, _| {
            // No OK button: Enter in a field (Index, the cache size) must not close it.
            dialog.title("Preferences").w(px(DIALOG_W)).child(view.clone()).on_ok(|_, _, _| false)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_groups_like_to_locale_string() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(thousands(-12345), "-12,345");
    }
}
