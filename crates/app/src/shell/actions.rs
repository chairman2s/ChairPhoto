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
//!   cite only an open ticket ([`OPEN_TICKETS`], checked by the tests). The list is empty
//!   since #158; the machinery stays for the next stub.

use gpui_kit::{actions, Action, App, InteractiveElement, Window};

actions!(
    chairphoto,
    [
        /// `[`: show or hide the tags & collections column (narrow: its overlay).
        ToggleLeftPanel,
        /// `]`: show or hide the inspector column (narrow: its overlay).
        ToggleRightPanel,
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
// `Name => ("what the user asked for", ticket)`, and its ticket must be in [`OPEN_TICKETS`].
not_yet_ported! {}

/// The tickets a not-yet-ported stub may cite: the GPUI map's (#92) open tickets, as the
/// issue tracker listed them on 2026-10-03. Closing one of them takes it out of this list in
/// the same change; a stub still citing it then fails [`stub_ticket`]'s test, so a stub
/// cannot outlive its ticket unnoticed. (The tests run offline, so this list is the record,
/// not GitHub.)
pub const OPEN_TICKETS: &[u32] = &[
    107, 108, 109, 110, 111, 112, 113, 115, 119, 121, 123, 124, 125, 126, 128, 129, 130, 134, 151, 159, 160,
    161, 162, 163,
];

/// Whether a stub may cite `ticket`: only an open ticket of the map ([`OPEN_TICKETS`]).
pub fn stub_ticket(ticket: u32) -> Result<(), String> {
    if OPEN_TICKETS.contains(&ticket) {
        Ok(())
    } else {
        Err(format!("#{ticket} is not an open GPUI-map ticket (closed, or missing from OPEN_TICKETS)"))
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
            // Every ticket is one of the map's open tickets.
            stub_ticket(*ticket).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }

    /// The guard rejects a closed ticket — #114 (Storage and import), which the loupe's stubs
    /// still cited after it closed, #158, which ported them, and every other ticket of the map
    /// closed by 2026-10-03 — and accepts an open one.
    #[test]
    fn the_guard_rejects_a_stub_citing_a_closed_ticket() {
        const CLOSED: &[u32] =
            &[93, 94, 95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 114, 116, 117, 118, 120, 122, 127, 157, 158];
        for &ticket in CLOSED {
            assert!(stub_ticket(ticket).is_err(), "#{ticket} is closed");
        }
        assert!(stub_ticket(159).is_ok());
        assert!(stub_ticket(0).is_err() && stub_ticket(9999).is_err());
    }
}
