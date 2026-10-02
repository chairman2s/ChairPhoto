//! The shared publish flow (`src/modules/plugins/publishing.tsx`, ticket #123): what every
//! publish target's form reads and writes, and the two forms the OAuth services share.
//!
//! - [`PublishSubject`] — the photos a form publishes, snapshotted when it opens: the
//!   selection (or the active photo), the active photo and the version the loupe shows, and
//!   the catalog their ids were read from.
//! - [`VersionPicker`] — the active photo's versions, read on a worker under that catalog,
//!   and the one chosen (default: the active version; `None` = Original).
//! - [`record_publications`] — records publications under the module's marker, bound to the
//!   subject's catalog.
//! - [`PublishService`] — the per-service plumbing Flickr and SmugMug implement (#124), and
//!   the forms they share: [`panel::PublishPanel`] (version, title, description, tags,
//!   album, Publish → a publication with its URL) and [`oauth::OAuthSettings`] (API key and
//!   secret, max long edge, Connect, paste the verifier, Finish).
//!
//! **Catalog identity** (#92's standing rule). Every id a form holds is the Library's, read
//! under [`ShellState::rows_from`]; every backend call it makes is bound to that catalog, so
//! it fails closed with `CATALOG_CHANGED` once another catalog is open, even before
//! `catalog:switched` reaches the UI. When the event does arrive the form's dialog closes
//! ([`super::dialog::close_on_switch`]).
//!
//! **Temp renders** are core's: each publish renders into a job-scoped, private directory
//! (`chairphoto_core::publishing::JobTempDir`, mode 0700, a random name) removed however the
//! job ends.

pub mod oauth;
pub mod panel;

#[cfg(test)]
mod tests;

use crate::shell::ShellState;
use crate::storage::Runner;
use chairphoto_core::app::{with_catalog_as, AppState, CatalogIdentity};
use chairphoto_core::catalog::PhotoVersion;
use gpui_kit::component::button::Button;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::*;
use gpui_kit::{App, ClickEvent, Context, Entity, SharedString, WeakEntity, Window};
use std::sync::Arc;

/// The photo the loupe/inspector shows: what the version picker and the pre-flight read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActivePhoto {
    pub id: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
}

/// What a publish form publishes, as of when it opened (`getSelectedPhotos()` /
/// `getActivePhotoId()` / `getActiveVersionId()`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PublishSubject {
    /// The selection, else the active photo — what a multi-photo target (LocalSend) sends.
    pub targets: Vec<i64>,
    pub active: Option<ActivePhoto>,
    /// The version the loupe shows for the active photo (`None` = Original).
    pub active_version: Option<i64>,
    /// The catalog the ids were read from; `None` when no rows have been read (every backend
    /// call then refuses).
    pub catalog: Option<CatalogIdentity>,
}

impl PublishSubject {
    pub fn take(shell: &Entity<ShellState>, cx: &App) -> Self {
        let shell = shell.read(cx);
        let sel = shell.library.selection();
        let active = sel.active.map(|p| ActivePhoto { id: p.id, width: p.width, height: p.height });
        PublishSubject {
            targets: sel.targets.clone(),
            active,
            active_version: shell.active_version().map(|v| v.id),
            catalog: shell.rows_from(),
        }
    }
}

/// What a form answers when it has no catalog to bind its ids to.
pub const NO_ROWS: &str = "The library has not been read yet; select a photo and open Publish again.";

/// The active photo's versions and the one chosen.
#[derive(Debug, Clone, Default)]
pub struct VersionPicker {
    pub versions: Vec<PhotoVersion>,
    /// `None` = Original.
    pub chosen: Option<i64>,
}

impl VersionPicker {
    /// Starts on the active version.
    pub fn new(subject: &PublishSubject) -> Self {
        VersionPicker { versions: Vec::new(), chosen: subject.active_version }
    }

    /// The chosen version, when it is one of the listed ones.
    pub fn chosen_version(&self) -> Option<&PhotoVersion> {
        self.chosen.and_then(|id| self.versions.iter().find(|v| v.id == id))
    }

    /// The chosen version's name, or "Original".
    pub fn chosen_label(&self) -> SharedString {
        self.chosen_version().map_or("Original".into(), |v| v.name.clone().into())
    }

