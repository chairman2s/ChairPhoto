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
//! - **Photos** are matched by `photos.uuid`. A photo the catalog has never seen is
//!   inserted with the bundle's uuid and its full state. An existing photo keeps
//!   everything it has (owner decision on #185, 2026-10-04): the bundle only fills
//!   what it lacks — a rating of 0, an empty label, a pick of "none" — and its develop
//!   edit record and versions are added as **new versions** whose settings the photo
//!   does not already have; the photo's own edit record and versions are not changed.
//!   Its blank IPTC fields are filled too, but not here: a store to an existing row's
//!   IPTC owes its sidecar the change, so the fields the bundle offers are returned
//!   ([`MergeOutcome::iptc_fills`]) for the importer to store and write under the
//!   sidecar's write turn (`bundle::importer`). Tag assignments are *unioned* (new tags
//!   added, none removed). A row the importer created for this bundle a moment before
//!   ([`Catalog::merge_bundle_into`]'s `fresh`) already holds the bundle's state and is
//!   not filled again.
//! - **The tag taxonomy** is unioned: each bundle tag is resolved by `tags.uuid`
//!   first, then by normalized `full_path`; a missing tag (and any missing ancestors)
//!   is created, adopting the bundle's uuid. Terms are added if absent, never removed
//!   or re-flagged. An existing tag's `exportable`/uuid is left as-is.
//! - **The import batch** is inserted idempotently by `import_batches.uuid` — a
//!   re-merge of the same bundle finds the existing batch and changes nothing.
//! - **Tag assignments** union additively (a raw `INSERT OR IGNORE` — deliberately
//!   *not* [`Catalog::assign_tag`], which prunes redundant ancestors and would delete
//!   rows, breaking the additive invariant).
//! - **A photo with no row and no free path** — its identity is new here, but another
//!   photo holds its `relative_path` (the importer found the same capture there under
//!   another identity, #246/#185) — is kept apart: neither inserted (`photos.path` is
//!   UNIQUE) nor merged onto the photo at that path, whose identity differs. Counted in
//!   [`MergeSummary::photos_kept_apart`]. So is a photo with no row whose original the
//!   importer found in the library at another name (a ` (n)` one) under another identity
//!   ([`Catalog::merge_bundle_into`]'s `kept_apart`): its own path may be free, but the
//!   photo is in the library already, as the other row's — a row inserted there would
//!   describe a file that is not this photo's, or none.
//!
//! - **Once per bundle** (#248): every photo a bundle's photo is merged into is recorded
//!   with the bundle's batch uuid (`bundle_merges`). Merging that batch into it again fills
//!   nothing and adds no version — a rating the user cleared, or an imported version they
//!   deleted, after the first import stays cleared — and only its tags union again.
//!
//! Consequences: **re-merging the same bundle is a no-op** (a photo it was merged into is
//! not filled again, and tag assignments union idempotently), and merging a bundle whose
//! photos/tags partly pre-exist adds only what is genuinely new.
//!
//! The whole apply runs in a single transaction so a failure rolls back cleanly.

use super::tag_path::{normalize_lookup, normalize_tag_path};
use super::{Catalog, CatalogError, Result};
use crate::bundle::{BundleManifest, BundlePhoto, BundleTag};
use rusqlite::{params, OptionalExtension, Transaction};
use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

/// What a merge did, for the importer to report. All counts are of rows the merge
/// actually created — nothing is ever removed, so there are no "deleted" counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeSummary {
    /// Photos inserted (the catalog had never seen these UUIDs).
    pub photos_added: usize,
    /// Photos matched to an existing row by UUID (its values kept, its blanks filled,
    /// assignments unioned).
    pub photos_existing: usize,
    /// Tags created (leaf or ancestor) because neither the uuid nor the path existed.
    pub tags_created: usize,
    /// Terms (translations/synonyms) added to a tag that lacked them.
    pub terms_added: usize,
    /// Tag→photo assignment rows added by the union (across new and existing photos).
    pub assignments_added: usize,
    /// `true` if the batch was inserted, `false` if it already existed (re-merge).
    pub batch_added: bool,
    /// Photos with no identity (a blank uuid from a pre-#146 bundle) and no original in the
    /// bundle, whose path another photo already holds: neither matched nor inserted (#150).
    pub photos_skipped: usize,
    /// Existing photos the bundle filled something in on: a blank rating, label or pick, or
    /// a new version (#185). Their blank IPTC is filled by the importer, not counted here.
    pub photos_filled: usize,
    /// Versions added to existing photos from the bundle's edit records and versions.
    pub versions_added: usize,
    /// Photos whose identity no row holds while another photo, under another identity,
    /// holds their path — the same capture imported separately on each side (#246): kept
    /// apart, neither inserted nor merged.
    pub photos_kept_apart: usize,
    /// Existing photos this bundle's batch was merged into by an earlier import (#248): not
    /// filled again and given no versions, so a value the user cleared since stays cleared.
    /// Their tags still union.
    pub photos_merged_before: usize,
}

/// The name of the version an existing photo gains from a bundle's photo-level edit record.
pub const IMPORTED_EDIT_VERSION: &str = "Imported edit";

