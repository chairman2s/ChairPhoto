// Composable dropdown/popover menu primitive for the new shell (title bar, icon rail,
// bottom bench). It plays the same role App.tsx's right-click context menu does — a
// backdrop that closes on click, a panel of items styled like `.ctx-item` — but as a
// reusable component tree instead of one-off JSX, and deliberately portal-free: every
// trigger in the new shell lives in a title bar that never clips its children, so an
// absolutely-positioned panel anchored to a `position: relative` wrapper is simpler than a
// portal and avoids its extra z-index/repaint bookkeeping for no benefit here.
//
// State model: MenuButton owns the single "is this menu tree open" boolean and hands a
// `close` callback down through context. MenuItem/MenuCheckItem read it from context rather
// than taking one as a prop, so `{cond && <MenuItem/>}`, separators, labels and nested
// MenuSubs can all be mixed into `children` freely — no parent has to thread a close
// callback through conditional/nested children by hand.
//
// Keyboard: a single onKeyDown on the outer `.menu-wrap` owns Escape and Arrow Up/Down
// while (and only while) the menu is open — it covers both the trigger, before focus has
// moved into the panel, and the panel's items, since both are descendants of the wrap. The
// handler is unregistered outright (not just early-returning) while closed, so it never
// shadows an unrelated app-level Escape handler (loupe close, the grid's own ctx-menu).
// Enter/Space activation is left to the browser's native <button> behavior.

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";

type Align = "left" | "right";

// Which edge the ancestor MenuButton's panel lines up to — read by MenuSub so its flyout
// flips to the left when the panel itself is right-aligned (hugging the window edge).
const MenuAlignContext = createContext<Align>("left");
// Closes the *whole* menu tree, including any open MenuSub flyouts (they're unmounted along
// with the panel). Provided once by MenuButton; MenuSub does not re-provide it, so a
// MenuItem nested inside a MenuSub still closes the top-level menu, not just the flyout.
const MenuCloseContext = createContext<() => void>(() => {});

function focusableItems(panel: HTMLElement | null): HTMLElement[] {
  if (!panel) return [];
  return Array.from(panel.querySelectorAll<HTMLElement>('[role^="menuitem"]:not([disabled])'));
}

/** Escape closes the menu; Arrow Up/Down roving-focus the enabled items in document order
 * (a flat query over the whole open panel — including an open MenuSub's items — rather than
 * per-level tracking, which is simple enough for a menu this shallow). */
function handleMenuKeyDown(
  e: React.KeyboardEvent,
  panelRef: React.RefObject<HTMLDivElement | null>,
  close: () => void,
) {
  if (e.key === "Escape") {
    // Stop the *native* event here, not just React's synthetic dispatch — otherwise it
    // keeps bubbling past this component to whatever window-level Escape handler the app
    // registered outside React (loupe close, the grid's own ctx-menu dismiss).
    e.stopPropagation();
    close();
    return;
  }
  if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
  const items = focusableItems(panelRef.current);
  if (items.length === 0) return;
  e.preventDefault();
  e.stopPropagation();
  const active = document.activeElement as HTMLElement | null;
  const idx = active ? items.indexOf(active) : -1;
  const next =
    idx === -1
      ? e.key === "ArrowDown"
        ? 0
        : items.length - 1
      : e.key === "ArrowDown"
        ? (idx + 1) % items.length
        : (idx - 1 + items.length) % items.length;
  items[next]?.focus();
}

export interface MenuButtonProps {
  label?: ReactNode;
  icon?: ReactNode;
  className?: string;
  disabled?: boolean;
  title?: string;
  /** Which edge of the trigger the panel lines up to. "right" for menus that sit near the
   * window's right edge, so the panel opens inward instead of running off-screen. */
  align?: Align;
  children?: ReactNode;
}

