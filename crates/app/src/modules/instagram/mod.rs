//! The Instagram module (`instagram.tsx`, ticket #124): post the selected version to Instagram
//! by driving Chrome — supervised by default — as one target in the Publish dialog. There is
//! no API key: signing in is the Chrome login, in ChairPhoto's own Chrome profile.
//!
//! The publish target ([`InstagramPanel`]): the version (default: the active one), the caption
//! (title + `#hashtags`, prefilled until edited), "Publish automatically" (off by default: the
//! post is composed and left before Share), Post. The outcomes (docs/instagram.md):
//!
//! - **needs login** — Chrome is open at the login page; log in and Post again.
//! - **awaiting review** — composed and left on screen. ChairPhoto cannot see a click it did
//!   not make, so it asks "Did you click Share?": "Yes, I posted it" records the publication
//!   (the photo and version that were posted, in the catalog they came from), "No, skip"
//!   records nothing.
//! - **posted** — shared and confirmed: the publication is recorded at once.
//!
//! The post is core's publish job (`chairphoto_core::app::{uploads, instagram}`): claimed and
//! rendered (1080 px wide) on workers, bound to the catalog the photo came from; Cancel stops it
//! until Chrome has the render, and from then on the browser window is the cancel. The render
//! is kept while the composer may still read it. Chrome is an [`InstagramDriver`]: tests fake
//! it and never launch a browser.
//!
//! **Privacy.** Enabling the module is the opt-in; a photo goes to Instagram only on Post, and
//! by default only once the user clicks Share in the browser.

#[cfg(feature = "instagram")]
pub mod panel;
#[cfg(all(test, feature = "instagram"))]
mod tests;

use super::{Module, ModuleHost, ModuleInstance, ModuleMeta};
#[cfg(feature = "instagram")]
use super::{view, Contributions, PublishTarget};
#[cfg(feature = "instagram")]
use chairphoto_core::app::instagram::{ChromeDriver, InstagramDriver};
use gpui_kit::App;
#[cfg(feature = "instagram")]
pub use panel::InstagramPanel;
#[cfg(feature = "instagram")]
use std::sync::Arc;

pub const INSTAGRAM_ID: &str = "instagram";

/// The Instagram module. `driver` is Chrome unless a test hands in a fake.
pub struct InstagramModule {
    #[cfg(feature = "instagram")]
    pub driver: Arc<dyn InstagramDriver>,
}

impl Default for InstagramModule {
    fn default() -> Self {
        InstagramModule {
            #[cfg(feature = "instagram")]
            driver: Arc::new(ChromeDriver),
        }
    }
}

impl Module for InstagramModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(INSTAGRAM_ID, "Instagram")
            .description("Post a photo to Instagram by driving Chrome; records which version was posted. Supervised by default.")
            .backend_feature("instagram")
            .publication_marker(INSTAGRAM_ID)
    }

    #[cfg(feature = "instagram")]
    fn load(&self, host: ModuleHost, _: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        Ok(Box::new(Instance { host, driver: self.driver.clone() }))
    }

    #[cfg(not(feature = "instagram"))]
    fn load(&self, _: ModuleHost, _: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        Err("The Instagram backend is not included in this build".into())
    }
}

#[cfg(feature = "instagram")]
struct Instance {
    host: ModuleHost,
    driver: Arc<dyn InstagramDriver>,
}

#[cfg(feature = "instagram")]
impl ModuleInstance for Instance {
    fn contributions(&self) -> Contributions {
        let (host, driver) = (self.host.clone(), self.driver.clone());
        let form = view(move |window, cx| {
            let dialog = super::dialog::DialogHost::new(host.model(), host.shell(), None, cx);
            InstagramPanel::new(dialog, driver.clone(), host.meta().marker().to_string(), window, cx)
        });
        Contributions {
            publish_targets: vec![PublishTarget { id: INSTAGRAM_ID.into(), label: "Instagram".into(), view: form }],
            ..Default::default()
        }
    }
}
