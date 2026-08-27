// The Darkroom redesign's title bar (see docs mockup "Darkroom"): a 46px strip that
// replaces the old `<header className="topbar">` — one row of ~21 ungrouped buttons — with
// three regions: a catalog pill (left), empty flex space (center — rich search lands here
// later), and every command grouped behind two menus plus a plain Export button (right).
//
// Presentational only: every piece of state and every command is a prop. App.tsx stays the
// state owner (ready/selection/counts/etc.) and decides what each callback does; TitleBar
// decides only how to group and label them. It reaches no host (`modules/host.ts`) function
// directly — `moduleActionGroups` arrives pre-computed (via `host.toolbarActionGroups()`)
// and activation is a callback (`onModuleAction`), so this file's only coupling to the
// module system is the `ToolbarAction` *type* used to describe a group's contents.
//
// `children` is an extension slot between the Export button and the "More" menu. It used
// to carry the three progress readouts + status line, which moved to the bench (Bench.tsx)
// along with the selection-flavoured More ⋯ entries (Publish, Back up selection, Compare);
// App.tsx currently passes nothing. The Library/Develop/module view switcher that used to
// live here moved to IconRail.tsx — a sibling of TitleBar in App.tsx's `.body`, not a
// child of it (see docs mockup "Darkroom").
import type { ReactNode } from "react";
import { MenuButton, MenuCheckItem, MenuItem, MenuLabel, MenuSeparator, MenuSub } from "./Menu";
import type { ImportBatch } from "../../modules/api";
import type { ToolbarAction } from "../../modules/registry";

/** One enabled module's toolbar actions, grouped for the "Modules" submenu. Structurally
 *  identical to `host.toolbarActionGroups()`'s return type — kept as a local type (rather
 *  than importing that function) so this file's only host.ts coupling is a shape, not a
 *  call. */
export interface ModuleActionGroup {
  moduleId: string;
  moduleLabel: string;
  actions: ToolbarAction[];
}

// Same "last path segment" label an import batch gets everywhere else it's shown
// (BatchesPanel, CommandPill, App's bundleExportBatch dialog title) — kept local rather
// than shared because each of those call sites already carries its own copy.
function batchLabel(b: ImportBatch): string {
  return b.sourceLabel.replace(/\/+$/, "").split("/").pop() || b.sourceLabel || "(ingest)";
}

export interface TitleBarProps {
  // Catalog pill (left) — opens the existing CatalogSwitcher modal (App owns it).
  catalogName: string;
  photoCount: number;
  onOpenCatalogs: () => void;

  // Attention badges (right, before Import) — same entry points as the "Identity debt" /
  // "Back-up queue" rows further down the More menu, just surfaced when there's something
  // to look at.
  pendingCount: number;
  onReconcile: () => void;
  /** null = unknown (not yet fetched, or the last fetch failed) — renders "?", distinct
   *  from a confirmed 0. */
  identityDebtCount: number | null;
  onOpenIdentityDebt: () => void;

  // Import ▾
  ready: boolean;
  onImportCard: () => void;
  onImportBundle: () => void;
  onRescan: () => void;
  cachePreviews: boolean;
  onCachePreviews: (v: boolean) => void;
  exportableBatches: ImportBatch[];
  onExportBundle: (batch: ImportBatch) => void;

  // Export
  canExport: boolean;
  onExport: () => void;

  // More ⋯
  onPopOutLoupe: () => void;
  loupeOn: boolean;
  loupeEnabled: boolean;
  onToggleLoupe: () => void;
  /** Selected photo count — feeds the same "N selected photo(s)" vs. "the whole view"
   *  tooltip wording the old flat buttons used for Analyse burst / Propose stacks / Cull. */
  selectionCount: number;
  onAnalyseBurst: () => void;
  onProposeStacks: () => void;
  onCullSession: () => void;
  onTrash: () => void;
  moduleActionGroups: ModuleActionGroup[];
  onModuleAction: (action: ToolbarAction) => void;
  onOpenPrefs: () => void;
  leftHidden: boolean;
  onToggleLeft: () => void;
  rightHidden: boolean;
  onToggleRight: () => void;

  /** Extension slot between Export and More — currently empty. See file header. */
  children?: ReactNode;
}

