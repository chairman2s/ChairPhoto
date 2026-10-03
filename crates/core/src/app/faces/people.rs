//! The People view's reads and writes (#130), moved from the Tauri commands
//! (`faces_people_summary`, `faces_cluster_summary`, `faces_suggestion_list`,
//! `faces_name_cluster`) so the GPUI app runs the same bodies, plus the verbs the GPUI People
//! view adds: a cluster's faces, naming several clusters as one person (merge) or some of a
//! cluster's faces (split), ignoring faces, and reviewing suggestions as they were shown.
//!
//! **Blocking**, and each takes a `&Catalog` so the caller picks the lock: the Tauri commands
//! `with_catalog_blocking`, the GPUI app `with_catalog_as` bound to the identity the ids were
//! read under (map #92, "Catalog identity").
//!
//! **Avatars.** Each row carries its photo's `user_rotation`: thumbnails are drawn turned by
//! it, the stored boxes are not (docs/face-tagging.md, "The frame"), so a face crop turns the
//! box first.
//!
//! **Sidecars.** A verb that confirms faces re-exports those photos' MWG regions after its
//! transaction commits ([`super::write_regions`]: merge-safe, only ChairPhoto's own regions
//! are replaced), so an offline volume cannot roll back what the catalog recorded.

use super::super::now_secs;
use super::write_regions;
use crate::catalog::{Catalog, CatalogError, Result as CatalogResult};
use crate::plugins::faces::matcher::{self, MatchSettings};
use crate::plugins::faces::store::{ensure_schema, FaceBboxJson};

/// A named person: their confirmed faces, and a representative face for the avatar (the
/// first confirmed one).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonSummary {
    /// Person tag id (FK → tags.id).
    pub tag_id: i64,
    /// Leaf name of the person tag.
    pub name: String,
    /// Full hierarchical path of the person tag (e.g. "People/Family/Alice").
    pub full_path: String,
    /// Confirmed face rows for this person.
    pub face_count: i64,
    /// Distinct photos with at least one confirmed face of this person.
    pub photo_count: i64,
    pub avatar_photo_id: i64,
    /// The representative face's box (normalized 0–1, the unturned frame).
    pub avatar_bbox: FaceBboxJson,
    /// The avatar photo's `user_rotation` (degrees clockwise).
    pub avatar_rotation: i64,
}

/// An unnamed cluster: its member count and a representative face.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterSummary {
    pub cluster_id: i64,
    pub member_count: i64,
    pub avatar_photo_id: i64,
    pub avatar_bbox: FaceBboxJson,
    pub avatar_rotation: i64,
}

/// One suggested face in the review queue.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuggestionEntry {
    pub face_id: i64,
    pub photo_id: i64,
    pub bbox: FaceBboxJson,
    pub person_tag_id: i64,
    pub person_name: String,
    pub person_full_path: String,
    pub confidence: f64,
    /// The photo's `user_rotation`.
    pub rotation: i64,
}

/// One face of a cluster (the cluster's face sheet).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterFace {
    pub face_id: i64,
    pub photo_id: i64,
    pub bbox: FaceBboxJson,
    pub rotation: i64,
}

