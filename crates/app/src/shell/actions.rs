//! The shell's actions: what the title bar, its menus, the command pill, the bench, the
//! icon rail and the collection browser can ask for.
//!
//! Two kinds:
//!
//! - **Ported** — the shell does them itself ([`crate::shell::ShellState`]): panel
//!   toggles, the cache-previews toggle, widening to all photos, clearing the selection; and
//!   the Publish dialog ([`crate::modules`]); Preferences ([`crate::preferences`], #113);
//!   Storage and import's (#114), which the root view hands to `crate::storage`; Albums and
//!   export's (#115), to `crate::albums` and `crate::export`.
//! - **Not yet ported** — the feature's surface belongs to a later ticket. Its menu item or
//!   button still dispatches a real action, and [`crate::model::AppModel::not_yet_ported`]
//!   answers with a visible status line naming the ticket, rather than faking the feature.
//!   [`NOT_YET_PORTED`] is the one list: the root view registers a handler for each entry
//!   ([`on_not_yet_ported`]) and the tests dispatch each one ([`not_yet_ported_actions`]).
//!   When a feature lands, its row moves out of this list and into its own handler. A row may
//!   cite only a ticket the parity checklist names as missing work ([`stub_tickets`], checked
//!   by the tests). The list is empty since #158; the machinery stays for the next stub.

use gpui_kit::{actions, Action, App, InteractiveElement, Window};

actions!(
    chairphoto,
    [
        /// `[`: show or hide the tags & collections column (narrow: its overlay).
        ToggleLeftPanel,
        /// `]`: show or hide the inspector column (narrow: its overlay).
        ToggleRightPanel,
        /// The `[` key: [`ToggleLeftPanel`] where App.tsx's key handler ran (the Library, no
        /// cull session); nothing elsewhere. The View menu dispatches the toggle itself.
        PanelKeyLeft,
        /// The `]` key, likewise for [`ToggleRightPanel`].
        PanelKeyRight,
        /// Import ▾ → "Cache previews on import" (session-only, default on).
        ToggleCachePreviews,
        /// The collection browser's "All photos": widen to the whole library, keeping the sort.
        ShowAllPhotos,
        /// The icon rail's Library item.
        ShowLibrary,
        /// The icon rail's Develop: the Darkroom on the active photo (#111).
        OpenDevelop,
        /// The bench's ✕: clear the selection.
        ClearSelection,
        /// More ⋯ → Preferences…, and the rail's gear: Preferences (#113), on its Storage tab.
        OpenPreferences,
        /// The bench's Publish: the Publish dialog over the enabled modules' publish targets.
        PublishSelection,
        // Storage and import (#114, `crate::storage`):
        /// The catalog pill: the catalog switcher.
        OpenCatalogs,
        /// "⤓ N waiting for the NAS" and More ⋯ → Back-up queue: run the reconcile now.
        Reconcile,
        /// "N identity debt" and More ⋯ → Identity debt.
        OpenIdentityDebt,
        /// Import ▾ → Import from card….
        ImportFromCard,
        /// Import ▾ → Import a .chairphoto bundle….
        ImportBundle,
        /// Import ▾ → Rescan library.
        RescanLibrary,
        /// The bench's Cancel while an import runs.
        CancelImport,
        /// The collection browser's Trash.
        OpenTrash,
        /// The bench's Back up.
        BackUpSelection,
        /// More ⋯ → Analyse burst sharpness, and the bench's Analyse: over the selection,
        /// else the whole view (`ShellState::analyse_burst`).
        AnalyseBurst,
        /// More ⋯ → Propose stacks…, and the bench's Stack: the "Stack bursts" dialog over
        /// the selection, else the whole view (`library::stacks`).
        ProposeStacks,
        // Albums and export (#115, `crate::export`):
        /// The title bar's and the bench's Export: the Export dialog over the selection.
        ExportSelection,
        /// The bench's Cancel while an export runs.
        CancelExport,
        // The loupe (#109, `crate::loupe`):
        /// More ⋯ → Loupe, Enter with an active photo, a double-click on a tile: the inline
        /// loupe on or off.
        ToggleLoupe,
        /// More ⋯ → Start cull session, and the bench's Cull: over the selection, else the
        /// whole view.
        StartCullSession,
        /// The bench's Compare (C in the grid): Compare over the selection (two or more); in
        /// Compare it closes it again.
        OpenCompare,
        /// More ⋯ → Open loupe in a new window: the pop-out loupe (#110,
        /// `crate::loupe::window`), or bring it forward when it is open.
        PopOutLoupe,
        // The loupe's "unavailable" state (#158, `crate::library::photo_actions`), on the
        // photo the inline loupe shows:
        /// Relocate…: point the photo at its moved file.
        RelocatePhoto,
        /// Retrieve from NAS: copy the backup back to the local volume.
        RetrieveFromNas,
        /// Remove from catalog (after a confirm).
        RemoveFromCatalog,
    ]
);