export function TitleBar({
  catalogName,
  photoCount,
  onOpenCatalogs,
  pendingCount,
  onReconcile,
  identityDebtCount,
  onOpenIdentityDebt,
  ready,
  onImportCard,
  onImportBundle,
  onRescan,
  cachePreviews,
  onCachePreviews,
  exportableBatches,
  onExportBundle,
  canExport,
  onExport,
  onPopOutLoupe,
  loupeOn,
  loupeEnabled,
  onToggleLoupe,
  selectionCount,
  onAnalyseBurst,
  onProposeStacks,
  onCullSession,
  onTrash,
  moduleActionGroups,
  onModuleAction,
  onOpenPrefs,
  leftHidden,
  onToggleLeft,
  rightHidden,
  onToggleRight,
  children,
}: TitleBarProps) {
  return (
    <header className="titlebar">
      <div className="titlebar-left">
        <button className="catpill" onClick={onOpenCatalogs} title="Open or create a catalog">
          <span className="catpill-name">{catalogName || "Catalog"}</span>
          <span className="catpill-count">· {photoCount.toLocaleString()}</span>
          <svg
            width="10"
            height="10"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
            aria-hidden
          >
            <polyline points="6 9 12 15 18 9" />
          </svg>
        </button>
      </div>

      {/* Rich search lands here later (deferred) — empty for now. */}
      <div className="titlebar-center" />

      <div className="titlebar-right">
        {pendingCount > 0 && (
          <button className="attn" onClick={onReconcile} title="Back up photos waiting for the NAS">
            ⤓ {pendingCount} waiting for the NAS
          </button>
        )}
        {(identityDebtCount === null || identityDebtCount > 0) && (
          <button
            className="attn"
            onClick={onOpenIdentityDebt}
            title={
              identityDebtCount === null
                ? "Identity debt count could not be checked — open to see the current queue"
                : "Photo copies whose sidecar doesn't carry their identity yet. Most of this " +
                  "is normally Unreachable (an offline volume), not a failure."
            }
          >
            {identityDebtCount === null ? "?" : identityDebtCount} identity debt
          </button>
        )}

        <MenuButton label="Import ▾">
          <MenuItem
            disabled={!ready}
            title="Copy from a card into your library"
            onSelect={onImportCard}
          >
            Import from card…
          </MenuItem>
          <MenuItem
            disabled={!ready}
            title="Import a .chairphoto bundle from another machine"
            onSelect={onImportBundle}
          >
            Import a .chairphoto bundle…
          </MenuItem>
          <MenuSeparator />
          <MenuItem disabled={!ready} title="Re-index your library folder" onSelect={onRescan}>
            Rescan library
          </MenuItem>
          <MenuCheckItem checked={cachePreviews} onChange={onCachePreviews}>
            Cache previews on import
          </MenuCheckItem>
          <MenuSeparator />
          <MenuSub label="Export a bundle">
            {exportableBatches.length === 0 ? (
              <MenuItem disabled>No import batches yet</MenuItem>
            ) : (
              exportableBatches.map((b) => (
                <MenuItem key={b.id} onSelect={() => onExportBundle(b)}>
                  {batchLabel(b)}
                </MenuItem>
              ))
            )}
          </MenuSub>
        </MenuButton>

        <button
          className="btn-ghost"
          onClick={onExport}
          disabled={!canExport}
          title="Export the selected photo(s) to files (RAW + XMP, or JPEG)"
        >
          Export
        </button>

        {children}

        <MenuButton icon="⋯" title="More" align="right">
          <MenuItem
            onSelect={onPopOutLoupe}
            title="Open loupe in a separate window (move it to another screen)"
          >
            Open loupe in a new window
          </MenuItem>
          <MenuItem
            disabled={!loupeEnabled}
            badge={loupeOn ? "On" : undefined}
            title="Toggle large view (Enter)"
            onSelect={onToggleLoupe}
          >
            Loupe
          </MenuItem>
          <MenuSeparator />
          <MenuLabel>Whole view</MenuLabel>
          <MenuItem
            disabled={!ready}
            title={
              selectionCount
                ? `Rank burst sharpness for ${selectionCount} selected photo(s)`
                : "Rank burst sharpness for all visible photos (H16e)"
            }
            onSelect={onAnalyseBurst}
          >
            Analyse burst sharpness
          </MenuItem>
          <MenuItem
            disabled={!ready}
            title={
              selectionCount
                ? `Propose stacks for ${selectionCount} selected photo(s)`
                : "Propose stacks for all visible photos — review each group before it collapses"
            }
            onSelect={onProposeStacks}
          >
            Propose stacks…
          </MenuItem>
          <MenuItem
            disabled={!ready}
            title={
              selectionCount
                ? `Cull ${selectionCount} selected photo(s) full screen, keyboard only`
                : "Cull the whole view full screen, keyboard only — resumes where you left off"
            }
            onSelect={onCullSession}
          >
            Start cull session
          </MenuItem>
          <MenuSeparator />
          <MenuItem
            disabled={!ready}
            title="Photos you have hidden. Nothing there has been deleted — restoring is one click."
            onSelect={onTrash}
          >
            Trash…
          </MenuItem>
          {moduleActionGroups.length > 0 && (
            <>
              <MenuSeparator />
              <MenuSub label="Modules">
                {moduleActionGroups.map((group) => (
                  <div key={group.moduleId}>
                    <MenuLabel>{group.moduleLabel}</MenuLabel>
                    {group.actions.map((action) => (
                      <MenuItem
                        key={action.id}
                        title={action.label}
                        onSelect={() => onModuleAction(action)}
                      >
                        {action.icon ? `${action.icon} ${action.label}` : action.label}
                      </MenuItem>
                    ))}
                  </div>
                ))}
              </MenuSub>
            </>
          )}
          <MenuSeparator />
          <MenuItem
            badge={identityDebtCount === null ? "?" : identityDebtCount}
            onSelect={onOpenIdentityDebt}
          >
            Identity debt
          </MenuItem>
          <MenuItem badge={pendingCount} onSelect={onReconcile}>
            Back-up queue
          </MenuItem>
          <MenuSeparator />
          <MenuLabel>View</MenuLabel>
          <MenuCheckItem checked={!leftHidden} onChange={onToggleLeft}>
            Tags &amp; collections panel
          </MenuCheckItem>
          <MenuCheckItem checked={!rightHidden} onChange={onToggleRight}>
            Inspector
          </MenuCheckItem>
          <MenuSeparator />
          <MenuItem title="Preferences (storage, AI, modules)" onSelect={onOpenPrefs}>
            Preferences…
          </MenuItem>
        </MenuButton>
      </div>
    </header>
  );
}
