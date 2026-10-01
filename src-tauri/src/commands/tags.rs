//! Tag commands — the controlled vocabulary: the hierarchical tag tree, assignment,
//! tag groups, per-tag terms/synonyms (the thesaurus layer), and the export flags
//! that decide what leaves the catalog. See `docs/taxonomy.md`.

use super::*;
use tauri::State;

#[tauri::command]
pub async fn list_tags(state: State<'_, AppState>) -> Result<Vec<TagWithCount>, String> {
    with_catalog_blocking(&state, move |c| c.list_tags_with_counts()).await
}

#[tauri::command(async)]
pub fn create_tag(state: State<'_, AppState>, path: String) -> Result<i64, String> {
    with_catalog(&state, |c| c.create_tag(&path))
}

#[tauri::command(async)]
pub fn assign_tag(
    state: State<'_, AppState>,
    photo_id: i64,
    tag_id: i64,
) -> Result<(), String> {
    with_catalog(&state, |c| c.assign_tag(photo_id, tag_id))
}

#[tauri::command(async)]
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
#[tauri::command(async)]
pub fn rename_tag(
    state: State<'_, AppState>,
    tag_id: i64,
    new_name: String,
) -> Result<(), String> {
    with_catalog(&state, |c| c.rename_tag(tag_id, &new_name))
}

/// Delete a tag and its whole subtree (assignments and terms cascade).
#[tauri::command(async)]
pub fn delete_tag(state: State<'_, AppState>, tag_id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.delete_tag(tag_id))
}

/// Re-apply all auto-tags (e.g. monochrome) across the catalog. Useful to populate
/// auto-tags for photos imported before the rule existed, without a rescan.
#[tauri::command(async)]
pub fn apply_auto_tags(state: State<'_, AppState>) -> Result<(), String> {
    with_catalog(&state, |c| c.apply_auto_tags())
}

// --- tag groups (fast tagging) -------------------------------------------

#[tauri::command(async)]
pub fn list_tag_groups(state: State<'_, AppState>) -> Result<Vec<TagGroup>, String> {
    with_catalog(&state, |c| c.list_tag_groups())
}

#[tauri::command(async)]
pub fn create_tag_group(state: State<'_, AppState>, name: String) -> Result<i64, String> {
    with_catalog(&state, |c| c.create_tag_group(&name))
}

#[tauri::command(async)]
pub fn rename_tag_group(
    state: State<'_, AppState>,
    group_id: i64,
    name: String,
) -> Result<(), String> {
    with_catalog(&state, |c| c.rename_tag_group(group_id, &name))
}

#[tauri::command(async)]
pub fn delete_tag_group(state: State<'_, AppState>, group_id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.delete_tag_group(group_id))
}

#[tauri::command(async)]
pub fn get_group_members(state: State<'_, AppState>, group_id: i64) -> Result<Vec<Tag>, String> {
    with_catalog(&state, |c| c.group_members(group_id))
}

/// The tags most recently applied by hand, newest first. Backs the virtual
/// "Recently used" quick-tag group.
#[tauri::command(async)]
pub fn recently_used_tags(state: State<'_, AppState>, limit: usize) -> Result<Vec<Tag>, String> {
    with_catalog(&state, |c| c.recently_used_tags(limit))
}

/// Add a tag (by path, created if new) to a group. Returns the tag id.
#[tauri::command(async)]
pub fn add_tag_to_group(
    state: State<'_, AppState>,
    group_id: i64,
    path: String,
) -> Result<i64, String> {
    with_catalog(&state, |c| crate::app::tags::add_tag_to_group(c, group_id, &path))
}

#[tauri::command(async)]
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
#[tauri::command(async)]
pub fn suggest_tags_by_time(
    state: State<'_, AppState>,
    photo_id: i64,
    window_seconds: Option<i64>,
) -> Result<Vec<TagWithCount>, String> {
    let window = window_seconds.unwrap_or(120);
    with_catalog(&state, |c| c.suggest_tags_by_time(photo_id, window))
}

/// Reparent a tag (drag-and-drop). `newParentId = null` moves it to the top level.
#[tauri::command(async)]
pub fn move_tag(
    state: State<'_, AppState>,
    tag_id: i64,
    new_parent_id: Option<i64>,
) -> Result<(), String> {
    with_catalog(&state, |c| c.move_tag(tag_id, new_parent_id))
}

/// Set a tag's description (internal metadata; not exported to image sidecars).
#[tauri::command(async)]
pub fn set_tag_description(
    state: State<'_, AppState>,
    tag_id: i64,
    description: String,
) -> Result<(), String> {
    with_catalog(&state, |c| c.set_tag_description(tag_id, &description))
}

/// Whether a tag is emitted on export (false = organizational; descendants still export).
#[tauri::command(async)]
pub fn get_tag_exportable(state: State<'_, AppState>, tag_id: i64) -> Result<bool, String> {
    with_catalog(&state, |c| c.tag_exportable(tag_id))
}

