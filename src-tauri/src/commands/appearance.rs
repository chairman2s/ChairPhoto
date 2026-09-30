//! Appearance: the "Follow Omarchy" system-theme bridge.
//!
//! The parser and watcher live in `crate::appearance` (see docs/appearance.md); this
//! module is only the command surface. No `AppState`: the Omarchy theme is per-machine
//! state, unrelated to which catalog is open.

use crate::appearance::SystemThemeResult;

/// Read the current Omarchy theme (palette + name). Absent or broken Omarchy state is a
/// normal answer (`available: false`), never an `Err` — the error path is reserved for the
/// blocking worker itself failing, which the frontend treats as unavailable too.
#[tauri::command]
pub async fn get_system_theme() -> Result<SystemThemeResult, String> {
    // Two small reads, but they still touch disk — off the UI thread like every other
    // disk-touching command (the `with_catalog_blocking` rule).
    crate::app::spawn_blocking(crate::appearance::read_current_theme)
        .await
        .map_err(|e| e.to_string())
}
