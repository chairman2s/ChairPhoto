// @vitest-environment jsdom
/**
 * The trash is two verbs pulling in opposite directions, and the contract is that
 * asymmetry:
 *
 *  - restoring is one click with no confirmation, because nothing was destroyed to hide a
 *    photo and easy restore is what makes trashing safe to do quickly.
 *  - destroying asks you to type the word. A second button is just a slower first button,
 *    and this is the only action in the app with no undo.
 *  - a delete that could not reach every copy is *reported*, not silently partial — the
 *    user has to learn that a disk needs reconnecting rather than assume it worked.
 *  - "no selection" means everything, and the button says which, because deleting all when
 *    you meant one is the mistake this dialog exists to prevent.
 */
import { describe, expect, it, vi, beforeEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

import { TrashDialog } from "../TrashDialog";
import type { Photo } from "../../modules/registry";

const calls: { command: string; args: Record<string, unknown> }[] = [];
let trash: Photo[] = [];
let emptyResult: () => Promise<unknown> = () =>
  Promise.resolve({ deleted: 1, filesDeleted: 2, skippedUnreachable: [], failed: [], restoredMeanwhile: [], aborted: false });

vi.mock("@tauri-apps/api/core", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@tauri-apps/api/core")>();
  return {
    ...actual,
    convertFileSrc: (id: string) => `thumb://${id}`,
    invoke: (command: string, args: Record<string, unknown>) => {
      calls.push({ command, args: args ?? {} });
      if (command === "list_trash") return Promise.resolve(trash);
      if (command === "empty_trash") return emptyResult();
      if (command === "restore_photos") return Promise.resolve(1);
      return Promise.resolve(null);
    },
  };
});

function photo(id: number): Photo {
  return {
    id,
    uuid: `u${id}`,
    path: `2026/DSC_000${id}.ARW`,
    rating: 0,
    label: "",
    pickState: "none",
    captureTime: null,
    width: null,
    height: null,
    cameraModel: null,
    lens: null,
    aperture: null,
    shutterSpeed: null,
    iso: null,
    metadataReady: 1,
    sharpness: null,
    sharpnessMethod: null,
    burstFlag: null,
  };
}

const text = () => (document.body.textContent ?? "").replace(/\s+/g, " ");
const sent = (c: string) => calls.filter((x) => x.command === c);

beforeEach(() => {
  calls.length = 0;
  trash = [photo(1), photo(2), photo(3)];
  emptyResult = () => Promise.resolve({ deleted: 1, filesDeleted: 2, skippedUnreachable: [], failed: [], restoredMeanwhile: [], aborted: false });
});