/// Declares the not-yet-ported actions, and from the same rows the list, the handler
/// registration and the test catalogue, so the three cannot drift.
macro_rules! not_yet_ported {
    ($( $(#[$attr:meta])* $name:ident => ($label:literal, $ticket:literal) ),* $(,)?) => {
        actions!(chairphoto, [ $( $(#[$attr])* $name ),* ]);

        /// `(action, what the user asked for, the ticket that ports it)`.
        pub const NOT_YET_PORTED: &[(&str, &str, u32)] = &[
            $( (stringify!($name), $label, $ticket) ),*
        ];

        /// Every not-yet-ported action, boxed, with its label and ticket.
        pub fn not_yet_ported_actions() -> Vec<(Box<dyn Action>, &'static str, u32)> {
            vec![ $( (Box::new($name) as Box<dyn Action>, $label, $ticket) ),* ]
        }

        /// Register `handler(label, ticket)` for every not-yet-ported action on `element`.
        pub fn on_not_yet_ported<E: InteractiveElement>(
            element: E,
            handler: impl Fn(&'static str, u32, &mut Window, &mut App) + Clone + 'static,
        ) -> E {
            let _ = &handler; // an empty list registers nothing
            $(
                let element = {
                    let handler = handler.clone();
                    element.on_action(move |_: &$name, window, cx| handler($label, $ticket, window, cx))
                };
            )*
            element
        }
    };
}

// Empty since #158 ported the loupe's unavailable-state actions. A row is
// `Name => ("what the user asked for", ticket)`, and its ticket must be one [`stub_tickets`]
// finds in `docs/plans/gpui/parity.md`.
not_yet_ported! {}

/// The parity checklist the stubs are held to (`docs/plans/gpui/parity.md`).
pub const PARITY_DOC: &str = include_str!("../../../../docs/plans/gpui/parity.md");

/// The tickets a not-yet-ported stub may cite, read from the parity checklist: a ticket named
/// in what a row still **misses** — the text after "Missing:" in a `partial` row's Status
/// paragraph, or a `to port` row's whole Status paragraph. Built, checked and dropped rows
/// name none, and the tickets a partial row names *before* "Missing:" built its other parts,
/// so they name none either.
///
/// A row is the inventory table's line (`` | `src/…` | … | status word | ``, the word in the
/// last cell) plus the file's section (`` ### `src/…` ``), whose `**Status:**` paragraph is
/// its last ([`row_status`]). The word decides; the paragraph supplies the tickets.
///
/// The checklist is edited anyway when a feature lands (its "Missing:" item goes, or the row
/// becomes `built`), so a stub citing that ticket then fails its test with no list to keep by
/// hand. To add a stub, name its ticket in the row's "Missing:" item, e.g. "Missing: the
/// splash (#160)". The tests run offline, so GitHub's open/closed state is not consulted.
pub fn stub_tickets(parity: &str) -> std::collections::BTreeSet<u32> {
    let mut tickets = std::collections::BTreeSet::new();
    for (file, word) in inventory(parity) {
        let Some(status) = row_status(parity, &file) else { continue };
        let missing = match word.as_str() {
            "to port" => status,
            "partial" => match status.find("Missing:") {
                Some(at) => &status[at..],
                None => continue,
            },
            _ => continue, // built, checked, dropped
        };
        tickets.extend(ticket_numbers(missing));
    }
    tickets
}

/// The inventory's rows as (file cell, status word): every table line that starts with a
/// `` `src/…` `` cell, its word the last non-empty cell.
fn inventory(parity: &str) -> Vec<(String, String)> {
    parity
        .lines()
        .filter(|l| l.starts_with("| `src/"))
        .filter_map(|line| {
            let cells = cells(line);
            let file = cells.get(1)?.clone();
            let word = cells.into_iter().rev().find(|c| !c.is_empty())?;
            Some((file, word))
        })
        .collect()
}

/// The `**Status:**` paragraph of `file`'s section (heading `` ### `src/…` ``), from its label
/// to the end of the section (the next `##`/`###` heading). `None` without such a section.
fn row_status<'a>(parity: &'a str, file: &str) -> Option<&'a str> {
    let heading = format!("### {file}");
    let mut offset = 0;
    let mut start = None;
    for line in parity.split_inclusive('\n') {
        let text = line.trim_end();
        if let Some(s) = start {
            if text.starts_with("## ") || text.starts_with("### ") {
                return status_in(&parity[s..offset]);
            }
        } else if text == heading {
            start = Some(offset + line.len());
        }
        offset += line.len();
    }
    status_in(&parity[start?..])
}

fn status_in(section: &str) -> Option<&str> {
    section.find("**Status:**").map(|at| section[at..].trim_end())
}

/// A table row's cells, split on the pipes Markdown reads as separators (not `\|`).
fn cells(line: &str) -> Vec<String> {
    let (mut out, mut cell, mut chars) = (Vec::new(), String::new(), line.chars().peekable());
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => cell.push(chars.next().unwrap()),
            '|' => out.push(std::mem::take(&mut cell).trim().to_string()),
            c => cell.push(c),
        }
    }
    out.push(cell.trim().to_string());
    out
}

/// Every `#N` in `text`.
fn ticket_numbers(text: &str) -> impl Iterator<Item = u32> + '_ {
    text.split('#').skip(1).filter_map(|rest| {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    })
}

/// Whether a stub may cite `ticket` ([`stub_tickets`] over [`PARITY_DOC`]).
pub fn stub_ticket(ticket: u32) -> Result<(), String> {
    if stub_tickets(PARITY_DOC).contains(&ticket) {
        Ok(())
    } else {
        Err(format!(
            "#{ticket} is named in no parity row's \"Missing:\" (docs/plans/gpui/parity.md): \
             its feature is built, or the row does not say which ticket ports it"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_not_yet_ported_row_names_a_ticket_and_a_label() {
        let actions = not_yet_ported_actions();
        assert_eq!(actions.len(), NOT_YET_PORTED.len());
        for ((action, label, ticket), (name, label2, ticket2)) in actions.iter().zip(NOT_YET_PORTED) {
            assert_eq!(action.name(), format!("chairphoto::{name}"));
            assert_eq!((label, ticket), (label2, ticket2));
            assert!(!label.is_empty());
            // Every ticket is one the parity checklist still names as missing work.
            stub_ticket(*ticket).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    /// Only what rows still miss counts: a built or dropped row's ticket, and a partial row's
    /// tickets before "Missing:" (the ones that built its other parts), are refused.
    ///
    /// The word in the table decides and the section's Status paragraph supplies the tickets:
    /// a "Missing:" outside the Status paragraph (B's spec), in a built or checked row's
    /// status (A, F), or in the next section (C's, after B's) does not count, and a `\|` in a
    /// table cell does not shift the word.
    #[test]
    fn stub_tickets_come_from_what_rows_still_miss() {
        let doc = "\
| React file | Area | Tickets | GPUI path | Status |
|---|---|---|---|---|
| `src/A.tsx` | x | #114 | `a.rs` | built |
| `src/B.tsx` | x \\| y | #105, #158 | `b.rs` | partial |
| `src/C.tsx` | x | #106 | `c.rs` | partial |
| `src/D.tsx` | x | #160 | — | to port |
| `src/E.tsx` | x | #157 | — | dropped |
| `src/F.tsx` | x | #108 | `f.rs` | checked |
| `src/G.tsx` | x | #162 | — | partial |
Text that mentions #999 outside any row.

## Area

### `src/A.tsx`

**Status:** built (#114: `a.rs`), awaiting the visual check. Missing: nothing (#170).

### `src/B.tsx`

**Spec:** b. Missing: (#171).

**Status:** partial (#105, #158: `b.rs`): built X.
Missing: Y (#159) | Z (#161)

### `src/C.tsx`

**Status:** partial (#106: `c.rs`): no ticket named for what is missing. Missing: W

### `src/D.tsx`

**Status:** to port (#160)

### `src/E.tsx`

**Status:** dropped (decision: #157). Missing: (#172)

### `src/F.tsx`

**Status:** built (#108), visually checked. Missing: (#173)

## Later

### `src/G.tsx` without its Status paragraph

**Status:** partial (#162). Missing: (#174)
";
        assert_eq!(stub_tickets(doc).into_iter().collect::<Vec<_>>(), vec![159, 160, 161]);
    }

    /// Every inventory row has a section whose Status paragraph starts with the row's word
    /// (`checked` rows are `built` ones that were visually checked), and every section is in
    /// the inventory, so the word [`stub_tickets`] trusts cannot drift from the narrative.
    #[test]
    fn every_parity_row_has_a_status_that_agrees_with_its_word() {
        let rows = inventory(PARITY_DOC);
        assert_eq!(rows.len(), 114, "one row per React file");
        for (file, word) in &rows {
            let status = row_status(PARITY_DOC, file).unwrap_or_else(|| panic!("{file}: no section"));
            let lead = match word.as_str() {
                "built" | "checked" => "built",
                "partial" | "to port" | "dropped" => word.as_str(),
                other => panic!("{file}: unknown status word {other:?}"),
            };
            let narrative = status.trim_start_matches("**Status:**").trim_start();
            assert!(narrative.starts_with(lead), "{file}: word {word:?}, status {narrative:.60}");
        }
        let sections = PARITY_DOC.lines().filter(|l| l.starts_with("### `src/")).count();
        assert_eq!(sections, rows.len(), "every section is in the inventory");
    }

    /// Against the real checklist: #114 and #158 (which built parts of the partial App.tsx
    /// row, and whose stubs this guard exists to catch) are refused.
    #[test]
    fn the_guard_rejects_tickets_that_only_built_things() {
        for ticket in [114, 158, 105, 106] {
            assert!(stub_ticket(ticket).is_err(), "#{ticket}");
        }
        assert!(stub_ticket(0).is_err() && stub_ticket(9999).is_err());
        assert!(stub_tickets(PARITY_DOC).len() < 20, "only open work is named: {:?}", stub_tickets(PARITY_DOC));
    }
}