/// Every named person (≥ 1 confirmed face), by tag path (`faces_people_summary`).
pub fn people_summary(c: &Catalog) -> CatalogResult<Vec<PersonSummary>> {
    ensure_schema(c.conn())?;
    let mut stmt = c.conn().prepare(
        "SELECT g.person_tag_id, t.name, t.full_path, g.face_count, g.photo_count,
                r.photo_id, r.bbox, COALESCE(p.user_rotation, 0)
           FROM (SELECT person_tag_id,
                        COUNT(id)                AS face_count,
                        COUNT(DISTINCT photo_id) AS photo_count,
                        MIN(id)                  AS rep
                   FROM faces__faces
                  WHERE state = 'confirmed' AND person_tag_id IS NOT NULL
                  GROUP BY person_tag_id) g
           JOIN tags t ON t.id = g.person_tag_id
           JOIN faces__faces r ON r.id = g.rep
           LEFT JOIN photos p ON p.id = r.photo_id
          ORDER BY t.full_path",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(PersonSummary {
            tag_id: r.get(0)?,
            name: r.get(1)?,
            full_path: r.get(2)?,
            face_count: r.get(3)?,
            photo_count: r.get(4)?,
            avatar_photo_id: r.get(5)?,
            avatar_bbox: FaceBboxJson::from_str(&r.get::<_, String>(6)?),
            avatar_rotation: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Every unnamed cluster (faces with a cluster and no decision yet), largest first
/// (`faces_cluster_summary`).
pub fn cluster_summary(c: &Catalog) -> CatalogResult<Vec<ClusterSummary>> {
    ensure_schema(c.conn())?;
    let mut stmt = c.conn().prepare(
        "SELECT g.cluster_id, g.cnt, r.photo_id, r.bbox, COALESCE(p.user_rotation, 0)
           FROM (SELECT cluster_id, COUNT(*) AS cnt, MIN(id) AS rep
                   FROM faces__faces
                  WHERE cluster_id IS NOT NULL AND state IN ('unassigned', 'suggested')
                  GROUP BY cluster_id) g
           JOIN faces__faces r ON r.id = g.rep
           LEFT JOIN photos p ON p.id = r.photo_id
          ORDER BY g.cnt DESC, g.cluster_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ClusterSummary {
            cluster_id: r.get(0)?,
            member_count: r.get(1)?,
            avatar_photo_id: r.get(2)?,
            avatar_bbox: FaceBboxJson::from_str(&r.get::<_, String>(3)?),
            avatar_rotation: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Every suggested face, most confident first (`faces_suggestion_list`).
pub fn suggestion_list(c: &Catalog) -> CatalogResult<Vec<SuggestionEntry>> {
    ensure_schema(c.conn())?;
    let mut stmt = c.conn().prepare(
        "SELECT f.id, f.photo_id, f.bbox, f.person_tag_id, t.name, t.full_path,
                COALESCE(f.match_confidence, 0.0), COALESCE(p.user_rotation, 0)
           FROM faces__faces f
           JOIN tags t ON t.id = f.person_tag_id
           LEFT JOIN photos p ON p.id = f.photo_id
          WHERE f.state = 'suggested' AND f.person_tag_id IS NOT NULL
          ORDER BY f.match_confidence DESC NULLS LAST, f.id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(SuggestionEntry {
            face_id: r.get(0)?,
            photo_id: r.get(1)?,
            bbox: FaceBboxJson::from_str(&r.get::<_, String>(2)?),
            person_tag_id: r.get(3)?,
            person_name: r.get(4)?,
            person_full_path: r.get(5)?,
            confidence: r.get(6)?,
            rotation: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The faces of cluster `cluster` still pending a decision, in index order.
pub fn cluster_faces(c: &Catalog, cluster: i64) -> CatalogResult<Vec<ClusterFace>> {
    ensure_schema(c.conn())?;
    let mut stmt = c.conn().prepare(
        "SELECT f.id, f.photo_id, f.bbox, COALESCE(p.user_rotation, 0)
           FROM faces__faces f
           LEFT JOIN photos p ON p.id = f.photo_id
          WHERE f.cluster_id = ?1 AND f.state IN ('unassigned', 'suggested')
          ORDER BY f.id",
    )?;
    let rows = stmt.query_map([cluster], |r| {
        Ok(ClusterFace {
            face_id: r.get(0)?,
            photo_id: r.get(1)?,
            bbox: FaceBboxJson::from_str(&r.get::<_, String>(2)?),
            rotation: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The people root the matcher uses (`faces.people_root`, trimmed; `People` when unset or
/// blank): where a person named in the People view or created in the person picker goes, so
/// the matcher counts them.
pub fn effective_people_root(c: &Catalog) -> CatalogResult<String> {
    Ok(MatchSettings::load(c.conn())?.people_root)
}

/// What a naming did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameOutcome {
    /// The person tag (found or created).
    pub tag_id: i64,
    /// Faces confirmed as the person.
    pub faces: usize,
    /// Distinct photos they are on, now tagged with the person.
    pub photos: usize,
    /// Faces asked for that were no longer pending (confirmed, ignored or gone meanwhile).
    pub skipped: usize,
}

/// What the People view answers when nothing it named was still pending.
pub const NOTHING_TO_NAME: &str = "These faces changed since the list was read (named, ignored or regrouped by a \
                                   matching run) — refresh and try again.";

/// Name `face_ids` as the person at `tag_path` (found or created): confirm the faces still
/// pending a decision, tag their photos, then re-export those photos' regions. In one
/// transaction — the tag is not created, and nothing changes, when none of the faces is
/// still pending ([`NOTHING_TO_NAME`]).
pub fn name_faces(c: &Catalog, face_ids: &[i64], tag_path: &str) -> CatalogResult<NameOutcome> {
    let path = tag_path.trim();
    if path.is_empty() || path.split('/').all(|s| s.trim().is_empty()) {
        return Err(CatalogError::Validation("a person needs a name".into()));
    }
    ensure_schema(c.conn())?;
    let tx = c.conn().unchecked_transaction()?;
    let tag_id = c.create_tag(path)?;
    let changed = matcher::name_faces(&tx, face_ids, tag_id)?;
    if changed.faces == 0 {
        return Err(CatalogError::Validation(NOTHING_TO_NAME.into()));
    }
    for &photo in &changed.photos {
        // Same connection as `tx`, so this is part of the transaction.
        c.assign_tag(photo, tag_id)?;
    }
    matcher::tidy_clusters(&tx, &changed.clusters)?;
    tx.commit()?;
    for &photo in &changed.photos {
        write_regions(c, photo);
    }
    let asked = face_ids.iter().collect::<std::collections::HashSet<_>>().len();
    Ok(NameOutcome { tag_id, faces: changed.faces, photos: changed.photos.len(), skipped: asked - changed.faces })
}

/// Name every face of `clusters` as one person — one cluster (`faces_name_cluster`), or
/// several merged into the same person. Refused ([`NOTHING_TO_NAME`]) when the clusters have
/// no pending face left: a matching run since the list was read regroups faces into new
/// clusters (ids are never reused), so a stale cluster id names nothing.
pub fn name_clusters(c: &Catalog, clusters: &[i64], tag_path: &str) -> CatalogResult<NameOutcome> {
    let mut faces = Vec::new();
    for &cluster in clusters {
        faces.extend(cluster_faces(c, cluster)?.into_iter().map(|f| f.face_id));
    }
    if faces.is_empty() {
        return Err(CatalogError::Validation(NOTHING_TO_NAME.into()));
    }
    name_faces(c, &faces, tag_path)
}

/// Mark `face_ids` ignored (only those still pending); the number ignored.
pub fn ignore_faces(c: &Catalog, face_ids: &[i64]) -> CatalogResult<usize> {
    ensure_schema(c.conn())?;
    let tx = c.conn().unchecked_transaction()?;
    let changed = matcher::ignore_faces(&tx, face_ids)?;
    matcher::tidy_clusters(&tx, &changed.clusters)?;
    tx.commit()?;
    Ok(changed.faces)
}

/// A verdict on one suggestion, as the queue showed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Confirm,
    Reject,
}

/// One reviewed suggestion: the face, the person it was shown as, and the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Review {
    pub face_id: i64,
    pub tag_id: i64,
    pub verdict: Verdict,
}

/// What a review did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewOutcome {
    pub confirmed: usize,
    pub rejected: usize,
    /// Suggestions that had changed since they were shown (confirmed elsewhere, or re-matched
    /// to someone else): left alone.
    pub stale: usize,
    /// Confirmations of a suggestion whose person tag is an auto-tag, which can't be assigned
    /// by hand (#181): skipped, the face left suggested.
    pub auto_tag: usize,
}

/// Apply the review queue's verdicts (✓, ✕, "Confirm all ≥ X%"), each only while the face
/// is still suggested as the person shown ([`matcher::accept_suggestion`]). A confirmation
/// tags the photo with the person. One transaction; the confirmed photos' regions are
/// re-exported after it commits.
pub fn review_suggestions(c: &Catalog, reviews: &[Review]) -> CatalogResult<ReviewOutcome> {
    ensure_schema(c.conn())?;
    let now = now_secs();
    let tx = c.conn().unchecked_transaction()?;
    let mut out = ReviewOutcome::default();
    let mut photos: Vec<i64> = Vec::new();
    for r in reviews {
        match r.verdict {
            Verdict::Confirm if c.auto_tag_refusal(r.tag_id)?.is_some() => out.auto_tag += 1,
            Verdict::Confirm => match matcher::accept_suggestion(&tx, r.face_id, r.tag_id)? {
                Some(photo) => {
                    c.assign_tag(photo, r.tag_id)?;
                    out.confirmed += 1;
                    if !photos.contains(&photo) {
                        photos.push(photo);
                    }
                }
                None => out.stale += 1,
            },
            Verdict::Reject => {
                if matcher::reject_suggestion(&tx, r.face_id, r.tag_id, now)? {
                    out.rejected += 1;
                } else {
                    out.stale += 1;
                }
            }
        }
    }
    tx.commit()?;
    for photo in photos {
        write_regions(c, photo);
    }
    Ok(out)
}
