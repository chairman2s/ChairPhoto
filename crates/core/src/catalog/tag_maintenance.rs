//! Tag maintenance (A5): merge, split, and the finders that say what needs cleaning.
//!
//! The operations here are the ones that used to be hand-written SQL against a live
//! catalog — the 293→186 root reorg and the Brand consolidation were both merges. They are
//! **pure catalog transactions**: `xmp::write_keywords` has one caller (the export
//! destination, `export/mod.rs`), so there is no in-library sidecar to rewrite, no job
//! family, and nothing to resume. The difficulty is entirely referential.
//!
//! Every function takes `&Connection` rather than owning its transaction, so the caller
//! decides whether to commit. That is what makes a dry run trustworthy: the preview runs
//! the *same* mutation and rolls it back, so a preview and the operation it previews cannot
//! disagree about what would happen.
//!
//! ## What a merge has to get right
//!
//! | Hazard | Handling |
//! |---|---|
//! | `tags.parent_id … ON DELETE CASCADE` | children are reparented onto the target *before* the source is deleted, or deleting the source would take its whole subtree with it |
//! | `full_path_norm` is UNIQUE | descendants' paths are recomputed and every collision is found **before** the first write, then refused by name |
//! | `photo_tags` PK `(photo_id, tag_id)` | a photo carrying both tags collapses to one row, keeping the **earlier** `created_at` — when the user first said this about this photo |
//! | `tag_synonyms.synonym_norm` UNIQUE **globally** | nothing to do: the same norm cannot exist twice, so moving `tag_id` can never collide (the design predicted a hazard the schema rules out) |
//! | `tag_terms` unique per `(tag, text, language)` | same: moved or skipped-and-named |
//! | Smart album rules store tag **ids** | `rule_json` conditions are rewritten, and the albums named in the report |
//! | Auto-tags recompute membership | merging *into* an auto-tag is refused: the engine deletes and re-derives its assignments, so the merge would silently undo itself |
//! | `tags.uuid` survives in bundles | a `tag_aliases` tombstone stops a later bundle import from re-creating what was merged away |
//!
//! Plugin tables that hold a tag id (`faces__faces.person_tag_id`,
//! `faces__rejections.person_tag_id`, `smarttags__classifiers` keyed by tag path) are
//! **not** touched here — core must not know `faces__*` exists. The command layer composes
//! the core merge with each plugin's own repoint inside one transaction; see
//! `commands::tags::merge_tags`.

use rusqlite::{params, Connection, OptionalExtension};

use super::tag_path::normalize_lookup;
use super::{CatalogError, Result};

/// A tag as a merge reports it — enough for a human to recognize what was touched.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergedSource {
    pub id: i64,
    pub path: String,
    /// The uuid now tombstoned in `tag_aliases`, or `None` for a tag that never had one.
    pub uuid: Option<String>,
}

/// What a merge did, or — run inside a rolled-back transaction — what it would do.
///
/// Every count is of work actually performed, so a dry run and the real thing produce the
/// same report. Plugin-side fields are `None` when that plugin is compiled out, which is a
/// different statement from `Some(0)`: "not checked" rather than "checked, nothing to do".
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagMergeReport {
    pub target_id: i64,
    pub target_path: String,
    pub sources: Vec<MergedSource>,
    /// Photos that gained the target tag because they carried a source tag.
    pub photos_retagged: usize,
    /// Photos that already carried the target, so two assignments became one.
    pub assignments_collapsed: usize,
    pub children_reparented: usize,
    pub descendants_repathed: usize,
    pub terms_moved: usize,
    /// Terms left behind because the target already had that text in that language.
    pub terms_skipped: Vec<String>,
    /// Synonyms moved. There is no "skipped" counterpart: `synonym_norm` is unique
    /// catalog-wide, so a synonym the target already held could not have been on the source.
    pub synonyms_moved: usize,
    pub groups_repointed: usize,
    /// Smart albums whose rule referred to a source tag, by name.
    pub smart_albums_rewritten: Vec<String>,
    pub aliases_recorded: usize,
    /// Things that are not refusals but that the user should read before committing.
    pub warnings: Vec<String>,
    /// `faces__faces` rows repointed at the target. `None` = faces compiled out.
    pub faces_repointed: Option<usize>,
    /// `faces__rejections` rows repointed. `None` = faces compiled out.
    pub face_rejections_repointed: Option<usize>,
    /// Per-tag classifiers dropped because the tag path they were trained for is gone.
    /// `None` = smarttags compiled out.
    pub classifiers_dropped: Option<usize>,
    /// Pending tag suggestions moved from a dead tag path onto the target's.
    /// `None` = smarttags compiled out.
    pub suggestions_repointed: Option<usize>,
}

/// One tag row, as the maintenance operations need it.
struct TagRow {
    id: i64,
    full_path: String,
    uuid: Option<String>,
    auto_rule: Option<String>,
}

