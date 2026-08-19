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


// ── Tag maintenance (A5) ─────────────────────────────────────────────────────
//
// Merge/split/find, over `catalog::tag_maintenance`. Two things happen here that cannot
// happen in core:
//
//  1. **The transaction is owned here**, so `dry_run` is a rollback of the real mutation
//     rather than a separate "what would happen" implementation that could drift from the
//     one that writes. The preview and the operation are the same code.
//  2. **Plugin state is repointed here.** `faces__faces.person_tag_id` and the smarttags
//     tables key on tags, but core owns no knowledge of `faces__*` or `smarttags__*` — so
//     the plugins expose their own repoint and this layer composes them into the one
//     transaction. That composition is required, not cosmetic: a face repoint that failed
//     while the merge committed would silently un-name every face on the source side, so a
//     failure takes the whole merge down with it.

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
        merge_tags_in_catalog(c, &source_ids, target_id, dry_run)
    })
    .await
}

/// The whole merge, over a real catalog: core transaction, plugin repoints, commit or roll
/// back. Split from the command so it can be tested without a Tauri `State`.
fn merge_tags_in_catalog(
    c: &crate::catalog::Catalog,
    source_ids: &[i64],
    target_id: i64,
    dry_run: bool,
) -> crate::catalog::Result<crate::catalog::tag_maintenance::TagMergeReport> {
    let tx = c.conn().unchecked_transaction()?;

    // **Plugins first, and this order is load-bearing.** `faces__faces.person_tag_id` is
    // `ON DELETE SET NULL`, so the moment core deletes a source tag the cascade nulls every
    // face that named it — repointing afterwards would find nothing to repoint and report a
    // truthful-looking zero while the faces were being un-named. Smart Tagging keys on the
    // tag *path*, which likewise only exists while the tag does.
    let plugins = repoint_plugin_tag_state(&tx, source_ids, target_id)?;

    let mut report =
        crate::catalog::tag_maintenance::merge_tags(&tx, source_ids, target_id, now_secs())?;
    plugins.fill(&mut report);

    if dry_run {
        tx.rollback()?;
    } else {
        tx.commit()?;
    }
    Ok(report)
}

/// What each plugin did with its tag-keyed state, before core removed the tags.
///
/// `None` means "not checked" — that plugin is compiled out — which is a different claim
/// from `Some(0)`, "checked, nothing there". A plugin that is present but has never run
/// reports `Some(0)`: its tables do not exist yet, so there genuinely is nothing to move.
#[derive(Default)]
struct PluginTagState {
    faces: Option<usize>,
    face_rejections: Option<usize>,
    classifiers: Option<usize>,
    suggestions: Option<usize>,
}

impl PluginTagState {
    fn fill(self, report: &mut crate::catalog::tag_maintenance::TagMergeReport) {
        report.faces_repointed = self.faces;
        report.face_rejections_repointed = self.face_rejections;
        report.classifiers_dropped = self.classifiers;
        report.suggestions_repointed = self.suggestions;
    }
}

/// Hand each plugin its own tag-keyed state to repoint, inside the merge's transaction and
/// **before** core deletes the source tags — see the ordering note in `merge_tags_in_catalog`.
///
/// A plugin failure fails the whole merge. That is the opposite of the usual "a missing
/// optional capability degrades only the cosmetic behaviour" rule, and deliberately so: a
/// half-applied merge leaves faces detached from the people they belong to, which is data
/// loss, not a missing nicety.
#[allow(unused_variables, unused_mut)]
fn repoint_plugin_tag_state(
    conn: &rusqlite::Connection,
    source_ids: &[i64],
    target_id: i64,
) -> crate::catalog::Result<PluginTagState> {
    let mut state = PluginTagState::default();

    #[cfg(feature = "faces")]
    {
        let (faces, rejections) =
            crate::plugins::faces::store::repoint_person_tags(conn, source_ids, target_id)?;
        state.faces = Some(faces);
        state.face_rejections = Some(rejections);
    }

    #[cfg(feature = "smarttags")]
    {
        // Paths, not ids — and readable only while the tags still exist.
        let dead_paths = tag_paths(conn, source_ids)?;
        let target_path = tag_paths(conn, &[target_id])?.pop().unwrap_or_default();
        let (dropped, repointed) = crate::plugins::smarttags::classifier::forget_merged_tag_paths(
            conn,
            &dead_paths,
            &target_path,
        )?;
        state.classifiers = Some(dropped);
        state.suggestions = Some(repointed);
    }

    Ok(state)
}

