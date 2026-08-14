/**
 * Frontend tests for the identity-debt panel (issue #50) and its conflict resolutions
 * (#33). This file stays in the default plain-Node environment: `STATE_LABEL`/`STATE_CLASS`,
 * the header/paging string math, and the "which field does a decision act on" helpers are
 * pure and exported specifically so they're testable without a DOM. The parts that need one
 * — clicking Adopt/Overwrite/Dismiss and seeing what reaches the backend — live in
 * `IdentityDebtPanel.actions.test.tsx`, which opts into jsdom (issue #57's harness).
 */

import { describe, it, expect } from "vitest";

import {
  STATE_LABEL,
  STATE_CLASS,
  conflictField,
  dismissedField,
  repairProgressLine,
  repairSummaryLine,
  resolutionMessage,
  summaryHeadline,
  pagingLabel,
} from "../IdentityDebtPanel";
import type {
  IdentityDebtState,
  IdentityRepairSummary,
  PendingIdentity,
  PendingIdentityField,
  PendingIdentitySummary,
} from "../../modules/api";

function field(over: Partial<PendingIdentityField> = {}): PendingIdentityField {
  return {
    field: "identifier",
    state: "conflict",
    attempts: 1,
    error: "sidecar carries a different identity (other-uuid); left untouched",
    lastAttemptAt: 0,
    dismissedAt: 0,
    ...over,
  };
}

function copy(fields: PendingIdentityField[]): PendingIdentity {
  return { photoId: 1, path: "a.arw", volumeId: 1, relativePath: "a.arw", fields };
}

describe("STATE_LABEL / STATE_CLASS", () => {
  const states: IdentityDebtState[] = ["unreachable", "unwritable", "conflict", "dismissed"];

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

  it("styles Dismissed as neutral — the decision is already made", () => {
    // A dismissed copy asks nothing of anyone: it is on the record, never retried, and not
    // counted as debt. Styling it as attention/warn would re-raise the very thing the user
    // just decided to stop being asked about.
    expect(STATE_CLASS.dismissed).toBe("identity-state-normal");
  });
});

describe("conflictField / dismissedField", () => {
  // Only `identifier` can conflict — `bind_sidecar_import_batch` returns Bound or
  // Unwritable, never Conflict — so a decision is never offered for an import-batch field.
  it("finds only an un-dismissed identifier conflict", () => {
    expect(conflictField(copy([field()]))?.field).toBe("identifier");
    expect(conflictField(copy([field({ state: "unwritable" })]))).toBeNull();
    expect(conflictField(copy([field({ state: "dismissed", dismissedAt: 5 })]))).toBeNull();
    expect(conflictField(copy([field({ field: "import_batch" })]))).toBeNull();
  });

  it("finds a dismissed identifier so it can be restored", () => {
    const dismissed = copy([field({ state: "dismissed", dismissedAt: 5 })]);
    expect(dismissedField(dismissed)?.dismissedAt).toBe(5);
    expect(dismissedField(copy([field()]))).toBeNull();
  });

  it("picks the identifier field out of a copy that owes both", () => {
    // A copy can owe its import batch as well; the decision is still only about identity.
    const both = copy([field(), field({ field: "import_batch", state: "unreachable" })]);
    expect(conflictField(both)?.field).toBe("identifier");
  });
});

describe("resolutionMessage", () => {
  const base = {
    catalogUuid: "cat-uuid",
    previousSidecarUuid: "file-uuid",
    recheckedCopies: 0,
    sidecarBackup: null as string | null,
  };

  it("says what Adopt did to the CATALOG, and mentions re-checked copies only when there were any", () => {
    expect(resolutionMessage({ ...base, action: "adopt", catalogUuid: "file-uuid" })).toBe(
      "Adopted file-uuid from the sidecar. The catalog now uses it.",
    );
    expect(
      resolutionMessage({ ...base, action: "adopt", catalogUuid: "file-uuid", recheckedCopies: 1 }),
    ).toContain("1 other copy of this photo re-checked");
    expect(
      resolutionMessage({ ...base, action: "adopt", catalogUuid: "file-uuid", recheckedCopies: 2 }),
    ).toContain("2 other copies of this photo re-checked");
  });

  it("names both the identity written and the one destroyed, and where the backup went", () => {
    const message = resolutionMessage({
      ...base,
      action: "overwrite",
      sidecarBackup: "/photos/a.arw.xmp.chairphoto-backup",
    });
    expect(message).toContain("cat-uuid");
    expect(message).toContain("file-uuid");
    expect(message).toContain("/photos/a.arw.xmp.chairphoto-backup");
  });

  it("says an older backup was kept rather than implying nothing was preserved", () => {
    // `sidecarBackup: null` means an earlier snapshot already existed and was deliberately
    // left alone — NOT that the overwrite went through unprotected.
    const message = resolutionMessage({ ...base, action: "overwrite" });
    expect(message).toContain("already kept and left untouched");
  });

  it("states plainly that Dismiss changed nothing", () => {
    const message = resolutionMessage({ ...base, action: "dismiss" });
    expect(message).toContain("neither retried nor counted as debt");
  });
});