/// What [`Catalog::merge_bundle_into`] did, and what it leaves the caller to do.
#[derive(Debug, Clone, Default)]
pub struct MergeOutcome {
    pub summary: MergeSummary,
    /// For each existing photo (not `fresh`), the bundle's IPTC fields that the row has no
    /// value for. Not stored: the caller stores what the photo's sidecar has no value for
    /// either, through `Catalog::set_iptc`, under the sidecar's write turn.
    pub iptc_fills: Vec<(i64, super::IptcFields)>,
    /// The photos versions were added to, existing or new — each owes the monochrome
    /// refresh a version write owes.
    pub versions_added_to: Vec<i64>,
}

impl Catalog {
    /// Apply a parsed bundle manifest to this catalog, additively (see the module
    /// docs). Pure-DB: it never touches the filesystem — placing originals is the
    /// importer's job. The whole apply is one transaction.
    pub fn merge_bundle(&self, manifest: &BundleManifest) -> Result<MergeSummary> {
        Ok(self.merge_bundle_into(manifest, &HashSet::new(), &HashSet::new())?.summary)
    }

    /// [`Self::merge_bundle`], told which rows the caller created for this bundle's photos
    /// just before (`fresh`): those already carry the bundle's state, so an existing photo
    /// in `fresh` is not filled again. `kept_apart` holds the identities (canonical, as
    /// [`super::photo_identity_for`] gives them) of the bundle's photos the caller kept apart:
    /// their original is in the library already under another identity, so one with no row is
    /// kept apart here too ([`MergeSummary::photos_kept_apart`]) rather than inserted at its
    /// path. Returns the IPTC fills left to the caller (see [`MergeOutcome`]).
    pub fn merge_bundle_into(
        &self,
        manifest: &BundleManifest,
        fresh: &HashSet<i64>,
        kept_apart: &HashSet<String>,
    ) -> Result<MergeOutcome> {
        let tx = self.conn.unchecked_transaction()?;
        let outcome = {
            let mut ctx = MergeCtx {
                tx: &tx,
                outcome: MergeOutcome::default(),
                tag_id_by_uuid: HashMap::new(),
                fresh,
                kept_apart,
                batch_uuid: manifest.batch.uuid.trim(),
            };
            ctx.run(manifest)?;
            ctx.outcome
        };
        tx.commit()?;
        Ok(outcome)
    }
}

