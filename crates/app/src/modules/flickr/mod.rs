//! The Flickr module (`flickr.tsx`, ticket #124): publish a photo to Flickr through its
//! official OAuth 1.0a API, and backfill years of past posts as publications.
//!
//! - **Settings** (its Preferences tab): the shared [`OAuthSettings`] — API key and secret,
//!   max long edge, Connect, paste the verifier, Finish — and "Import published from Flickr"
//!   ([`import::ImportPublishedPanel`]): Preview, resolve the ambiguous photos by hand, import.
//! - **Publish target** "Flickr": the shared [`PublishPanel`], with Tags prefilled from the
//!   photo's export keywords until edited. A publish records a `flickr` publication with the
//!   photo's page URL.
//!
//! The bodies are core's (`chairphoto_core::app::{oauth, flickr, uploads}`); the network is a
//! [`FlickrApi`], which tests fake — they never reach Flickr.
//!
//! **Privacy.** Enabling the module and connecting are the opt-in; a photo leaves only on the
//! user's Publish. The importer only reads the user's own photostream, on Preview, and never
//! writes to Flickr.
//!
//! The `flickr` backend is not in the default build: without it the
//! module still registers, so the Modules panel says "backend "flickr" not included in this
//! build" and refuses to enable it.

#[cfg(feature = "flickr")]
pub mod import;
#[cfg(all(test, feature = "flickr"))]
mod tests;

use super::{Module, ModuleHost, ModuleInstance, ModuleMeta};
#[cfg(feature = "flickr")]
use super::{
    publishing::{oauth::OAuthSettings, panel::PublishPanel, PublishRequest, PublishService},
    view, Contributions, ModuleSettings, PublishTarget, SettingsPanel,
};
#[cfg(feature = "flickr")]
use chairphoto_core::app::flickr::{self as core_flickr, FlickrApi, LiveFlickr};
#[cfg(feature = "flickr")]
use chairphoto_core::app::oauth;
#[cfg(feature = "flickr")]
use chairphoto_core::app::uploads::{RenderedJob, UploadJob, UploadService};
#[cfg(feature = "flickr")]
use chairphoto_core::app::{AppState, CatalogIdentity};
#[cfg(feature = "flickr")]
use gpui_kit::SharedString;
use gpui_kit::App;
#[cfg(feature = "flickr")]
use std::sync::Arc;

pub const FLICKR_ID: &str = "flickr";

/// The Flickr module. `api` is the network (the real Flickr unless a test hands in a fake).
pub struct FlickrModule {
    #[cfg(feature = "flickr")]
    pub api: Arc<dyn FlickrApi>,
}

impl Default for FlickrModule {
    fn default() -> Self {
        FlickrModule {
            #[cfg(feature = "flickr")]
            api: Arc::new(LiveFlickr),
        }
    }
}

impl Module for FlickrModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(FLICKR_ID, "Flickr")
            .description("Publish photos to Flickr via the official API; records which version was posted.")
            .backend_feature("flickr")
            .publication_marker(FLICKR_ID)
    }

    #[cfg(feature = "flickr")]
    fn load(&self, host: ModuleHost, cx: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        let app = host.model().read(cx).state().clone();
        Ok(Box::new(Instance { service: Arc::new(FlickrService { api: self.api.clone(), app }), host }))
    }

    #[cfg(not(feature = "flickr"))]
    fn load(&self, _: ModuleHost, _: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        Err("The Flickr backend is not included in this build".into())
    }
}

/// The Flickr plumbing the shared publish forms call: core's bodies over [`FlickrApi`].
#[cfg(feature = "flickr")]
pub struct FlickrService {
    pub api: Arc<dyn FlickrApi>,
    pub app: AppState,
}

#[cfg(feature = "flickr")]
impl PublishService for FlickrService {
    fn name(&self) -> SharedString {
        "Flickr".into()
    }
    fn signup_url(&self) -> SharedString {
        "flickr.com/services/apps/create".into()
    }
    fn service(&self) -> UploadService {
        core_flickr::SERVICE
    }
    fn begin_auth(&self, settings: &ModuleSettings) -> Result<String, String> {
        oauth::begin_auth(&*self.api, settings, FLICKR_ID)
    }
    fn complete_auth(&self, settings: &ModuleSettings, verifier: &str) -> Result<(), String> {
        oauth::complete_auth(&*self.api, settings, FLICKR_ID, verifier)
    }
    fn connected(&self, settings: &ModuleSettings) -> Result<bool, String> {
        oauth::connected(settings)
    }
    fn ready(&self, settings: &ModuleSettings) -> Result<(), String> {
        oauth::credentials(settings, FLICKR_ID).map(|_| ())
    }
    fn render(&self, settings: &ModuleSettings, job: UploadJob) -> Result<RenderedJob, String> {
        core_flickr::render(settings, job)
    }
    fn upload(&self, settings: &ModuleSettings, rendered: &RenderedJob, r: &PublishRequest) -> Result<String, String> {
        core_flickr::upload(&*self.api, settings, rendered, &r.title, &r.description, &r.tags)
    }
    fn has_tags(&self) -> bool {
        true
    }
    fn suggest_tags(&self, _: &ModuleSettings, catalog: CatalogIdentity, photo_id: i64) -> Result<String, String> {
        core_flickr::suggest_tags(&self.app, Some(catalog), photo_id)
    }
}

#[cfg(feature = "flickr")]
struct Instance {
    host: ModuleHost,
    service: Arc<FlickrService>,
}

#[cfg(feature = "flickr")]
impl ModuleInstance for Instance {
    fn contributions(&self) -> Contributions {
        let (host, service) = (self.host.clone(), self.service.clone());
        let connect = view(move |window, cx| {
            let service: Arc<dyn PublishService> = service.clone();
            OAuthSettings::new(host.settings(), host.model(), service, window, cx)
        });
        let (host, api) = (self.host.clone(), self.service.api.clone());
        let import = view(move |_, cx| import::ImportPublishedPanel::new(&host, api.clone(), cx));
        let (host, service) = (self.host.clone(), self.service.clone());
        let publish = view(move |window, cx| {
            let state = host.model().read(cx).state().clone();
            let marker: SharedString = host.meta().marker().to_string().into();
            let service: Arc<dyn PublishService> = service.clone();
            PublishPanel::new(state, host.model().clone(), host.shell(), host.settings(), marker, service, window, cx)
        });
        Contributions {
            settings: vec![
                SettingsPanel { id: "flickr-connect".into(), view: connect },
                SettingsPanel { id: "flickr-import".into(), view: import },
            ],
            publish_targets: vec![PublishTarget { id: FLICKR_ID.into(), label: "Flickr".into(), view: publish }],
            ..Default::default()
        }
    }
}