/// `full_path` for each id that still exists, in the order given.
#[cfg(feature = "smarttags")]
fn tag_paths(conn: &rusqlite::Connection, ids: &[i64]) -> crate::catalog::Result<Vec<String>> {
    use rusqlite::OptionalExtension;
    let mut out = Vec::new();
    for &id in ids {
        if let Some(path) = conn
            .query_row("SELECT full_path FROM tags WHERE id = ?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
        {
            out.push(path);
        }
    }
    Ok(out)
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
        split_tag_in_catalog(c, source_id, &photo_ids, &new_path, keep_source, dry_run)
    })
    .await
}

/// See [`merge_tags_in_catalog`] — same split for the same reason.
fn split_tag_in_catalog(
    c: &crate::catalog::Catalog,
    source_id: i64,
    photo_ids: &[i64],
    new_path: &str,
    keep_source: bool,
    dry_run: bool,
) -> crate::catalog::Result<crate::catalog::tag_maintenance::TagSplitReport> {
    let tx = c.conn().unchecked_transaction()?;
    let report = crate::catalog::tag_maintenance::split_tag(
        &tx,
        source_id,
        photo_ids,
        new_path,
        keep_source,
        now_secs(),
    )?;
    if dry_run {
        tx.rollback()?;
    } else {
        tx.commit()?;
    }
    Ok(report)
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
    let threshold = min_similarity.unwrap_or(0.82).clamp(0.0, 1.0);
    with_catalog_blocking(&state, move |c| {
        crate::catalog::tag_maintenance::find_similar_tags(c.conn(), threshold)
    })
    .await
}

#[cfg(test)]
mod tag_maintenance_tests {
    use super::*;
    use crate::catalog::Catalog;

