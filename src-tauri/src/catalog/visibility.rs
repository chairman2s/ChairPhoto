//! Which photos a query is allowed to see, and a test that keeps it that way.
//!
//! The rule that hides a photo from the user used to be spelled out at every call site —
//! `missing = 0`, 36 times across 11 files, including two that had leaked out of `catalog/`
//! into a command and a plugin. Trash doubles that (cluster B, D7), and every miss is
//! silent: a hidden photo still counted in stats, still on the map, still in an album
//! count, still resolved for export.
//!
//! So the rule lives in one place — the `photos_visible` view — and this module holds the
//! test that stops it leaking back out.
//!
//! ## What the view is not for
//!
//! Reading `photos` directly is correct and common: single-row lookups by id (the caller
//! has the row and wants it whatever its state), the background indexer queues, migrations,
//! and maintenance paths like purge and offload-eligibility. Those must see hidden photos.
//! Forcing them through the view would not be a refactor, it would be a behaviour change.
//!
//! ## What the test enforces
//!
//! 1. **No query re-spells the predicate.** A SQL literal that reads `photos` must not also
//!    contain `missing = 0` — that is what the view is for. This is exact and needs no
//!    allow-list, and it catches the realistic regression: someone copies an older query.
//! 2. **In a module that already reads the view**, any *additional* read of the base table
//!    must say why, with an `includes-hidden:` marker. A file that reads `photos_visible`
//!    is by definition a module that lists photos for the user, so a bare `FROM photos`
//!    there is either a deliberate exception or a bug. The rule needs no curated file list:
//!    it derives the set from the code itself.
//! 3. **The two copies of the view's definition agree** — `schema.rs` builds fresh
//!    catalogs, `mod.rs` migrates existing ones, and a drift between them would give old
//!    and new catalogs different ideas of what is visible.
//!
//! What it deliberately does *not* catch: a brand-new listing query, in a brand-new file,
//! that filters nothing at all. No syntactic rule distinguishes that from a legitimate
//! maintenance query. What protects it in practice is that every listing query in the tree
//! now reads `photos_visible`, so the neighbour a new query gets copied from is right.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// The marker that justifies reading the base table from a listing module.
    const MARKER: &str = "includes-hidden:";

    fn src_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
    }

    fn rust_files() -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let mut out = Vec::new();
        walk(&src_dir(), &mut out);
        out.sort();
        out
    }

    /// Every double-quoted string literal in `text`, with the line it starts on.
    fn string_literals(text: &str) -> Vec<(usize, String)> {
        let bytes: Vec<char> = text.chars().collect();
        let mut out = Vec::new();
        let mut line = 1usize;
        let mut i = 0usize;
        while i < bytes.len() {
            match bytes[i] {
                '\n' => line += 1,
                '"' => {
                    let start_line = line;
                    let mut j = i + 1;
                    let mut lit = String::new();
                    while j < bytes.len() {
                        match bytes[j] {
                            '\\' => {
                                j += 2;
                                continue;
                            }
                            '"' => break,
                            c => {
                                if c == '\n' {
                                    line += 1;
                                }
                                lit.push(c);
                            }
                        }
                        j += 1;
                    }
                    out.push((start_line, lit));
                    i = j;
                }
                _ => {}
            }
            i += 1;
        }
        out
    }

    /// Does this literal read the base `photos` table (as opposed to `photos_visible` or
    /// `photo_tags`)? `DELETE FROM photos` is a write, not a read.
    fn reads_base_table(sql: &str) -> bool {
        let lower = sql.to_lowercase();
        for keyword in ["from photos", "join photos"] {
            let mut at = 0usize;
            while let Some(found) = lower[at..].find(keyword) {
                let idx = at + found;
                let after = idx + keyword.len();
                let next = lower[after..].chars().next().unwrap_or(' ');
                let is_word_boundary = !next.is_alphanumeric() && next != '_';
                let is_delete = lower[..idx].trim_end().ends_with("delete");
                if is_word_boundary && !is_delete {
                    return true;
                }
                at = after;
            }
        }
        false
    }

    /// A query must not re-spell the rule the view exists to hold.
    #[test]
    fn no_query_respells_the_visibility_predicate() {
        let mut offenders = Vec::new();
        for file in rust_files() {
            let text = std::fs::read_to_string(&file).unwrap();
            for (line, lit) in string_literals(&text) {
                if !reads_base_table(&lit) {
                    continue;
                }
                let has_predicate = lit.contains("missing = 0") || lit.contains("missing=0");
                // The view's own definition is where the predicate belongs.
                let is_view_ddl = lit.contains("CREATE VIEW");
                if has_predicate && !is_view_ddl && !lit.contains(MARKER) {
                    offenders.push(format!("{}:{line}", file.display()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "these queries spell the visibility rule themselves instead of reading \
             `photos_visible`; the rule belongs in the view so trash and missing stay in \
             one place:\n  {}",
            offenders.join("\n  ")
        );
    }

    /// In a module that lists photos, reading the base table needs a stated reason.
    #[test]
    fn a_listing_module_justifies_every_base_table_read() {
        let mut offenders = Vec::new();
        for file in rust_files() {
            let text = std::fs::read_to_string(&file).unwrap();
            // A file that reads the view is, by that fact, a module that lists photos.
            if !text.contains("photos_visible") || text.contains("CREATE VIEW photos_visible") {
                continue;
            }
            for (line, lit) in string_literals(&text) {
                if reads_base_table(&lit) && !lit.contains(MARKER) {
                    offenders.push(format!(
                        "{}:{line}  {}",
                        file.display(),
                        lit.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect::<String>()
                    ));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "these read `photos` directly inside a module that lists photos. Either read \
             `photos_visible`, or add an `{MARKER} <why>` note in the SQL saying why this \
             one must see hidden photos:\n  {}",
            offenders.join("\n  ")
        );
    }

    /// `schema.rs` builds fresh catalogs and `mod.rs` migrates existing ones. If the two
    /// definitions drift, old and new catalogs disagree about what is visible — the exact
    /// class of bug the view exists to prevent.
    #[test]
    fn both_copies_of_the_view_definition_agree() {
        let normalise = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let extract = |text: &str| -> Option<String> {
            let at = text.find("CREATE VIEW photos_visible")?;
            let end = text[at..].find(';')? + at;
            Some(normalise(&text[at..end]))
        };
        let schema = std::fs::read_to_string(src_dir().join("catalog/schema.rs")).unwrap();
        let migration = std::fs::read_to_string(src_dir().join("catalog/mod.rs")).unwrap();

        let a = extract(&schema).expect("schema.rs defines photos_visible");
        let b = extract(&migration).expect("mod.rs migrates photos_visible");

        assert_eq!(a, b, "the two view definitions have drifted apart");
    }
}
