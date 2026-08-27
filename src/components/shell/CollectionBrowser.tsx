// The Darkroom redesign's collection browser (see agent-notes mockup Main.dc.html
// `.brow`/`.gh`/`.li`): merges the four panels that used to stack in `.leftcol` — each
// independently backed, independently scrolling and capped at 30% height — into one
// scrollable list with lowercase hairline section headers, plus a new Library section
// (All photos / Trash) on top.
//
// Rehost, not rewrite: TagPanel, AlbumsPanel, SmartAlbumsPanel and BatchesPanel each keep
// their own fetching, modals, context menus, drag/drop and quick-search. This file only
// wraps each in a collapsible section and renders it with the exact prop bag App.tsx used
// to pass it directly (`ComponentProps<typeof X>`, so a prop added to one of those panels
// shows up here as a type error rather than silently going unwired).
//
// Per-panel header decision — each of the four panels' own `.panel-header` would clash
// visually with the section header above it, so it is hidden wherever the section header
// fully replaces it:
//   - BatchesPanel's header carries no action (batches are auto-created, read-only) — its
//     section header replaces it outright; hidden via `.brow-section-hide-header
//     .panel-header { display: none }`.
//   - TagPanel (+ New tags…, ⊕/⊖ expand/collapse-all), AlbumsPanel (+ New album) and
//     SmartAlbumsPanel (+ New smart album) each carry an add affordance that lives in
//     internal state with no prop to trigger it externally (New album/New smart album
//     read `window.prompt` directly; New tags opens TagPanel's own modal state). Reaching
//     into those panels to add such a prop is out of this rehost's scope, so — per "render
//     the affordance via a prop if one exists, else keep the panel's own header visible" —
//     their own header stays visible beneath the section's hairline header. A little
//     redundant (two header rows), but nothing is lost.
import { useState, type ComponentProps, type ReactNode } from "react";
import { TagPanel } from "../TagPanel";
import { AlbumsPanel } from "../AlbumsPanel";
import { SmartAlbumsPanel } from "../SmartAlbumsPanel";
import { BatchesPanel } from "../BatchesPanel";

export interface CollectionBrowserProps {
  // -- Library section --------------------------------------------------------------
  /** No tag/album/batch/smart-album scope active — the whole library, unfiltered by any
   *  of the four primary scopes (culling/facet chips can still be layered on top). */
  isAllScope: boolean;
  /** library.clearScope — widen back to the whole library. */
  onSelectAll: () => void;
  /** null = unknown (not yet fetched, or the last fetch failed) — hides the count rather
   *  than showing a stale or misleading number. */
  trashCount: number | null;
  onOpenTrash: () => void;

  // -- Pass-throughs — exact current prop bags, unchanged by this rehost -------------
  tagPanel: ComponentProps<typeof TagPanel>;
  albumsPanel: ComponentProps<typeof AlbumsPanel>;
  smartAlbumsPanel: ComponentProps<typeof SmartAlbumsPanel>;
  batchesPanel: ComponentProps<typeof BatchesPanel>;
}

/**
 * One collapsible section: a lowercase hairline header that doubles as the disclosure
 * toggle, and a body that unmounts entirely while collapsed. Open state persists per
 * section in localStorage — same pattern as PhotoInspector.tsx's `Section`, except these
 * sections default to *open* (they are the primary navigation, not a detail accordion).
 */
function Section({
  id,
  label,
  hideOwnHeader,
  children,
}: {
  id: string;
  label: string;
  /** True when the wrapped panel's own `.panel-header` carries no affordance this
   *  section's header doesn't already provide, so it can be hidden outright. */
  hideOwnHeader?: boolean;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(() => {
    const v = localStorage.getItem(`panel.section.${id}`);
    return v === null ? true : v === "1";
  });
  const toggle = () =>
    setOpen((o) => {
      localStorage.setItem(`panel.section.${id}`, o ? "0" : "1");
      return !o;
    });
  return (
    <div className={`brow-section ${hideOwnHeader ? "brow-section-hide-header" : ""}`}>
      <button type="button" className="brow-gh" onClick={toggle} aria-expanded={open}>
        <span className="brow-gh-caret">{open ? "▾" : "▸"}</span>
        <span className="brow-gh-label">{label}</span>
        <span className="brow-gh-rule" />
      </button>
      {open && <div className="brow-section-body">{children}</div>}
    </div>
  );
}

export function CollectionBrowser({
  isAllScope,
  onSelectAll,
  trashCount,
  onOpenTrash,
  tagPanel,
  albumsPanel,
  smartAlbumsPanel,
  batchesPanel,
}: CollectionBrowserProps) {
  return (
    <div className="brow">
      {/* Library: not collapsible — two fixed rows, always visible. */}
      <div className="brow-gh brow-gh-fixed">
        <span className="brow-gh-label">library</span>
        <span className="brow-gh-rule" />
      </div>
      <button
        type="button"
        className={`brow-li ${isAllScope ? "on" : ""}`}
        onClick={onSelectAll}
      >
        All photos
      </button>
      <button type="button" className="brow-li" onClick={onOpenTrash}>
        Trash
        {trashCount != null && <span className="n">{trashCount.toLocaleString()}</span>}
      </button>

      <Section id="tags" label="tags">
        <TagPanel {...tagPanel} />
      </Section>
      <Section id="smartAlbums" label="smart albums">
        <SmartAlbumsPanel {...smartAlbumsPanel} />
      </Section>
      <Section id="albums" label="albums">
        <AlbumsPanel {...albumsPanel} />
      </Section>
      <Section id="batches" label="import batches" hideOwnHeader>
        <BatchesPanel {...batchesPanel} />
      </Section>
    </div>
  );
}
