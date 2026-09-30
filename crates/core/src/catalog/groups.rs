//! Tag groups: user-defined named sets of tags surfaced as quick-tag buttons.
//! A core organising feature (not a plugin). Groups and their members are ordered.

use super::{Catalog, Result, Tag, TagGroup};
use rusqlite::params;
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

impl Catalog {
    pub fn create_tag_group(&self, name: &str) -> Result<i64> {
        let position: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(position), -1) + 1 FROM tag_groups", [], |r| {
                r.get(0)
            })?;
        self.conn.execute(
            "INSERT INTO tag_groups(name, position, created_at) VALUES(?1, ?2, ?3)",
            params![name.trim(), position, now()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn rename_tag_group(&self, group_id: i64, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tag_groups SET name = ?1 WHERE id = ?2",
            params![name.trim(), group_id],
        )?;
        Ok(())
    }

    pub fn delete_tag_group(&self, group_id: i64) -> Result<()> {
        // Members cascade via the foreign key.
        self.conn
            .execute("DELETE FROM tag_groups WHERE id = ?1", params![group_id])?;
        Ok(())
    }

    pub fn list_tag_groups(&self) -> Result<Vec<TagGroup>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name FROM tag_groups ORDER BY position, name")?;
        let rows = stmt.query_map([], |r| {
            Ok(TagGroup {
                id: r.get(0)?,
                name: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Add a tag to a group (idempotent), placed at the end.
    pub fn add_tag_to_group(&self, group_id: i64, tag_id: i64) -> Result<()> {
        let position: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM tag_group_members WHERE group_id = ?1",
            params![group_id],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO tag_group_members(group_id, tag_id, position) VALUES(?1, ?2, ?3)
             ON CONFLICT(group_id, tag_id) DO NOTHING",
            params![group_id, tag_id, position],
        )?;
        Ok(())
    }

    pub fn remove_tag_from_group(&self, group_id: i64, tag_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM tag_group_members WHERE group_id = ?1 AND tag_id = ?2",
            params![group_id, tag_id],
        )?;
        Ok(())
    }

    /// Assemble a social **hashtag bundle** from a tag group (G3): each member tag's
    /// export labels rendered as `#hashtags`, de-duplicated, optionally capped to a
    /// platform's tag limit. These are reach/distribution labels emitted at publish
    /// time, kept separate from a photo's content keywords (see docs/taxonomy.md).
    pub fn assemble_hashtag_bundle(&self, group_id: i64, limit: Option<usize>) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for tag in self.group_members(group_id)? {
            for label in self.export_labels(tag.id, &[])? {
                let tag_form = to_hashtag(&label);
                if tag_form.len() > 1 && seen.insert(tag_form.clone()) {
                    out.push(tag_form);
                }
            }
        }
        // A 0 limit is meaningless (would silently drop everything) — ignore it.
        if let Some(n) = limit.filter(|n| *n > 0) {
            out.truncate(n);
        }
        Ok(out)
    }

    /// The tags in a group, in order.
    pub fn group_members(&self, group_id: i64) -> Result<Vec<Tag>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.name, t.full_path, t.parent_id, t.description, t.auto_rule, t.uuid, t.private
             FROM tag_group_members m JOIN tags t ON t.id = m.tag_id
             WHERE m.group_id = ?1
             ORDER BY m.position",
        )?;
        let rows = stmt.query_map(params![group_id], |r| {
            Ok(Tag {
                id: r.get(0)?,
                name: r.get(1)?,
                full_path: r.get(2)?,
                parent_id: r.get(3)?,
                description: r.get(4)?,
                auto_rule: r.get(5)?,
                uuid: r.get(6)?,
                private: r.get::<_, i64>(7)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The tags most recently applied by hand (via `assign_tag`), newest first, up to
    /// `limit`. Backs the virtual "Recently used" quick-tag group. Auto-tags are excluded
    /// implicitly: the auto-tag engine never sets `last_used_at`, so they stay null here.
    pub fn recently_used_tags(&self, limit: usize) -> Result<Vec<Tag>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, full_path, parent_id, description, auto_rule, uuid, private
             FROM tags
             WHERE last_used_at IS NOT NULL
             ORDER BY last_used_at DESC, id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(Tag {
                id: r.get(0)?,
                name: r.get(1)?,
                full_path: r.get(2)?,
                parent_id: r.get(3)?,
                description: r.get(4)?,
                auto_rule: r.get(5)?,
                uuid: r.get(6)?,
                private: r.get::<_, i64>(7)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

/// Render a label as a hashtag: keep alphanumerics (Unicode), lowercase, prefix `#`.
/// An already-`#`-prefixed synonym (`#bnw`) round-trips to itself.
fn to_hashtag(label: &str) -> String {
    let core: String = label.chars().filter(|c| c.is_alphanumeric()).collect();
    format!("#{}", core.to_lowercase())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
