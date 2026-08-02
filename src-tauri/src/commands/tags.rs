//! Tag commands — the controlled vocabulary: the hierarchical tag tree, assignment,
//! tag groups, per-tag terms/synonyms (the thesaurus layer), and the export flags
//! that decide what leaves the catalog. See `docs/taxonomy.md`.

use super::*;
use tauri::State;

#[tauri::command]
pub async fn list_tags(state: State<'_, AppState>) -> Result<Vec<TagWithCount>, String> {
    with_catalog_blocking(&state, move |c| c.list_tags_with_counts()).await
}

#[tauri::command]
pub fn create_tag(state: State<'_, AppState>, path: String) -> Result<i64, String> {
    with_catalog(&state, |c| c.create_tag(&path))
}

#[tauri::command]
pub fn assign_tag(
    state: State<'_, AppState>,
    photo_id: i64,
    tag_id: i64,
) -> Result<(), String> {
    with_catalog(&state, |c| c.assign_tag(photo_id, tag_id))
}

#[tauri::command]
pub fn remove_tag(
    state: State<'_, AppState>,
    photo_id: i64,
    tag_id: i64,
) -> Result<(), String> {
    with_catalog(&state, |c| c.remove_tag(photo_id, tag_id))
}

#[tauri::command]
pub async fn get_photo_tags(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<Vec<Tag>, String> {
    with_catalog_blocking(&state, move |c| c.get_photo_tags(photo_id)).await
}

/// Rename a tag's canonical name (rewrites its path and all descendants' paths).
#[tauri::command]
pub fn rename_tag(
    state: State<'_, AppState>,
    tag_id: i64,
    new_name: String,
) -> Result<(), String> {
    with_catalog(&state, |c| c.rename_tag(tag_id, &new_name))
}

/// Delete a tag and its whole subtree (assignments and terms cascade).
#[tauri::command]
pub fn delete_tag(state: State<'_, AppState>, tag_id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.delete_tag(tag_id))
}

/// Re-apply all auto-tags (e.g. monochrome) across the catalog. Useful to populate
/// auto-tags for photos imported before the rule existed, without a rescan.
#[tauri::command]
pub fn apply_auto_tags(state: State<'_, AppState>) -> Result<(), String> {
    with_catalog(&state, |c| c.apply_auto_tags())
}

// --- tag groups (fast tagging) -------------------------------------------

#[tauri::command]
pub fn list_tag_groups(state: State<'_, AppState>) -> Result<Vec<TagGroup>, String> {
    with_catalog(&state, |c| c.list_tag_groups())
}

#[tauri::command]
pub fn create_tag_group(state: State<'_, AppState>, name: String) -> Result<i64, String> {
    with_catalog(&state, |c| c.create_tag_group(&name))
}

#[tauri::command]
pub fn rename_tag_group(
    state: State<'_, AppState>,
    group_id: i64,
    name: String,
) -> Result<(), String> {
    with_catalog(&state, |c| c.rename_tag_group(group_id, &name))
}

#[tauri::command]
pub fn delete_tag_group(state: State<'_, AppState>, group_id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.delete_tag_group(group_id))
}

#[tauri::command]
pub fn get_group_members(state: State<'_, AppState>, group_id: i64) -> Result<Vec<Tag>, String> {
    with_catalog(&state, |c| c.group_members(group_id))
}

/// The tags most recently applied by hand, newest first. Backs the virtual
/// "Recently used" quick-tag group.
#[tauri::command]
pub fn recently_used_tags(state: State<'_, AppState>, limit: usize) -> Result<Vec<Tag>, String> {
    with_catalog(&state, |c| c.recently_used_tags(limit))
}

/// Add a tag (by path, created if new) to a group. Returns the tag id.
#[tauri::command]
pub fn add_tag_to_group(
    state: State<'_, AppState>,
    group_id: i64,
    path: String,
) -> Result<i64, String> {
    with_catalog(&state, |c| {
        let tag_id = match c.find_tag_id_by_path(&path)? {
            Some(id) => id,
            None => c.create_tag(&path)?,
        };
        c.add_tag_to_group(group_id, tag_id)?;
        Ok(tag_id)
    })
}

#[tauri::command]
pub fn remove_tag_from_group(
    state: State<'_, AppState>,
    group_id: i64,
    tag_id: i64,
) -> Result<(), String> {
    with_catalog(&state, |c| c.remove_tag_from_group(group_id, tag_id))
}

// --- edit record (non-destructive editing contract) -----------------------

/// Suggest existing tags from photos taken near this one in time (default ±120s).
/// Non-AI heuristic; session tags (Events/Places) rank highest by frequency.
#[tauri::command]
pub fn suggest_tags_by_time(
    state: State<'_, AppState>,
    photo_id: i64,
    window_seconds: Option<i64>,
) -> Result<Vec<TagWithCount>, String> {
    let window = window_seconds.unwrap_or(120);
    with_catalog(&state, |c| c.suggest_tags_by_time(photo_id, window))
}

/// Reparent a tag (drag-and-drop). `newParentId = null` moves it to the top level.
#[tauri::command]
pub fn move_tag(
    state: State<'_, AppState>,
    tag_id: i64,
    new_parent_id: Option<i64>,
) -> Result<(), String> {
    with_catalog(&state, |c| c.move_tag(tag_id, new_parent_id))
}

/// Set a tag's description (internal metadata; not exported to image sidecars).
#[tauri::command]
pub fn set_tag_description(
    state: State<'_, AppState>,
    tag_id: i64,
    description: String,
) -> Result<(), String> {
    with_catalog(&state, |c| c.set_tag_description(tag_id, &description))
}

/// Whether a tag is emitted on export (false = organizational; descendants still export).
#[tauri::command]
pub fn get_tag_exportable(state: State<'_, AppState>, tag_id: i64) -> Result<bool, String> {
    with_catalog(&state, |c| c.tag_exportable(tag_id))
}

/// Library-wide tidy: remove redundant ancestor tags (a parent a child already implies)
/// from every photo. Returns how many assignments were removed.
#[tauri::command]
pub fn tidy_redundant_tags(state: State<'_, AppState>) -> Result<usize, String> {
    with_catalog(&state, |c| c.tidy_redundant_tags())
}

/// Mark a tag as exported or organizational (not written as a keyword on export).
#[tauri::command]
pub fn set_tag_exportable(
    state: State<'_, AppState>,
    tag_id: i64,
    exportable: bool,
) -> Result<(), String> {
    with_catalog(&state, |c| c.set_tag_exportable(tag_id, exportable))
}

/// Whether a tag is private (withheld from external/cloud AI; local AI still sees it).
#[tauri::command]
pub fn get_tag_private(state: State<'_, AppState>, tag_id: i64) -> Result<bool, String> {
    with_catalog(&state, |c| c.tag_private(tag_id))
}

/// Mark a tag private or not (see [`get_tag_private`]). With `recursive`, applies to the
/// tag and every descendant. Returns the number of tags changed.
#[tauri::command]
pub fn set_tag_private(
    state: State<'_, AppState>,
    tag_id: i64,
    private: bool,
    recursive: bool,
) -> Result<usize, String> {
    with_catalog(&state, |c| c.set_tag_private(tag_id, private, recursive))
}

// --- taxonomy: tag terms (translations & synonyms) ------------------------

/// All terms (translations + synonyms) for a tag.
#[tauri::command]
pub fn list_tag_terms(state: State<'_, AppState>, tag_id: i64) -> Result<Vec<TagTerm>, String> {
    with_catalog(&state, |c| c.list_terms(tag_id))
}

/// Add a term to a tag. `isPrimary` makes it the canonical name for its language
/// (a translation); otherwise it's a synonym. `language` may be empty for neutral.
#[tauri::command]
pub fn add_tag_term(
    state: State<'_, AppState>,
    tag_id: i64,
    text: String,
    language: Option<String>,
    is_primary: bool,
    export: bool,
) -> Result<i64, String> {
    with_catalog(&state, |c| {
        c.add_term(tag_id, &text, language.as_deref(), is_primary, export)
    })
}

#[tauri::command]
pub fn update_tag_term(
    state: State<'_, AppState>,
    term_id: i64,
    text: String,
    language: Option<String>,
    is_primary: bool,
    export: bool,
) -> Result<(), String> {
    with_catalog(&state, |c| {
        c.update_term(term_id, &text, language.as_deref(), is_primary, export)
    })
}

#[tauri::command]
pub fn set_term_export(
    state: State<'_, AppState>,
    term_id: i64,
    export: bool,
) -> Result<(), String> {
    with_catalog(&state, |c| c.set_term_export(term_id, export))
}

#[tauri::command]
pub fn remove_tag_term(state: State<'_, AppState>, term_id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.remove_term(term_id))
}

/// Distinct languages used across the taxonomy (for UI pickers).
#[tauri::command]
pub fn list_languages(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    with_catalog(&state, |c| c.list_languages())
}

/// Preview the labels that would be exported for a tag given selected languages —
/// the transition-layer building block, surfaced for the UI.
#[tauri::command]
pub fn tag_export_preview(
    state: State<'_, AppState>,
    tag_id: i64,
    languages: Vec<String>,
) -> Result<Vec<String>, String> {
    with_catalog(&state, |c| c.export_labels(tag_id, &languages))
}

