//! The SmugMug module (`smugmug.tsx`, ticket #124): publish a photo into a SmugMug album
//! through its official OAuth 1.0a API.
//!
//! - **Settings** (its Preferences tab): the shared [`OAuthSettings`].
//! - **Publish target** "SmugMug": the shared [`PublishPanel`] with the album picker (the
//!   cached list at once, Refresh, "+ New", the last album remembered) and no tags. A publish
//!   records a `smugmug` publication with the image's URL.
//!
//! The bodies are core's (`chairphoto_core::app::{oauth, smugmug, uploads}`); the network is a
//! [`SmugMugApi`], which tests fake — they never reach
//! SmugMug.
//!
//! **Privacy.** Enabling the module and connecting are the opt-in; a photo leaves only on the
//! user's Publish, into the album they chose.
//!
//! The `smugmug` backend is not in the default build: without it the module still registers,
//! so the Modules panel says the backend is not included and refuses to enable it.

#[cfg(all(test, feature = "smugmug"))]
mod tests;

use super::{Module, ModuleHost, ModuleInstance, ModuleMeta};
#[cfg(feature = "smugmug")]
use super::{
    publishing::{oauth::OAuthSettings, panel::PublishPanel, Album, PublishRequest, PublishService},
    view, Contributions, ModuleSettings, PublishTarget, SettingsPanel,
};
#[cfg(feature = "smugmug")]
use chairphoto_core::app::oauth;
#[cfg(feature = "smugmug")]
use chairphoto_core::app::smugmug::{self as core_smugmug, LiveSmugMug, SmugMugApi};
#[cfg(feature = "smugmug")]
use chairphoto_core::app::uploads::{RenderedJob, UploadJob, UploadService};
use gpui_kit::App;
#[cfg(feature = "smugmug")]
use gpui_kit::SharedString;
#[cfg(feature = "smugmug")]
use std::sync::Arc;

pub const SMUGMUG_ID: &str = "smugmug";

/// The SmugMug module. `api` is the network (the real SmugMug unless a test hands in a fake).
pub struct SmugMugModule {
    #[cfg(feature = "smugmug")]
    pub api: Arc<dyn SmugMugApi>,
}

impl Default for SmugMugModule {
    fn default() -> Self {
        SmugMugModule {
            #[cfg(feature = "smugmug")]
            api: Arc::new(LiveSmugMug),
        }
    }
}

impl Module for SmugMugModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(SMUGMUG_ID, "SmugMug")
            .description("Publish photos to a SmugMug album via the official API; records which version was posted.")
            .backend_feature("smugmug")
            .publication_marker(SMUGMUG_ID)
    }

    #[cfg(feature = "smugmug")]
    fn load(&self, host: ModuleHost, _: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        Ok(Box::new(Instance { service: Arc::new(SmugMugService { api: self.api.clone() }), host }))
    }

    #[cfg(not(feature = "smugmug"))]
    fn load(&self, _: ModuleHost, _: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        Err("The SmugMug backend is not included in this build".into())
    }
}

/// The SmugMug plumbing the shared publish forms call: core's bodies over [`SmugMugApi`].
#[cfg(feature = "smugmug")]
pub struct SmugMugService {
    pub api: Arc<dyn SmugMugApi>,
}

#[cfg(feature = "smugmug")]
fn album(a: core_smugmug::Album) -> Album {
    Album { uri: a.uri, name: a.name }
}

#[cfg(feature = "smugmug")]
impl PublishService for SmugMugService {
    fn name(&self) -> SharedString {
        "SmugMug".into()
    }
    fn signup_url(&self) -> SharedString {
        "smugmug.com/api/developer/apply".into()
    }
    fn service(&self) -> UploadService {
        core_smugmug::SERVICE
    }
    fn begin_auth(&self, settings: &ModuleSettings) -> Result<String, String> {
        oauth::begin_auth(&*self.api, settings, SMUGMUG_ID)
    }
    fn complete_auth(&self, settings: &ModuleSettings, verifier: &str) -> Result<(), String> {
        oauth::complete_auth(&*self.api, settings, SMUGMUG_ID, verifier)
    }
    fn connected(&self, settings: &ModuleSettings) -> Result<bool, String> {
        oauth::connected(settings)
    }
    fn ready(&self, settings: &ModuleSettings) -> Result<(), String> {
        oauth::credentials(settings, SMUGMUG_ID).map(|_| ())
    }
    fn render(&self, settings: &ModuleSettings, job: UploadJob) -> Result<RenderedJob, String> {
        core_smugmug::render(settings, job)
    }
    fn upload(&self, settings: &ModuleSettings, rendered: &RenderedJob, r: &PublishRequest) -> Result<String, String> {
        // SmugMug's caption is the form's description.
        core_smugmug::upload(&*self.api, settings, rendered, &r.album_uri, &r.title, &r.description)
    }
    fn has_albums(&self) -> bool {
        true
    }
    fn list_albums(&self, settings: &ModuleSettings) -> Result<Vec<Album>, String> {
        Ok(core_smugmug::list_albums(&*self.api, settings)?.into_iter().map(album).collect())
    }
    fn can_create_album(&self) -> bool {
        true
    }
    fn create_album(&self, settings: &ModuleSettings, name: &str) -> Result<Album, String> {
        core_smugmug::create_album(&*self.api, settings, name).map(album)
    }
}

#[cfg(feature = "smugmug")]
struct Instance {
    host: ModuleHost,
    service: Arc<SmugMugService>,
}

#[cfg(feature = "smugmug")]
impl ModuleInstance for Instance {
    fn contributions(&self) -> Contributions {
        let (host, service) = (self.host.clone(), self.service.clone());
        let connect = view(move |window, cx| {
            let service: Arc<dyn PublishService> = service.clone();
            OAuthSettings::new(host.settings(), host.model(), service, window, cx)
        });
        let (host, service) = (self.host.clone(), self.service.clone());
        let publish = view(move |window, cx| {
            let state = host.model().read(cx).state().clone();
            let marker: SharedString = host.meta().marker().to_string().into();
            let service: Arc<dyn PublishService> = service.clone();
            PublishPanel::new(state, host.model().clone(), host.shell(), host.settings(), marker, service, window, cx)
        });
        Contributions {
            settings: vec![SettingsPanel { id: "smugmug-connect".into(), view: connect }],
            publish_targets: vec![PublishTarget { id: SMUGMUG_ID.into(), label: "SmugMug".into(), view: publish }],
            ..Default::default()
        }
    }
}