    fn temp_catalog(label: &str) -> (Catalog, crate::test_support::TestTmpDir) {
        let dir = crate::test_support::TestTmpDir::new(&format!("tag-maint-{label}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("catalog.chairphoto"), &root).unwrap();
        (catalog, dir)
    }

    fn photo(c: &Catalog, id: i64) {
        c.conn()
            .execute(
                "INSERT INTO photos(id, uuid, path, mtime_ns, size, extension, created_at, updated_at)
                 VALUES(?1, ?2, ?3, 0, 0, 'nef', 0, 0)",
                rusqlite::params![id, format!("uuid-{id}"), format!("{id}.NEF")],
            )
            .unwrap();
    }

    /// A dry run must leave the catalog exactly as it found it — including the plugin state
    /// this layer repoints, which core's own rollback test cannot see.
    #[test]
    fn a_dry_run_reports_the_work_and_rolls_all_of_it_back() {
        let (c, _dir) = temp_catalog("dryrun");
        let source = c.create_tag("Bike").unwrap();
        let target = c.create_tag("Cycling").unwrap();
        photo(&c, 1);
        c.assign_tag(1, source).unwrap();

        let report = merge_tags_in_catalog(&c, &[source], target, true).unwrap();
        assert_eq!(report.photos_retagged, 1, "the preview describes real work");

        // Nothing survived it.
        assert!(c.get_tag(source).is_ok(), "the source tag is still there");
        let tags = c.get_photo_tags(1).unwrap();
        assert_eq!(tags.iter().map(|t| t.id).collect::<Vec<_>>(), vec![source]);
        let aliases: i64 = c
            .conn()
            .query_row("SELECT COUNT(*) FROM tag_aliases", [], |r| r.get(0))
            .unwrap();
        assert_eq!(aliases, 0);

        // And the same call without dry_run does commit, so the preview was not a lie.
        let report = merge_tags_in_catalog(&c, &[source], target, false).unwrap();
        assert_eq!(report.photos_retagged, 1);
        assert!(c.get_tag(source).is_err());
        assert_eq!(
            c.get_photo_tags(1).unwrap().iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![target]
        );
    }

    /// Merging two person tags is the most common merge there is, and
    /// `faces__faces.person_tag_id` is `ON DELETE SET NULL` — so without the plugin repoint
    /// this merge would silently un-name every face on the source side.
    #[cfg(feature = "faces")]
    #[test]
    fn merging_person_tags_carries_the_faces_and_their_rejections() {
        use crate::plugins::faces::store;

        let (c, _dir) = temp_catalog("faces");
        store::ensure_schema(c.conn()).unwrap();
        let source = c.create_tag("People/Anders").unwrap();
        let target = c.create_tag("People/Andreas").unwrap();
        photo(&c, 1);
        photo(&c, 2);

        let face = |photo_id: i64| {
            store::insert_face(c.conn(), photo_id, "[0,0,0.1,0.1]", "[]", 0.9, None, "detect", 0)
                .unwrap()
        };
        let (f1, f2) = (face(1), face(2));
        c.conn()
            .execute(
                "UPDATE faces__faces SET person_tag_id = ?2, state = 'confirmed' WHERE id = ?1",
                rusqlite::params![f1, source],
            )
            .unwrap();
        // f2 rejected both people; the rejection rows must not collide when merged.
        for tag in [source, target] {
            c.conn()
                .execute(
                    "INSERT INTO faces__rejections(face_id, person_tag_id, rejected_at)
                     VALUES(?1, ?2, 0)",
                    rusqlite::params![f2, tag],
                )
                .unwrap();
        }

        let report = merge_tags_in_catalog(&c, &[source], target, false).unwrap();

        assert_eq!(report.faces_repointed, Some(1));
        let person: Option<i64> = c
            .conn()
            .query_row("SELECT person_tag_id FROM faces__faces WHERE id = ?1", [f1], |r| r.get(0))
            .unwrap();
        assert_eq!(person, Some(target), "the face is still named, and named correctly");

        let rejections: Vec<i64> = {
            let mut stmt = c
                .conn()
                .prepare("SELECT person_tag_id FROM faces__rejections WHERE face_id = ?1")
                .unwrap();
            let rows = stmt.query_map([f2], |r| r.get(0)).unwrap();
            rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        assert_eq!(rejections, vec![target], "two rejections collapse to one, not a collision");
    }

    /// The faces plugin present but never run: its tables do not exist, so the honest report
    /// is `Some(0)` — "checked, nothing there" — and the merge must not fail on the missing
    /// table.
    #[cfg(feature = "faces")]
    #[test]
    fn a_merge_succeeds_before_faces_has_ever_indexed_anything() {
        let (c, _dir) = temp_catalog("faces-absent");
        let source = c.create_tag("People/Anders").unwrap();
        let target = c.create_tag("People/Andreas").unwrap();

        let report = merge_tags_in_catalog(&c, &[source], target, false).unwrap();
        assert_eq!(report.faces_repointed, Some(0));
        assert!(c.get_tag(source).is_err(), "the merge still happened");
    }

    /// Smart Tagging keys on the tag **path**, so a merge strands a classifier that can never
    /// fire again and a suggestion that can never be accepted.
    #[cfg(feature = "smarttags")]
    #[test]
    fn merging_forgets_the_dead_tag_path_in_smart_tagging() {
        use crate::plugins::smarttags::{classifier, suggest};

        let (c, _dir) = temp_catalog("smarttags");
        classifier::ensure_schema(c.conn()).unwrap();
        suggest::ensure_suggestions_schema(c.conn()).unwrap();
        let source = c.create_tag("Bike").unwrap();
        let target = c.create_tag("Cycling").unwrap();
        photo(&c, 1);

        c.conn()
            .execute(
                "INSERT INTO smarttags__classifiers(tag_path, weights, bias, sample_count, trained_at)
                 VALUES('Bike', x'00', 0.0, 10, 0)",
                [],
            )
            .unwrap();
        c.conn()
            .execute(
                "INSERT INTO smarttags__suggestions(photo_id, path, created_at) VALUES(1, 'Bike', 0)",
                [],
            )
            .unwrap();

        let report = merge_tags_in_catalog(&c, &[source], target, false).unwrap();

        assert_eq!(report.classifiers_dropped, Some(1));
        assert_eq!(report.suggestions_repointed, Some(1));
        let paths: Vec<String> = {
            let mut stmt = c
                .conn()
                .prepare("SELECT path FROM smarttags__suggestions")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        assert_eq!(paths, vec!["Cycling".to_string()], "the suggestion follows the merge");
    }

    /// A split's dry run rolls back too, and a tag it would have created is not left behind.
    #[test]
    fn a_split_dry_run_creates_no_tag() {
        let (c, _dir) = temp_catalog("split-dry");
        let source = c.create_tag("Concert").unwrap();
        photo(&c, 1);
        c.assign_tag(1, source).unwrap();

        let report = split_tag_in_catalog(&c, source, &[1], "Venue", false, true).unwrap();
        assert!(report.created_new_tag);
        assert_eq!(report.photos_moved, 1);

        let venue: Option<i64> = c
            .conn()
            .query_row(
                "SELECT id FROM tags WHERE full_path_norm = 'venue'",
                [],
                |r| r.get(0),
            )
            .ok();
        assert!(venue.is_none(), "the previewed tag was rolled back with everything else");
        assert_eq!(
            c.get_photo_tags(1).unwrap().iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![source]
        );
    }
}