    /// Read the active photo's versions on a worker, bound to the subject's catalog. Lands in
    /// `set` on the view; a failure leaves the list empty (React's `.catch(() => [])`).
    pub fn load<V: 'static>(
        app: &AppState,
        subject: &PublishSubject,
        cx: &mut Context<V>,
        set: impl FnOnce(&mut V, Vec<PhotoVersion>, &mut Context<V>) + 'static,
    ) {
        let (Some(active), Some(catalog)) = (subject.active, subject.catalog) else { return };
        let app = app.clone();
        let rx = Runner::get(cx).run(move || with_catalog_as(&app, catalog, |c| c.list_versions(active.id)));
        cx.spawn(async move |this, cx| {
            let versions = rx.await.ok().and_then(Result::ok).unwrap_or_default();
            this.update(cx, |v, cx| set(v, versions, cx)).ok();
        })
        .detach();
    }

    /// The version dropdown ("Original" + every version, the chosen one checked); a pick
    /// calls `pick` on the view.
    pub fn menu<V: 'static>(
        &self,
        id: &'static str,
        this: WeakEntity<V>,
        pick: impl Fn(&mut V, Option<i64>, &mut Context<V>) + 'static,
    ) -> impl IntoElement {
        let versions = self.versions.clone();
        let current = self.chosen;
        let pick = Arc::new(pick);
        Button::new(id).outline().small().label(self.chosen_label()).tooltip("Which version to send").dropdown_menu(
            move |menu, _, _| {
                let item = |label: SharedString, value: Option<i64>| {
                    let (this, pick) = (this.clone(), pick.clone());
                    PopupMenuItem::new(label).checked(current == value).on_click(
                        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                            this.update(cx, |v, cx| {
                                pick(v, value, cx);
                                cx.notify();
                            })
                            .ok();
                        },
                    )
                };
                let mut menu = menu.item(item("Original".into(), None));
                for v in &versions {
                    menu = menu.item(item(v.name.clone().into(), Some(v.id)));
                }
                menu
            },
        )
    }
}

/// Record that each `(photo id, version id)` of `published` went to `marker` (the module's
/// [`super::ModuleMeta::marker`]), with `url` when there is one — in one catalog lock hold, and
/// only while `catalog` is still the open one (`CATALOG_CHANGED` otherwise, nothing written).
/// Blocking: call it on a worker.
pub fn record_publications(
    app: &AppState,
    catalog: CatalogIdentity,
    published: &[(i64, Option<i64>)],
    marker: &str,
    url: Option<&str>,
) -> Result<(), String> {
    with_catalog_as(app, catalog, |c| {
        for &(photo, version) in published {
            c.record_publication(photo, version, marker, url)?;
        }
        Ok(())
    })
}

/// An upload-target album (`SmugMugAlbum`): SmugMug is the only service with albums today.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Album {
    pub uri: String,
    pub name: String,
}

/// What one Publish asks a service to do.
#[derive(Debug, Clone, PartialEq)]
pub struct PublishRequest {
    /// The catalog the photo id was read from: the service's render must be bound to it
    /// (`chairphoto_core::publishing::render_upload_jpeg(state, Some(catalog), …)`).
    pub catalog: CatalogIdentity,
    pub photo_id: i64,
    /// `None` = Original.
    pub version_id: Option<i64>,
    pub title: String,
    pub description: String,
    /// The chosen album's URI; empty for a service without albums.
    pub album_uri: String,
    /// The service's own tag format; empty for a service without tags.
    pub tags: String,
}

/// The per-service plumbing the shared forms call (`PublishService` in publishing.tsx): what
/// the Flickr and SmugMug modules implement (#124). Every method is **blocking** (network,
/// catalog) and runs on a worker; `settings` is the module's own namespaced settings handle,
/// bound to the catalog its form opened on.
pub trait PublishService: Send + Sync + 'static {
    /// Display name, e.g. "Flickr".
    fn name(&self) -> SharedString;
    /// Where to register the developer app (shown as a hint).
    fn signup_url(&self) -> SharedString;
    /// Start the OAuth 1.0a out-of-band flow; the URL to authorize at.
    fn begin_auth(&self, settings: &super::ModuleSettings) -> Result<String, String>;
    /// Finish it with the verifier the user pasted.
    fn complete_auth(&self, settings: &super::ModuleSettings, verifier: &str) -> Result<(), String>;
    fn connected(&self, settings: &super::ModuleSettings) -> Result<bool, String>;
    /// Upload; returns the published page/image URL (recorded on the publication when it is a
    /// web address).
    fn publish(&self, settings: &super::ModuleSettings, request: PublishRequest) -> Result<String, String>;
    /// Services with native tags (Flickr): whether the form shows a Tags field.
    fn has_tags(&self) -> bool {
        false
    }
    /// The Tags prefill from the photo's catalog keywords, in the service's own format.
    fn suggest_tags(&self, _settings: &super::ModuleSettings, _catalog: CatalogIdentity, _photo_id: i64) -> Result<String, String> {
        Ok(String::new())
    }
    /// Services with albums (SmugMug): whether the form shows the album picker.
    fn has_albums(&self) -> bool {
        false
    }
    fn list_albums(&self, _settings: &super::ModuleSettings) -> Result<Vec<Album>, String> {
        Ok(Vec::new())
    }
    /// `None`: the service cannot create albums (no "+ New").
    fn can_create_album(&self) -> bool {
        false
    }
    fn create_album(&self, _settings: &super::ModuleSettings, _name: &str) -> Result<Album, String> {
        Err("This service cannot create albums".into())
    }
}