/// Library-wide tidy: remove redundant ancestor tags (a parent a child already implies)
/// from every photo. Returns how many assignments were removed.
#[tauri::command(async)]
pub fn tidy_redundant_tags(state: State<'_, AppState>) -> Result<usize, String> {
    with_catalog(&state, |c| c.tidy_redundant_tags())
}

/// Mark a tag as exported or organizational (not written as a keyword on export).
#[tauri::command(async)]
pub fn set_tag_exportable(
    state: State<'_, AppState>,
    tag_id: i64,
    exportable: bool,
) -> Result<(), String> {
    with_catalog(&state, |c| c.set_tag_exportable(tag_id, exportable))
}

/// Whether a tag is private (withheld from external/cloud AI; local AI still sees it).
#[tauri::command(async)]
pub fn get_tag_private(state: State<'_, AppState>, tag_id: i64) -> Result<bool, String> {
    with_catalog(&state, |c| c.tag_private(tag_id))
}

/// Mark a tag private or not (see [`get_tag_private`]). With `recursive`, applies to the
/// tag and every descendant. Returns the number of tags changed.
#[tauri::command(async)]
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
#[tauri::command(async)]
pub fn list_tag_terms(state: State<'_, AppState>, tag_id: i64) -> Result<Vec<TagTerm>, String> {
    with_catalog(&state, |c| c.list_terms(tag_id))
}

/// Add a term to a tag. `isPrimary` makes it the canonical name for its language
/// (a translation); otherwise it's a synonym. `language` may be empty for neutral.
#[tauri::command(async)]
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

#[tauri::command(async)]
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

#[tauri::command(async)]
pub fn set_term_export(
    state: State<'_, AppState>,
    term_id: i64,
    export: bool,
) -> Result<(), String> {
    with_catalog(&state, |c| c.set_term_export(term_id, export))
}

#[tauri::command(async)]
pub fn remove_tag_term(state: State<'_, AppState>, term_id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.remove_term(term_id))
}

/// Distinct languages used across the taxonomy (for UI pickers).
#[tauri::command(async)]
pub fn list_languages(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    with_catalog(&state, |c| c.list_languages())
}

/// Preview the labels that would be exported for a tag given selected languages —
/// the transition-layer building block, surfaced for the UI.
#[tauri::command(async)]
pub fn tag_export_preview(
    state: State<'_, AppState>,
    tag_id: i64,
    languages: Vec<String>,
) -> Result<Vec<String>, String> {
    with_catalog(&state, |c| c.export_labels(tag_id, &languages))
}


// ── Tag maintenance (A5) ─────────────────────────────────────────────────────
//
// Merge/split/find, over `catalog::tag_maintenance`. The merge and split bodies — the
// transaction that makes `dry_run` a rollback of the real mutation, and the plugin repoints
// composed into it — live in core (`app::tags`), shared with the GPUI app.

/// Merge `source_ids` into `target_id`. With `dry_run`, everything runs and is then rolled
/// back, so the returned report describes exactly what a real run would do.
#[tauri::command]
pub async fn merge_tags(
    state: State<'_, AppState>,
    source_ids: Vec<i64>,
    target_id: i64,
    dry_run: bool,
) -> Result<crate::catalog::tag_maintenance::TagMergeReport, String> {
    with_catalog_blocking(&state, move |c| {
        crate::app::tags::merge_tags(c, &source_ids, target_id, dry_run)
    })
    .await
}

/// Split a tag: give `photo_ids` the tag at `new_path`, and unless `keep_source` take the
/// source tag off exactly those photos. `dry_run` previews, as for a merge.
#[tauri::command]
pub async fn split_tag(
    state: State<'_, AppState>,
    source_id: i64,
    photo_ids: Vec<i64>,
    new_path: String,
    keep_source: bool,
    dry_run: bool,
) -> Result<crate::catalog::tag_maintenance::TagSplitReport, String> {
    with_catalog_blocking(&state, move |c| {
        crate::app::tags::split_tag(c, source_id, &photo_ids, &new_path, keep_source, dry_run)
    })
    .await
}

/// Tags holding no photos anywhere in their subtree — the cleanup list.
#[tauri::command]
pub async fn find_orphan_tags(
    state: State<'_, AppState>,
) -> Result<Vec<crate::catalog::tag_maintenance::OrphanTag>, String> {
    with_catalog_blocking(&state, move |c| {
        crate::catalog::tag_maintenance::find_orphan_tags(c.conn())
    })
    .await
}

/// Candidate duplicate tags, most-alike first. A suggestion for a human, never an action:
/// "Cycling" and "Cycles" may be a typo or two real concepts.
#[tauri::command]
pub async fn find_similar_tags(
    state: State<'_, AppState>,
    min_similarity: Option<f64>,
) -> Result<Vec<crate::catalog::tag_maintenance::SimilarTagPair>, String> {
    let threshold = min_similarity
        .unwrap_or(crate::catalog::tag_maintenance::DEFAULT_MIN_SIMILARITY)
        .clamp(0.0, 1.0);
    with_catalog_blocking(&state, move |c| {
        crate::catalog::tag_maintenance::find_similar_tags(c.conn(), threshold)
    })
    .await
}