fn load_tag(conn: &Connection, id: i64) -> Result<TagRow> {
    conn.query_row(
        "SELECT id, full_path, uuid, auto_rule FROM tags WHERE id = ?1",
        [id],
        |r| {
            Ok(TagRow {
                id: r.get(0)?,
                full_path: r.get(1)?,
                uuid: r.get(2)?,
                auto_rule: r.get(3)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| CatalogError::Tag(format!("no tag with id {id}")))
}

/// `(id, full_path)` for a tag and every descendant.
fn subtree(conn: &Connection, tag_id: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE d(id) AS (
             SELECT id FROM tags WHERE id = ?1
             UNION ALL
             SELECT tags.id FROM tags JOIN d ON tags.parent_id = d.id
         )
         SELECT tags.id, tags.full_path FROM tags JOIN d ON d.id = tags.id",
    )?;
    let rows = stmt.query_map([tag_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Merge `source_ids` into `target_id`: every photo, child, term, synonym, group membership
/// and smart-album reference moves to the target, and the sources are deleted with a
/// tombstone left behind.
///
/// **The caller owns the transaction.** Nothing here commits, so a preview is the same code
/// path rolled back rather than a second implementation that can drift. Refusals are
/// `Err(CatalogError::Tag)` with the reason named, and every refusal is raised before the
/// first write.
///
/// `now` is the timestamp stamped on tombstones and touched rows, injected for tests.
pub fn merge_tags(
    conn: &Connection,
    source_ids: &[i64],
    target_id: i64,
    now: i64,
) -> Result<TagMergeReport> {
    // ── Validate everything before writing anything ─────────────────────────────
    let target = load_tag(conn, target_id)?;
    if let Some(rule) = &target.auto_rule {
        // The auto-tag engine deletes and re-derives this tag's assignments on its next
        // pass (`autotags.rs`), so anything merged in would vanish without a trace.
        return Err(CatalogError::Tag(format!(
            "'{}' is an auto-tag (rule '{rule}'): its membership is recomputed, so merged \
             assignments would be dropped on the next pass. Merge the other direction.",
            target.full_path
        )));
    }

    let mut sources: Vec<TagRow> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for &id in source_ids {
        if id == target_id || !seen.insert(id) {
            continue; // merging a tag into itself, or naming it twice, is a no-op
        }
        let source = load_tag(conn, id)?;
        // Reparenting the source's children onto a target *inside* the source would make
        // the target its own ancestor.
        if subtree(conn, id)?.iter().any(|(sid, _)| *sid == target_id) {
            return Err(CatalogError::Tag(format!(
                "cannot merge '{}' into '{}': the target is inside the source's own subtree",
                source.full_path, target.full_path
            )));
        }
        sources.push(source);
    }
    if sources.is_empty() {
        return Err(CatalogError::Tag("no source tags to merge".into()));
    }

    // Plan every path change first: a collision found half-way through is a corrupted tree.
    let mut repaths: Vec<(i64, String, String)> = Vec::new();
    let moving: std::collections::HashSet<i64> = sources
        .iter()
        .map(|s| s.id)
        .chain(std::iter::once(target_id))
        .collect();
    let mut planned_norms: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for source in &sources {
        for (child_id, child_path) in children_of(conn, source.id)? {
            // `A/x` under source `A` becomes `B/x` under target `B`; its own descendants
            // follow by prefix.
            let old_prefix = format!("{}/", source.full_path);
            let leaf_suffix = child_path
                .strip_prefix(&old_prefix)
                .unwrap_or(&child_path)
                .to_string();
            for (id, path) in subtree(conn, child_id)? {
                let tail = path.strip_prefix(&child_path).unwrap_or("");
                let new_path = format!("{}/{}{}", target.full_path, leaf_suffix, tail);
                let new_norm = normalize_lookup(&new_path);
                if let Some(other) = tag_id_by_norm(conn, &new_norm)? {
                    if !moving.contains(&other) && !repaths.iter().any(|(rid, _, _)| *rid == other)
                    {
                        return Err(CatalogError::Tag(format!(
                            "merging '{}' into '{}' would collide at '{new_path}', which \
                             already exists. Merge that pair first, or rename one of them.",
                            source.full_path, target.full_path
                        )));
                    }
                }
                // Two sources can both carry a child of the same name; they would land on
                // the same path under the target.
                if let Some(first) = planned_norms.insert(new_norm.clone(), new_path.clone()) {
                    return Err(CatalogError::Tag(format!(
                        "two of the merged tags would both become '{first}'. Merge them into \
                         each other first."
                    )));
                }
                repaths.push((id, new_path, new_norm));
            }
        }
    }

    let mut report = TagMergeReport {
        target_id,
        target_path: target.full_path.clone(),
        ..Default::default()
    };
    for source in &sources {
        if let Some(rule) = &source.auto_rule {
            report.warnings.push(format!(
                "'{}' is an auto-tag (rule '{rule}'). The engine re-creates it by path on its \
                 next pass, so it will come back — empty.",
                source.full_path
            ));
        }
    }

    // ── Mutate ──────────────────────────────────────────────────────────────────
    for source in &sources {
        move_photo_assignments(conn, source.id, target_id, &mut report)?;
        move_terms(conn, source.id, target_id, &mut report)?;
        move_synonyms(conn, source.id, target_id, &mut report)?;
        move_group_memberships(conn, source.id, target_id, &mut report)?;

        // Reparent before the delete, or the cascade takes the subtree.
        report.children_reparented += conn.execute(
            "UPDATE tags SET parent_id = ?2, updated_at = ?3 WHERE parent_id = ?1",
            params![source.id, target_id, now],
        )?;
    }

    for (id, new_path, new_norm) in &repaths {
        conn.execute(
            "UPDATE tags SET full_path = ?1, full_path_norm = ?2, updated_at = ?3 WHERE id = ?4",
            params![new_path, new_norm, now, id],
        )?;
    }
    report.descendants_repathed = repaths.len();

    report.smart_albums_rewritten = rewrite_smart_album_rules(
        conn,
        &sources.iter().map(|s| s.id).collect::<Vec<_>>(),
        target_id,
        now,
    )?;

    for source in &sources {
        // A merge chain must not strand an older tombstone: anything that pointed at this
        // source now points at where the source went.
        conn.execute(
            "UPDATE tag_aliases SET target_tag_id = ?2, merged_at = ?3 WHERE target_tag_id = ?1",
            params![source.id, target_id, now],
        )?;
        if let Some(uuid) = source.uuid.as_deref().filter(|u| !u.is_empty()) {
            conn.execute(
                "INSERT INTO tag_aliases(dead_uuid, target_tag_id, dead_path, merged_at)
                 VALUES(?1, ?2, ?3, ?4)
                 ON CONFLICT(dead_uuid) DO UPDATE
                   SET target_tag_id = excluded.target_tag_id, merged_at = excluded.merged_at",
                params![uuid, target_id, source.full_path, now],
            )?;
            report.aliases_recorded += 1;
        } else {
            report.warnings.push(format!(
                "'{}' has no uuid, so nothing tombstones it. A bundle carrying it by path \
                 could re-create it.",
                source.full_path
            ));
        }
        conn.execute("DELETE FROM tags WHERE id = ?1", [source.id])?;
    }

    report.sources = sources
        .into_iter()
        .map(|s| MergedSource {
            id: s.id,
            path: s.full_path,
            uuid: s.uuid.filter(|u| !u.is_empty()),
        })
        .collect();
    Ok(report)
}

/// Direct children of a tag, as `(id, full_path)`.
fn children_of(conn: &Connection, tag_id: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare("SELECT id, full_path FROM tags WHERE parent_id = ?1")?;
    let rows = stmt.query_map([tag_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn tag_id_by_norm(conn: &Connection, norm: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT id FROM tags WHERE full_path_norm = ?1", [norm], |r| {
            r.get(0)
        })
        .optional()?)
}

/// Move `photo_tags` rows, collapsing a photo that carries both onto the **earlier**
/// `created_at` — the moment the user first made this statement about this photo.
fn move_photo_assignments(
    conn: &Connection,
    source_id: i64,
    target_id: i64,
    report: &mut TagMergeReport,
) -> Result<()> {
    let total: usize = conn.query_row(
        "SELECT COUNT(*) FROM photo_tags WHERE tag_id = ?1",
        [source_id],
        |r| r.get::<_, i64>(0),
    )? as usize;
    let collapsed: usize = conn.query_row(
        "SELECT COUNT(*) FROM photo_tags s
          WHERE s.tag_id = ?1
            AND EXISTS (SELECT 1 FROM photo_tags t
                         WHERE t.photo_id = s.photo_id AND t.tag_id = ?2)",
        params![source_id, target_id],
        |r| r.get::<_, i64>(0),
    )? as usize;

    conn.execute(
        "INSERT INTO photo_tags(photo_id, tag_id, created_at)
         SELECT photo_id, ?2, created_at FROM photo_tags WHERE tag_id = ?1
         ON CONFLICT(photo_id, tag_id) DO UPDATE
           SET created_at = MIN(photo_tags.created_at, excluded.created_at)",
        params![source_id, target_id],
    )?;
    conn.execute("DELETE FROM photo_tags WHERE tag_id = ?1", [source_id])?;

    report.photos_retagged += total - collapsed;
    report.assignments_collapsed += collapsed;
    Ok(())
}

/// Move per-tag terms, skipping any the target already has in that language.
fn move_terms(
    conn: &Connection,
    source_id: i64,
    target_id: i64,
    report: &mut TagMergeReport,
) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT text FROM tag_terms s
          WHERE s.tag_id = ?1
            AND EXISTS (SELECT 1 FROM tag_terms t
                         WHERE t.tag_id = ?2 AND t.text_norm = s.text_norm
                           AND coalesce(t.language, '') = coalesce(s.language, ''))",
    )?;
    let skipped = stmt
        .query_map(params![source_id, target_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let moved = conn.execute(
        "INSERT INTO tag_terms(tag_id, text, text_norm, language, is_primary, export, created_at)
         SELECT ?2, text, text_norm, language, is_primary, export, created_at
           FROM tag_terms WHERE tag_id = ?1
         ON CONFLICT(tag_id, text_norm, coalesce(language, '')) DO NOTHING",
        params![source_id, target_id],
    )?;

    report.terms_moved += moved;
    report.terms_skipped.extend(skipped);
    Ok(())
}

/// Move synonyms onto the target.
///
/// A plain `UPDATE`, and deliberately not `UPDATE OR IGNORE`: `tag_synonyms.synonym_norm` is
/// `UNIQUE` **catalog-wide** (`schema.rs`), so two tags can never hold the same normalized
/// synonym in the first place — and this statement changes only `tag_id`, never the norm.
/// There is no collision to skip. (The A5 design predicted one; the schema rules it out.)
///
/// If that index is ever relaxed to `(tag_id, synonym_norm)`, collisions become possible and
/// this must fail loudly rather than silently drop a synonym — which is exactly what a plain
/// UPDATE does.
fn move_synonyms(
    conn: &Connection,
    source_id: i64,
    target_id: i64,
    report: &mut TagMergeReport,
) -> Result<()> {
    report.synonyms_moved += conn.execute(
        "UPDATE tag_synonyms SET tag_id = ?2 WHERE tag_id = ?1",
        params![source_id, target_id],
    )?;
    Ok(())
}

/// Repoint quick-tag group membership, ignoring a group that already holds the target.
fn move_group_memberships(
    conn: &Connection,
    source_id: i64,
    target_id: i64,
    report: &mut TagMergeReport,
) -> Result<()> {
    let moved = conn.execute(
        "UPDATE OR IGNORE tag_group_members SET tag_id = ?2 WHERE tag_id = ?1",
        params![source_id, target_id],
    )?;
    conn.execute(
        "DELETE FROM tag_group_members WHERE tag_id = ?1",
        [source_id],
    )?;
    report.groups_repointed += moved;
    Ok(())
}

/// Rewrite `tag` conditions in every smart album rule that names a source tag, and return
/// the affected album names. Rules store tag **ids** (`smart_albums.rs`'s `tag_predicate`
/// binds `json_integer(value)`), so a merge that ignored them would leave albums pointing at
/// a deleted tag and silently matching nothing.
fn rewrite_smart_album_rules(
    conn: &Connection,
    source_ids: &[i64],
    target_id: i64,
    now: i64,
) -> Result<Vec<String>> {
    let albums: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, name, rule_json FROM smart_albums")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut rewritten = Vec::new();
    for (id, name, rule_json) in albums {
        // A rule we cannot parse is one we must not rewrite — leave it exactly as found.
        let Ok(mut rule) = serde_json::from_str::<serde_json::Value>(&rule_json) else {
            continue;
        };
        let mut changed = false;
        if let Some(conditions) = rule.get_mut("conditions").and_then(|c| c.as_array_mut()) {
            for cond in conditions {
                if cond.get("field").and_then(|f| f.as_str()) != Some("tag") {
                    continue;
                }
                let Some(tag_id) = cond.get("value").and_then(|v| v.as_i64()) else {
                    continue;
                };
                if source_ids.contains(&tag_id) {
                    cond["value"] = serde_json::Value::from(target_id);
                    changed = true;
                }
            }
        }
        if changed {
            conn.execute(
                "UPDATE smart_albums SET rule_json = ?1, updated_at = ?2 WHERE id = ?3",
                params![rule.to_string(), now, id],
            )?;
            rewritten.push(name);
        }
    }
    Ok(rewritten)
}

// ── Split ───────────────────────────────────────────────────────────────────────

/// What a split did, or would do.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TagSplitReport {
    pub source_path: String,
    pub new_tag_id: i64,
    pub new_tag_path: String,
    /// Photos that gained the new tag.
    pub photos_moved: usize,
    /// Photos that already carried it, so the split had nothing to add for them.
    pub photos_already_tagged: usize,
    /// Photos that lost the source tag (0 when `keep_source`).
    pub photos_untagged: usize,
    /// Named photo ids that do not carry the source tag at all — reported, not tagged.
    pub photos_without_source: usize,
    pub created_new_tag: bool,
}

/// Split a tag in two: give `photo_ids` the tag at `new_path` and, unless `keep_source`,
/// take the source tag off exactly those photos.
///
/// This is the "this tag is doing two jobs" operation — `Concert` that turned out to mean
/// both *the event* and *the venue*. It acts only on the photos named: a split cannot guess
/// which half a photo belongs to, and inventing that would be worse than asking.
///
/// A photo in `photo_ids` that never carried the source tag is **counted and skipped**, not
/// tagged: the caller asked to split a tag, not to apply one.
pub fn split_tag(
    conn: &Connection,
    source_id: i64,
    photo_ids: &[i64],
    new_path: &str,
    keep_source: bool,
    now: i64,
) -> Result<TagSplitReport> {
    let source = load_tag(conn, source_id)?;
    let (new_tag_id, created) = ensure_tag_by_path(conn, new_path, now)?;
    if new_tag_id == source_id {
        return Err(CatalogError::Tag(
            "the new tag is the tag being split; give the other half a different path".into(),
        ));
    }

    let mut report = TagSplitReport {
        source_path: source.full_path,
        new_tag_id,
        new_tag_path: tag_full_path(conn, new_tag_id)?,
        created_new_tag: created,
        ..Default::default()
    };

    let mut seen = std::collections::HashSet::new();
    for &photo_id in photo_ids {
        if !seen.insert(photo_id) {
            continue;
        }
        let carries_source = conn
            .query_row(
                "SELECT 1 FROM photo_tags WHERE photo_id = ?1 AND tag_id = ?2",
                params![photo_id, source_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !carries_source {
            report.photos_without_source += 1;
            continue;
        }
        let added = conn.execute(
            "INSERT OR IGNORE INTO photo_tags(photo_id, tag_id, created_at) VALUES(?1, ?2, ?3)",
            params![photo_id, new_tag_id, now],
        )?;
        if added > 0 {
            report.photos_moved += 1;
        } else {
            report.photos_already_tagged += 1;
        }
        if !keep_source {
            report.photos_untagged += conn.execute(
                "DELETE FROM photo_tags WHERE photo_id = ?1 AND tag_id = ?2",
                params![photo_id, source_id],
            )?;
        }
    }
    Ok(report)
}

fn tag_full_path(conn: &Connection, tag_id: i64) -> Result<String> {
    Ok(conn.query_row("SELECT full_path FROM tags WHERE id = ?1", [tag_id], |r| {
        r.get(0)
    })?)
}

/// Find or create the tag at `path`, creating any missing ancestors. Returns the leaf id and
/// whether anything was created. Mirrors `Catalog::create_tag`, but on a bare connection so
/// it composes inside a caller-owned transaction.
fn ensure_tag_by_path(conn: &Connection, path: &str, now: i64) -> Result<(i64, bool)> {
    let normalized = super::tag_path::normalize_tag_path(path).map_err(CatalogError::Tag)?;
    let mut parent_id: Option<i64> = None;
    let mut prefix: Vec<String> = Vec::new();
    let mut created_any = false;
    let mut leaf = None;

    for component in &normalized.components {
        prefix.push(component.clone());
        let full_path = prefix.join("/");
        let full_path_norm = normalize_lookup(&full_path);
        let id = match tag_id_by_norm(conn, &full_path_norm)? {
            Some(id) => id,
            None => {
                conn.execute(
                    "INSERT INTO tags(uuid, name, name_norm, parent_id, full_path, full_path_norm,
                                      created_at, updated_at)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                    params![
                        uuid::Uuid::new_v4().to_string(),
                        component,
                        normalize_lookup(component),
                        parent_id,
                        full_path,
                        full_path_norm,
                        now,
                    ],
                )?;
                created_any = true;
                conn.last_insert_rowid()
            }
        };
        parent_id = Some(id);
        leaf = Some(id);
    }

    leaf.map(|id| (id, created_any))
        .ok_or_else(|| CatalogError::Tag("empty tag path".into()))
}

// ── Finders ─────────────────────────────────────────────────────────────────────

/// A tag nothing uses.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrphanTag {
    pub id: i64,
    pub path: String,
    /// True when the tag has children — an empty *branch* is structure, not litter, so the
    /// UI can present it differently from an empty leaf.
    pub has_children: bool,
    pub is_auto_tag: bool,
}

/// Tags carrying no photos, anywhere in their subtree.
///
/// A branch whose descendants hold photos is not an orphan even though it holds none itself:
/// deleting it would cascade the whole subtree away. The `has_children` flag distinguishes
/// an empty leaf (litter) from an empty branch that still organizes other empty tags.
pub fn find_orphan_tags(conn: &Connection) -> Result<Vec<OrphanTag>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.full_path,
                EXISTS (SELECT 1 FROM tags c WHERE c.parent_id = t.id),
                t.auto_rule IS NOT NULL
           FROM tags t
          WHERE NOT EXISTS (
                  WITH RECURSIVE d(id) AS (
                    SELECT t.id
                    UNION ALL
                    SELECT tags.id FROM tags JOIN d ON tags.parent_id = d.id
                  )
                  SELECT 1 FROM photo_tags pt WHERE pt.tag_id IN (SELECT id FROM d)
                )
          ORDER BY t.full_path",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(OrphanTag {
            id: r.get(0)?,
            path: r.get(1)?,
            has_children: r.get(2)?,
            is_auto_tag: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Two tags that look like they mean the same thing.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimilarTagPair {
    pub a_id: i64,
    pub a_path: String,
    pub a_photos: usize,
    pub b_id: i64,
    pub b_path: String,
    pub b_photos: usize,
    /// 0–1 similarity of the two leaf names (normalized edit distance). 1.0 = identical.
    pub name_similarity: f64,
    /// How many photos carry both — high co-occurrence on near-identical names is the
    /// strongest duplicate signal there is.
    pub co_occurrence: usize,
    /// Why this pair surfaced, for a UI that has to justify a destructive suggestion.
    pub reason: String,
}

/// Candidate duplicate tags: leaf names within `min_similarity` of each other, each reported
/// with how often the two are used together.
///
/// Deliberately a *suggestion*, never an action — "Cycling" and "Cycles" may be a typo or two
/// real concepts, and only the user knows which. Comparison is over **leaf names**, so
/// `Place/Bergen` and `People/Bergen` surface as a pair worth a human glance rather than being
/// hidden by their differing paths.
///
/// **Co-occurrence is a signal on candidates, not a way of finding them.** Finding pairs by
/// co-occurrence needs an all-pairs self-join over `photo_tags` — quadratic in tags-per-photo
/// across the entire library, tens of millions of rows on a six-figure catalog — so this
/// walks names (cheap, in memory, with a length prefilter that skips pairs too far apart to
/// possibly qualify) and asks the database only about the pairs that survive. The cost is
/// that two duplicates with unlike names, `Bike` and `Velocipede`, will not surface here.
pub fn find_similar_tags(conn: &Connection, min_similarity: f64) -> Result<Vec<SimilarTagPair>> {
    let tags: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare("SELECT id, name_norm, full_path FROM tags ORDER BY full_path")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let counts = photo_counts(conn)?;

    let mut out = Vec::new();
    for (i, (a_id, a_name, a_path)) in tags.iter().enumerate() {
        for (b_id, b_name, b_path) in tags.iter().skip(i + 1) {
            // Edit distance is at least the length difference, so a pair that far apart can
            // never reach the threshold — skip it before paying for the matrix.
            let (short, long) = (a_name.chars().count(), b_name.chars().count());
            let (short, long) = (short.min(long), short.max(long));
            if long == 0 || 1.0 - ((long - short) as f64 / long as f64) < min_similarity {
                continue;
            }
            let similarity = name_similarity(a_name, b_name);
            if similarity < min_similarity {
                continue;
            }
            let co = co_occurrence(conn, *a_id, *b_id)?;
            let reason = if similarity >= 1.0 {
                "same name in two places".to_string()
            } else if co > 0 {
                format!("similar names, and {co} photos carry both")
            } else {
                "similar names".to_string()
            };
            out.push(SimilarTagPair {
                a_id: *a_id,
                a_path: a_path.clone(),
                a_photos: counts.get(a_id).copied().unwrap_or(0),
                b_id: *b_id,
                b_path: b_path.clone(),
                b_photos: counts.get(b_id).copied().unwrap_or(0),
                name_similarity: similarity,
                co_occurrence: co,
                reason,
            });
        }
    }
    // Most-alike first: the pairs a human should look at before the marginal ones.
    out.sort_by(|x, y| {
        y.name_similarity
            .partial_cmp(&x.name_similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(y.co_occurrence.cmp(&x.co_occurrence))
    });
    Ok(out)
}

/// Photo count per tag, in one pass — a query per tag would dominate the pair walk.
fn photo_counts(conn: &Connection) -> Result<std::collections::HashMap<i64, usize>> {
    let mut stmt = conn.prepare("SELECT tag_id, COUNT(*) FROM photo_tags GROUP BY tag_id")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as usize)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn co_occurrence(conn: &Connection, a: i64, b: i64) -> Result<usize> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM photo_tags x JOIN photo_tags y ON y.photo_id = x.photo_id
          WHERE x.tag_id = ?1 AND y.tag_id = ?2",
        params![a, b],
        |r| r.get::<_, i64>(0),
    )? as usize)
}

/// Normalized Levenshtein similarity in 0–1: `1 - distance / longest`.
fn name_similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    // Two rows are enough for the distance itself; we never reconstruct the edit script.
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let distance = prev[b.len()] as f64;
    1.0 - distance / a.len().max(b.len()) as f64
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::schema;

    /// A real catalog schema in memory. The hazards under test are referential — cascades,
    /// unique indexes, foreign keys — so a hand-rolled fixture table would test a different
    /// database than the one that ships.
    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(schema::SCHEMA_SQL).unwrap();
        conn
    }

    fn tag(conn: &Connection, path: &str) -> i64 {
        ensure_tag_by_path(conn, path, 0).unwrap().0
    }

    fn photo(conn: &Connection, id: i64) {
        conn.execute(
            "INSERT INTO photos(id, uuid, path, mtime_ns, size, extension, created_at, updated_at)
             VALUES(?1, ?2, ?3, 0, 0, 'nef', 0, 0)",
            params![id, format!("uuid-{id}"), format!("{id}.NEF")],
        )
        .unwrap();
    }

    fn assign(conn: &Connection, photo_id: i64, tag_id: i64, at: i64) {
        conn.execute(
            "INSERT INTO photo_tags(photo_id, tag_id, created_at) VALUES(?1, ?2, ?3)",
            params![photo_id, tag_id, at],
        )
        .unwrap();
    }

    fn path_of(conn: &Connection, tag_id: i64) -> Option<String> {
        conn.query_row("SELECT full_path FROM tags WHERE id = ?1", [tag_id], |r| {
            r.get(0)
        })
        .optional()
        .unwrap()
    }

    fn tags_on(conn: &Connection, photo_id: i64) -> Vec<i64> {
        let mut stmt = conn
            .prepare("SELECT tag_id FROM photo_tags WHERE photo_id = ?1 ORDER BY tag_id")
            .unwrap();
        let rows = stmt.query_map([photo_id], |r| r.get(0)).unwrap();
        rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }

    // ── Merge: the cascade ──────────────────────────────────────────────────────

    /// `tags.parent_id` cascades on delete, so a merge that deleted the source first would
    /// take its children with it — the single most destructive way to get this wrong.
    #[test]
    fn merge_reparents_children_before_deleting_the_source() {
        let c = conn();
        let old_root = tag(&c, "Transportation/Brands");
        let child = tag(&c, "Transportation/Brands/Volvo");
        let grandchild = tag(&c, "Transportation/Brands/Volvo/240");
        let new_root = tag(&c, "Brand");

        let report = merge_tags(&c, &[old_root], new_root, 100).unwrap();

        assert_eq!(report.children_reparented, 1);
        assert!(path_of(&c, old_root).is_none(), "the source is gone");
        assert_eq!(path_of(&c, child).as_deref(), Some("Brand/Volvo"));
        assert_eq!(
            path_of(&c, grandchild).as_deref(),
            Some("Brand/Volvo/240"),
            "a grandchild's path follows its parent, not just the direct child"
        );
        assert_eq!(report.descendants_repathed, 2);
    }

    /// Photo assignments move, and a photo that carried both tags keeps the *earlier*
    /// timestamp — when the user first made that statement about that photo.
    #[test]
    fn merge_collapses_double_assignments_onto_the_earlier_timestamp() {
        let c = conn();
        let source = tag(&c, "Bike");
        let target = tag(&c, "Cycling");
        photo(&c, 1);
        photo(&c, 2);
        assign(&c, 1, source, 500); // only the source
        assign(&c, 2, source, 300); // both, source is older
        assign(&c, 2, target, 900);

        let report = merge_tags(&c, &[source], target, 1000).unwrap();

        assert_eq!(report.photos_retagged, 1);
        assert_eq!(report.assignments_collapsed, 1);
        assert_eq!(tags_on(&c, 1), vec![target]);
        assert_eq!(tags_on(&c, 2), vec![target], "one row, not two");
        let created: i64 = c
            .query_row(
                "SELECT created_at FROM photo_tags WHERE photo_id = 2 AND tag_id = ?1",
                [target],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(created, 300, "the earlier of the two survives");
    }

    // ── Merge: refusals, all raised before any write ────────────────────────────

    /// A path collision must be found by the planner, not discovered half-way through the
    /// UPDATEs — and nothing may have changed when it is.
    #[test]
    fn merge_refuses_a_colliding_child_and_writes_nothing() {
        let c = conn();
        let source = tag(&c, "Places");
        let source_child = tag(&c, "Places/Bergen");
        let target = tag(&c, "Place");
        tag(&c, "Place/Bergen"); // the collision
        photo(&c, 1);
        assign(&c, 1, source, 10);

        let err = merge_tags(&c, &[source], target, 100).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Place/Bergen"), "names the collision: {message}");

        // Nothing moved.
        assert_eq!(path_of(&c, source).as_deref(), Some("Places"));
        assert_eq!(path_of(&c, source_child).as_deref(), Some("Places/Bergen"));
        assert_eq!(tags_on(&c, 1), vec![source]);
    }

    /// Merging into an auto-tag is refused: the engine deletes and re-derives that tag's
    /// assignments, so everything merged in would disappear on the next pass.
    #[test]
    fn merge_into_an_auto_tag_is_refused() {
        let c = conn();
        let source = tag(&c, "Mono");
        let target = tag(&c, "Treatment/Black & White");
        c.execute(
            "UPDATE tags SET auto_rule = 'monochrome' WHERE id = ?1",
            [target],
        )
        .unwrap();

        let err = merge_tags(&c, &[source], target, 100).unwrap_err().to_string();
        assert!(err.contains("auto-tag"), "{err}");
        assert!(path_of(&c, source).is_some(), "the source survives a refusal");
    }

    /// Merging a tag into its own descendant would make the target its own ancestor.
    #[test]
    fn merge_into_own_descendant_is_refused() {
        let c = conn();
        let parent = tag(&c, "Animals");
        let child = tag(&c, "Animals/Birds");
        let err = merge_tags(&c, &[parent], child, 100).unwrap_err().to_string();
        assert!(err.contains("subtree"), "{err}");
        assert_eq!(path_of(&c, parent).as_deref(), Some("Animals"));
    }

    /// Merging an auto-tag *away* is allowed but warned about: the engine re-creates it by
    /// path on its next pass, so the user should know it will reappear.
    #[test]
    fn merging_an_auto_tag_away_warns_that_it_returns() {
        let c = conn();
        let source = tag(&c, "Treatment/Black & White");
        c.execute(
            "UPDATE tags SET auto_rule = 'monochrome' WHERE id = ?1",
            [source],
        )
        .unwrap();
        let target = tag(&c, "Mono");

        let report = merge_tags(&c, &[source], target, 100).unwrap();
        assert!(
            report.warnings.iter().any(|w| w.contains("auto-tag")),
            "{:?}",
            report.warnings
        );
    }

    // ── Merge: the unique constraints ───────────────────────────────────────────

    /// Synonyms move wholesale. The collision the A5 design predicted cannot be built: the
    /// `UNIQUE` on `synonym_norm` is catalog-wide, so no two tags can hold the same norm —
    /// asserted here, because the merge's plain `UPDATE` depends on it.
    #[test]
    fn merge_moves_synonyms_and_the_predicted_collision_cannot_exist() {
        let c = conn();
        let source = tag(&c, "Bike");
        let target = tag(&c, "Cycling");
        let elsewhere = tag(&c, "Sport/Racing");
        let add_synonym = |tag_id: i64, text: &str| {
            c.execute(
                "INSERT INTO tag_synonyms(tag_id, synonym, synonym_norm, created_at)
                 VALUES(?1, ?2, ?3, 0)",
                params![tag_id, text, normalize_lookup(text)],
            )
        };
        add_synonym(source, "velo").unwrap();
        add_synonym(source, "pushbike").unwrap();
        add_synonym(elsewhere, "bicycle").unwrap();
        // The state the design worried about — two tags holding one norm — is unreachable.
        assert!(
            add_synonym(source, "Bicycle").is_err(),
            "synonym_norm is UNIQUE catalog-wide, so this must be rejected"
        );

        let report = merge_tags(&c, &[source], target, 100).unwrap();

        assert_eq!(report.synonyms_moved, 2);
        let owner: i64 = c
            .query_row(
                "SELECT tag_id FROM tag_synonyms WHERE synonym_norm = 'bicycle'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, elsewhere, "another tag's synonym is untouched");
    }

    /// Terms the target already has in that language are skipped and named.
    #[test]
    fn merge_reports_terms_it_could_not_move() {
        let c = conn();
        let source = tag(&c, "Bike");
        let target = tag(&c, "Cycling");
        let term = |tag_id: i64, text: &str, lang: &str| {
            c.execute(
                "INSERT INTO tag_terms(tag_id, text, text_norm, language, created_at)
                 VALUES(?1, ?2, ?3, ?4, 0)",
                params![tag_id, text, normalize_lookup(text), lang],
            )
            .unwrap();
        };
        term(source, "Sykkel", "nb");
        term(source, "Fahrrad", "de");
        term(target, "Sykkel", "nb"); // already there

        let report = merge_tags(&c, &[source], target, 100).unwrap();
        assert_eq!(report.terms_moved, 1);
        assert_eq!(report.terms_skipped, vec!["Sykkel".to_string()]);
    }

    // ── Merge: everything else that points at a tag id ──────────────────────────

    /// Smart album rules store tag **ids**. A merge that ignored them would leave an album
    /// pointing at a deleted tag, matching nothing, with no error anywhere.
    #[test]
    fn merge_rewrites_smart_album_rules_that_name_the_source() {
        let c = conn();
        let source = tag(&c, "Bike");
        let target = tag(&c, "Cycling");
        let other = tag(&c, "Landscape");
        let rule = format!(
            r#"{{"match":"all","conditions":[{{"field":"tag","op":"under","value":{source}}},{{"field":"rating","op":"gte","value":3}}]}}"#
        );
        c.execute(
            "INSERT INTO smart_albums(uuid, name, rule_json, created_at, updated_at)
             VALUES('a', 'Rides', ?1, 0, 0)",
            [&rule],
        )
        .unwrap();
        let untouched = format!(
            r#"{{"match":"all","conditions":[{{"field":"tag","op":"is","value":{other}}}]}}"#
        );
        c.execute(
            "INSERT INTO smart_albums(uuid, name, rule_json, created_at, updated_at)
             VALUES('b', 'Vistas', ?1, 0, 0)",
            [&untouched],
        )
        .unwrap();

        let report = merge_tags(&c, &[source], target, 100).unwrap();

        assert_eq!(report.smart_albums_rewritten, vec!["Rides".to_string()]);
        let rewritten: String = c
            .query_row("SELECT rule_json FROM smart_albums WHERE name = 'Rides'", [], |r| r.get(0))
            .unwrap();
        assert!(rewritten.contains(&format!(r#""value":{target}"#)), "{rewritten}");
        assert!(rewritten.contains(r#""op":"under""#), "other fields survive: {rewritten}");
        let kept: String = c
            .query_row("SELECT rule_json FROM smart_albums WHERE name = 'Vistas'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, untouched, "an album naming no source tag is left alone");
    }

    /// A rule we cannot parse is left exactly as found rather than rewritten by guesswork.
    #[test]
    fn merge_leaves_an_unparseable_rule_untouched() {
        let c = conn();
        let source = tag(&c, "Bike");
        let target = tag(&c, "Cycling");
        c.execute(
            "INSERT INTO smart_albums(uuid, name, rule_json, created_at, updated_at)
             VALUES('a', 'Broken', 'not json at all', 0, 0)",
            [],
        )
        .unwrap();

        let report = merge_tags(&c, &[source], target, 100).unwrap();
        assert!(report.smart_albums_rewritten.is_empty());
        let kept: String = c
            .query_row("SELECT rule_json FROM smart_albums", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, "not json at all");
    }

    /// Quick-tag group membership is repointed, and a group holding both tags keeps one row.
    #[test]
    fn merge_repoints_group_membership_without_duplicating_it() {
        let c = conn();
        let source = tag(&c, "Bike");
        let target = tag(&c, "Cycling");
        c.execute(
            "INSERT INTO tag_groups(id, name, created_at) VALUES(1, 'Quick', 0), (2, 'Sports', 0)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO tag_group_members(group_id, tag_id, position)
             VALUES(1, ?1, 0), (2, ?1, 0), (2, ?2, 1)",
            params![source, target],
        )
        .unwrap();

        merge_tags(&c, &[source], target, 100).unwrap();

        let members: Vec<(i64, i64)> = {
            let mut stmt = c
                .prepare("SELECT group_id, tag_id FROM tag_group_members ORDER BY group_id")
                .unwrap();
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
        };
        assert_eq!(members, vec![(1, target), (2, target)], "group 2 keeps one row");
    }

    /// The tombstone is what stops a later bundle import from re-creating the merged-away
    /// tag by uuid. Also covers the chain case: merging B into C repoints A's tombstone.
    #[test]
    fn merge_tombstones_the_dead_uuid_and_follows_a_chain() {
        let c = conn();
        let a = tag(&c, "Bike");
        let b = tag(&c, "Cycling");
        let cc = tag(&c, "Sport/Cycling");
        let a_uuid: String = c
            .query_row("SELECT uuid FROM tags WHERE id = ?1", [a], |r| r.get(0))
            .unwrap();

        merge_tags(&c, &[a], b, 100).unwrap();
        let target: i64 = c
            .query_row(
                "SELECT target_tag_id FROM tag_aliases WHERE dead_uuid = ?1",
                [&a_uuid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(target, b);

        // Now B is itself merged away: A's tombstone must follow, not dangle.
        merge_tags(&c, &[b], cc, 200).unwrap();
        let target: i64 = c
            .query_row(
                "SELECT target_tag_id FROM tag_aliases WHERE dead_uuid = ?1",
                [&a_uuid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(target, cc, "a two-step merge chain still resolves");
    }

    /// Several sources at once, including the "two sources own the same child name" case,
    /// which cannot be allowed to silently drop one.
    #[test]
    fn merge_refuses_two_sources_whose_children_would_collide() {
        let c = conn();
        let a = tag(&c, "Cars");
        tag(&c, "Cars/Volvo");
        let b = tag(&c, "Autos");
        tag(&c, "Autos/Volvo");
        let target = tag(&c, "Brand");

        let err = merge_tags(&c, &[a, b], target, 100).unwrap_err().to_string();
        assert!(err.contains("Brand/Volvo"), "{err}");
        assert_eq!(path_of(&c, a).as_deref(), Some("Cars"), "nothing was written");
    }

    /// A caller-owned rollback is what makes the dry run honest: same code path, no commit.
    #[test]
    fn a_rolled_back_merge_leaves_the_catalog_untouched() {
        let mut c = conn();
        let source = tag(&c, "Bike");
        let target = tag(&c, "Cycling");
        photo(&c, 1);
        assign(&c, 1, source, 10);

        let before = catalog_fingerprint(&c);
        let tx = c.transaction().unwrap();
        let report = merge_tags(&tx, &[source], target, 100).unwrap();
        assert_eq!(report.photos_retagged, 1, "the preview reports real work");
        tx.rollback().unwrap();

        assert_eq!(catalog_fingerprint(&c), before, "and none of it survived");
        assert_eq!(tags_on(&c, 1), vec![source]);
    }

    /// Row counts across every table a merge touches — enough to catch a stray write.
    fn catalog_fingerprint(conn: &Connection) -> Vec<(String, i64)> {
        ["tags", "photo_tags", "tag_terms", "tag_synonyms", "tag_group_members", "tag_aliases"]
            .iter()
            .map(|t| {
                let n: i64 = conn
                    .query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
                    .unwrap();
                (t.to_string(), n)
            })
            .collect()
    }

    // ── Split ───────────────────────────────────────────────────────────────────

    /// The named photos move to the new tag and lose the old one; photos that never carried
    /// the source are reported, not quietly tagged.
    #[test]
    fn split_moves_only_the_named_photos_that_carry_the_source() {
        let c = conn();
        let source = tag(&c, "Concert");
        for id in 1..=4 {
            photo(&c, id);
        }
        assign(&c, 1, source, 10);
        assign(&c, 2, source, 20);
        assign(&c, 3, source, 30);
        // Photo 4 carries nothing.

        let report = split_tag(&c, source, &[1, 2, 4], "Venue", false, 100).unwrap();

        assert!(report.created_new_tag);
        assert_eq!(report.new_tag_path, "Venue");
        assert_eq!(report.photos_moved, 2);
        assert_eq!(report.photos_untagged, 2);
        assert_eq!(report.photos_without_source, 1, "photo 4 is reported, not tagged");
        assert_eq!(tags_on(&c, 4), Vec::<i64>::new(), "and really not tagged");
        assert_eq!(tags_on(&c, 3), vec![source], "an unnamed photo keeps the source");
        assert_eq!(tags_on(&c, 1), vec![report.new_tag_id]);
    }

    /// `keep_source` is the "both apply" case — the photo ends up carrying two tags.
    #[test]
    fn split_can_keep_the_source_tag() {
        let c = conn();
        let source = tag(&c, "Concert");
        photo(&c, 1);
        assign(&c, 1, source, 10);

        let report = split_tag(&c, source, &[1], "Venue/Grieghallen", true, 100).unwrap();

        assert_eq!(report.photos_untagged, 0);
        let mut expected = vec![source, report.new_tag_id];
        expected.sort();
        assert_eq!(tags_on(&c, 1), expected);
    }

    /// Splitting into an existing tag is normal — the other half may already exist.
    #[test]
    fn split_into_an_existing_tag_does_not_create_one() {
        let c = conn();
        let source = tag(&c, "Concert");
        let existing = tag(&c, "Venue");
        photo(&c, 1);
        assign(&c, 1, source, 10);

        let report = split_tag(&c, source, &[1], "Venue", false, 100).unwrap();
        assert!(!report.created_new_tag);
        assert_eq!(report.new_tag_id, existing);
    }

    // ── Finders ─────────────────────────────────────────────────────────────────

    /// An empty leaf is litter; an empty branch over empty children is structure. A branch
    /// whose descendants hold photos is not an orphan at all — deleting it would cascade
    /// those descendants away.
    #[test]
    fn orphan_finder_separates_empty_leaves_from_empty_branches() {
        let c = conn();
        let used = tag(&c, "Place/Bergen");
        let _empty_leaf = tag(&c, "Place/Nowhere");
        let branch = tag(&c, "Unused");
        let _under_branch = tag(&c, "Unused/Also");
        photo(&c, 1);
        assign(&c, 1, used, 10);

        let orphans = find_orphan_tags(&c).unwrap();
        let paths: Vec<&str> = orphans.iter().map(|o| o.path.as_str()).collect();

        assert!(paths.contains(&"Place/Nowhere"));
        assert!(paths.contains(&"Unused"));
        assert!(paths.contains(&"Unused/Also"));
        assert!(!paths.contains(&"Place/Bergen"), "it holds a photo");
        assert!(
            !paths.contains(&"Place"),
            "a branch whose descendant holds photos is not an orphan"
        );
        let branch_row = orphans.iter().find(|o| o.id == branch).unwrap();
        assert!(branch_row.has_children, "the UI needs to tell these apart");
    }

    /// Near-identical names surface; unrelated ones do not; an exact name in two places is
    /// called out for what it is.
    #[test]
    fn similar_tag_finder_ranks_the_obvious_duplicates_first() {
        let c = conn();
        tag(&c, "Sport/Cycling");
        tag(&c, "Sport/Cyceling"); // typo
        tag(&c, "Place/Bergen");
        tag(&c, "People/Bergen"); // same leaf, different axis
        tag(&c, "Food/Pizza");

        let pairs = find_similar_tags(&c, 0.8).unwrap();
        let names: Vec<(&str, &str)> = pairs
            .iter()
            .map(|p| (p.a_path.as_str(), p.b_path.as_str()))
            .collect();

        assert_eq!(
            names.first(),
            Some(&("People/Bergen", "Place/Bergen")),
            "an exact leaf match outranks a typo: {names:?}"
        );
        assert!(
            names.contains(&("Sport/Cyceling", "Sport/Cycling")),
            "the typo pair surfaces, ordered by path: {names:?}"
        );
        assert!(
            !names.iter().any(|(a, b)| *a == "Food/Pizza" || *b == "Food/Pizza"),
            "unrelated names do not: {names:?}"
        );
    }

    /// Co-occurrence rides along on a candidate pair — it is what tells a typo apart from
    /// two real concepts — but it is not what finds the pair. Names that look nothing alike
    /// stay out, however often they are used together, and the doc comment says so.
    #[test]
    fn similar_tag_finder_reports_co_occurrence_but_does_not_search_by_it() {
        let c = conn();
        let a = tag(&c, "Sport/Cycling");
        let b = tag(&c, "Sport/Cyceling");
        let unlike_a = tag(&c, "Bike");
        let unlike_b = tag(&c, "Velocipede");
        for id in 1..=12 {
            photo(&c, id);
            assign(&c, id, a, 10);
            assign(&c, id, b, 10);
            assign(&c, id, unlike_a, 10);
            assign(&c, id, unlike_b, 10);
        }

        let pairs = find_similar_tags(&c, 0.8).unwrap();

        let typo = pairs
            .iter()
            .find(|p| [p.a_id, p.b_id].contains(&a) && [p.a_id, p.b_id].contains(&b))
            .expect("the typo pair surfaces on names");
        assert_eq!(typo.co_occurrence, 12, "and carries the co-occurrence signal");
        assert_eq!(typo.a_photos, 12);
        assert!(typo.reason.contains("12 photos carry both"), "{}", typo.reason);

        assert!(
            !pairs
                .iter()
                .any(|p| [p.a_id, p.b_id].contains(&unlike_a)
                    && [p.a_id, p.b_id].contains(&unlike_b)),
            "unlike names stay out even at 12/12 co-occurrence — a documented limit"
        );
    }

    #[test]
    fn name_similarity_is_a_normalized_edit_distance() {
        assert_eq!(name_similarity("bergen", "bergen"), 1.0);
        assert!(name_similarity("cycling", "cyceling") > 0.8);
        assert!(name_similarity("pizza", "bergen") < 0.3);
        assert_eq!(name_similarity("", "bergen"), 0.0);
    }
}
