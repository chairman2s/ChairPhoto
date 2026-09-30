//! Parse pasted tag text into full tag paths. Port of `src/modules/tagPaste.ts`, used by the
//! "create tags" dialog (`TagCreateModal`).
//!
//! Semantic choices against the TypeScript:
//! - Whitespace is JavaScript's `String.prototype.trim` set ([`crate::js_compat::is_js_whitespace`]), not
//!   Rust's `char::is_whitespace`: the two differ on U+FEFF (JS trims a BOM, Rust does not)
//!   and U+0085 (Rust trims NEL, JS does not). A BOM at the head of pasted text is the case
//!   that matters.
//! - Lines split on `'\n'` only, as `text.split("\n")` did; a `"\r\n"` line's trailing
//!   `'\r'` is removed by the trim.
//! - Indentation is counted per `char` (a Unicode scalar), which equals JS's per-code-point
//!   `for…of` for the only characters it counts (tab and space).

use crate::js_compat::js_trim;
use std::collections::HashSet;

/// Indentation width of a line: tabs count as 4, spaces count as 1.
fn indent_width(line: &str) -> usize {
    let mut w = 0;
    for ch in line.chars() {
        match ch {
            '\t' => w += 4,
            ' ' => w += 1,
            _ => break,
        }
    }
    w
}

/// Parse pasted text — single names, slash paths, or an indented hierarchy — into full tag
/// paths (one per non-empty line, parents included).
///
/// Algorithm:
///   - Maintain a stack of `(indent, name)` entries.
///   - For each non-blank line, compute its indent width.
///   - Pop the stack while the top entry's indent >= the current line's indent (i.e. we've
///     dedented to this level or beyond).
///   - Push the current `(indent, trimmed line)` onto the stack.
///   - Emit the full path: stack names joined by `/`.
///   - Lines that contain `/` or `|` are left as-is — the backend splits them.
///   - De-duplicate the output while preserving order.
pub fn parse_tag_paste(text: &str) -> Vec<String> {
    if js_trim(text).is_empty() {
        return Vec::new();
    }

    let mut stack: Vec<(usize, &str)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut result: Vec<String> = Vec::new();

    for raw_line in text.split('\n') {
        let name = js_trim(raw_line);
        // Skip blank / whitespace-only lines.
        if name.is_empty() {
            continue;
        }
        let indent = indent_width(raw_line);

        // Pop stack entries that are at the same or greater indent level.
        while stack.last().is_some_and(|&(top, _)| top >= indent) {
            stack.pop();
        }
        stack.push((indent, name));

        let full_path = stack.iter().map(|&(_, n)| n).collect::<Vec<_>>().join("/");
        if seen.insert(full_path.clone()) {
            result.push(full_path);
        }
    }
    result
}

// `tagPaste.ts` has no vitest file, so there is nothing to translate (0 cases). These tests
// are new: they pin the algorithm as the doc comment above states it.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_text_parses_to_nothing() {
        assert!(parse_tag_paste("").is_empty());
        assert!(parse_tag_paste("  \n\t\n").is_empty());
    }

    #[test]
    fn an_indented_hierarchy_emits_every_path_with_its_parents() {
        let text = "Places\n  Norway\n    Oslo\n  Sweden\nPeople\n\tAnna";
        assert_eq!(
            parse_tag_paste(text),
            vec![
                "Places",
                "Places/Norway",
                "Places/Norway/Oslo",
                "Places/Sweden",
                "People",
                "People/Anna",
            ]
        );
    }

    #[test]
    fn slash_paths_pass_through_and_duplicates_collapse_in_order() {
        let text = "a/b\r\nc|d\na/b\n";
        assert_eq!(parse_tag_paste(text), vec!["a/b", "c|d"]);
    }

    #[test]
    fn trims_like_javascript_a_leading_bom_is_whitespace() {
        assert_eq!(parse_tag_paste("\u{FEFF}Birds\n  Owl"), vec!["Birds", "Birds/Owl"]);
    }
}
