// The Darkroom redesign's icon rail (see docs mockup "Darkroom"): a 52px column, the
// leftmost track of `.body`, replacing the Library/Develop/module-view segmented control
// that used to ride in the TitleBar's `children` slot. Preferences also gets a foot-of-rail
// gear button here; the More ⋯ menu's "Preferences…" entry stays too (both live).
//
// Presentational only, like TitleBar: App.tsx owns activeViewId/develop state and decides
// what each `onSelect` id does. This file only renders the icons and reports which one was
// clicked.
import type { MainView } from "../../modules/registry";

export interface IconRailProps {
  /** The currently active item's id: "library", "develop", or a module view's id.
   *  App.tsx computes this as `activeView?.id ?? (inDevelop ? "develop" : "library")`. */
  active: "library" | "develop" | string;
  /** Whether the Develop item shows at all (mirrors the old seg's `canEdit` gate — no
   *  edit renderer registered means there's no develop surface to switch to). */
  canDevelop: boolean;
  /** Whether Develop is clickable right now (mirrors the old seg's `disabled={!selected}`
   *  — a renderer is registered, but nothing is selected to develop yet). */
  developEnabled: boolean;
  /** Module main views, already in display order — pass `railOrder(mainViews())`. */
  moduleViews: MainView[];
  onSelect: (id: "library" | "develop" | string) => void;
  onOpenPrefs: () => void;
}

const DEVELOP_TOOLTIP = "Develop the selected photo (crop & tone)";

export function IconRail({
  active,
  canDevelop,
  developEnabled,
  moduleViews,
  onSelect,
  onOpenPrefs,
}: IconRailProps) {
  return (
    <nav className="rail">
      <button
        type="button"
        className={`rail-item ${active === "library" ? "on" : ""}`}
        title="Library"
        aria-label="Library"
        onClick={() => onSelect("library")}
      >
        {/* Grid glyph — copied from the old seg's Library button. */}
        <svg
          width="17"
          height="17"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.8"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden
        >
          <rect x="3" y="3" width="7" height="7" />
          <rect x="14" y="3" width="7" height="7" />
          <rect x="14" y="14" width="7" height="7" />
          <rect x="3" y="14" width="7" height="7" />
        </svg>
      </button>

      {canDevelop && (
        <button
          type="button"
          className={`rail-item ${active === "develop" ? "on" : ""}`}
          title={DEVELOP_TOOLTIP}
          aria-label="Develop"
          disabled={!developEnabled}
          onClick={() => onSelect("develop")}
        >
          {/* Sliders glyph — copied from the old seg's Develop button. */}
          <svg
            width="17"
            height="17"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.8"
            strokeLinecap="round"
            strokeLinejoin="round"
            aria-hidden
          >
            <line x1="21" y1="6" x2="14" y2="6" />
            <line x1="10" y1="6" x2="3" y2="6" />
            <line x1="21" y1="18" x2="12" y2="18" />
            <line x1="8" y1="18" x2="3" y2="18" />
            <line x1="14" y1="4" x2="14" y2="8" />
            <line x1="8" y1="16" x2="8" y2="20" />
          </svg>
        </button>
      )}

      {moduleViews.map((v) => (
        <button
          key={v.id}
          type="button"
          className={`rail-item ${active === v.id ? "on" : ""}`}
          title={v.label}
          aria-label={v.label}
          onClick={() => onSelect(v.id)}
        >
          {v.icon}
        </button>
      ))}

      <div className="rail-sp" />

      <button
        type="button"
        className="rail-item"
        title="Preferences"
        aria-label="Preferences"
        onClick={onOpenPrefs}
      >
        {/* Gear glyph — copied from the mockup's foot-of-rail Preferences button. */}
        <svg
          width="17"
          height="17"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="1.8"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden
        >
          <circle cx="12" cy="12" r="3" />
          <path d="M19.4 15a1.6 1.6 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.6 1.6 0 0 0-1.8-.3 1.6 1.6 0 0 0-1 1.5V21a2 2 0 0 1-4 0v-.1A1.6 1.6 0 0 0 9 19.4a1.6 1.6 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.6 1.6 0 0 0 .3-1.8 1.6 1.6 0 0 0-1.5-1H3a2 2 0 0 1 0-4h.1A1.6 1.6 0 0 0 4.6 9a1.6 1.6 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1A1.6 1.6 0 0 0 9 4.6a1.6 1.6 0 0 0 1-1.5V3a2 2 0 0 1 4 0v.1a1.6 1.6 0 0 0 1 1.5 1.6 1.6 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.6 1.6 0 0 0-.3 1.8V9a1.6 1.6 0 0 0 1.5 1H21a2 2 0 0 1 0 4h-.1a1.6 1.6 0 0 0-1.5 1z" />
        </svg>
      </button>
    </nav>
  );
}
