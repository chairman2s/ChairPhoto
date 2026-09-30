//! Tag terms: translations and synonyms layered on the nested tag hierarchy.
//!
//! A tag's `name`/`full_path` are its language-neutral internal identity (used for
//! nesting, dedup, and assignment). Terms are the human/interop labels:
//!
//! - a **translation** is a `is_primary` term in a given language,
//! - a **synonym** is a non-primary term (optionally language-scoped),
//! - every term has an `export` flag.
//!
//! [`Catalog::export_labels`] is the building block the future export/transition
//! layer calls: given a tag and a set of languages, it returns the label strings to
//! emit. We deliberately do NOT model any other system here — interop formatting
//! (Lightroom `|` paths, etc.) is the exporter's job, built on top of these labels.

use super::tag_path::normalize_lookup;
use super::{Catalog, ExportKeywords, Result, TagTerm};
use rusqlite::{params, OptionalExtension};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

impl Catalog {
    /// Add a term to a tag. If `is_primary`, any existing primary for the same
    /// (tag, language) is demoted first, so a language has at most one canonical name.
    pub fn add_term(
        &self,
        tag_id: i64,
        text: &str,
        language: Option<&str>,
        is_primary: bool,
        export: bool,
    ) -> Result<i64> {
        let text = text.trim();
        let lang = normalize_language(language);
        if is_primary {
            self.demote_primary(tag_id, lang.as_deref())?;
        }
        self.conn.execute(
            "INSERT INTO tag_terms(tag_id, text, text_norm, language, is_primary, export, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(tag_id, text_norm, coalesce(language, ''))
             DO UPDATE SET is_primary = excluded.is_primary, export = excluded.export",
            params![
                tag_id,
                text,
                normalize_lookup(text),
                lang,
                is_primary as i64,
                export as i64,
                now()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Convenience: set the canonical name for a language (a translation).
    pub fn set_translation(&self, tag_id: i64, language: &str, text: &str) -> Result<i64> {
        self.add_term(tag_id, text, Some(language), true, true)
    }

    /// Convenience: add a synonym (non-primary), optionally language-scoped.
    pub fn add_synonym(
        &self,
        tag_id: i64,
        text: &str,
        language: Option<&str>,
        export: bool,
    ) -> Result<i64> {
        self.add_term(tag_id, text, language, false, export)
    }

    /// Update a term's editable fields. `is_primary = true` demotes the existing
    /// primary for its (tag, language) first.
    pub fn update_term(
        &self,
        term_id: i64,
        text: &str,
        language: Option<&str>,
        is_primary: bool,
        export: bool,
    ) -> Result<()> {
        let lang = normalize_language(language);
        if is_primary {
            // Find the term's tag so we can demote siblings in the same language.
            if let Some(tag_id) = self
                .conn
                .query_row(
                    "SELECT tag_id FROM tag_terms WHERE id = ?1",
                    params![term_id],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
            {
                self.demote_primary(tag_id, lang.as_deref())?;
            }
        }
        let text = text.trim();
        self.conn.execute(
            "UPDATE tag_terms
             SET text = ?1, text_norm = ?2, language = ?3, is_primary = ?4, export = ?5
             WHERE id = ?6",
            params![
                text,
                normalize_lookup(text),
                lang,
                is_primary as i64,
                export as i64,
                term_id
            ],
        )?;
        Ok(())
    }

    pub fn set_term_export(&self, term_id: i64, export: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE tag_terms SET export = ?1 WHERE id = ?2",
            params![export as i64, term_id],
        )?;
        Ok(())
    }

    pub fn remove_term(&self, term_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM tag_terms WHERE id = ?1", params![term_id])?;
        Ok(())
    }

    /// All terms for a tag (primaries first, then synonyms; stable order).
    pub fn list_terms(&self, tag_id: i64) -> Result<Vec<TagTerm>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, tag_id, text, language, is_primary, export
             FROM tag_terms WHERE tag_id = ?1
             ORDER BY is_primary DESC, language IS NULL, language, text_norm",
        )?;
        let rows = stmt.query_map(params![tag_id], row_to_term)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Distinct languages used across all terms (for UI language pickers).
    pub fn list_languages(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT language FROM tag_terms
             WHERE language IS NOT NULL AND language != '' ORDER BY language",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The display name of a tag in a language: its primary term for that language,
    /// else the canonical tag name.
    pub fn display_name(&self, tag_id: i64, language: Option<&str>) -> Result<String> {
        if let Some(lang) = normalize_language(language) {
            if let Some(text) = self
                .conn
                .query_row(
                    "SELECT text FROM tag_terms
                     WHERE tag_id = ?1 AND language = ?2 AND is_primary = 1 LIMIT 1",
                    params![tag_id, lang],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
            {
                return Ok(text);
            }
        }
        Ok(self.get_tag(tag_id)?.name)
    }

    /// The labels to emit for a tag given the selected `languages` — the transition
    /// layer's building block:
    ///
    /// - for each language: its exportable primary term, else the canonical name;
    /// - plus exportable synonyms whose language is one of `languages` or neutral.
    ///
    /// With no languages selected, returns the canonical name plus neutral synonyms.
    pub fn export_labels(&self, tag_id: i64, languages: &[String]) -> Result<Vec<String>> {
        // Organizational tags emit nothing (their descendants still export).
        if !self.tag_exportable(tag_id)? {
            return Ok(Vec::new());
        }
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let push = |label: String, out: &mut Vec<String>, seen: &mut HashSet<String>| {
            if !label.is_empty() && seen.insert(label.clone()) {
                out.push(label);
            }
        };

        let canonical = self.get_tag(tag_id)?.name;

        if languages.is_empty() {
            push(canonical.clone(), &mut out, &mut seen);
        } else {
            for lang in languages {
                // An empty token means "the default (canonical) name" — this lets the
                // UI offer a "default" choice that combines with real languages.
                if lang.is_empty() {
                    push(canonical.clone(), &mut out, &mut seen);
                    continue;
                }
                let primary: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT text FROM tag_terms
                         WHERE tag_id = ?1 AND language = ?2 AND is_primary = 1 AND export = 1
                         LIMIT 1",
                        params![tag_id, lang],
                        |r| r.get(0),
                    )
                    .optional()?;
                push(primary.unwrap_or_else(|| canonical.clone()), &mut out, &mut seen);
            }
        }

        // Exportable synonyms in a selected language or language-neutral.
        let mut stmt = self.conn.prepare(
            "SELECT text, language FROM tag_terms
             WHERE tag_id = ?1 AND is_primary = 0 AND export = 1
             ORDER BY text_norm",
        )?;
        let synonyms = stmt.query_map(params![tag_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        for syn in synonyms {
            let (text, lang) = syn?;
            let included = match lang {
                None => true,
                Some(ref l) => languages.iter().any(|sel| sel == l),
            };
            if included {
                push(text, &mut out, &mut seen);
            }
        }

        Ok(out)
    }

    /// Export labels for a tag *and all its ancestors*, root → leaf — the "export
    /// contained keywords" behaviour stock/social targets expect (tagging a photo
    /// `Animals/Birds/Eagle` should emit keywords for Animals, Birds, and Eagle).
    ///
    /// Each level contributes its [`export_labels`] (primary/canonical + exportable
    /// synonyms) for the selected `languages`; duplicates across the whole chain are
    /// dropped, preserving first-seen order (so ancestors precede descendants).
    pub fn export_labels_with_ancestors(
        &self,
        tag_id: i64,
        languages: &[String],
    ) -> Result<Vec<String>> {
        // Walk leaf → root, collecting tag ids, then reverse to root → leaf.
        let mut chain: Vec<i64> = Vec::new();
        let mut current = Some(tag_id);
        while let Some(id) = current {
            chain.push(id);
            current = self
                .conn
                .query_row("SELECT parent_id FROM tags WHERE id = ?1", params![id], |r| {
                    r.get::<_, Option<i64>>(0)
                })
                .optional()?
                .flatten();
        }
        chain.reverse();

        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for id in chain {
            for label in self.export_labels(id, languages)? {
                if seen.insert(label.clone()) {
                    out.push(label);
                }
            }
        }
        Ok(out)
    }

    /// Assemble the keyword set to emit for a photo on export: for every assigned tag,
    /// the export labels of it **and its ancestors** (flat `dc:subject`), plus each
    /// tag's translated full path joined with `|` (`lr:hierarchicalSubject`). With no
    /// `languages`, uses canonical names + neutral synonyms.
    ///
    /// Keywords are **ordered by specificity** (G4) — the convention stock libraries
    /// expect: the most specific assigned tag's keywords first, and within each tag the
    /// tag itself before its (more general) ancestors. De-duplicated, first-seen.
    pub fn assemble_export_keywords(
        &self,
        photo_id: i64,
        languages: &[String],
    ) -> Result<ExportKeywords> {
        // Languages to render hierarchical paths in: each selected language, or just
        // the canonical (None) when none are selected.
        let path_langs: Vec<Option<String>> = if languages.is_empty() {
            vec![None]
        } else {
            languages
                .iter()
                .map(|l| if l.is_empty() { None } else { Some(l.clone()) })
                .collect()
        };

        // Process assigned tags most-specific (deepest) first; ties by path for
        // determinism.
        let mut tags = self.get_photo_tags(photo_id)?;
        let depth = |t: &super::Tag| t.full_path.split('/').count();
        tags.sort_by(|a, b| depth(b).cmp(&depth(a)).then_with(|| a.full_path.cmp(&b.full_path)));

        let mut flat = Vec::new();
        let mut seen_flat = HashSet::new();
        let mut hierarchical = Vec::new();
        let mut seen_hier = HashSet::new();

        for tag in &tags {
            // Walk this tag's chain leaf → root so the specific term precedes its
            // ancestors in the emitted order.
            for tag_id in self.tag_chain_leaf_to_root(tag.id)? {
                for label in self.export_labels(tag_id, languages)? {
                    if seen_flat.insert(label.clone()) {
                        flat.push(label);
                    }
                }
            }
            for lang in &path_langs {
                let path = self.path_labels(tag.id, lang.as_deref())?.join("|");
                if !path.is_empty() && seen_hier.insert(path.clone()) {
                    hierarchical.push(path);
                }
            }
        }
        Ok(ExportKeywords { flat, hierarchical })
    }

    /// The chain of tag ids from `tag_id` up to its root, leaf first.
    fn tag_chain_leaf_to_root(&self, tag_id: i64) -> Result<Vec<i64>> {
        let mut chain = Vec::new();
        let mut current = Some(tag_id);
        while let Some(id) = current {
            chain.push(id);
            current = self
                .conn
                .query_row("SELECT parent_id FROM tags WHERE id = ?1", params![id], |r| {
                    r.get::<_, Option<i64>>(0)
                })
                .optional()?
                .flatten();
        }
        Ok(chain)
    }

    /// Translated labels along a tag's full path, root → leaf, in a language. The
    /// future exporter joins these into a hierarchical string in whatever format the
    /// target system wants.
    pub fn path_labels(&self, tag_id: i64, language: Option<&str>) -> Result<Vec<String>> {
        let mut chain: Vec<String> = Vec::new();
        let mut current = Some(tag_id);
        while let Some(id) = current {
            // Skip organizational (non-exportable) segments so they don't leak into the
            // hierarchical path either — consistent with the flat keyword gate.
            if self.tag_exportable(id)? {
                chain.push(self.display_name(id, language)?);
            }
            current = self
                .conn
                .query_row("SELECT parent_id FROM tags WHERE id = ?1", params![id], |r| {
                    r.get::<_, Option<i64>>(0)
                })
                .optional()?
                .flatten();
        }
        chain.reverse();
        Ok(chain)
    }

    /// Demote any existing primary term for a (tag, language) to a synonym.
    fn demote_primary(&self, tag_id: i64, language: Option<&str>) -> Result<()> {
        match language {
            Some(lang) => self.conn.execute(
                "UPDATE tag_terms SET is_primary = 0
                 WHERE tag_id = ?1 AND language = ?2 AND is_primary = 1",
                params![tag_id, lang],
            )?,
            None => self.conn.execute(
                "UPDATE tag_terms SET is_primary = 0
                 WHERE tag_id = ?1 AND language IS NULL AND is_primary = 1",
                params![tag_id],
            )?,
        };
        Ok(())
    }
}

/// Normalize a language: trim, and treat empty as neutral (None).
fn normalize_language(language: Option<&str>) -> Option<String> {
    language
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
}

fn row_to_term(r: &rusqlite::Row) -> rusqlite::Result<TagTerm> {
    Ok(TagTerm {
        id: r.get(0)?,
        tag_id: r.get(1)?,
        text: r.get(2)?,
        language: r.get(3)?,
        is_primary: r.get::<_, i64>(4)? != 0,
        export: r.get::<_, i64>(5)? != 0,
    })
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
