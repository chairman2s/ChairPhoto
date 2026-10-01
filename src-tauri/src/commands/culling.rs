//! "Why is this flagged" — the derivation behind a photo's culling badges (C6).
//!
//! The derivation lives in the core (`crate::photo_signals`, shared with the GPUI app); this
//! is its command, plus the C3 auto-stack proposal commands.

use super::{with_catalog_blocking, AppState};
use crate::photo_signals::PhotoSignals;
use crate::stack_proposals::{StackApplied, StackProposals};
use tauri::State;

/// Explain every culling signal on one photo (C6); see `photo_signals::explain_photo_signals`.
#[tauri::command]
pub async fn explain_photo_signals(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<PhotoSignals, String> {
    with_catalog_blocking(&state, move |c| crate::photo_signals::explain_photo_signals(c, photo_id)).await
}

// ── C3 — auto-stack proposals ────────────────────────────────────────────────
//
// The proposals and their acceptance live in the core (`crate::stack_proposals`), shared
// with the GPUI app; these are the commands over them.

/// Propose stacks over `photo_ids` (C3) — typically the selection, or the whole view.
/// Read-only; see `stack_proposals::propose_stacks`.
#[tauri::command]
pub async fn propose_stacks(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
) -> Result<StackProposals, String> {
    with_catalog_blocking(&state, move |c| crate::stack_proposals::propose_stacks(c, &photo_ids)).await
}

/// Stack `member_ids` under `keeper_id` (C3), accepting one proposal; see
/// `stack_proposals::apply_stack_proposal`.
#[tauri::command]
pub async fn apply_stack_proposal(
    state: State<'_, AppState>,
    keeper_id: i64,
    member_ids: Vec<i64>,
) -> Result<StackApplied, String> {
    with_catalog_blocking(&state, move |c| {
        crate::stack_proposals::apply_stack_proposal(c, keeper_id, &member_ids)
    })
    .await
}
