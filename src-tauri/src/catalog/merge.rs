//! Additive merge engine (F1c): apply a parsed bundle [`BundleManifest`] to this
//! catalog. This is the metadata half of the laptop ⇄ desktop merge (Epic F, see
//! `docs/storage-and-import.md`, "Catalog topology & merge"). The physical half —
//! copying originals into place — is the importer's job (F1d); this module is
//! **pure-DB, no file IO**.
//!
//! ## The binding invariant: additive only
//!
//! The merge **never deletes or overwrites** anything the catalog already has. It
//! only *adds*:
//!
//! - **Photos** are matched by `photos.uuid`. An existing photo row is left
//!   completely untouched (rating, label, pick, IPTC, edits, versions all preserved);
//!   only its tag assignments are *unioned* (new tags added, none removed). A photo
//!   the catalog has never seen is inserted with the bundle's uuid and its full state.
//! - **The tag taxonomy** is unioned: each bundle tag is resolved by `tags.uuid`
//!   first, then by normalized `full_path`; a missing tag (and any missing ancestors)
//!   is created, adopting the bundle's uuid. Terms are added if absent, never removed
//!   or re-flagged. An existing tag's `exportable`/uuid is left as-is.
//! - **The import batch** is inserted idempotently by `import_batches.uuid` — a
//!   re-merge of the same bundle finds the existing batch and changes nothing.
//! - **Tag assignments** union additively (a raw `INSERT OR IGNORE` — deliberately
//!   *not* [`Catalog::assign_tag`], which prunes redundant ancestors and would delete
//!   rows, breaking the additive invariant).
//! - **Versions / IPTC / rating / label / pick / edit record** travel only for
//!   **new** photos (the additive-only decision means no edit conflicts are possible:
//!   the laptop only adds new import batches, never checks out existing photos).
//!
//! Consequences: **re-merging the same bundle is a no-op**, and merging a bundle
//! whose photos/tags partly pre-exist adds only the genuinely new rows.
//!
//! The whole apply runs in a single transaction so a failure rolls back cleanly.

use super::tag_path::{normalize_lookup, normalize_tag_path};
use super::{Catalog, CatalogError, Result};
use crate::bundle::{BundleManifest, BundlePhoto, BundleTag};
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// What a merge did, for the importer to report. All counts are of rows the merge
/// actually created — nothing is ever removed, so there are no "deleted" counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeSummary {
    /// Photos inserted (the catalog had never seen these UUIDs).
    pub photos_added: usize,
    /// Photos matched to an existing row by UUID (left untouched; assignments unioned).
    pub photos_existing: usize,
    /// Tags created (leaf or ancestor) because neither the uuid nor the path existed.
    pub tags_created: usize,
    /// Terms (translations/synonyms) added to a tag that lacked them.
    pub terms_added: usize,
    /// Tag→photo assignment rows added by the union (across new and existing photos).
    pub assignments_added: usize,
    /// `true` if the batch was inserted, `false` if it already existed (re-merge).
    pub batch_added: bool,
}

impl Catalog {
    /// Apply a parsed bundle manifest to this catalog, additively (see the module
    /// docs). Pure-DB: it never touches the filesystem — placing originals is the
    /// importer's job. The whole apply is one transaction.
    pub fn merge_bundle(&self, manifest: &BundleManifest) -> Result<MergeSummary> {
        let tx = self.conn.unchecked_transaction()?;
        let summary = {
            let mut ctx = MergeCtx {
                tx: &tx,
                summary: MergeSummary::default(),
                tag_id_by_uuid: HashMap::new(),
            };
            ctx.run(manifest)?;
            ctx.summary
        };
        tx.commit()?;
        Ok(summary)
    }
}

/// Working state for one merge, so the helpers can share the transaction and the
/// resolved bundle-uuid → catalog tag-id map (built while unioning the taxonomy,
/// consumed while unioning assignments).
struct MergeCtx<'a> {
    tx: &'a Transaction<'a>,
    summary: MergeSummary,
    /// Maps each bundle tag uuid to the catalog tag id it resolved/created to.
    tag_id_by_uuid: HashMap<String, i64>,
}

