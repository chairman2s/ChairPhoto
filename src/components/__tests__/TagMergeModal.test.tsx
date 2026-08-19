// @vitest-environment jsdom
/**
 * The merge preview is the feature (A5). Tag merge replaces hand-written SQL against a live
 * catalog, and what makes it less frightening than the SQL is that you can see what it will
 * do before it does it — so the properties worth pinning are about *that*, not about layout:
 *
 *  - picking a target previews with `dryRun: true` and commits with `dryRun: false`. If the
 *    preview ever ran with dryRun false, the "preview" would be the merge.
 *  - Merge cannot be pressed until a preview has come back. A confirm button that is live
 *    before the report is a confirm button that gets pressed before the report is read.
 *  - a refusal (path collision, auto-tag, a tag inside its own subtree) is shown, and does
 *    not leave a committable state behind.
 *  - the report distinguishes a plugin that is compiled out (`null`, nothing checked) from
 *    one that was checked and had nothing to do (`0`). Rendering `0` as a line would claim
 *    work that never happened; rendering `null` as `0` would claim a check that never ran.
 */
import { describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";

import { TagMergeModal, MergePreview } from "../TagMergeModal";
import type { TagMergeReport, TagWithCount } from "../../modules/api";

/** Every invoke the component made, in order. */
const calls: { command: string; args: Record<string, unknown> }[] = [];
/** What `merge_tags` should do next; replaced per test. */
let mergeBehavior: (args: Record<string, unknown>) => Promise<unknown> = () =>
  Promise.resolve(report());

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    invoke: (command: string, args: Record<string, unknown>) => {
      calls.push({ command, args: args ?? {} });
      if (command === "merge_tags") return mergeBehavior(args ?? {});
      return Promise.resolve(null);
    },
  };
});

function tag(id: number, fullPath: string, photoCount = 0): TagWithCount {
  const name = fullPath.split("/").pop() ?? fullPath;
  return {
    id,
    name,
    fullPath,
    parentId: null,
    photoCount,
  } as TagWithCount;
}

function report(over: Partial<TagMergeReport> = {}): TagMergeReport {
  return {
    targetId: 2,
    targetPath: "Cycling",
    sources: [{ id: 1, path: "Bike", uuid: "u-1" }],
    photosRetagged: 12,
    assignmentsCollapsed: 3,
    childrenReparented: 0,
    descendantsRepathed: 0,
    termsMoved: 0,
    termsSkipped: [],
    synonymsMoved: 0,
    groupsRepointed: 0,
    smartAlbumsRewritten: [],
    aliasesRecorded: 1,
    warnings: [],
    facesRepointed: null,
    faceRejectionsRepointed: null,
    classifiersDropped: null,
    suggestionsRepointed: null,
    ...over,
  };
}

const SOURCE = tag(1, "Bike", 12);
const TAGS = [SOURCE, tag(2, "Cycling", 40), tag(3, "Bike/Parts", 4), tag(4, "Food", 9)];

function open(onMerged = vi.fn()) {
  calls.length = 0;
  render(
    <TagMergeModal source={SOURCE} tags={TAGS} onClose={() => {}} onMerged={onMerged} />,
  );
  return onMerged;
}

const mergeCalls = () => calls.filter((c) => c.command === "merge_tags");

describe("TagMergeModal", () => {
  it("previews with a dry run, and only commits when the user confirms", async () => {
    const onMerged = open();

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: /^Cycling/ }));
    });

    // The preview is a real merge that was rolled back — hence dryRun: true.
    await waitFor(() => expect(mergeCalls()).toHaveLength(1));
    expect(mergeCalls()[0].args).toEqual({ sourceIds: [1], targetId: 2, dryRun: true });
    await screen.findByText(/12 photos gain Cycling/);
    expect(onMerged).not.toHaveBeenCalled();

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Merge" }));
    });
    await waitFor(() => expect(mergeCalls()).toHaveLength(2));
    expect(mergeCalls()[1].args).toEqual({ sourceIds: [1], targetId: 2, dryRun: false });
    expect(onMerged).toHaveBeenCalledTimes(1);
  });

  it("does not offer the source's own subtree as a target", () => {
    open();
    // "Bike/Parts" would put a tag inside itself; the backend refuses it, so it is not shown.
    expect(screen.queryByRole("button", { name: /Bike\/Parts/ })).toBeNull();
    expect(screen.getByRole("button", { name: /^Cycling/ })).toBeTruthy();
  });

  it("shows a refusal and leaves nothing committable", async () => {
    mergeBehavior = () =>
      Promise.reject(
        "merging 'Places' into 'Place' would collide at 'Place/Bergen', which already exists.",
      );
    const onMerged = open();

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: /^Cycling/ }));
    });

    await screen.findByRole("alert");
    expect(screen.getByRole("alert").textContent).toContain("Place/Bergen");
    // The Merge button exists but must be unusable: there is no report behind it.
    expect((screen.getByRole("button", { name: "Merge" }) as HTMLButtonElement).disabled).toBe(
      true,
    );
    expect(onMerged).not.toHaveBeenCalled();
    mergeBehavior = () => Promise.resolve(report());
  });
});

describe("MergePreview", () => {
  it("reports what a merge will touch, including what it cannot move", () => {
    render(
      <MergePreview
        report={report({
          childrenReparented: 2,
          descendantsRepathed: 5,
          smartAlbumsRewritten: ["Rides"],
          termsSkipped: ["Sykkel"],
          warnings: ["'Mono' is an auto-tag. The engine re-creates it by path…"],
        })}
      />,
    );

    expect(screen.getByText(/2 child tags move across/).textContent).toContain(
      "5 tag paths are rewritten",
    );
    expect(screen.getByText(/Smart album rules rewritten: Rides/)).toBeTruthy();
    // The skipped term is named, not just counted — it is what the user has to decide about.
    expect(screen.getByText(/already has them: Sykkel/)).toBeTruthy();
    expect(screen.getByText(/auto-tag/)).toBeTruthy();
  });

  it("separates a plugin that was checked from one that is not there", () => {
    const { rerender } = render(<MergePreview report={report({ facesRepointed: null })} />);
    expect(screen.queryByText(/face/i)).toBeNull();

    // Present but nothing to do: still no line — claiming "0 faces stay named" is noise.
    rerender(<MergePreview report={report({ facesRepointed: 0 })} />);
    expect(screen.queryByText(/face/i)).toBeNull();

    // Actual work gets a line.
    rerender(<MergePreview report={report({ facesRepointed: 7 })} />);
    expect(screen.getByText(/7 face\(s\) stay named/)).toBeTruthy();
  });

  it("says plainly when there is nothing to move", () => {
    render(
      <MergePreview
        report={report({ photosRetagged: 0, assignmentsCollapsed: 0, aliasesRecorded: 0 })}
      />,
    );
    expect(screen.getByText(/Nothing to move — the tag is empty/)).toBeTruthy();
  });
});
