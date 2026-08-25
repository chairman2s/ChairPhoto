import { useCallback, useEffect, useState } from "react";
import { convertFileSrc } from "@tauri-apps/api/core";
import { emptyTrash, listTrash, restorePhotos, type EmptyTrashReport } from "../modules/api";
import type { Photo } from "../modules/registry";

// The trash (cluster B, B2): the one surface whose job is to show photos the rest of the
// app hides.
//
// Two things it has to get right, and they pull in opposite directions.
//
// **Restoring must be effortless**, because that is what makes trashing safe to do
// quickly. Nothing was destroyed to hide a photo, so putting it back is one click and
// needs no confirmation.
//
// **Destroying must be hard**, because it is the only action in the app with no undo. It
// asks for a typed confirmation rather than a second button, since a second button is just
// a slower first button. And it can refuse: a photo whose copies are not all reachable is
// reported back rather than half-deleted, because deleting the copies we can see while a
// disconnected disk still holds one leaves an unreferenced survivor.

const CONFIRM_WORD = "delete";

export function TrashDialog({
  onClose,
  onChanged,
}: {
  onClose: () => void;
  /** Called after anything leaves the trash, so the grid picks the photos back up. */
  onChanged: () => void;
}) {
  const [photos, setPhotos] = useState<Photo[] | null>(null);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [typed, setTyped] = useState("");
  const [report, setReport] = useState<EmptyTrashReport | null>(null);
  const [busy, setBusy] = useState(false);

  const reload = useCallback(() => {
    listTrash()
      .then((p) => {
        setPhotos(p);
        setSelected(new Set());
      })
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(reload, [reload]);

  const toggle = (id: number) =>
    setSelected((s) => {
      const next = new Set(s);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const targets = () => (selected.size ? [...selected] : (photos ?? []).map((p) => p.id));

  const restore = async () => {
    setBusy(true);
    try {
      await restorePhotos(targets());
      reload();
      onChanged();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const destroy = async () => {
    setBusy(true);
    try {
      const r = await emptyTrash({ photoIds: targets(), confirm: true });
      setReport(r);
      setConfirming(false);
      setTyped("");
      reload();
      onChanged();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const count = photos?.length ?? 0;
  const acting = selected.size || count;

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal trash-dialog" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <div className="modal-title">Trash</div>
          <button className="chip" onClick={onClose}>
            Close
          </button>
        </div>

        <div className="modal-body">
          {error && <div className="modal-error">{error}</div>}

          {report && (
            <div className="trash-report">
              Deleted {report.deleted} photo{report.deleted === 1 ? "" : "s"} and{" "}
              {report.filesDeleted} file{report.filesDeleted === 1 ? "" : "s"}.
              {report.skippedUnreachable.length > 0 && (
                <>
                  {" "}
                  <b>
                    {report.skippedUnreachable.length} left alone — a disk holding a copy
                    could not be reached.
                  </b>{" "}
                  Nothing was deleted for those: removing the copies we can see would leave
                  one behind that nothing points at. Reconnect the disk and try again.
                </>
              )}
              {report.restoredMeanwhile.length > 0 && (
                <>
                  {" "}
                  {report.restoredMeanwhile.length} were restored while this was running and
                  were left alone.
                </>
              )}
              {report.aborted && (
                <>
                  {" "}
                  <b>Stopped early.</b> Something else took over — a restore, or the library
                  being switched — so the rest of the trash was not touched.
                </>
              )}
              {report.failed.length > 0 && (
                <>
                  {" "}
                  <b>{report.failed.length} could not be fully deleted.</b> Those photos
                  keep their place in the trash so you can retry — a file we could not
                  remove is recoverable, one with no catalog entry is not.
                  <ul className="trash-failures">
                    {report.failed.map(([id, why]) => (
                      <li key={id}>{why}</li>
                    ))}
                  </ul>
                </>
              )}
            </div>
          )}

          {!photos && <div className="panel-empty">Loading…</div>}

          {photos && count === 0 && (
            <div className="panel-empty">
              The trash is empty. Photos you trash are hidden everywhere but keep every
              tag, rating and edit until you delete them here.
            </div>
          )}

          {photos && count > 0 && (
            <>
              <div className="modal-sub">
                {count} photo{count === 1 ? "" : "s"}, most recently trashed first. Nothing
                here has been changed on disk — trashing hides, it does not delete.
              </div>

              <div className="trash-grid">
                {photos.map((p) => (
                  <button
                    key={p.id}
                    className={`trash-tile ${selected.has(p.id) ? "is-selected" : ""}`}
                    onClick={() => toggle(p.id)}
                    title={p.path}
                  >
                    <img src={convertFileSrc(String(p.id), "thumb")} alt="" />
                    <span className="trash-tile-name">{p.path.split("/").pop()}</span>
                  </button>
                ))}
              </div>

              <div className="row">
                <button className="scan-btn" onClick={restore} disabled={busy}>
                  Restore {selected.size ? selected.size : "all"}
                </button>
                {!confirming ? (
                  <button
                    className="chip trash-danger"
                    onClick={() => setConfirming(true)}
                    disabled={busy}
                  >
                    Delete {selected.size ? selected.size : "all"} permanently…
                  </button>
                ) : (
                  <span className="trash-confirm">
                    <label>
                      This destroys {acting} photo{acting === 1 ? "" : "s"} and every copy
                      of {acting === 1 ? "it" : "them"}. Type <code>{CONFIRM_WORD}</code> to
                      confirm:
                    </label>
                    <input
                      className="folder-input"
                      value={typed}
                      autoFocus
                      onChange={(e) => setTyped(e.target.value)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter" && typed === CONFIRM_WORD) destroy();
                        if (e.key === "Escape") {
                          setConfirming(false);
                          setTyped("");
                        }
                      }}
                    />
                    <button
                      className="chip trash-danger"
                      onClick={destroy}
                      disabled={typed !== CONFIRM_WORD || busy}
                    >
                      Delete
                    </button>
                    <button
                      className="chip"
                      onClick={() => {
                        setConfirming(false);
                        setTyped("");
                      }}
                    >
                      Cancel
                    </button>
                  </span>
                )}
              </div>

              <div className="trash-note">
                Deleting removes every copy ChairPhoto can reach, and its sidecars with it.
                A photo whose copies are not all reachable is skipped rather than partly
                deleted.
              </div>
            </>
          )}
        </div>
      </div>
    </div>
  );
}