/** The trigger button plus its dropdown. Open/closed state is entirely internal. */
export function MenuButton({
  label,
  icon,
  className,
  disabled = false,
  title,
  align = "left",
  children,
}: MenuButtonProps) {
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);

  const close = useCallback(() => {
    setOpen(false);
    // The trigger is never unmounted by closing, so focusing it back synchronously (even
    // from inside a MenuItem's click handler, right before it calls onSelect) is safe.
    triggerRef.current?.focus();
  }, []);

  // A menu that goes disabled out from under the user (its module unloads, a required
  // capability drops) shouldn't leave a dropdown open with nothing behind it.
  useEffect(() => {
    if (disabled) setOpen(false);
  }, [disabled]);

  return (
    <span
      className="menu-wrap"
      onKeyDown={open ? (e) => handleMenuKeyDown(e, panelRef, close) : undefined}
    >
      <button
        type="button"
        ref={triggerRef}
        className={className}
        disabled={disabled}
        title={title}
        // Text-labeled triggers get their accessible name from that text; an icon-only
        // trigger (no string label) falls back to `title` via aria-label instead.
        aria-label={title && typeof label !== "string" ? title : undefined}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => {
          if (disabled) return;
          setOpen((o) => !o);
        }}
      >
        {icon}
        {label}
      </button>
      {open && !disabled && (
        <MenuAlignContext.Provider value={align}>
          <MenuCloseContext.Provider value={close}>
            <div
              className="menu-backdrop"
              onClick={close}
              onContextMenu={(e) => {
                e.preventDefault();
                close();
              }}
            />
            <div
              ref={panelRef}
              className={`menu-panel${align === "right" ? " menu-panel-right" : ""}`}
              role="menu"
              onClick={(e) => e.stopPropagation()}
            >
              {children}
            </div>
          </MenuCloseContext.Provider>
        </MenuAlignContext.Provider>
      )}
    </span>
  );
}

export interface MenuItemProps {
  onSelect?: () => void;
  disabled?: boolean;
  danger?: boolean;
  /** Right-aligned muted text, e.g. a count. */
  badge?: ReactNode;
  title?: string;
  children?: ReactNode;
}

/** A selectable row. Closes the menu, THEN fires onSelect — so an action that opens a
 * modal of its own isn't fighting this menu's backdrop for focus on the same tick. */
export function MenuItem({ onSelect, disabled, danger, badge, title, children }: MenuItemProps) {
  const close = useContext(MenuCloseContext);
  return (
    <button
      type="button"
      role="menuitem"
      className={`menu-item${danger ? " menu-item-danger" : ""}`}
      disabled={disabled}
      title={title}
      onClick={() => {
        if (disabled) return;
        close();
        onSelect?.();
      }}
    >
      <span className="menu-item-label">{children}</span>
      {badge != null && <span className="menu-badge">{badge}</span>}
    </button>
  );
}

export interface MenuCheckItemProps {
  checked: boolean;
  onChange: (checked: boolean) => void;
  disabled?: boolean;
  children?: ReactNode;
}

/** A togglable row. Never closes the menu — toggling one checkbox is normally followed by
 * toggling another, not by the panel disappearing after the first click. */
export function MenuCheckItem({ checked, onChange, disabled, children }: MenuCheckItemProps) {
  return (
    <button
      type="button"
      role="menuitemcheckbox"
      aria-checked={checked}
      className="menu-item"
      disabled={disabled}
      onClick={() => {
        if (disabled) return;
        onChange(!checked);
      }}
    >
      <span className="menu-check" aria-hidden="true">
        {checked && (
          <svg
            width="10"
            height="10"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="3"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <polyline points="20 6 9 17 4 12" />
          </svg>
        )}
      </span>
      <span className="menu-item-label">{children}</span>
    </button>
  );
}

export interface MenuSubProps {
  label: ReactNode;
  icon?: ReactNode;
  children?: ReactNode;
}

/** One level of nesting: a row that opens a flyout panel to the side, on hover or click.
 * Flips to the left when the ancestor MenuButton opened right-aligned, so the flyout opens
 * inward instead of running off the window edge. */
export function MenuSub({ label, icon, children }: MenuSubProps) {
  const [open, setOpen] = useState(false);
  const align = useContext(MenuAlignContext);
  return (
    <div
      className="menu-sub-wrap"
      onMouseEnter={() => setOpen(true)}
      onMouseLeave={() => setOpen(false)}
    >
      <button
        type="button"
        role="menuitem"
        aria-haspopup="menu"
        aria-expanded={open}
        className="menu-item"
        onClick={() => setOpen((o) => !o)}
      >
        {icon}
        <span className="menu-item-label">{label}</span>
        <svg
          width="10"
          height="10"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden="true"
        >
          <polyline points="9 6 15 12 9 18" />
        </svg>
      </button>
      {open && (
        <div
          className={`menu-sub-panel${align === "right" ? " menu-panel-right" : ""}`}
          role="menu"
        >
          {children}
        </div>
      )}
    </div>
  );
}

export function MenuSeparator() {
  return <div className="menu-sep" role="separator" />;
}

export function MenuLabel({ children }: { children?: ReactNode }) {
  return <div className="menu-label">{children}</div>;
}