describe("summaryHeadline", () => {
  it("shows a loading state before the summary resolves", () => {
    expect(summaryHeadline(null)).toBe("Loading…");
  });

  it("uses singular copy/owes for exactly one copy", () => {
    const s: PendingIdentitySummary = { total: 1, conflicts: 0, dismissed: 0 };
    expect(summaryHeadline(s)).toBe("1 copy owes their identity to a sidecar");
  });

  it("uses plural copies/owe for zero copies", () => {
    const s: PendingIdentitySummary = { total: 0, conflicts: 0, dismissed: 0 };
    expect(summaryHeadline(s)).toBe("0 copies owe their identity to a sidecar");
  });

  it("uses plural copies/owe for many copies", () => {
    const s: PendingIdentitySummary = { total: 74488, conflicts: 0, dismissed: 0 };
    expect(summaryHeadline(s)).toBe("74488 copies owe their identity to a sidecar");
  });

  it("appends the conflict count only when there is at least one", () => {
    const noConflicts: PendingIdentitySummary = { total: 5, conflicts: 0, dismissed: 0 };
    expect(summaryHeadline(noConflicts)).not.toContain("conflict");

    const oneConflict: PendingIdentitySummary = { total: 5, conflicts: 1, dismissed: 0 };
    expect(summaryHeadline(oneConflict)).toBe(
      "5 copies owe their identity to a sidecar — 1 conflict",
    );

    const manyConflicts: PendingIdentitySummary = { total: 5, conflicts: 3, dismissed: 0 };
    expect(summaryHeadline(manyConflicts)).toBe(
      "5 copies owe their identity to a sidecar — 3 conflicts",
    );
  });

  it("names dismissed copies separately, because they are not part of the total", () => {
    // `total` deliberately excludes dismissed copies (they stopped being debt), so without
    // naming them "0 copies owe…" would read as "nothing was ever wrong here".
    const onlyDismissed: PendingIdentitySummary = { total: 0, conflicts: 0, dismissed: 2 };
    expect(summaryHeadline(onlyDismissed)).toBe(
      "0 copies owe their identity to a sidecar — 2 dismissed",
    );

    const both: PendingIdentitySummary = { total: 5, conflicts: 1, dismissed: 2 };
    expect(summaryHeadline(both)).toBe(
      "5 copies owe their identity to a sidecar — 1 conflict, 2 dismissed",
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

describe("repairSummaryLine", () => {
  function summary(over: Partial<IdentityRepairSummary> = {}): IdentityRepairSummary {
    return {
      bound: 0,
      unreachable: 0,
      conflicts: 0,
      failed: 0,
      superseded: 0,
      total: 0,
      aborted: false,
      ...over,
    };
  }

  it("says nothing before a pass has reported", () => {
    expect(repairSummaryLine(null)).toBe("");
  });

  it("reports a completed pass in the four CONTEXT.md outcomes", () => {
    expect(
      repairSummaryLine(summary({ bound: 3, unreachable: 2, conflicts: 1, failed: 4, total: 10 })),
    ).toBe("Finished — bound 3 · still unreachable 2 · conflict 1 · failed 4");
  });

  // The whole point of `aborted`: a cancel can land on row 3 of 74,488. Presenting those
  // counters the way a finished pass's are read as "the queue is now clean" — which the user
  // acts on, by not running another pass.
  it("labels a stopped pass as stopped, with how far it got", () => {
    const line = repairSummaryLine(
      summary({ bound: 2, unreachable: 1, total: 74488, aborted: true }),
    );
    expect(line).toContain("Stopped after 3 of 74488");
    expect(line).not.toContain("Finished");
  });

  it("names superseded rows only when there were some", () => {
    expect(repairSummaryLine(summary({ bound: 1, total: 1 }))).not.toContain("decided elsewhere");
    // A row somebody else decided under the pass is not a failure, and not an outcome the
    // pass produced (#34) — so it is reported, and reported apart from the four.
    const line = repairSummaryLine(summary({ bound: 1, superseded: 2, total: 3 }));
    expect(line).toContain("2 decided elsewhere while the pass ran");
    expect(line).toContain("failed 0");
  });

  it("counts superseded rows toward how far a stopped pass got", () => {
    expect(
      repairSummaryLine(summary({ bound: 1, superseded: 1, total: 9, aborted: true })),
    ).toContain("Stopped after 2 of 9");
  });
});

describe("repairProgressLine", () => {
  it("shows the denominator once the pass has counted the queue", () => {
    expect(repairProgressLine(120, 74488)).toBe("Repairing… 120 of 74488");
  });

  it("omits it while the queue is still uncounted, rather than claiming a total of 0", () => {
    expect(repairProgressLine(0, 0)).toBe("Repairing…");
  });
});
