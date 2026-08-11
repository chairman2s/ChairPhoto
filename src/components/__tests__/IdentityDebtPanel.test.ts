/**
 * Frontend tests for the identity-debt panel (issue #50). `vitest.config.ts` runs in
 * plain Node (no jsdom), so rendering `IdentityDebtPanel` itself is out of scope here
 * (that would need harness/config work this fix doesn't do). `STATE_LABEL`/`STATE_CLASS`
 * and the header/paging plural-singular logic are pure and exported specifically so
 * they're testable without a DOM.
 */

import { describe, it, expect } from "vitest";

import {
  STATE_LABEL,
  STATE_CLASS,
  summaryHeadline,
  pagingLabel,
} from "../IdentityDebtPanel";
import type { IdentityDebtState, PendingIdentitySummary } from "../../modules/api";

describe("STATE_LABEL / STATE_CLASS", () => {
  const states: IdentityDebtState[] = ["unreachable", "unwritable", "conflict"];

  it("has a label and a class for every debt state", () => {
    for (const s of states) {
      expect(STATE_LABEL[s]).toBeTruthy();
      expect(STATE_CLASS[s]).toBeTruthy();
    }
  });

  it("never styles Unreachable as a failure — it's the normal-state class", () => {
    // CONTEXT.md § Identity / AGENTS.md storage invariant: missing/unmounted storage is
    // a normal state, not evidence anything is wrong. Unreachable must render as neutral,
    // not warn/attention.
    expect(STATE_CLASS.unreachable).toBe("identity-state-normal");
    expect(STATE_CLASS.unwritable).not.toBe("identity-state-normal");
    expect(STATE_CLASS.conflict).not.toBe("identity-state-normal");
  });

  it("gives Unwritable and Conflict distinct treatments from each other", () => {
    // Unwritable is a hard failure; Conflict needs a human decision, not a retry — they
    // should not collapse into the same visual bucket.
    expect(STATE_CLASS.unwritable).not.toBe(STATE_CLASS.conflict);
  });
});

describe("summaryHeadline", () => {
  it("shows a loading state before the summary resolves", () => {
    expect(summaryHeadline(null)).toBe("Loading…");
  });

  it("uses singular copy/owes for exactly one copy", () => {
    const s: PendingIdentitySummary = { total: 1, conflicts: 0 };
    expect(summaryHeadline(s)).toBe("1 copy owes their identity to a sidecar");
  });

  it("uses plural copies/owe for zero copies", () => {
    const s: PendingIdentitySummary = { total: 0, conflicts: 0 };
    expect(summaryHeadline(s)).toBe("0 copies owe their identity to a sidecar");
  });

  it("uses plural copies/owe for many copies", () => {
    const s: PendingIdentitySummary = { total: 74488, conflicts: 0 };
    expect(summaryHeadline(s)).toBe("74488 copies owe their identity to a sidecar");
  });

  it("appends the conflict count only when there is at least one", () => {
    const noConflicts: PendingIdentitySummary = { total: 5, conflicts: 0 };
    expect(summaryHeadline(noConflicts)).not.toContain("conflict");

    const oneConflict: PendingIdentitySummary = { total: 5, conflicts: 1 };
    expect(summaryHeadline(oneConflict)).toBe(
      "5 copies owe their identity to a sidecar — 1 conflict",
    );

    const manyConflicts: PendingIdentitySummary = { total: 5, conflicts: 3 };
    expect(summaryHeadline(manyConflicts)).toBe(
      "5 copies owe their identity to a sidecar — 3 conflicts",
    );
  });
});

describe("pagingLabel", () => {
  it("reports an empty queue distinctly from an empty page past the end", () => {
    expect(pagingLabel(0, 0, 0)).toBe("No pending copies");
    expect(pagingLabel(1000, 0, 742)).toBe("No rows on this page");
  });

  it("shows the range and total for a full first page", () => {
    expect(pagingLabel(0, 500, 742)).toBe("Showing 1–500 of 742");
  });

  it("shows the range and total for a partial last page", () => {
    expect(pagingLabel(500, 242, 742)).toBe("Showing 501–742 of 742");
  });

  it("omits the total while the summary hasn't resolved yet", () => {
    expect(pagingLabel(0, 500, null)).toBe("Showing 1–500");
  });

  // The backend keeps `shown` (rows.length, refetched on every page turn) and `total`
  // (summarizePendingIdentity, fetched once when the panel mounts) in the same unit —
  // copies — and pins that with `list_pending_identity_page_windows_every_copy_exactly_once`
  // in `identity.rs`. But the two are still fetched independently: a concurrent scan or
  // repair can grow or shrink the queue while the panel is open, and nothing re-fetches
  // `total` until the panel closes and reopens — so `total` can be stale even though
  // neither backend query is wrong. `pagingLabel` must not trust a stale `total` at face
  // value: it must never render a range whose upper bound is larger than the total it
  // claims, e.g. "Showing 1–4 of 3".
  it("clamps a stale total so the label can never contradict itself", () => {
    expect(pagingLabel(0, 4, 3)).toBe("Showing 1–4 of 4");
    expect(pagingLabel(500, 4, 3)).toBe("Showing 501–504 of 504");
  });

  it("uses the real total once it catches up to (or exceeds) what's shown", () => {
    expect(pagingLabel(0, 4, 10)).toBe("Showing 1–4 of 10");
    expect(pagingLabel(0, 4, 4)).toBe("Showing 1–4 of 4");
  });
});
