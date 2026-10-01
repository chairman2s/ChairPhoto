//! The shell's actions: what the title bar, its menus, the command pill, the bench, the
//! icon rail and the collection browser can ask for.
//!
//! Two kinds:
//!
//! - **Ported** — the shell does them itself ([`crate::shell::ShellState`]): panel
//!   toggles, the cache-previews toggle, widening to all photos, clearing the selection; and
//!   the Publish dialog ([`crate::modules`]); Preferences ([`crate::preferences`], #113);
//!   Storage and import's (#114), which the root view hands to `crate::storage`.
//! - **Not yet ported** — the feature's surface belongs to a later ticket. Its menu item or
//!   button still dispatches a real action, and [`crate::model::AppModel::not_yet_ported`]
//!   answers with a visible status line naming the ticket, rather than faking the feature.
//!   [`NOT_YET_PORTED`] is the one list: the root view registers a handler for each entry
//!   ([`on_not_yet_ported`]) and the tests dispatch each one ([`not_yet_ported_actions`]).
//!   When a feature lands, its row moves out of this list and into its own handler.

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

not_yet_ported! {
    /// The title bar's and the bench's Export.
    ExportSelection => ("Export", 115),
    /// More ⋯ → Open loupe in a new window.
    PopOutLoupe => ("Open loupe in a new window", 110),
    /// The icon rail's Develop.
    OpenDevelop => ("Develop", 111),
    /// The loupe's "unavailable" state: Relocate… (a file picker; the grid context menu's
    /// commands are Storage and import's).
    RelocatePhoto => ("Relocate…", 114),
    /// The loupe's "unavailable" state: Retrieve from NAS.
    RetrieveFromNas => ("Retrieve from NAS", 114),
    /// The loupe's "unavailable" state: Remove from catalog.
    RemoveFromCatalog => ("Remove from catalog", 114),
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
            // Every ticket is a GPUI-map ticket (#93–#130).
            assert!((93..=130).contains(ticket), "{name} → #{ticket}");
        }
    }
}
