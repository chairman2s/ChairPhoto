//! The LocalSend and Snapchat modules (`localsend.tsx`, `snapchat.tsx`, ticket #123): each
//! contributes one publish target that renders the shared [`send::SendToDevicePanel`].
//! Compiled with the `localsend` feature, like their backend.
//!
//! - **LocalSend** — "Device (LocalSend)": a transfer, not a publication; it records nothing.
//! - **Snapchat** — "Snapchat": the same panel, plus a non-blocking 9:16 pre-flight warning
//!   and, after a successful send, a `snapchat` publication for every photo that reached the
//!   device (the module's marker). It requires LocalSend.
//!
//! The send is core's job (`chairphoto_core::app::localsend`): claimed and run on a worker,
//! bound to the catalog the selection was read from, cancellable (Cancel, a newer send or a
//! catalog switch). `localsend:progress` only moves the counter, and only for this panel's
//! job; the job's return value is the result. [`LocalSendBackend`] is the seam tests fake the
//! network at — they never discover or send on a real network.
//!
//! **Privacy.** Enabling the module is the feature-specific opt-in; discovery (multicast and
//! the subnet sweep, docs/localsend.md § Privacy posture) runs only once the target's form is
//! shown — the Publish dialog builds a form when its chip is chosen — or on Refresh; photos
//! leave only on Send, to the device the user picked.

pub mod send;

#[cfg(test)]
mod tests;

use super::{view, Contributions, Module, ModuleHost, ModuleInstance, ModuleMeta, PublishTarget};
use chairphoto_core::app::localsend::{Device, LocalSendJob, SendOutcome};
use gpui_kit::App;
use std::sync::Arc;

pub const LOCALSEND_ID: &str = "localsend";
pub const SNAPCHAT_ID: &str = "snapchat";

/// How long a discovery pass listens after its announcement burst (the Tauri default).
pub const DISCOVERY_MS: u64 = 2500;

/// Where the network comes from: discovery and running a claimed send. Both are blocking and
/// run on a worker.
#[derive(Clone)]
pub struct LocalSendBackend {
    pub discover: Arc<dyn Fn() -> Result<Vec<Device>, String> + Send + Sync>,
    pub send: Arc<dyn Fn(LocalSendJob, &Device, Option<&str>) -> Result<SendOutcome, String> + Send + Sync>,
}

impl Default for LocalSendBackend {
    fn default() -> Self {
        LocalSendBackend {
            discover: Arc::new(|| chairphoto_core::localsend::discover_blocking(DISCOVERY_MS)),
            send: Arc::new(|job, device, pin| job.run(device, pin)),
        }
    }
}

/// What a panel does besides sending.
#[derive(Clone, Debug, PartialEq)]
pub enum SendMode {
    /// LocalSend: pure transfer.
    Transfer,
    /// Snapchat: the 9:16 pre-flight, and a publication under `marker` for each photo sent.
    Snapchat { marker: String },
}

#[derive(Default)]
pub struct LocalSendModule {
    pub backend: LocalSendBackend,
}

#[derive(Default)]
pub struct SnapchatModule {
    pub backend: LocalSendBackend,
}

struct Instance {
    host: ModuleHost,
    backend: LocalSendBackend,
    target: (&'static str, &'static str),
    mode: SendMode,
}

impl Module for LocalSendModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(LOCALSEND_ID, "LocalSend")
            .description(
                "Send a photo to a device on the local network (phone, laptop) via LocalSend's HTTP protocol. \
                 Transfer only — records no publication.",
            )
            .backend_feature("localsend")
    }

    fn load(&self, host: ModuleHost, _: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        Ok(Box::new(Instance {
            host,
            backend: self.backend.clone(),
            target: (LOCALSEND_ID, "Device (LocalSend)"),
            mode: SendMode::Transfer,
        }))
    }
}

impl Module for SnapchatModule {
    fn meta(&self) -> ModuleMeta {
        ModuleMeta::new(SNAPCHAT_ID, "Snapchat")
            .description(
                "Send a photo to your phone via LocalSend, then post the Snapchat story by hand — and record it as \
                 published to Snapchat. Warns when the crop isn't vertical 9:16.",
            )
            .backend_feature("localsend")
            .publication_marker(SNAPCHAT_ID)
            .requires(LOCALSEND_ID)
    }

    fn load(&self, host: ModuleHost, _: &mut App) -> Result<Box<dyn ModuleInstance>, String> {
        let marker = host.meta().marker().to_string();
        Ok(Box::new(Instance {
            host,
            backend: self.backend.clone(),
            target: (SNAPCHAT_ID, "Snapchat"),
            mode: SendMode::Snapchat { marker },
        }))
    }
}

impl ModuleInstance for Instance {
    fn contributions(&self) -> Contributions {
        let (host, backend, mode) = (self.host.clone(), self.backend.clone(), self.mode.clone());
        let form = view(move |window, cx| {
            let dialog = super::dialog::DialogHost::new(host.model(), host.shell(), None, cx);
            send::SendToDevicePanel::new(dialog, backend.clone(), mode.clone(), window, cx)
        });
        Contributions {
            publish_targets: vec![PublishTarget { id: self.target.0.into(), label: self.target.1.into(), view: form }],
            ..Default::default()
        }
    }
}