/// Working state for one merge, so the helpers can share the transaction and the
/// resolved bundle-uuid → catalog tag-id map (built while unioning the taxonomy,
/// consumed while unioning assignments).
struct MergeCtx<'a> {
    tx: &'a Transaction<'a>,
    outcome: MergeOutcome,
    /// Maps each bundle tag uuid to the catalog tag id it resolved/created to.
    tag_id_by_uuid: HashMap<String, i64>,
    /// Rows the importer created for this bundle's photos (see `merge_bundle_into`).
    fresh: &'a HashSet<i64>,
    /// Identities the importer kept apart (see `merge_bundle_into`).
    kept_apart: &'a HashSet<String>,
    /// The bundle's batch uuid, which `bundle_merges` records per photo (#248). Blank: a
    /// bundle with no batch identity, recorded nowhere.
    batch_uuid: &'a str,
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
        self.outcome.summary.batch_added = true;
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
            // (a2) By tombstone — this uuid was merged away here (A5). Without this the
            // taxonomy union would treat it as a tag we have never seen and re-create it,
            // silently undoing the merge on the next bundle import. Resolving to the merge
            // target instead means the bundle's photos land on the tag that replaced it.
            if let Some(id) = self
                .tx
                .query_row(
                    "SELECT target_tag_id FROM tag_aliases WHERE dead_uuid = ?1",
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
                    // Bundles don't carry the private flag, so a created tag inherits
                    // the local parent's padlock — a name merged under a padlocked
                    // "People" must not arrive cloud-visible.
                    let private = super::inherited_private(&self.tx, parent_id)?;
                    self.tx.execute(
                        "INSERT INTO tags(uuid, name, name_norm, parent_id, full_path,
                            full_path_norm, exportable, private, created_at, updated_at)
                         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                        params![
                            uuid,
                            component,
                            normalize_lookup(component),
                            parent_id,
                            full_path,
                            full_path_norm,
                            exportable,
                            private as i64,
                            ts
                        ],
                    )?;
                    self.outcome.summary.tags_created += 1;
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
            self.outcome.summary.terms_added += changed;
        }
        Ok(())
    }

    // --- photos -------------------------------------------------------------

    /// Merge one photo: insert it with full state if the uuid is new, else fill in what the
    /// existing row lacks ([`Self::fill_existing`]). Either way, union its tag assignments.
    ///
    /// A UUID is matched in its canonical lowercase spelling (#146), so a bundle that carries
    /// it in another case still finds the photo. A bundle written before #146 can carry a
    /// non-UUID id that an older catalog had adopted; it is matched and stored as its
    /// [`super::legacy_photo_identity`] — the identity schema v23 gave that photo in every
    /// catalog — and recorded as the row's legacy identifier unless another row already
    /// holds it (#150; see [`super::identity::RECORD_LEGACY_IDENTIFIER_SQL`]), so it never
    /// becomes a non-UUID `photos.uuid` again.
    ///
    /// A photo with an empty or blank uuid has no identity to match (#146 review N4), so it is
    /// never matched to another catalog's such photo, and never to whatever photo this
    /// catalog has at its path either (#150): that can be a different photo — the importer
    /// renames a different-size collision to ` (n)`, and the photo already at the path is
    /// the user's own. The bundle importer, which knows which row it indexed each blank-uuid
    /// original into, hands merge that row's identity instead
    /// (`bundle::importer::index_bundle_with`). A blank-uuid photo that still arrives here is
    /// one without an original in the bundle: inserted with a fresh v4, as schema v23 does
    /// for such a row, when its path is free, and otherwise skipped
    /// ([`MergeSummary::photos_skipped`]) — a second row cannot take the path
    /// (`photos.path` is UNIQUE), and nothing says the photo there is this one.
    fn merge_photo(&mut self, photo: &BundlePhoto, batch_id: i64) -> Result<()> {
        let (uuid, existing): (String, Option<i64>) = match super::photo_identity_for(&photo.uuid) {
            Some(uuid) => {
                let existing = self
                    .tx
                    .query_row("SELECT id FROM photos WHERE uuid = ?1", params![uuid], |r| r.get(0))
                    .optional()?;
                (uuid, existing)
            }
            None => {
                if self.path_taken(&photo.relative_path)? {
                    eprintln!(
                        "bundle merge: a photo with no identity at {} was skipped: another \
                         photo is at that path",
                        photo.relative_path
                    );
                    self.outcome.summary.photos_skipped += 1;
                    return Ok(());
                }
                (uuid::Uuid::new_v4().to_string(), None)
            }
        };

        let photo_id = match existing {
            Some(id) => {
                // Existing photo: never overwritten; only what it lacks is filled in — once per
                // bundle batch (#248).
                self.outcome.summary.photos_existing += 1;
                if self.fresh.contains(&id) {
                    // Created by the importer for this bundle a moment ago, with its state.
                } else if self.merged_before(id)? {
                    self.outcome.summary.photos_merged_before += 1;
                } else {
                    self.fill_existing(id, photo)?;
                }
                id
            }
            None if self.kept_apart.contains(&uuid) || self.path_taken(&photo.relative_path)? => {
                // No row holds this identity, but another photo holds the path, or the importer
                // found this photo's original in the library under another identity (#246) —
                // perhaps at a ` (n)` name, its own path free. It is neither this photo's row
                // nor a free path — keep the two apart.
                eprintln!(
                    "bundle merge: photo {uuid} kept apart: another photo, under another identity, \
                     is at {}",
                    photo.relative_path
                );
                self.outcome.summary.photos_kept_apart += 1;
                return Ok(());
            }
            None => {
                let id = self.insert_photo(photo, &uuid, batch_id)?;
                self.outcome.summary.photos_added += 1;
                id
            }
        };

        if super::identity::is_legacy_identifier(&photo.uuid) {
            self.tx.execute(
                super::identity::RECORD_LEGACY_IDENTIFIER_SQL,
                params![photo_id, photo.uuid],
            )?;
        }

        self.record_merged(photo_id)?;
        self.union_assignments(photo_id, photo)?;
        Ok(())
    }

    /// Whether this bundle's batch was merged into `photo_id` by an earlier import (#248).
    fn merged_before(&self, photo_id: i64) -> Result<bool> {
        if self.batch_uuid.is_empty() {
            return Ok(false);
        }
        Ok(self
            .tx
            .query_row(
                "SELECT 1 FROM bundle_merges WHERE photo_id = ?1 AND batch_uuid = ?2",
                params![photo_id, self.batch_uuid],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Record that this bundle's batch has been merged into `photo_id` (#248), so importing
    /// it again fills nothing in and adds no version there.
    fn record_merged(&self, photo_id: i64) -> Result<()> {
        if self.batch_uuid.is_empty() {
            return Ok(());
        }
        self.tx.execute(
            "INSERT OR IGNORE INTO bundle_merges(photo_id, batch_uuid, merged_at) VALUES(?1, ?2, ?3)",
            params![photo_id, self.batch_uuid, now()],
        )?;
        Ok(())
    }

    /// Whether a photo already holds `relative_path` (`photos.path` is UNIQUE).
    fn path_taken(&self, relative_path: &str) -> Result<bool> {
        Ok(self
            .tx
            .query_row("SELECT 1 FROM photos WHERE path = ?1", params![relative_path], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Fill in what an existing photo lacks from the bundle's photo (#185, owner decision
    /// 2026-10-04). The photo's own values always win:
    /// - **Culling:** a rating of 0, an empty label and a pick of "none" take the bundle's.
    /// - **Edits:** the bundle's photo-level edit record and each of its versions are added as
    ///   new versions, after the photo's own, unless the photo already has those settings (its
    ///   edit record or a version with an equal JSON value) — so a re-merge adds nothing. The
    ///   photo's edit record and versions are not changed. The edit record's version is named
    ///   [`IMPORTED_EDIT_VERSION`]; a version keeps its name.
    /// - **IPTC:** the bundle's fields the row has no value for are returned in
    ///   [`MergeOutcome::iptc_fills`], not stored (see there).
    fn fill_existing(&mut self, photo_id: i64, photo: &BundlePhoto) -> Result<()> {
        let culled = self.tx.execute(
            "UPDATE photos SET
                rating = CASE WHEN rating = 0 THEN ?2 ELSE rating END,
                color_label = CASE WHEN color_label = '' THEN ?3 ELSE color_label END,
                pick_state = CASE WHEN pick_state = 'none' THEN ?4 ELSE pick_state END,
                updated_at = ?5
             WHERE id = ?1
               AND ((rating = 0 AND ?2 <> 0) OR (color_label = '' AND ?3 <> '')
                    OR (pick_state = 'none' AND ?4 <> 'none'))",
            params![photo_id, photo.rating, photo.label, photo.pick_state.as_db_str(), now()],
        )? > 0;

        // Settings the photo already has, as JSON values (so formatting is no difference).
        let parse = |json: &str| serde_json::from_str::<serde_json::Value>(json.trim()).ok();
        let mut has: Vec<serde_json::Value> = Vec::new();
        {
            let mut stmt = self.tx.prepare(
                "SELECT edit_json FROM photo_edits WHERE photo_id = ?1
                 UNION ALL SELECT edit_json FROM photo_versions WHERE photo_id = ?1",
            )?;
            let rows = stmt.query_map(params![photo_id], |r| r.get::<_, String>(0))?;
            for json in rows {
                has.extend(parse(&json?));
            }
        }
        // A blank edit record is no edit (as `insert_photo` treats it), so no version.
        let offered = photo
            .edit_record
            .iter()
            .filter(|e| !e.trim().is_empty())
            .map(|e| (IMPORTED_EDIT_VERSION, e.as_str()))
            .chain(photo.versions.iter().map(|v| (v.name.as_str(), v.edit_json.as_str())));
        let mut added = 0;
        for (name, edit_json) in offered {
            let trimmed = edit_json.trim();
            let value = if trimmed.is_empty() { "{}" } else { trimmed };
            // Not JSON: the bundle is malformed there; a version must hold valid JSON.
            let Some(parsed) = parse(value) else { continue };
            if has.contains(&parsed) {
                continue;
            }
            let name = if name.trim().is_empty() { IMPORTED_EDIT_VERSION } else { name };
            let ts = now();
            self.tx.execute(
                "INSERT INTO photo_versions(photo_id, name, edit_json, position, created_at, updated_at)
                 VALUES(?1, ?2, ?3,
                        (SELECT COALESCE(MAX(position), -1) + 1 FROM photo_versions WHERE photo_id = ?1),
                        ?4, ?4)",
                params![photo_id, name, value, ts],
            )?;
            // Not a change made here: an existing photo's face stays where it is (#252,
            // decision 2026-10-06). The version keeps `changed_seq` 0 — never written in this
            // catalog — which the automatic face passes over until it is edited here
            // (`edits::face_under`). A stale bundle never regresses the face to an older look.
            has.push(parsed);
            added += 1;
        }
        if added > 0 {
            self.outcome.summary.versions_added += added;
            self.outcome.versions_added_to.push(photo_id);
        }
        if culled || added > 0 {
            self.outcome.summary.photos_filled += 1;
        }

        let current: super::IptcFields = self.tx.query_row(
            "SELECT iptc_description, iptc_headline, iptc_title, iptc_creator, iptc_copyright,
                iptc_credit, iptc_source, iptc_city, iptc_state, iptc_country, iptc_country_code
             FROM photos WHERE id = ?1",
            params![photo_id],
            |r| {
                Ok(super::IptcFields {
                    description: r.get(0)?,
                    headline: r.get(1)?,
                    title: r.get(2)?,
                    creator: r.get(3)?,
                    copyright: r.get(4)?,
                    credit: r.get(5)?,
                    source: r.get(6)?,
                    city: r.get(7)?,
                    state: r.get(8)?,
                    country: r.get(9)?,
                    country_code: r.get(10)?,
                })
            },
        )?;
        let blank_here = super::IptcMask::present_in(&photo.iptc).without(super::IptcMask::present_in(&current));
        if !blank_here.is_empty() {
            let mut fill = super::IptcFields::default();
            for m in super::IptcMask::EACH.into_iter().filter(|m| blank_here.contains(*m)) {
                if let Some(slot) = m.value_mut(&mut fill) {
                    *slot = m.value(&photo.iptc).to_string();
                }
            }
            self.outcome.iptc_fills.push((photo_id, fill));
        }
        Ok(())
    }

    /// Insert a brand-new photo row carrying the bundle's uuid and full non-destructive
    /// state (rating/label/pick/IPTC + edit record + versions). No location is recorded
    /// — placing the bytes and recording where they live is the importer's job (F1d).
    fn insert_photo(&mut self, photo: &BundlePhoto, uuid: &str, batch_id: i64) -> Result<i64> {
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
                uuid,
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
            // In bundle order, so the last one is the new photo's automatic face (#252).
            super::edits::settings_written(self.tx, self.tx.last_insert_rowid())?;
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
            self.outcome.summary.assignments_added += changed;
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

    // --- an existing photo (#185) -------------------------------------------------------

    /// An existing photo keeps every value it has — culling, IPTC, its edit record and its
    /// version — and gains the bundle's edit record and version as new versions after its
    /// own; its tags union. Merging again adds nothing more.
    // --- the Library face (#252): `photo_cover` is local; a new photo's merged versions move it

    #[test]
    fn merged_versions_give_a_new_photo_its_face_and_leave_a_pin_alone() {
        let (cat, _root) = temp_catalog("face");
        cat.merge_bundle(&sample_manifest()).unwrap();
        let photo = cat.get_photo_by_uuid("photo-a").unwrap();
        let versions = cat.list_versions(photo.id).unwrap();
        let last = versions.last().unwrap().id;
        assert_eq!(cat.cover_of(photo.id).unwrap().map(|f| f.0), Some(last), "the last merged version");
        assert_eq!(photo.cover_pin, crate::catalog::CoverPin::Auto);

        // A pinned face stays where the user put it when another merge adds versions.
        cat.set_cover_pin(photo.id, crate::catalog::CoverPin::Original).unwrap();
        let mut more = sample_manifest();
        more.batch.uuid = "batch-2".into();
        more.photos[0].versions[0].edit_json = r#"{"crop":"4:5"}"#.into();
        cat.merge_bundle(&more).unwrap();
        assert_eq!(cat.list_versions(photo.id).unwrap().len(), versions.len() + 1, "a version was added");
        assert_eq!(cat.cover_of(photo.id).unwrap(), None, "the pinned original stays the face");
    }

    /// #252 (decision 2026-10-06; the merge test the #252 review found missing): versions
    /// merged into an EXISTING photo do not move its face. A photo showing its own edited
    /// version keeps showing it, rev unchanged; a photo with no versions keeps its original,
    /// even when a later face refresh runs. Editing a merged version here makes it the face.
    #[test]
    fn versions_merged_into_an_existing_photo_leave_its_face_alone() {
        let face = |cat: &Catalog, id: i64| cat.cover_of(id).unwrap().map(|f| (f.0, f.1));
        let insert = |cat: &Catalog| {
            cat.conn()
                .execute(
                    "INSERT INTO photos(uuid, path, mtime_ns, size, extension, created_at, updated_at)
                     VALUES(?1, 'existing/local.ARW', 1, 1, 'arw', 1, 1)",
                    params![crate::catalog::photo_identity_for("photo-a").unwrap()],
                )
                .unwrap();
            cat.conn().last_insert_rowid()
        };

        // Its own edited version is the face, and stays so.
        let (cat, _root) = temp_catalog("face-existing");
        let id = insert(&cat);
        let mine = cat.create_version(id, "Mine").unwrap();
        cat.set_version_edit(mine, r#"{"local":1}"#).unwrap();
        let before = face(&cat, id);
        assert_eq!(before.map(|f| f.0), Some(mine));
        let out = cat.merge_bundle_into(&sample_manifest(), &HashSet::new(), &HashSet::new()).unwrap();
        assert_eq!(out.summary.versions_added, 2);
        assert_eq!(face(&cat, id), before, "the face and its rev are unchanged");

        // No versions of its own: the original stays the face, through a later refresh too.
        let (cat, _root) = temp_catalog("face-original");
        let id = insert(&cat);
        cat.merge_bundle_into(&sample_manifest(), &HashSet::new(), &HashSet::new()).unwrap();
        let merged = cat.list_versions(id).unwrap();
        assert_eq!(merged.len(), 2);
        assert_eq!(face(&cat, id), None, "the original is still the face");
        cat.delete_version(merged[0].id).unwrap();
        assert_eq!(face(&cat, id), None, "a refresh does not pick a merged version");
        assert_eq!(cat.cover_pin(id).unwrap(), crate::catalog::CoverPin::Auto, "and nothing was pinned");

        // An edit here is a change here: the face follows it.
        cat.set_version_edit(merged[1].id, r#"{"crop":"4:5"}"#).unwrap();
        assert_eq!(face(&cat, id).map(|f| f.0), Some(merged[1].id));
    }

    #[test]
    fn existing_photo_keeps_its_values_and_gains_the_bundles_edits_as_new_versions() {
        let (cat, _root) = temp_catalog("preserve");
        // Seed a photo the "desktop" already has, matching photo-a's identity but with
        // DIFFERENT local state (higher rating, a manual tag, its own version). The fixture's
        // "photo-a" is not a UUID, so the identity it names is its legacy mapping (#146).
        cat.conn()
            .execute(
                "INSERT INTO photos(uuid, path, mtime_ns, size, extension, rating,
                    color_label, pick_state, iptc_headline, created_at, updated_at)
                 VALUES(?1, 'existing/local.ARW', 1, 1, 'arw', 5, 'red', 'reject',
                        'Local headline', 1, 1)",
                params![crate::catalog::photo_identity_for("photo-a").unwrap()],
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

        let out = cat.merge_bundle_into(&sample_manifest(), &HashSet::new(), &HashSet::new()).unwrap();
        let s = &out.summary;
        assert_eq!(s.photos_added, 0);
        assert_eq!(s.photos_existing, 1);
        // The bundle's tag assignment (Birds/Owls) unions in.
        assert_eq!(s.assignments_added, 1);
        assert_eq!((s.photos_filled, s.versions_added), (1, 2));
        assert_eq!(out.versions_added_to, vec![local.id]);
        // IPTC is not stored by the merge: only the field the row has no value for (City;
        // the Headline is the row's own) is handed back for the sidecar-safe fill.
        assert_eq!(
            out.iptc_fills,
            vec![(local.id, IptcFields { city: "Bergen".into(), ..Default::default() })]
        );
        assert_eq!(cat.get_iptc(local.id).unwrap().city, "", "not stored by the merge");

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
        // The local version is preserved, first; the bundle's edit record and version follow
        // as new versions.
        let versions: Vec<(String, String)> =
            cat.list_versions(after.id).unwrap().into_iter().map(|v| (v.name, v.edit_json)).collect();
        assert_eq!(
            versions,
            [
                ("Local version".to_string(), r#"{"local":1}"#.to_string()),
                (IMPORTED_EDIT_VERSION.to_string(), r#"{"basic-editor":{"exposure":0.3}}"#.to_string()),
                ("Insta".to_string(), r#"{"crop":"1:1"}"#.to_string()),
            ]
        );

        // A second merge adds no version: the photo has those settings now.
        let again = cat.merge_bundle_into(&sample_manifest(), &HashSet::new(), &HashSet::new()).unwrap();
        assert_eq!((again.summary.versions_added, again.summary.photos_filled), (0, 0));
        assert_eq!(cat.list_versions(after.id).unwrap().len(), 3);

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

    /// A blank rating, label and pick take the bundle's; a version whose settings the photo
    /// already has (its edit record, in another JSON spelling) is not added again.
    #[test]
    fn an_existing_photos_blank_culling_is_filled_and_known_settings_are_not_versioned_again() {
        let (cat, _root) = temp_catalog("fill");
        cat.conn()
            .execute(
                "INSERT INTO photos(uuid, path, mtime_ns, size, extension, rating, created_at, updated_at)
                 VALUES(?1, 'existing/local.ARW', 1, 1, 'arw', 2, 1, 1)",
                params![crate::catalog::photo_identity_for("photo-a").unwrap()],
            )
            .unwrap();
        let local = cat.get_photo_by_uuid("photo-a").unwrap();
        cat.set_edit_record(local.id, r#"{ "basic-editor": { "exposure": 0.3 } }"#).unwrap();

        let out = cat.merge_bundle_into(&sample_manifest(), &HashSet::new(), &HashSet::new()).unwrap();
        let after = cat.get_photo(local.id).unwrap();
        assert_eq!(after.rating, 2, "a rating it has wins");
        assert_eq!(after.label, "green", "a blank label is filled");
        assert_eq!(after.pick_state, PickState::Pick, "a blank pick is filled");
        let names: Vec<String> = cat.list_versions(local.id).unwrap().into_iter().map(|v| v.name).collect();
        assert_eq!(names, ["Insta"], "the edit record it already has is not added again");
        assert_eq!(out.summary.versions_added, 1);
        assert_eq!(
            out.iptc_fills,
            vec![(local.id, IptcFields { headline: "Sunset".into(), city: "Bergen".into(), ..Default::default() })]
        );
    }

    /// L-2 of the #246/#185 review: a blank edit record (empty or whitespace) is no edit — an
    /// existing photo gains no "Imported edit" version from it, as a new photo gains no edit
    /// record. Its versions still arrive.
    #[test]
    fn a_blank_edit_record_adds_no_version() {
        let (cat, _root) = temp_catalog("blank-edit");
        cat.conn()
            .execute(
                "INSERT INTO photos(uuid, path, mtime_ns, size, extension, created_at, updated_at)
                 VALUES(?1, 'existing/local.ARW', 1, 1, 'arw', 1, 1)",
                params![crate::catalog::photo_identity_for("photo-a").unwrap()],
            )
            .unwrap();
        let id = cat.conn().last_insert_rowid();
        for blank in ["", "   "] {
            let mut manifest = sample_manifest();
            manifest.photos[0].edit_record = Some(blank.into());
            manifest.photos[0].versions.clear();
            let out = cat.merge_bundle_into(&manifest, &HashSet::new(), &HashSet::new()).unwrap();
            assert_eq!(out.summary.versions_added, 0, "{blank:?}");
            assert!(cat.list_versions(id).unwrap().is_empty(), "{blank:?}");
        }
    }

    /// A row the importer created for this bundle (`fresh`) already has the bundle's state:
    /// nothing is filled in on it again.
    #[test]
    fn a_fresh_row_is_not_filled_again() {
        let (cat, _root) = temp_catalog("fresh");
        cat.conn()
            .execute(
                "INSERT INTO photos(uuid, path, mtime_ns, size, extension, created_at, updated_at)
                 VALUES(?1, 'existing/local.ARW', 1, 1, 'arw', 1, 1)",
                params![crate::catalog::photo_identity_for("photo-a").unwrap()],
            )
            .unwrap();
        let id = cat.conn().last_insert_rowid();
        let out = cat.merge_bundle_into(&sample_manifest(), &HashSet::from([id]), &HashSet::new()).unwrap();
        assert_eq!((out.summary.photos_filled, out.summary.versions_added), (0, 0));
        assert!(out.iptc_fills.is_empty());
        assert_eq!(cat.get_photo(id).unwrap().rating, 0);
        assert!(cat.list_versions(id).unwrap().is_empty());
        assert_eq!(out.summary.assignments_added, 1, "its tags still union");
    }

    // --- once per bundle batch (#248) ------------------------------------------------------

    /// #248: what a bundle filled in on an existing photo, and the versions it added, are the
    /// user's to clear. Merging the same bundle batch again brings none of it back — no
    /// culling, no IPTC offered, no version — and only unions its tags; another batch fills
    /// the blanks as before.
    #[test]
    fn a_batch_merged_into_a_photo_before_fills_nothing_again() {
        let (cat, _root) = temp_catalog("once");
        cat.conn()
            .execute(
                "INSERT INTO photos(uuid, path, mtime_ns, size, extension, created_at, updated_at)
                 VALUES(?1, 'existing/local.ARW', 1, 1, 'arw', 1, 1)",
                params![crate::catalog::photo_identity_for("photo-a").unwrap()],
            )
            .unwrap();
        let id = cat.conn().last_insert_rowid();
        let first = cat.merge_bundle_into(&sample_manifest(), &HashSet::new(), &HashSet::new()).unwrap();
        assert_eq!((first.summary.photos_filled, first.summary.versions_added), (1, 2));
        assert_eq!(first.iptc_fills.len(), 1);

        // The user clears what the bundle filled in and removes what it added.
        cat.set_culling(id, Some(0), Some(""), Some(PickState::None)).unwrap();
        for v in cat.list_versions(id).unwrap() {
            cat.delete_version(v.id).unwrap();
        }
        cat.conn().execute("DELETE FROM photo_tags WHERE photo_id = ?1", params![id]).unwrap();

        let again = cat.merge_bundle_into(&sample_manifest(), &HashSet::new(), &HashSet::new()).unwrap();
        let s = &again.summary;
        assert_eq!((s.photos_existing, s.photos_merged_before, s.photos_filled, s.versions_added), (1, 1, 0, 0), "{s:?}");
        assert!(again.iptc_fills.is_empty(), "no IPTC offered again");
        assert!(again.versions_added_to.is_empty());
        let after = cat.get_photo(id).unwrap();
        assert_eq!((after.rating, after.label.as_str(), after.pick_state), (0, "", PickState::None));
        assert!(cat.list_versions(id).unwrap().is_empty(), "the removed versions stay removed");
        assert_eq!(s.assignments_added, 1, "its tags still union");

        // Another batch is another bundle: its blanks are filled.
        let mut other = sample_manifest();
        other.batch.uuid = "batch-2".into();
        let third = cat.merge_bundle_into(&other, &HashSet::new(), &HashSet::new()).unwrap();
        assert_eq!((third.summary.photos_merged_before, third.summary.photos_filled), (0, 1));
        assert_eq!(cat.get_photo(id).unwrap().rating, 4);
    }

    /// #248: a photo the merge inserted, and a row the importer created for the bundle
    /// (`fresh`), are recorded too: a value cleared on either after the import stays cleared
    /// when the bundle is imported again.
    #[test]
    fn an_inserted_or_fresh_photo_is_recorded_as_merged() {
        let (cat, _root) = temp_catalog("once-new");
        cat.merge_bundle(&sample_manifest()).unwrap();
        let inserted = cat.get_photo_by_uuid("photo-a").unwrap().id;
        cat.set_culling(inserted, Some(0), None, None).unwrap();
        let again = cat.merge_bundle(&sample_manifest()).unwrap();
        assert_eq!((again.photos_merged_before, again.photos_filled), (1, 0));
        assert_eq!(cat.get_photo(inserted).unwrap().rating, 0);

        let (cat, _root) = temp_catalog("once-fresh");
        cat.conn()
            .execute(
                "INSERT INTO photos(uuid, path, mtime_ns, size, extension, created_at, updated_at)
                 VALUES(?1, 'existing/local.ARW', 1, 1, 'arw', 1, 1)",
                params![crate::catalog::photo_identity_for("photo-a").unwrap()],
            )
            .unwrap();
        let fresh = cat.conn().last_insert_rowid();
        cat.merge_bundle_into(&sample_manifest(), &HashSet::from([fresh]), &HashSet::new()).unwrap();
        let again = cat.merge_bundle_into(&sample_manifest(), &HashSet::new(), &HashSet::new()).unwrap();
        assert_eq!((again.summary.photos_merged_before, again.summary.photos_filled), (1, 0));
        assert_eq!(cat.get_photo(fresh).unwrap().rating, 0, "the blank the user left is not filled");
    }

    /// A bundle photo whose identity no row holds, at a path another photo (another identity)
    /// holds, is kept apart and counted — neither inserted (the path is UNIQUE; before, the
    /// whole merge failed on it) nor merged onto the photo there.
    #[test]
    fn a_new_identity_at_a_path_another_photo_holds_is_kept_apart() {
        let (cat, _root) = temp_catalog("apart");
        cat.conn()
            .execute(
                "INSERT INTO photos(uuid, path, mtime_ns, size, extension, created_at, updated_at)
                 VALUES('11111111-2222-4333-8444-555555555555', '2026/06/28/DSC01234.ARW', 1, 1, 'arw', 1, 1)",
                [],
            )
            .unwrap();
        let s = cat.merge_bundle(&sample_manifest()).unwrap();
        assert_eq!((s.photos_added, s.photos_existing, s.photos_kept_apart), (0, 0, 1));
        assert_eq!(s.assignments_added, 0, "its tags do not land on the other photo");
        let only: (String, i64) = cat
            .conn()
            .query_row("SELECT uuid, rating FROM photos", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(only, ("11111111-2222-4333-8444-555555555555".to_string(), 0));
    }

    /// An identity the importer kept apart (its original found in the library at another
    /// name, under another identity) is kept apart here too, with its own path free: no
    /// metadata-only row is inserted for it.
    #[test]
    fn an_identity_the_importer_kept_apart_is_not_inserted_at_its_free_path() {
        let (cat, _root) = temp_catalog("apart-free");
        let kept = HashSet::from([crate::catalog::photo_identity_for("photo-a").unwrap()]);
        let out = cat.merge_bundle_into(&sample_manifest(), &HashSet::new(), &kept).unwrap();
        let s = out.summary;
        assert_eq!((s.photos_added, s.photos_existing, s.photos_kept_apart), (0, 0, 1));
        assert_eq!(cat.count_photos(&Default::default()).unwrap(), 0);
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

    /// A merged-away tag must not come back. `tags.uuid` is the strongest match here, and a
    /// bundle exported before the merge still carries the dead uuid — without the `tag_aliases`
    /// tombstone the union would treat it as a tag this catalog has never seen and re-create
    /// it, quietly undoing the reorg. This is the test that makes A5 permanent rather than
    /// cosmetic, so it asserts both halves: the tag stays gone, *and* the bundle's photo lands
    /// on the tag that replaced it.
    #[test]
    fn a_merged_away_tag_is_not_resurrected_by_a_later_bundle_import() {
        let (cat, _root) = temp_catalog("resurrect");
        // The catalog has both tags and merges the bundle's one away into the survivor.
        let owls = cat.create_tag("Birds/Owls").unwrap();
        let raptors = cat.create_tag("Birds/Raptors").unwrap();
        cat.conn()
            .execute("UPDATE tags SET uuid = 'tag-owl' WHERE id = ?1", [owls])
            .unwrap();

        let report = crate::catalog::tag_maintenance::merge_tags(cat.conn(), &[owls], raptors, 1)
            .unwrap();
        assert_eq!(report.aliases_recorded, 1);
        assert!(cat.get_tag(owls).is_err(), "the merged tag is gone");

        // Now import a bundle whose taxonomy still names it by that uuid.
        let summary = cat.merge_bundle(&sample_manifest()).unwrap();

        assert_eq!(
            summary.tags_created, 0,
            "the dead uuid resolved through the tombstone instead of creating a tag"
        );
        assert!(
            cat.list_tags_with_counts()
                .unwrap()
                .iter()
                .all(|t| t.tag.full_path != "Birds/Owls"),
            "Birds/Owls must not reappear"
        );
        let photo = cat.get_photo_by_uuid("photo-a").unwrap();
        let tags = cat.get_photo_tags(photo.id).unwrap();
        assert_eq!(
            tags.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![raptors],
            "the bundle's photo lands on the tag that replaced the one it named"
        );
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

    /// #146 (L5): merge matches a UUID in either case, and stores a new one lowercase.
    #[test]
    fn merge_matches_and_stores_an_identity_lowercase() {
        let (cat, _root) = temp_catalog("identity-case");
        const KNOWN: &str = "6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f";
        cat.conn()
            .execute(
                "INSERT INTO photos(uuid, path, mtime_ns, size, extension, created_at, updated_at)
                 VALUES(?1, 'local/known.ARW', 1, 1, 'arw', 1, 1)",
                params![KNOWN],
            )
            .unwrap();
        let mut m = sample_manifest();
        m.photos[0].uuid = KNOWN.to_ascii_uppercase();
        let mut arriving = m.photos[0].clone();
        arriving.uuid = "0D9C8B7A-6F5E-4D3C-8B2A-190807060504".into();
        arriving.relative_path = "2026/06/28/DSC09999.ARW".into();
        m.photos.push(arriving);

        let s = cat.merge_bundle(&m).unwrap();
        assert_eq!((s.photos_existing, s.photos_added), (1, 1));
        let uuids: Vec<String> = cat
            .conn()
            .prepare("SELECT uuid FROM photos ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(uuids, [KNOWN, "0d9c8b7a-6f5e-4d3c-8b2a-190807060504"]);
    }

    // --- legacy identifiers (#150) ----------------------------------------------------

    /// #150 (review N3 of #146): an old build merged a migrated catalog's bundle, leaving a
    /// row holding the v5 of `dam:asset/1`; this catalog then migrated, and v23 kept its own
    /// row for that value apart with a v4 and recorded the value there. Merging an old bundle
    /// that still says `dam:asset/1` matches the v5 row — but must not record the value on
    /// it as well: two owners would make a scan refuse both, so the original would be
    /// catalogued again the next time it moved.
    #[test]
    fn merge_does_not_give_a_legacy_value_a_second_owner() {
        let (cat, _root) = temp_catalog("legacy-second-owner");
        let v5 = crate::catalog::legacy_photo_identity("dam:asset/1");
        cat.conn()
            .execute_batch(&format!(
                "INSERT INTO photos(uuid, path, mtime_ns, size, extension, created_at, updated_at)
                 VALUES('6f1c1f0e-2b7a-4c3d-9e8f-0a1b2c3d4e5f', 'a/original.jpg', 1, 1, 'jpg', 1, 1);
                 INSERT INTO photo_legacy_identifiers(photo_id, identifier)
                 VALUES(last_insert_rowid(), 'dam:asset/1');
                 INSERT INTO photos(uuid, path, mtime_ns, size, extension, created_at, updated_at)
                 VALUES('{v5}', 'b/merged-by-an-old-build.jpg', 1, 1, 'jpg', 1, 1);"
            ))
            .unwrap();
        let mut m = sample_manifest();
        m.photos[0].uuid = "dam:asset/1".into();

        let s = cat.merge_bundle(&m).unwrap();
        assert_eq!((s.photos_existing, s.photos_added), (1, 0), "matched the v5 row");
        let owners: Vec<String> = cat
            .conn()
            .prepare(
                "SELECT p.path FROM photo_legacy_identifiers l JOIN photos p ON p.id = l.photo_id
                 WHERE l.identifier = 'dam:asset/1'",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(owners, ["a/original.jpg"], "the value keeps its one owner");

        // A value nobody holds yet is still recorded on the row it matched or created.
        let mut fresh = sample_manifest();
        fresh.photos[0].uuid = "dam:asset/2".into();
        fresh.photos[0].relative_path = "c/new.jpg".into();
        cat.merge_bundle(&fresh).unwrap();
        let recorded: i64 = cat
            .conn()
            .query_row(
                "SELECT count(*) FROM photo_legacy_identifiers WHERE identifier = 'dam:asset/2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(recorded, 1);
    }
}