impl MergeCtx<'_> {
    fn run(&mut self, manifest: &BundleManifest) -> Result<()> {
        // 1) Batch — idempotent by uuid; needed before photos so new photos get its id.
        let batch_id = self.merge_batch(manifest)?;

        // 2) Taxonomy — union tags + terms, populating tag_id_by_uuid for step 3.
        for tag in &manifest.taxonomy {
            let tag_id = self.merge_tag(tag)?;
            self.tag_id_by_uuid.insert(tag.uuid.clone(), tag_id);
            self.merge_terms(tag_id, tag)?;
        }

        // 3) Photos — insert new ones (full state), union assignments for all.
        for photo in &manifest.photos {
            self.merge_photo(photo, batch_id)?;
        }

        Ok(())
    }

    // --- batch --------------------------------------------------------------

    /// Insert the batch if its uuid is new; otherwise reuse the existing row. Returns
    /// the catalog batch id so new photos can be assigned to it.
    fn merge_batch(&mut self, manifest: &BundleManifest) -> Result<i64> {
        let batch = &manifest.batch;
        if let Some(id) = self
            .tx
            .query_row(
                "SELECT id FROM import_batches WHERE uuid = ?1",
                params![batch.uuid],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            return Ok(id);
        }
        self.tx.execute(
            "INSERT INTO import_batches(uuid, source_label, note, created_at)
             VALUES(?1, ?2, ?3, ?4)",
            params![batch.uuid, batch.source_label, batch.note, batch.created_at],
        )?;
        self.summary.batch_added = true;
        Ok(self.tx.last_insert_rowid())
    }

    // --- taxonomy -----------------------------------------------------------

    /// Resolve a bundle tag to a catalog tag id, creating it (and any missing
    /// ancestors) if neither its uuid nor its normalized full path exists yet.
    /// Matching order: uuid first (stable identity across renames), then normalized
    /// full_path (the fallback when the catalogs grew the same tag independently).
    /// An existing tag is left untouched — its uuid and `exportable` flag are never
    /// rewritten (additive only).
    fn merge_tag(&mut self, tag: &BundleTag) -> Result<i64> {
        // (a) By uuid — the strongest match.
        if !tag.uuid.is_empty() {
            if let Some(id) = self
                .tx
                .query_row(
                    "SELECT id FROM tags WHERE uuid = ?1",
                    params![tag.uuid],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
            {
                return Ok(id);
            }
        }

        // (b) By normalized full path — same concept grown independently. Create any
        // missing ancestors along the way; only the leaf carries the bundle metadata.
        // A malformed path is skipped-with-error (never silently mismatched).
        let normalized = normalize_tag_path(&tag.full_path).map_err(CatalogError::Tag)?;
        let mut parent_id: Option<i64> = None;
        let mut prefix: Vec<String> = Vec::new();
        let mut leaf_id: Option<i64> = None;

        for (idx, component) in normalized.components.iter().enumerate() {
            prefix.push(component.clone());
            let full_path = prefix.join("/");
            let full_path_norm = normalize_lookup(&full_path);
            let is_leaf = idx + 1 == normalized.components.len();

            let existing: Option<i64> = self
                .tx
                .query_row(
                    "SELECT id FROM tags WHERE full_path_norm = ?1",
                    params![full_path_norm],
                    |r| r.get(0),
                )
                .optional()?;

            let id = match existing {
                Some(id) => id,
                None => {
                    // Create it. The leaf adopts the bundle's uuid + exportable flag so
                    // its identity/interop semantics travel; intermediate ancestors get
                    // a fresh uuid and the default exportable (they aren't in the bundle
                    // as their own entry unless referenced separately, where uuid-match
                    // above already resolved them).
                    let ts = now();
                    let (uuid, exportable) = if is_leaf && !tag.uuid.is_empty() {
                        (tag.uuid.clone(), tag.exportable as i64)
                    } else {
                        (uuid::Uuid::new_v4().to_string(), 1)
                    };
                    self.tx.execute(
                        "INSERT INTO tags(uuid, name, name_norm, parent_id, full_path,
                            full_path_norm, exportable, created_at, updated_at)
                         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
                        params![
                            uuid,
                            component,
                            normalize_lookup(component),
                            parent_id,
                            full_path,
                            full_path_norm,
                            exportable,
                            ts
                        ],
                    )?;
                    self.summary.tags_created += 1;
                    self.tx.last_insert_rowid()
                }
            };

            parent_id = Some(id);
            if is_leaf {
                leaf_id = Some(id);
            }
        }

        leaf_id.ok_or_else(|| CatalogError::Tag("empty tag path".into()))
    }

    /// Union the bundle tag's terms onto the catalog tag: add any term (matched by the
    /// same (text_norm, language) key the schema's unique index uses) that isn't there.
    /// Existing terms are never modified or removed — a re-merge adds nothing.
    fn merge_terms(&mut self, tag_id: i64, tag: &BundleTag) -> Result<()> {
        for term in &tag.terms {
            let text = term.text.trim();
            if text.is_empty() {
                continue;
            }
            let text_norm = normalize_lookup(text);
            // ON CONFLICT DO NOTHING keeps it additive: an existing term (same tag,
            // text_norm, language) is left exactly as the catalog has it.
            let changed = self.tx.execute(
                "INSERT INTO tag_terms(tag_id, text, text_norm, language, is_primary, export, created_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(tag_id, text_norm, coalesce(language, '')) DO NOTHING",
                params![
                    tag_id,
                    text,
                    text_norm,
                    term.language,
                    term.is_primary as i64,
                    term.export as i64,
                    now()
                ],
            )?;
            self.summary.terms_added += changed;
        }
        Ok(())
    }

    // --- photos -------------------------------------------------------------

    /// Merge one photo: insert it with full state if the uuid is new, else leave the
    /// existing row untouched. Either way, union its tag assignments.
    fn merge_photo(&mut self, photo: &BundlePhoto, batch_id: i64) -> Result<()> {
        let existing: Option<i64> = self
            .tx
            .query_row(
                "SELECT id FROM photos WHERE uuid = ?1",
                params![photo.uuid],
                |r| r.get(0),
            )
            .optional()?;

        let photo_id = match existing {
            Some(id) => {
                // Existing photo: never overwritten. Only assignments union below.
                self.summary.photos_existing += 1;
                id
            }
            None => {
                let id = self.insert_photo(photo, batch_id)?;
                self.summary.photos_added += 1;
                id
            }
        };

        self.union_assignments(photo_id, photo)?;
        Ok(())
    }

    /// Insert a brand-new photo row carrying the bundle's uuid and full non-destructive
    /// state (rating/label/pick/IPTC + edit record + versions). No location is recorded
    /// — placing the bytes and recording where they live is the importer's job (F1d).
    fn insert_photo(&mut self, photo: &BundlePhoto, batch_id: i64) -> Result<i64> {
        let ts = now();
        let extension = std::path::Path::new(&photo.relative_path)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let iptc = &photo.iptc;

        self.tx.execute(
            "INSERT INTO photos(
                uuid, path, folder_id, mtime_ns, size, extension, missing,
                import_batch_id, rating, color_label, pick_state,
                iptc_description, iptc_headline, iptc_title, iptc_creator, iptc_copyright,
                iptc_credit, iptc_source, iptc_city, iptc_state, iptc_country,
                iptc_country_code, created_at, updated_at)
             VALUES(?1, ?2, NULL, 0, 0, ?3, 0, ?4, ?5, ?6, ?7,
                    ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?19)",
            params![
                photo.uuid,
                photo.relative_path,
                extension,
                batch_id,
                photo.rating,
                photo.label,
                photo.pick_state.as_db_str(),
                iptc.description,
                iptc.headline,
                iptc.title,
                iptc.creator,
                iptc.copyright,
                iptc.credit,
                iptc.source,
                iptc.city,
                iptc.state,
                iptc.country,
                iptc.country_code,
                ts,
            ],
        )?;
        let photo_id = self.tx.last_insert_rowid();

        // Edit record (photo-level), if the bundle carried one.
        if let Some(edit_json) = &photo.edit_record {
            let trimmed = edit_json.trim();
            if !trimmed.is_empty() {
                self.tx.execute(
                    "INSERT INTO photo_edits(photo_id, edit_json, updated_at)
                     VALUES(?1, ?2, ?3)",
                    params![photo_id, trimmed, ts],
                )?;
            }
        }

        // Named versions, preserving their relative order.
        for v in &photo.versions {
            let edit_json = {
                let t = v.edit_json.trim();
                if t.is_empty() { "{}" } else { t }
            };
            self.tx.execute(
                "INSERT INTO photo_versions(photo_id, name, edit_json, position, created_at, updated_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?5)",
                params![photo_id, v.name, edit_json, v.position, ts],
            )?;
        }

        Ok(photo_id)
    }

    /// Union the photo's tag assignments: add a `photo_tags` row for each referenced
    /// bundle tag that isn't already assigned. Deliberately a raw `INSERT OR IGNORE`
    /// (not [`Catalog::assign_tag`], which prunes ancestors) so the merge never deletes
    /// an assignment — additive only. Tags whose uuid didn't resolve (not in the
    /// manifest's taxonomy) are skipped.
    fn union_assignments(&mut self, photo_id: i64, photo: &BundlePhoto) -> Result<()> {
        let ts = now();
        for tag_uuid in &photo.tag_uuids {
            let Some(&tag_id) = self.tag_id_by_uuid.get(tag_uuid) else {
                // A referenced tag missing from the taxonomy slice — can't place it.
                eprintln!(
                    "merge: photo {} references tag uuid {} not in the manifest taxonomy; skipped",
                    photo.uuid, tag_uuid
                );
                continue;
            };
            let changed = self.tx.execute(
                "INSERT OR IGNORE INTO photo_tags(photo_id, tag_id, created_at)
                 VALUES(?1, ?2, ?3)",
                params![photo_id, tag_id, ts],
            )?;
            self.summary.assignments_added += changed;
        }
        Ok(())
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{
        BundleBatch, BundleManifest, BundlePhoto, BundleTag, BundleTagTerm, BundleVersion,
    };
    use crate::catalog::{IptcFields, PickState};

    fn temp_catalog(tag: &str) -> (Catalog, crate::test_support::TestSubPath) {
        let dir = crate::test_support::TestTmpDir::new(&format!("merge-{tag}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("test.chairphoto"), &root).unwrap();
        (catalog, dir.into_subpath("photos"))
    }

    /// A manifest with one tagged, rated, edited photo + a two-level taxonomy.
    fn sample_manifest() -> BundleManifest {
        let mut m = BundleManifest::new(
            BundleBatch {
                uuid: "batch-1".into(),
                source_label: "Trip".into(),
                note: "island".into(),
                created_at: 1_700_000_000,
            },
            1_700_100_000,
        );
        m.photos.push(BundlePhoto {
            uuid: "photo-a".into(),
            relative_path: "2026/06/28/DSC01234.ARW".into(),
            rating: 4,
            label: "green".into(),
            pick_state: PickState::Pick,
            iptc: IptcFields {
                headline: "Sunset".into(),
                city: "Bergen".into(),
                ..Default::default()
            },
            edit_record: Some(r#"{"basic-editor":{"exposure":0.3}}"#.into()),
            versions: vec![BundleVersion {
                name: "Insta".into(),
                edit_json: r#"{"crop":"1:1"}"#.into(),
                position: 0,
            }],
            tag_uuids: vec!["tag-owl".into()],
        });
        // The referenced leaf tag + its ancestor, so the path can be built without gaps.
        m.taxonomy.push(BundleTag {
            uuid: "tag-birds".into(),
            full_path: "Birds".into(),
            exportable: true,
            terms: Vec::new(),
        });
        m.taxonomy.push(BundleTag {
            uuid: "tag-owl".into(),
            full_path: "Birds/Owls".into(),
            exportable: true,
            terms: vec![BundleTagTerm {
                text: "Ugle".into(),
                language: Some("nb".into()),
                is_primary: true,
                export: true,
            }],
        });
        m
    }

    #[test]
    fn merge_adds_photo_batch_tags_and_assignments() {
        let (cat, _root) = temp_catalog("basic");
        let m = sample_manifest();
        let s = cat.merge_bundle(&m).unwrap();

        assert_eq!(s.photos_added, 1);
        assert_eq!(s.photos_existing, 0);
        assert!(s.batch_added);
        // Birds + Birds/Owls both created.
        assert_eq!(s.tags_created, 2);
        assert_eq!(s.terms_added, 1);
        assert_eq!(s.assignments_added, 1);

        // The photo landed with its full state.
        let photo = cat.get_photo_by_uuid("photo-a").unwrap();
        assert_eq!(photo.rating, 4);
        assert_eq!(photo.label, "green");
        assert_eq!(photo.pick_state, PickState::Pick);
        assert_eq!(photo.path, "2026/06/28/DSC01234.ARW");

        // Batch, IPTC, edit record, version, assignment all present.
        let iptc = cat.get_iptc(photo.id).unwrap();
        assert_eq!(iptc.headline, "Sunset");
        assert_eq!(iptc.city, "Bergen");
        assert_eq!(
            cat.get_edit_record(photo.id).unwrap().as_deref(),
            Some(r#"{"basic-editor":{"exposure":0.3}}"#)
        );
        let versions = cat.list_versions(photo.id).unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].name, "Insta");
        let tags = cat.get_photo_tags(photo.id).unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].full_path, "Birds/Owls");
        assert_eq!(tags[0].uuid, "tag-owl");
    }

    #[test]
    fn re_merge_is_a_no_op() {
        let (cat, _root) = temp_catalog("noop");
        let m = sample_manifest();

        let first = cat.merge_bundle(&m).unwrap();
        assert_eq!(first.photos_added, 1);
        assert!(first.batch_added);

        // Second application of the identical bundle changes nothing.
        let second = cat.merge_bundle(&m).unwrap();
        assert_eq!(second.photos_added, 0);
        assert_eq!(second.photos_existing, 1);
        assert!(!second.batch_added);
        assert_eq!(second.tags_created, 0);
        assert_eq!(second.terms_added, 0);
        assert_eq!(second.assignments_added, 0);

        // Row counts are stable — nothing duplicated.
        let count = |sql: &str| -> i64 {
            cat.conn().query_row(sql, [], |r| r.get(0)).unwrap()
        };
        assert_eq!(count("SELECT COUNT(*) FROM photos"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM import_batches"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM tags"), 2);
        assert_eq!(count("SELECT COUNT(*) FROM tag_terms"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM photo_tags"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM photo_versions"), 1);
    }

    #[test]
    fn existing_photo_is_never_overwritten_only_assignments_union() {
        let (cat, _root) = temp_catalog("preserve");
        // Seed a photo the "desktop" already has, matching photo-a's uuid but with
        // DIFFERENT local state (higher rating, a manual tag, its own version).
        cat.conn()
            .execute(
                "INSERT INTO photos(uuid, path, mtime_ns, size, extension, rating,
                    color_label, pick_state, iptc_headline, created_at, updated_at)
                 VALUES('photo-a', 'existing/local.ARW', 1, 1, 'arw', 5, 'red', 'reject',
                        'Local headline', 1, 1)",
                [],
            )
            .unwrap();
        let local = cat.get_photo_by_uuid("photo-a").unwrap();
        cat.set_edit_record(local.id, r#"{"local":true}"#).unwrap();
        let v = cat.create_version(local.id, "Local version").unwrap();
        cat.set_version_edit(v, r#"{"local":1}"#).unwrap();
        // A manual tag the bundle doesn't know about.
        let manual = cat.create_tag("Manual/Keep").unwrap();
        cat.conn()
            .execute(
                "INSERT INTO photo_tags(photo_id, tag_id, created_at) VALUES(?1, ?2, 1)",
                params![local.id, manual],
            )
            .unwrap();

        let s = cat.merge_bundle(&sample_manifest()).unwrap();
        assert_eq!(s.photos_added, 0);
        assert_eq!(s.photos_existing, 1);
        // The bundle's tag assignment (Birds/Owls) unions in.
        assert_eq!(s.assignments_added, 1);

        // The existing photo row is byte-for-byte preserved.
        let after = cat.get_photo_by_uuid("photo-a").unwrap();
        assert_eq!(after.id, local.id);
        assert_eq!(after.rating, 5, "rating not overwritten");
        assert_eq!(after.label, "red", "label not overwritten");
        assert_eq!(after.pick_state, PickState::Reject, "pick not overwritten");
        assert_eq!(after.path, "existing/local.ARW", "path not overwritten");
        assert_eq!(
            cat.get_iptc(after.id).unwrap().headline,
            "Local headline",
            "IPTC not overwritten"
        );
        assert_eq!(
            cat.get_edit_record(after.id).unwrap().as_deref(),
            Some(r#"{"local":true}"#),
            "edit record not overwritten"
        );
        // The local version is preserved; the bundle's version did NOT graft on.
        let versions = cat.list_versions(after.id).unwrap();
        assert_eq!(versions.len(), 1, "bundle version must not be added to existing photo");
        assert_eq!(versions[0].name, "Local version");

        // Assignments: the manual tag is kept AND the bundle's tag is added (union).
        let paths: Vec<String> = cat
            .get_photo_tags(after.id)
            .unwrap()
            .into_iter()
            .map(|t| t.full_path)
            .collect();
        assert!(paths.contains(&"Manual/Keep".to_string()), "manual tag preserved");
        assert!(paths.contains(&"Birds/Owls".to_string()), "bundle tag unioned in");
    }

    #[test]
    fn tag_unions_by_path_when_uuid_differs_and_keeps_existing() {
        let (cat, _root) = temp_catalog("pathmatch");
        // The desktop already grew "Birds/Owls" independently (different uuid), marked
        // non-exportable, with its own term.
        let owls = cat.create_tag("Birds/Owls").unwrap();
        cat.set_tag_exportable(owls, false).unwrap();
        cat.add_term(owls, "Existing", None, false, true).unwrap();
        let existing_uuid = cat.get_tag(owls).unwrap().uuid;

        let s = cat.merge_bundle(&sample_manifest()).unwrap();
        // Both Birds and Birds/Owls already exist by path → nothing created.
        assert_eq!(s.tags_created, 0);
        // The bundle's "Ugle" term is added to the existing tag.
        assert_eq!(s.terms_added, 1);

        // The existing tag's identity + exportable flag are untouched.
        let after = cat.get_tag(owls).unwrap();
        assert_eq!(after.uuid, existing_uuid, "existing uuid not overwritten");
        assert!(
            !cat.tag_exportable(owls).unwrap(),
            "existing exportable flag not overwritten"
        );
        // The photo's assignment resolved to the pre-existing tag id (union by path).
        let photo = cat.get_photo_by_uuid("photo-a").unwrap();
        let tags = cat.get_photo_tags(photo.id).unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].id, owls);
    }

    #[test]
    fn new_leaf_tag_adopts_bundle_uuid_and_exportable() {
        let (cat, _root) = temp_catalog("adopt");
        let mut m = sample_manifest();
        // Make the leaf non-exportable in the bundle; the ancestor "Birds" is new too.
        m.taxonomy[1].exportable = false;

        cat.merge_bundle(&m).unwrap();
        let owls = cat.find_tag_id_by_path("Birds/Owls").unwrap().unwrap();
        let tag = cat.get_tag(owls).unwrap();
        assert_eq!(tag.uuid, "tag-owl", "leaf adopts the bundle uuid");
        assert!(!cat.tag_exportable(owls).unwrap(), "leaf adopts exportable=false");

        // The auto-created ancestor got a fresh uuid + default exportable=true.
        let birds = cat.find_tag_id_by_path("Birds").unwrap().unwrap();
        assert!(cat.tag_exportable(birds).unwrap());
    }

    #[test]
    fn partial_overlap_adds_only_new_rows() {
        let (cat, _root) = temp_catalog("partial");
        // First bundle lands fully.
        cat.merge_bundle(&sample_manifest()).unwrap();

        // A second bundle: same batch + tag, but a brand-new second photo also tagged
        // with the (already-present) owl tag.
        let mut m2 = sample_manifest();
        m2.photos.push(BundlePhoto {
            uuid: "photo-b".into(),
            relative_path: "2026/06/29/DSC01300.ARW".into(),
            rating: 0,
            label: String::new(),
            pick_state: PickState::None,
            iptc: IptcFields::default(),
            edit_record: None,
            versions: Vec::new(),
            tag_uuids: vec!["tag-owl".into()],
        });

        let s = cat.merge_bundle(&m2).unwrap();
        assert_eq!(s.photos_added, 1, "only the new photo is added");
        assert_eq!(s.photos_existing, 1, "photo-a matched by uuid");
        assert!(!s.batch_added, "batch already present");
        assert_eq!(s.tags_created, 0, "taxonomy already present");
        assert_eq!(s.assignments_added, 1, "only the new photo's assignment");

        assert_eq!(
            cat.conn()
                .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        // The new photo shares the same batch as the first.
        let a = cat.get_photo_by_uuid("photo-a").unwrap();
        let b = cat.get_photo_by_uuid("photo-b").unwrap();
        let batch_of = |id: i64| -> i64 {
            cat.conn()
                .query_row(
                    "SELECT import_batch_id FROM photos WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(batch_of(a.id), batch_of(b.id));
    }

    #[test]
    fn assignment_to_tag_missing_from_taxonomy_is_skipped_not_fatal() {
        let (cat, _root) = temp_catalog("dangling");
        let mut m = sample_manifest();
        // Reference a tag uuid that isn't in the taxonomy slice.
        m.photos[0].tag_uuids.push("tag-ghost".into());

        let s = cat.merge_bundle(&m).unwrap();
        // Only the resolvable owl assignment lands; the ghost is skipped, no error.
        assert_eq!(s.assignments_added, 1);
        let photo = cat.get_photo_by_uuid("photo-a").unwrap();
        assert_eq!(cat.get_photo_tags(photo.id).unwrap().len(), 1);
    }
}