describe("TrashDialog", () => {
  it("restores without asking, because nothing was destroyed to hide it", async () => {
    const onChanged = vi.fn();
    render(<TrashDialog onClose={() => {}} onChanged={onChanged} />);

    await waitFor(() => expect(screen.getByText(/Restore all/)).toBeTruthy());
    fireEvent.click(screen.getByText(/Restore all/));

    await waitFor(() => expect(sent("restore_photos").length).toBe(1));
    expect(sent("restore_photos")[0].args).toEqual({ photoIds: [1, 2, 3] });
    expect(onChanged).toHaveBeenCalled();
  });

  it("will not destroy anything until the word is typed", async () => {
    render(<TrashDialog onClose={() => {}} onChanged={vi.fn()} />);

    await waitFor(() => expect(screen.getByText(/Delete all permanently/)).toBeTruthy());
    fireEvent.click(screen.getByText(/Delete all permanently/));

    await waitFor(() => expect(text()).toMatch(/Type delete to confirm/));
    const confirmButton = screen.getByText("Delete");
    expect((confirmButton as HTMLButtonElement).disabled).toBe(true);

    fireEvent.change(screen.getByRole("textbox"), { target: { value: "delet" } });
    expect((screen.getByText("Delete") as HTMLButtonElement).disabled).toBe(true);
    expect(sent("empty_trash")).toHaveLength(0);

    fireEvent.change(screen.getByRole("textbox"), { target: { value: "delete" } });
    fireEvent.click(screen.getByText("Delete"));

    await waitFor(() => expect(sent("empty_trash").length).toBe(1));
    expect(sent("empty_trash")[0].args).toMatchObject({ confirm: true, photoIds: [1, 2, 3] });
  });

  it("acts on the selection once there is one, and says so", async () => {
    render(<TrashDialog onClose={() => {}} onChanged={vi.fn()} />);

    await waitFor(() => expect(screen.getByText(/Restore all/)).toBeTruthy());
    fireEvent.click(screen.getByTitle("2026/DSC_0002.ARW"));

    expect(screen.getByText(/Restore 1/)).toBeTruthy();
    fireEvent.click(screen.getByText(/Restore 1/));

    await waitFor(() => expect(sent("restore_photos").length).toBe(1));
    expect(sent("restore_photos")[0].args).toEqual({ photoIds: [2] });
  });

  it("reports what it refused to delete rather than implying success", async () => {
    emptyResult = () =>
      Promise.resolve({ deleted: 2, filesDeleted: 4, skippedUnreachable: [7, 8], failed: [], restoredMeanwhile: [], aborted: false });
    render(<TrashDialog onClose={() => {}} onChanged={vi.fn()} />);

    await waitFor(() => expect(screen.getByText(/Delete all permanently/)).toBeTruthy());
    fireEvent.click(screen.getByText(/Delete all permanently/));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "delete" } });
    fireEvent.click(screen.getByText("Delete"));

    await waitFor(() => expect(text()).toMatch(/2 left alone/));
    expect(text()).toMatch(/could not be reached/);
    expect(text()).toMatch(/Reconnect the disk/);
  });

  it("shows a partial failure rather than letting it vanish into a count", async () => {
    // A photo that could not be fully deleted keeps its catalog row on purpose, so the
    // report has to say which and why — otherwise the user believes it is gone.
    emptyResult = () =>
      Promise.resolve({
        deleted: 1,
        filesDeleted: 1,
        skippedUnreachable: [],
        failed: [[9, "/nas/2026/DSC_0009.ARW could not be removed"]],
        restoredMeanwhile: [],
        aborted: false,
      });
    render(<TrashDialog onClose={() => {}} onChanged={vi.fn()} />);

    await waitFor(() => expect(screen.getByText(/Delete all permanently/)).toBeTruthy());
    fireEvent.click(screen.getByText(/Delete all permanently/));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "delete" } });
    fireEvent.click(screen.getByText("Delete"));

    await waitFor(() => expect(text()).toMatch(/1 could not be fully deleted/));
    expect(text()).toContain("DSC_0009.ARW could not be removed");
    expect(text()).toMatch(/keep their place in the trash so you can retry/);
  });

  it("names the sidecar backups it took, apart from the originals", async () => {
    // Offload leaves these and says so; delete takes them and has to say so too. Folding
    // them into the file count would inflate "originals destroyed" with a file the user
    // never knew existed (#84).
    emptyResult = () =>
      Promise.resolve({
        deleted: 1,
        filesDeleted: 2,
        sidecarBackupsDeleted: 2,
        skippedUnreachable: [],
        failed: [],
        restoredMeanwhile: [],
        aborted: false,
      });
    render(<TrashDialog onClose={() => {}} onChanged={vi.fn()} />);

    await waitFor(() => expect(screen.getByText(/Delete all permanently/)).toBeTruthy());
    fireEvent.click(screen.getByText(/Delete all permanently/));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "delete" } });
    fireEvent.click(screen.getByText("Delete"));

    await waitFor(() => expect(text()).toMatch(/2 sidecar backups went with them/));
    expect(text()).toMatch(/Deleted 1 photo and 2 files/);
    expect(text()).toMatch(/nothing is left to describe/);
  });

  it("says nothing about sidecar backups when there were none", async () => {
    render(<TrashDialog onClose={() => {}} onChanged={vi.fn()} />);

    await waitFor(() => expect(screen.getByText(/Delete all permanently/)).toBeTruthy());
    fireEvent.click(screen.getByText(/Delete all permanently/));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "delete" } });
    fireEvent.click(screen.getByText("Delete"));

    await waitFor(() => expect(text()).toMatch(/Deleted 1 photo and 2 files/));
    expect(text()).not.toMatch(/sidecar backup/);
  });

  it("says when it stopped early rather than implying it finished", async () => {
    // A restore or a library switch stands the delete down mid-run. Reporting only the
    // count would let the user believe the rest of the trash was emptied.
    emptyResult = () =>
      Promise.resolve({
        deleted: 1,
        filesDeleted: 1,
        skippedUnreachable: [],
        failed: [],
        restoredMeanwhile: [4],
        aborted: true,
      });
    render(<TrashDialog onClose={() => {}} onChanged={vi.fn()} />);

    await waitFor(() => expect(screen.getByText(/Delete all permanently/)).toBeTruthy());
    fireEvent.click(screen.getByText(/Delete all permanently/));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "delete" } });
    fireEvent.click(screen.getByText("Delete"));

    await waitFor(() => expect(text()).toMatch(/Stopped early/));
    expect(text()).toMatch(/1 were restored while this was running/);
    expect(text()).toMatch(/the rest of the trash was not touched/);
  });

  it("cancelling the confirmation destroys nothing", async () => {
    render(<TrashDialog onClose={() => {}} onChanged={vi.fn()} />);

    await waitFor(() => expect(screen.getByText(/Delete all permanently/)).toBeTruthy());
    fireEvent.click(screen.getByText(/Delete all permanently/));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "delete" } });
    fireEvent.click(screen.getByText("Cancel"));

    expect(sent("empty_trash")).toHaveLength(0);
    expect(screen.getByText(/Delete all permanently/)).toBeTruthy();
  });

  it("says what the trash is when it is empty, not just that it is", async () => {
    trash = [];
    render(<TrashDialog onClose={() => {}} onChanged={vi.fn()} />);

    await waitFor(() => expect(text()).toMatch(/The trash is empty/));
    expect(text()).toMatch(/keep every tag, rating and edit/);
    expect(screen.queryByText(/Delete all permanently/)).toBeNull();
  });

  it("states that trashing has not touched the files", async () => {
    render(<TrashDialog onClose={() => {}} onChanged={vi.fn()} />);

    await waitFor(() => expect(text()).toMatch(/trashing hides, it does not delete/));
    expect(text()).toMatch(/removes every copy ChairPhoto can reach, and its sidecars/);
  });
});
