// @vitest-environment jsdom
/**
 * The menu/popover primitive has no Tauri or catalog dependency — it's pure React state
 * plus DOM focus management — so these tests exercise the contract directly rather than
 * mocking any backend:
 *
 *  - the trigger opens/closes a role="menu" panel; MenuItem always closes the menu before
 *    firing onSelect, so a selected action never fights the menu's own backdrop for focus.
 *  - a disabled MenuItem never fires, on principle (defense in depth even if the DOM
 *    itself already suppresses the click).
 *  - MenuCheckItem is the one row type that does NOT close on activation.
 *  - Escape closes the menu, returns focus to the trigger, and — critically — never
 *    reaches a window-level Escape handler (the app has one for the loupe and one for the
 *    grid's own right-click menu; this primitive must not also trigger those).
 *  - Arrow Up/Down roving focus is a flat query over the open panel's enabled items, so a
 *    disabled item is skipped by construction rather than by a step-over check.
 *  - MenuSub nests one flyout level and still closes the whole tree through the same
 *    context a top-level MenuItem uses.
 */
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

import { MenuButton, MenuCheckItem, MenuItem, MenuSub } from "../Menu";

describe("MenuButton", () => {
  it("opens on trigger click with role=menu", () => {
    render(
      <MenuButton label="Actions">
        <MenuItem onSelect={() => {}}>One</MenuItem>
      </MenuButton>,
    );
    expect(screen.queryByRole("menu")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Actions" }));
    expect(screen.getByRole("menu")).toBeTruthy();
  });

  it("item click fires onSelect exactly once and closes the menu", () => {
    const onSelect = vi.fn();
    render(
      <MenuButton label="Actions">
        <MenuItem onSelect={onSelect}>One</MenuItem>
      </MenuButton>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Actions" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "One" }));
    expect(onSelect).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("does not fire a disabled item's onSelect", () => {
    const onSelect = vi.fn();
    render(
      <MenuButton label="Actions">
        <MenuItem onSelect={onSelect} disabled>
          One
        </MenuItem>
      </MenuButton>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Actions" }));
    const item = screen.getByRole("menuitem", { name: "One" }) as HTMLButtonElement;
    expect(item.disabled).toBe(true);
    fireEvent.click(item);
    expect(onSelect).not.toHaveBeenCalled();
    // A disabled row is inert, not a dismissal — the menu stays open.
    expect(screen.getByRole("menu")).toBeTruthy();
  });

  it("toggles a MenuCheckItem without closing the menu", () => {
    function Harness() {
      const [checked, setChecked] = useState(false);
      return (
        <MenuButton label="View">
          <MenuCheckItem checked={checked} onChange={setChecked}>
            Show grid
          </MenuCheckItem>
        </MenuButton>
      );
    }
    render(<Harness />);
    fireEvent.click(screen.getByRole("button", { name: "View" }));
    const check = screen.getByRole("menuitemcheckbox", { name: "Show grid" });
    expect(check.getAttribute("aria-checked")).toBe("false");
    fireEvent.click(check);
    // Still open, and the controlled prop round-tripped through onChange.
    expect(screen.getByRole("menu")).toBeTruthy();
    expect(
      screen.getByRole("menuitemcheckbox", { name: "Show grid" }).getAttribute("aria-checked"),
    ).toBe("true");
  });

  it("Escape closes the menu, returns focus to the trigger, and does not reach window", () => {
    render(
      <MenuButton label="Actions">
        <MenuItem onSelect={() => {}}>One</MenuItem>
      </MenuButton>,
    );
    const trigger = screen.getByRole("button", { name: "Actions" });
    fireEvent.click(trigger);
    const item = screen.getByRole("menuitem", { name: "One" });
    item.focus();
    expect(document.activeElement).toBe(item);

    const windowSpy = vi.fn();
    window.addEventListener("keydown", windowSpy);
    try {
      fireEvent.keyDown(item, { key: "Escape" });
    } finally {
      window.removeEventListener("keydown", windowSpy);
    }

    expect(screen.queryByRole("menu")).toBeNull();
    expect(document.activeElement).toBe(trigger);
    expect(windowSpy).not.toHaveBeenCalled();
  });

  it("arrow-key roving focus skips a disabled item", () => {
    render(
      <MenuButton label="Actions">
        <MenuItem onSelect={() => {}}>One</MenuItem>
        <MenuItem onSelect={() => {}} disabled>
          Two
        </MenuItem>
        <MenuItem onSelect={() => {}}>Three</MenuItem>
      </MenuButton>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Actions" }));
    const one = screen.getByRole("menuitem", { name: "One" });
    const three = screen.getByRole("menuitem", { name: "Three" });
    one.focus();
    fireEvent.keyDown(one, { key: "ArrowDown" });
    expect(document.activeElement).toBe(three); // "Two" is disabled — skipped entirely
    fireEvent.keyDown(three, { key: "ArrowDown" });
    expect(document.activeElement).toBe(one); // wraps back around
    fireEvent.keyDown(one, { key: "ArrowUp" });
    expect(document.activeElement).toBe(three); // wraps the other way too
  });

  it("opens a MenuSub flyout and fires its nested item's onSelect", () => {
    const onSelect = vi.fn();
    render(
      <MenuButton label="Actions">
        <MenuSub label="More">
          <MenuItem onSelect={onSelect}>Nested</MenuItem>
        </MenuSub>
      </MenuButton>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Actions" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "More" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Nested" }));
    expect(onSelect).toHaveBeenCalledTimes(1);
    // Selecting the nested item closes the whole tree, not just the flyout.
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("renders a right-aligned muted badge", () => {
    render(
      <MenuButton label="Tags">
        <MenuItem onSelect={() => {}} badge="340">
          Portraits
        </MenuItem>
      </MenuButton>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Tags" }));
    const badge = screen.getByText("340");
    expect(badge.className).toContain("menu-badge");
  });

  it("applies the right-aligned panel class when align=right", () => {
    render(
      <MenuButton label="Actions" align="right">
        <MenuItem onSelect={() => {}}>One</MenuItem>
      </MenuButton>,
    );
    fireEvent.click(screen.getByRole("button", { name: "Actions" }));
    const menu = screen.getByRole("menu") as HTMLElement;
    expect(menu.classList.contains("menu-panel-right")).toBe(true);
  });
});
