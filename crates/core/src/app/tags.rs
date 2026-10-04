//! Tag maintenance over a real catalog — the bodies the GPUI tag panel's Merge and Split
//! dialogs and its tag-groups manager run (`docs/taxonomy.md` § Tag maintenance).
//!
//! Two things happen here that cannot happen in `catalog::tag_maintenance`:
//!
//!  1. **The transaction is owned here**, so `dry_run` is a rollback of the real mutation
//!     rather than a separate "what would happen" implementation that could drift from the
//!     one that writes. The preview and the operation are the same code.
//!  2. **Plugin state is repointed here.** `faces__faces.person_tag_id` and the smarttags
//!     tables key on tags, but the catalog owns no knowledge of `faces__*` or `smarttags__*`
//!     — so the plugins expose their own repoint and this layer composes them into the one
//!     transaction. That composition is required, not cosmetic: a face repoint that failed
//!     while the merge committed would silently un-name every face on the source side, so a
//!     failure takes the whole merge down with it.

use super::now_secs;
use crate::catalog::tag_maintenance::{self, TagMergeReport, TagSplitReport};
use crate::catalog::{Catalog, Result};

/// Merge `source_ids` into `target_id`, over a real catalog: core transaction, plugin
/// repoints, commit or roll back. With `dry_run`, everything runs and is then rolled back, so
/// the returned report describes exactly what a real run would do. **Blocking** (one SQLite
/// transaction): call it on a worker.
pub fn merge_tags(
    c: &Catalog,
    source_ids: &[i64],
    target_id: i64,
    dry_run: bool,
) -> Result<TagMergeReport> {
    let tx = c.conn().unchecked_transaction()?;

    // **Plugins first, and this order is load-bearing.** `faces__faces.person_tag_id` is
    // `ON DELETE SET NULL`, so the moment core deletes a source tag the cascade nulls every
    // face that named it — repointing afterwards would find nothing to repoint and report a
    // truthful-looking zero while the faces were being un-named. Smart Tagging keys on the
    // tag *path*, which likewise only exists while the tag does.
    let plugins = repoint_plugin_tag_state(&tx, source_ids, target_id)?;

    let mut report =
        tag_maintenance::merge_tags(&tx, source_ids, target_id, now_secs())?;
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
    fn fill(self, report: &mut TagMergeReport) {
        report.faces_repointed = self.faces;
        report.face_rejections_repointed = self.face_rejections;
        report.classifiers_dropped = self.classifiers;
        report.suggestions_repointed = self.suggestions;
    }
}

/// Hand each plugin its own tag-keyed state to repoint, inside the merge's transaction and
/// **before** core deletes the source tags — see the ordering note in [`merge_tags`].
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
) -> Result<PluginTagState> {
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
fn tag_paths(conn: &rusqlite::Connection, ids: &[i64]) -> Result<Vec<String>> {
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

/// Split a tag: give `photo_ids` the tag at `new_path` and, unless `keep_source`, take the
/// source tag off exactly those photos. `dry_run` previews, as for [`merge_tags`]: the real
/// mutation, rolled back. **Blocking**: call it on a worker.
pub fn split_tag(
    c: &Catalog,
    source_id: i64,
    photo_ids: &[i64],
    new_path: &str,
    keep_source: bool,
    dry_run: bool,
) -> Result<TagSplitReport> {
    let tx = c.conn().unchecked_transaction()?;
    let report = tag_maintenance::split_tag(
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

/// Add the tag at `path` to a tag group, creating the tag if it does not exist yet. Returns
/// the tag id. The Tags groups manager's "Add tag (path, created if new)". An auto-tag is
/// refused ([`CatalogError::AutoTag`](crate::catalog::CatalogError::AutoTag)): a group's
/// buttons assign by hand, which an auto-tag refuses (#181).
pub fn add_tag_to_group(c: &Catalog, group_id: i64, path: &str) -> Result<i64> {
    let tag_id = match c.find_tag_id_by_path(path)? {
        Some(id) => {
            c.refuse_auto_tag(id)?;
            id
        }
        None => c.create_tag(path)?,
    };
    c.add_tag_to_group(group_id, tag_id)?;
    Ok(tag_id)
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let report = merge_tags(&c, &[source], target, true).unwrap();
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
        let report = merge_tags(&c, &[source], target, false).unwrap();
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

        let report = merge_tags(&c, &[source], target, false).unwrap();

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

        let report = merge_tags(&c, &[source], target, false).unwrap();
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

        let report = merge_tags(&c, &[source], target, false).unwrap();

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

        let report = split_tag(&c, source, &[1], "Venue", false, true).unwrap();
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

    /// A path the vocabulary lacks is created; one it has is reused, not duplicated.
    #[test]
    fn adding_a_path_to_a_group_creates_the_tag_only_when_new() {
        let (c, _dir) = temp_catalog("group-add");
        let existing = c.create_tag("Street/Candid").unwrap();
        let group = c.create_tag_group("Street").unwrap();
        assert_eq!(add_tag_to_group(&c, group, "Street/Candid").unwrap(), existing);
        let created = add_tag_to_group(&c, group, "Street/Night").unwrap();
        assert_ne!(created, existing);
        assert_eq!(c.find_tag_id_by_path("Street/Night").unwrap(), Some(created));
        let members: Vec<i64> = c.group_members(group).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(members.len(), 2);
        assert!(members.contains(&existing) && members.contains(&created));
    }
}
