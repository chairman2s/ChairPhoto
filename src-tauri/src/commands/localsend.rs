//! LocalSend commands — send a photo to a device on the LAN over LocalSend's
//! documented v2 HTTP protocol (UDP multicast discovery + prepare-upload/upload).
//!
//! Send-only. Gated on the `localsend` Cargo feature; see `docs/localsend.md`. The send is
//! the core's job (`crate::app::localsend`), shared with the GPUI app; these are thin
//! wrappers.

use super::*;

/// Discover LocalSend devices on the LAN. `timeout_ms` (default 2500ms) is the reply window
/// `crate::localsend::discover` keeps open *after* the last of its burst of 3 announcements,
/// not the call's total duration — see that function's doc comment for why a single
/// announcement plus a short window is unreliable on wifi. Worst case for one Refresh is
/// therefore ~5s (2.5s announce-burst span + the 2.5s default reply window), not ~2.5s. A
/// blocked multicast just yields an empty list (the UI's manual-IP field is the fallback).
#[cfg(feature = "localsend")]
#[tauri::command]
pub async fn localsend_discover(
    timeout_ms: Option<u64>,
) -> Result<Vec<crate::localsend::Device>, String> {
    let timeout = timeout_ms.unwrap_or(2500);
    crate::localsend::discover(timeout).await
}

/// What a send produced: how many files reached the device and how many failed (original
/// offline, render failed). A network/handshake failure rejects the whole call instead.
#[cfg(feature = "localsend")]
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendResult {
    pub sent: usize,
    pub failed: usize,
}

/// Send the selected photos (the chosen version) to a LocalSend `device`: claim the core's
/// send job against the open catalog, then render and upload on a blocking worker
/// (`crate::app::localsend`). `pin` is forwarded for PIN-protected receivers. Streams
/// `localsend:progress`. Records nothing — it's a transfer (Snapchat layers
/// `recordPublication` on top in the UI).
#[cfg(feature = "localsend")]
#[tauri::command]
pub async fn localsend_send(
    state: tauri::State<'_, AppState>,
    photo_ids: Vec<i64>,
    version_id: Option<i64>,
    device: crate::localsend::Device,
    pin: Option<String>,
) -> Result<SendResult, String> {
    let state = state.inner().clone();
    crate::app::spawn_blocking(move || {
        let job = crate::app::localsend::claim_send(&state, None, &photo_ids, version_id)?;
        let outcome = job.run(&device, pin.as_deref())?;
        Ok(SendResult { sent: outcome.sent.len(), failed: outcome.failed })
    })
    .await
    .map_err(|e| e.to_string())?
}
