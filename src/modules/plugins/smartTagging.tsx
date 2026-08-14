// Smart Tagging module — H7d: index panel + suggestion UI.
//
// Architecture:
//   • `backendFeature: "smarttags"` — requires the Rust `smarttags` Cargo feature.
//   • Inspector panel ("Similar tags"): lists kNN suggestions for the active photo with
//     per-suggestion accept/reject actions; "Index" button to run the embedding job.
//   • Settings panel: model path, train-classifiers action (H7e), delete index action.
//
// The backend computes embeddings from cached previews and stores them in
// `smarttags__embeddings`. For each photo, kNN scans the indexed embeddings to find
// similar photos and propagates their confirmed tags as ranked suggestions. Mirroring
// the AI tagging module's architecture, but fully local (no API calls).

import { useCallback, useEffect, useRef, useState } from "react";
import type { ChairPhotoAPI, ChairPhotoModule } from "../registry";
import { useHostSelection } from "../host";
// The shared owned-listener utility (issue #13). It imports nothing but React and the
// contract's `Unsubscribe` type, so using it does not reach into app internals — the same
// standing as `useHostSelection` above.
import { forJob, terminalBuffer, useOwnedListeners } from "../ownedEvents";

// ── Backend commands and events (owned by this module) ────────────────────────
// Per the module contract these go through `ChairPhotoAPI.invoke` / `.onEvent`
// rather than core's `api.ts`, so the command and event names travel with the
// module rather than being pinned into the host.

/** Progress event for `smarttags:progress`. */
interface SmarttagsProgress {
  done: number;
  total: number;
  /** Indexing-job id. Events from a superseded run carry its old id — filter on this.
   *  Always present: commands/smarttags.rs:79-83 declares it `u64`, not an Option. */
  job: number;
}

/** Terminal event for the smart-tagging indexing job (`smarttags:index_done`). */
interface SmarttagsIndexDone {
  ok: boolean;
  done: number;
  total: number;
  /** Photos skipped because no reachable copy exists (NAS offline). Still queued. */
  offline: number;
  /** Photos whose preview couldn't be generated (unreadable file). Still queued. */
  failed: number;
  /** True when the run stopped early (cancel, superseded, catalog switch). */
  aborted: boolean;
  /** Which run finished — compare against the id `smarttagsIndexPhotos` returned. */
  job: number;
  error: string | null;
}

/** A ranked smart-tagging suggestion from kNN. */
interface SmarttagsSuggestion {
  /** Full tag path, e.g. `"Animals/Birds/Gull"`. */
  path: string;
  /** Normalized kNN score in [0, 1]. */
  confidence: number;
  /** Photo IDs of the neighbours that drove this suggestion (provenance). */
  sourcePhotoIds: number[];
  /** `true` if the tag already exists in the catalog. */
  existingTagId: number | null;
}

/** Presence report for the Smart Tagging CLIP model (mirrors the faces model report). */
interface SmarttagsModelReport {
  key: string;
  /** Present on disk and loadable. */
  present: boolean;
  /** Absolute path where the model lives (or would live once downloaded). */
  path: string;
  /** True when the path came from the `smarttags.model_path` setting. */
  custom: boolean;
  /** When absent/unreadable, a short human-readable reason. */
  detail: string | null;
}

/** Overall Smart Tagging model status — `ready` gates indexing and suggestions. */
interface SmarttagsModelStatus {
  ready: boolean;
  model: SmarttagsModelReport;
}

/** Cheap presence check for the Smart Tagging model (no hashing). */
const smarttagsModelStatus = (api: ChairPhotoAPI) =>
  api.invoke<SmarttagsModelStatus>("smarttags_model_status");

/** Download the pinned default CLIP model (~350 MB, checksum-verified). Safe to re-invoke. */
const smarttagsDownloadModel = (api: ChairPhotoAPI) =>
  api.invoke<SmarttagsModelStatus>("smarttags_download_model");

/** Start embedding all unindexed photos. Returns the job id for tracking progress. */
const smarttagsIndexPhotos = (api: ChairPhotoAPI) =>
  api.invoke<number>("smarttags_index_photos");

/** Snapshot of the running smart-tags indexing job, or null when idle. */
interface SmarttagsIndexStatus {
  job: number;
  done: number;
  total: number;
}

const smarttagsIndexStatus = (api: ChairPhotoAPI) =>
  api.invoke<SmarttagsIndexStatus | null>("smarttags_index_status");

/** Generate kNN suggestions for a photo (H7c). Stores them as pending in the database. */
const smarttagsSuggestTags = (api: ChairPhotoAPI, photoId: number) =>
  api.invoke<number>("smarttags_suggest_tags", { photoId });

/** Load pending suggestions for a photo from the database. */
const smarttagsLoadSuggestions = (api: ChairPhotoAPI, photoId: number) =>
  api.invoke<SmarttagsSuggestion[]>("smarttags_load_suggestions", { photoId });

/**
 * Accept a suggestion: assign the tag to the photo, mark it as confirmed. Resolves with
 * the assigned tag id, including the id of a tag created for a path that did not exist
 * yet (commands/smarttags.rs:417-438 returns `Result<i64, String>`). The id alone does
 * not say which of those happened — both branches return the same bare scalar.
 * `SmarttagsSuggestion.existingTagId` does not settle it either: it comes from a live
 * lookup when the pending suggestion is loaded (`plugins/smarttags/suggest.rs:351`) —
 * generation itself persists no tag id — and accept looks the path up again at
 * invocation. Neither that snapshot nor the returned id proves which branch a given
 * accept took.
 */
const smarttagsAcceptSuggestion = (api: ChairPhotoAPI, photoId: number, path: string) =>
  api.invoke<number>("smarttags_accept_suggestion", { photoId, path });

/** Reject a suggestion: won't be re-proposed for this photo. */
const smarttagsRejectSuggestion = (api: ChairPhotoAPI, photoId: number, path: string) =>
  api.invoke<void>("smarttags_reject_suggestion", { photoId, path });

/**
 * Subscribe to smarttags indexing progress events (`smarttags:progress`).
 *
 * `onEvent` is optional on the host contract, so this resolves to null when the host
 * predates it. Progress is cosmetic — see `subscribeToProgress`.
 */
const onSmarttagsProgress = (
  api: ChairPhotoAPI,
  handler: (p: SmarttagsProgress) => void,
): Promise<(() => void) | null> =>
  api.onEvent?.<SmarttagsProgress>("smarttags:progress", handler) ?? Promise.resolve(null);

/**
 * Subscribe to the smarttags indexing terminal event (`smarttags:index_done`).
 *
 * Unlike the two progress streams this one is **required**: it is the only path that
 * ends an indexing run in this panel, so a run started without it would sit in
 * "indexing" forever. Callers must refuse to start when this resolves to null.
 */
const onSmarttagsIndexDone = (
  api: ChairPhotoAPI,
  handler: (p: SmarttagsIndexDone) => void,
): Promise<(() => void) | null> =>
  api.onEvent?.<SmarttagsIndexDone>("smarttags:index_done", handler) ?? Promise.resolve(null);

/** Progress event for `smarttags:download_progress` — fired during `smarttagsDownloadModel`.
 *  `total` is null when the server omitted `Content-Length`. */
interface SmarttagsDownloadProgress {
  done: number;
  total: number | null;
}

/**
 * Subscribe to model-download progress events (`smarttags:download_progress`).
 * Attach before calling `smarttagsDownloadModel`; detach when it settles.
 *
 * Cosmetic: the download command resolves with the model status, so the outcome is
 * known without these events — they only drive the byte counter on the button.
 */
const onSmarttagsDownloadProgress = (
  api: ChairPhotoAPI,
  handler: (p: SmarttagsDownloadProgress) => void,
): Promise<(() => void) | null> =>
  api.onEvent?.<SmarttagsDownloadProgress>("smarttags:download_progress", handler) ??
  Promise.resolve(null);

/** Cancel an in-flight smarttags indexing job. */
const smarttagsCancelJob = (api: ChairPhotoAPI) =>
  api.invoke<void>("smarttags_index_cancel");

/** Delete the entire Smart Tagging index (embeddings + suggestions + classifiers). */
const smarttagsDeleteIndex = (api: ChairPhotoAPI) =>
  api.invoke<void>("smarttags_delete_index");

/** Summary returned by `smarttags_train_classifiers` (H7e). */
interface SmarttagsTrainResult {
  /** Tags examined (those with enough confirmed embeddings to qualify). */
  examined: number;
  /** Tags where a classifier was (re)trained. */
  trained: number;
  /** Tags whose classifier was still fresh (skipped). */
  skipped: number;
}

/** Train (or refresh) the per-tag logistic classifiers over the CLIP index. */
const smarttagsTrainClassifiers = (api: ChairPhotoAPI) =>
  api.invoke<SmarttagsTrainResult>("smarttags_train_classifiers");

// ── Shared model-download hook + button ────────────────────────────────────

/** Format download progress into a human-readable label. */
function formatDownloadProgress(p: SmarttagsDownloadProgress | null): string {
  if (!p) return "Downloading… (~350 MB)";
  const doneMB = (p.done / (1024 * 1024)).toFixed(1);
  if (p.total) {
    const pct = Math.round((p.done / p.total) * 100);
    const totalMB = Math.round(p.total / (1024 * 1024));
    return `Downloading… ${pct}% of ${totalMB} MB`;
  }
  return `Downloading… ${doneMB} MB`;
}

/** Listener slot for `smarttags:download_progress`. See `useOwnedListeners`. */
const DOWNLOAD_PROGRESS = "download-progress";

/**
 * Hook that encapsulates the model download lifecycle: subscribes to
 * `smarttags:download_progress` while a download is in flight, keeps local
 * progress state, and cleans up the listener on unmount.
 *
 * The listener lifecycle — an owner token per attempt, a late registration stopped rather
 * than stored, release scoped to the attempt that installed it — is `useOwnedListeners`
 * (issue #13), not hand-rolled here any more. What stays local is the *policy*: this stream
 * is cosmetic, so an unsupported host or a failed registration must not stop the download.
 */
function useModelDownload(api: ChairPhotoAPI, onSuccess: (s: SmarttagsModelStatus) => void) {
  const [downloading, setDownloading] = useState(false);
  const [downloadProgress, setDownloadProgress] = useState<SmarttagsDownloadProgress | null>(null);
  const [downloadError, setDownloadError] = useState("");
  const listeners = useOwnedListeners();
  /** The in-flight attempt. `downloading` is state and has not updated by the time a second
   *  same-tick click lands, so this ref is what actually excludes. */
  const attemptRef = useRef<symbol | null>(null);

  useEffect(
    () => () => {
      // The listeners release themselves on unmount; the claim is this hook's own.
      attemptRef.current = null;
    },
    [],
  );

  const startDownload = useCallback(async () => {
    if (downloading || attemptRef.current) return;
    const attempt = Symbol("smarttags-download");
    attemptRef.current = attempt;
    const mine = () => listeners.isMounted() && attemptRef.current === attempt;
    setDownloading(true);
    setDownloadError("");
    setDownloadProgress(null);
    try {
      // Subscribe before invoking so we don't miss early chunks. Cosmetic: it drives the
      // byte counter, while the outcome comes from the command's own result — so neither an
      // absent `onEvent` nor a rejected registration is a reason to refuse the download.
      const outcome = await listeners.attach(
        DOWNLOAD_PROGRESS,
        attempt,
        () => onSmarttagsDownloadProgress(api, (p) => {
          if (mine()) setDownloadProgress(p);
        }),
        mine,
      );
      // The panel can unmount, or a newer attempt take over, while that registration is
      // pending. `attach` has already stopped the listener; what is left is not to start a
      // ~350 MB download for a panel that is gone.
      if (outcome === "superseded") return;
      const s = await smarttagsDownloadModel(api);
      if (mine()) onSuccess(s);
    } catch (e) {
      if (mine()) setDownloadError(String(e));
    } finally {
      // Release this attempt's own listener and claim unconditionally; only the UI reset
      // needs the panel to still be here.
      listeners.release(DOWNLOAD_PROGRESS, attempt);
      if (attemptRef.current === attempt) attemptRef.current = null;
      if (listeners.isMounted()) {
        setDownloadProgress(null);
        setDownloading(false);
      }
    }
  }, [api, downloading, onSuccess, listeners]);

  return { downloading, downloadProgress, downloadError, startDownload };
}

/**
 * Download button with live progress. Rendered by both `SmarttagsSettings` and
 * `SimilarTagsPanel` — extracted here to avoid duplicating the label/button logic.
 */
function ModelDownloadButton({
  api,
  onSuccess,
}: {
  api: ChairPhotoAPI;
  onSuccess: (s: SmarttagsModelStatus) => void;
}) {
  const { downloading, downloadProgress, downloadError, startDownload } =
    useModelDownload(api, onSuccess);
  return (
    <>
      <button
        className="chip chip-on"
        onClick={() => void startDownload()}
        disabled={downloading}
      >
        {downloading
          ? formatDownloadProgress(downloadProgress)
          : "Download model (~350 MB)"}
      </button>
      {downloadError && <span className="iptc-error">{downloadError}</span>}
    </>
  );
}

// ── SimilarTagsPanel — inspector panel ────────────────────────────────────

/** Human-readable summary of how an index run ended, from its honest counters. */
function indexDoneMessage(d: SmarttagsIndexDone): string {
  if (d.total === 0) {
    return "Indexing: nothing to do — all photos are already indexed.";
  }
  const skips: string[] = [];
  if (d.offline > 0) skips.push(`${d.offline} offline (connect the NAS and re-run)`);
  if (d.failed > 0) skips.push(`${d.failed} unreadable`);
  const skipNote = skips.length > 0 ? ` Skipped: ${skips.join(", ")} — still queued.` : "";
  if (d.aborted) {
    return `Indexing cancelled at ${d.done} of ${d.total} photos.${skipNote}`;
  }
  if (d.done < d.total) {
    return `Indexing finished: ${d.done} of ${d.total} photos processed.${skipNote}`;
  }
  return `Indexing complete: ${d.total} photo${d.total !== 1 ? "s" : ""} processed.`;
}

/** Shown when starting a run is refused because the host cannot deliver the terminal event. */
const NO_EVENT_SUPPORT =
  "Update ChairPhoto to index photos — this build cannot report when indexing finishes.";
/** The host supports events but the registration itself failed — retrying may work. */
const EVENT_SUBSCRIBE_FAILED =
  "Could not subscribe to indexing completion, so the run was not started. Try again.";
/** The reattach equivalent: a run is already going, so the advice is not "start it again". */
const REATTACH_NO_EVENT_SUPPORT =
  "Indexing is running, but this build cannot report when it finishes. It will continue in the background.";
/** Reattach found a run but the subscription itself failed — retrying may work. */
const REATTACH_SUBSCRIBE_FAILED =
  "Indexing is running, but this panel could not subscribe to its completion. It will continue in the background.";
/** The status query failed, so backend state is unknown — which is not the same as idle. */
const REATTACH_VALIDATE_FAILED =
  "Could not check whether indexing is running. Waiting until the catalog answers again.";

/** Listener slot for the cosmetic `smarttags:progress` stream. */
const PROGRESS = "progress";
/** Listener slot for the required `smarttags:index_done` terminal event. */
const INDEX_DONE = "index-done";

function SimilarTagsPanel({ api }: { api: ChairPhotoAPI }) {
  useHostSelection(); // re-render when the active photo changes
  const photoId = api.getActivePhotoId();
  const [suggestions, setSuggestions] = useState<SmarttagsSuggestion[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [indexing, setIndexing] = useState(false);
  /** A backend run exists that this panel cannot follow, or backend state is unknown.
   *  Distinct from `indexing`: there is nothing to show progress for, but starting a
   *  second run would abort the first, so Index stays disabled until status says idle. */
  const [untracked, setUntracked] = useState(false);
  const [indexProgress, setIndexProgress] = useState<SmarttagsProgress | null>(null);
  /** True only while a `smarttags:progress` listener is actually live. The determinate
   *  bar is gated on this so a stale snapshot can never masquerade as a moving one, and
   *  so the caption never interpolates a null percentage. */
  const [progressLive, setProgressLive] = useState(false);
  /** Reactive mirror of the exclusive claim, so the buttons are honestly disabled across
   *  the awaits where `indexing` is still false. */
  const [preparing, setPreparing] = useState(false);
  const [indexResult, setIndexResult] = useState("");
  /** Both event subscriptions, each tagged with the lease token that installed it, so a
   *  stale attempt can release only what it owns and cannot orphan a replacement's — and a
   *  registration that resolves after teardown is stopped rather than stored. That policy is
   *  `useOwnedListeners` (issue #13); the slot names below are this panel's part of it. */
  const listeners = useOwnedListeners();
  // Id of the indexing job this panel is following. Filtering on this allows detection
  // of stale events from a superseded run.
  const jobIdRef = useRef<number | null>(null);
  /** The single-flight claim shared by reattach and indexing. The ref is what actually
   *  excludes (synchronous); `preparing` only mirrors it into render. A token rather than
   *  a boolean because StrictMode mounts, cleans up and remounts: a torn-down attempt's
   *  cleanup must not release the claim its replacement holds. */
  const claimRef = useRef<symbol | null>(null);
  // Model presence — checked once on mount so the panel can offer a download when the
  // model is missing, instead of silently failing on Index/Suggest.
  const [modelStatus, setModelStatus] = useState<SmarttagsModelStatus | null>(null);

  // The panel going away releases both subscriptions (that is `useOwnedListeners`' own
  // unmount cleanup, unconditional — it is the panel leaving, not one attempt tidying up
  // after itself). What is left here is the claim, cleared so a late `endExclusive` finds
  // no matching token and writes no state.
  useEffect(
    () => () => {
      claimRef.current = null;
    },
    [],
  );

  /** True while `token` still speaks for the panel: mounted, and the lease neither cleared
   *  by unmount nor handed to a replacement. UI and shared-state writes are gated on this;
   *  releasing this attempt's own listeners and lease never is. */
  const stillOwns = useCallback(
    (token: symbol) => listeners.isMounted() && claimRef.current === token,
    [listeners],
  );

  /** Claim the single-flight slot shared by reattach and indexing, or null if another is
   *  mid-flight. Must be called before the first await, since `indexing` is state and has
   *  not updated by the time a second click lands. */
  const beginExclusive = useCallback((): symbol | null => {
    if (claimRef.current) return null;
    const token = Symbol("smarttags-job");
    claimRef.current = token;
    setPreparing(true);
    return token;
  }, []);

  /** Releases only if `token` still holds the lease, so a stale attempt cannot free a
   *  replacement's claim. Safe to call more than once with the same token. */
  const endExclusive = useCallback((token: symbol | null) => {
    if (!token || claimRef.current !== token) return;
    claimRef.current = null;
    setPreparing(false);
  }, []);

  // Check model presence once on mount. We don't poll — a fresh status is enough to
  // decide whether to offer a download. After a successful download the callback
  // updates this state directly (see ModelDownloadButton's onSuccess).
  useEffect(() => {
    smarttagsModelStatus(api).then(setModelStatus).catch(() => setModelStatus(null));
  }, [api]);

  // Load persisted suggestions for this photo (no model call).
  useEffect(() => {
    if (photoId == null) return;
    setError("");
    setSuggestions(null);
    smarttagsLoadSuggestions(api, photoId)
      .then((s) => setSuggestions(s.length ? s : null))
      .catch(() => setSuggestions(null));
  }, [api, photoId]);

  // Subscribe to smarttags progress events for the duration of an active job.
  const subscribeToProgress = useCallback(
    async (token: symbol): Promise<boolean> => {
      if (listeners.has(PROGRESS)) return stillOwns(token); // already subscribed
      // Cosmetic stream: an absent `onEvent` or a rejected registration must not block the
      // work, so only "superseded" — the attempt no longer owning the panel — is a failure
      // here. `forJob` drops stragglers from a superseded indexing run, reading the id per
      // event so this one listener follows whichever job we end up adopting.
      const outcome = await listeners.attach(
        PROGRESS,
        token,
        () =>
          onSmarttagsProgress(
            api,
            forJob<SmarttagsProgress>(() => jobIdRef.current, setIndexProgress),
          ),
        () => stillOwns(token),
      );
      if (outcome === "superseded") return false;
      if (outcome === "installed") setProgressLive(true);
      return true;
    },
    [api, stillOwns, listeners],
  );

  /** With an `owner` this releases only that attempt's listener; without one (end of run,
   *  unmount) it releases whatever is there. The determinate bar follows the release, so it
   *  can never claim to be live when no subscription is. */
  const unsubscribeProgress = useCallback(
    (owner?: symbol) => {
      if (listeners.release(PROGRESS, owner)) setProgressLive(false);
    },
    [listeners],
  );

  const releaseDoneListener = useCallback(
    (owner?: symbol) => {
      listeners.release(INDEX_DONE, owner);
    },
    [listeners],
  );

  // The run is over, so both listeners go regardless of which attempt installed them —
  // this is the job ending, not one attempt tidying up. Only the render-state reset needs
  // a live panel.
  const finishIndexing = useCallback(() => {
    jobIdRef.current = null;
    unsubscribeProgress();
    releaseDoneListener();
    if (!listeners.isMounted()) return;
    setIndexing(false);
    setIndexProgress(null);
  }, [unsubscribeProgress, releaseDoneListener, listeners]);

  // Handle OUR run's terminal event (filtering by job id).
  const handleIndexDone = useCallback(
    (d: SmarttagsIndexDone) => {
      finishIndexing();
      if (d.ok) {
        const msg = indexDoneMessage(d);
        setIndexResult(msg);
        api.showToast(msg);
        api.notifyChange(); // new embeddings may enable suggestions
      } else {
        setError(`Indexing failed: ${d.error ?? "unknown error"}`);
      }
    },
    [finishIndexing, api],
  );

  // If an indexing job is already running when the panel mounts, re-attach to it.
  useEffect(() => {
    // Claim BEFORE the first status request. Otherwise a click can start indexing while
    // that request is in flight: the start takes the slot, this effect then sees an active
    // job and adopts it, and the two overlap.
    const own = { stale: false };
    const token = beginExclusive();
    if (!token) return;
    // Terminal events that arrive before adoption completes are held by `terminalBuffer`
    // rather than handled: running finishIndexing against refs this effect has not
    // populated yet would clear nothing, and the continuation would then enter "indexing"
    // for a job that had already ended — a frozen panel plus a leaked listener.
    //
    // `adopted` holds the job id we finally commit to, which is not necessarily the one
    // the first read observed: a newer run may have superseded it while we were
    // attaching. The buffer reads it per event rather than capturing it, which is what
    // lets the same live listener follow whichever job we end up adopting.
    let adopted: number | null = null;
    const terminal = terminalBuffer<SmarttagsIndexDone>(() => adopted);
    (async () => {
      const first = await smarttagsIndexStatus(api).then(
        (v) => ({ ok: true, s: v }) as const,
        () => ({ ok: false, s: null }) as const,
      );
      if (own.stale || !stillOwns(token)) return;
      if (!first.ok) {
        // A rejected query is not evidence of idleness — something may be running.
        setUntracked(true);
        setError(REATTACH_VALIDATE_FAILED);
        return;
      }
      if (!first.s || jobIdRef.current != null) return;
      // The terminal event is required, so acquire it before showing this panel as
      // indexing — adopting a run we could never see finish would freeze it there.
      //
      // `attach` installs only while this attempt still owns the panel; a registration
      // that resolves after teardown belongs to nobody and is stopped rather than
      // overwriting (and orphaning) a replacement's listener. Installing it under the slot
      // straight away, rather than holding it in a local until adoption, also means the
      // panel unmounting during the re-read below releases it.
      const observed = first.s.job;
      const outcome = await listeners.attach(
        INDEX_DONE,
        token,
        () => onSmarttagsIndexDone(api, terminal.handler(handleIndexDone)),
        () =>
          !own.stale &&
          stillOwns(token) &&
          jobIdRef.current == null &&
          !listeners.has(INDEX_DONE),
      );
      if (outcome === "superseded") return;
      if (outcome !== "installed") {
        // A run is going that we cannot follow to completion. Keep Index disabled
        // rather than presenting the panel as idle, and distinguish an absent API
        // from a registration that rejected.
        setUntracked(true);
        setError(
          outcome === "unsupported" ? REATTACH_NO_EVENT_SUPPORT : REATTACH_SUBSCRIBE_FAILED,
        );
        return;
      }
      // Re-read once with the listener already live. This is what keeps a run that
      // finished during the preflight from being displayed as still running. It
      // cannot recover an event emitted before registration existed — buffering
      // starts at registration — so it prevents a frozen panel, not a lost result.
      const now = await smarttagsIndexStatus(api).then(
        (v) => ({ ok: true, s: v }) as const,
        () => ({ ok: false, s: null }) as const,
      );
      if (own.stale || !stillOwns(token) || jobIdRef.current != null) {
        releaseDoneListener(token);
        return;
      }
      if (!now.ok) {
        // A failed query is not evidence of idleness either; drop the listener but keep
        // conflicting actions disabled, since something may still be running untracked.
        releaseDoneListener(token);
        setUntracked(true);
        setError(REATTACH_VALIDATE_FAILED);
        return;
      }
      if (!now.s) {
        // Confirmed idle — the only outcome that may return to an enabled panel. Replay
        // the observed run's terminal event if we caught it; never install a snapshot
        // of a job that has already ended.
        releaseDoneListener(token);
        const ended = terminal.buffered(observed);
        if (ended) handleIndexDone(ended);
        return;
      }
      // Adopt whatever is actually running now — the observed job, or a newer one that
      // superseded it. Either way the listener is live and can see it finish.
      adopted = now.s.job;
      jobIdRef.current = adopted;
      setIndexing(true);
      setIndexProgress({ done: now.s.done, total: now.s.total, job: adopted });
      // Replay before subscribing to progress: the required path must not depend on
      // the cosmetic one, which can still reject here.
      const ours = terminal.buffered(adopted);
      if (ours) {
        handleIndexDone(ours);
        return;
      }
      // The adopted run can finish while this registration is pending: finishIndexing
      // clears the job and both listeners, but the lease stays valid until the finally
      // below — so an ownership check alone would happily store a listener for a run
      // that has already ended, leaving progressLive true for a later run to inherit.
      // The job id is what distinguishes the two, so check it as well.
      const held = await subscribeToProgress(token);
      if (!held || jobIdRef.current !== adopted) {
        unsubscribeProgress(token);
        return;
      }
    })()
      .catch(() => {})
      // Free the claim whether we adopted, declined or failed. Once adopted, `indexing`
      // is what excludes a second start; the claim only covers this preflight. On the
      // chain rather than in a try/finally so the body keeps one indentation level.
      .finally(() => endExclusive(token));
    return () => {
      own.stale = true;
      endExclusive(token);
    };
  }, [
    api,
    subscribeToProgress,
    unsubscribeProgress,
    releaseDoneListener,
    handleIndexDone,
    beginExclusive,
    endExclusive,
    stillOwns,
    listeners,
  ]);

  // While untracked, poll status at a low rate purely to re-enable Index once the
  // backend goes idle. This deliberately reports nothing — no result message, no
  // finishIndexing — so smarttags:index_done remains the single definition of "finished".
  useEffect(() => {
    if (!untracked) return;
    // clearInterval stops future ticks but not a request already in flight, so the
    // resolution of the last one must not write into a replaced effect's state.
    let active = true;
    const id = setInterval(() => {
      smarttagsIndexStatus(api)
        .then((s) => {
          if (!active || s) return;
          setUntracked(false);
          setError("");
        })
        .catch(() => {});
    }, 5000);
    return () => {
      active = false;
      clearInterval(id);
    };
  }, [untracked, api]);

  const removeLocal = (path: string) =>
    setSuggestions((prev) => prev?.filter((s) => s.path !== path) ?? null);

  const handleIndexPhotos = async () => {
    // `untracked` means a run we cannot follow may still be going; starting another
    // would abort it.
    if (indexing || untracked) return;
    // `indexing` is state and will not have updated by the time a second click lands
    // during the awaits below, so the synchronous lease is what actually prevents two
    // concurrent starts — the second would abort the first backend run.
    const token = beginExclusive();
    if (!token) return;
    setIndexing(true);
    setError("");
    setIndexResult("");
    setIndexProgress(null);
    jobIdRef.current = null;

    // The run's id is not known until the start command answers, and the terminal listener
    // has to be live before that — so hold anything that arrives in between.
    const terminal = terminalBuffer<SmarttagsIndexDone>(() => jobIdRef.current);
    try {
      // Required, so it comes before the cosmetic progress stream and before the
      // command: a host that cannot deliver it must refuse the run rather than start
      // one that would sit in "indexing" forever.
      const outcome = await listeners.attach(
        INDEX_DONE,
        token,
        () => onSmarttagsIndexDone(api, terminal.handler(handleIndexDone)),
        () => stillOwns(token),
      );
      // Lost the lease or the panel mid-registration: `attach` has already stopped the
      // listener, and no run has been started, so there is nothing to report.
      if (outcome === "superseded") return;
      if (outcome !== "installed") {
        setIndexing(false);
        setError(outcome === "unsupported" ? NO_EVENT_SUPPORT : EVENT_SUBSCRIBE_FAILED);
        return;
      }
      // Losing the lease or the panel during the cosmetic subscription must abort: a
      // remount's reattach may already have observed this panel idle, and starting the
      // backend run now would leave it untracked.
      if (!(await subscribeToProgress(token))) return;
      const job = await smarttagsIndexPhotos(api); // job started
      // The job id and the pending-event replay are shared panel state, not this
      // attempt's — publishing them from a stale token would overwrite a replacement's.
      if (!stillOwns(token)) return;
      jobIdRef.current = job;
      const ours = terminal.buffered(job);
      if (ours) handleIndexDone(ours);
    } catch (e: unknown) {
      // Start-up failure (no catalog open / model missing) — no done event will come.
      // finishIndexing clears shared state, so it is only ours to call while we still
      // hold the lease; otherwise drop just this attempt's own listeners.
      if (stillOwns(token)) {
        finishIndexing();
        setError(`Indexing failed: ${String(e)}`);
      } else {
        unsubscribeProgress(token);
        releaseDoneListener(token);
      }
    } finally {
      endExclusive(token);
    }
  };

  const handleSuggestTags = async () => {
    if (photoId == null || busy) return;
    setBusy(true);
    setError("");
    try {
      await smarttagsSuggestTags(api, photoId);
      // Reload suggestions from the database.
      const s = await smarttagsLoadSuggestions(api, photoId);
      setSuggestions(s.length ? s : null);
    } catch (e: unknown) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const accept = async (s: SmarttagsSuggestion) => {
    try {
      await smarttagsAcceptSuggestion(api, photoId!, s.path);
      api.showToast(`Tagged: ${s.path}`);
      removeLocal(s.path);
      api.notifyChange(); // refresh the app's tags/counts
    } catch (e) {
      setError(String(e));
    }
  };

  const reject = async (s: SmarttagsSuggestion) => {
    try {
      await smarttagsRejectSuggestion(api, photoId!, s.path);
      removeLocal(s.path);
    } catch (e) {
      setError(String(e));
    }
  };

  if (photoId == null) return <span className="panel-empty">Select a photo</span>;

  // Model not yet downloaded — show a download prompt instead of surfacing cryptic
  // errors when the user clicks Index or Suggest.
  if (modelStatus != null && !modelStatus.ready) {
    if (!modelStatus.model.custom) {
      // Default model missing: offer a direct download so the user doesn't have to
      // navigate to Settings.
      return (
        <div className="smarttags-panel">
          <div className="term-note" style={{ marginBottom: 6 }}>
            The Smart Tagging model (~350 MB) is not downloaded yet.
          </div>
          <div className="iptc-actions">
            <ModelDownloadButton api={api} onSuccess={(s) => setModelStatus(s)} />
          </div>
        </div>
      );
    }
    // Custom path configured but broken — user must fix the path in Settings.
    return (
      <div className="smarttags-panel">
        <span className="modal-error">
          {modelStatus.model.detail ?? "Custom model path is missing or unreadable. Check Settings → Smart Tagging."}
        </span>
      </div>
    );
  }

  // A percentage is only shown when a live subscription can actually move it. Without
  // that, `pct` was null and the caption rendered a literal "Indexing… null%".
  const pct =
    progressLive && indexProgress && indexProgress.total > 0
      ? Math.round((indexProgress.done / indexProgress.total) * 100)
      : null;
  const indexLabel = indexing ? (pct == null ? "Indexing…" : `Indexing… ${pct}%`) : "Index";

  return (
    <div className="smarttags-panel">
      <div className="row">
        <button
          className="chip chip-on"
          onClick={() => void handleIndexPhotos()}
          disabled={indexing || untracked || preparing}
        >
          {indexLabel}
        </button>
        {indexing && (
          <button className="chip chip-danger" onClick={() => void smarttagsCancelJob(api)}>
            Cancel
          </button>
        )}
      </div>

      {indexResult && <div className="smarttags-result">{indexResult}</div>}
      {error && <div className="modal-error">{error}</div>}

      <div className="row">
        <button className="chip chip-on" onClick={() => void handleSuggestTags()} disabled={busy}>
          {busy ? "Suggesting…" : "Suggest"}
        </button>
      </div>

      {suggestions?.map((s) => (
        <div key={s.path} className="smarttags-suggestion">
          <div className="smarttags-sug-main">
            <span className="smarttags-sug-path">{s.path}</span>
            <span className="smarttags-sug-conf">{Math.round(s.confidence * 100)}%</span>
          </div>
          {s.sourcePhotoIds.length > 0 && (
            <div className="smarttags-sug-reason">
              from {s.sourcePhotoIds.length} similar photo{s.sourcePhotoIds.length !== 1 ? "s" : ""}
            </div>
          )}
          <div className="row">
            <button className="chip" onClick={() => accept(s)}>
              ✓ add
            </button>
            <button className="chip" onClick={() => reject(s)} title="Won't be suggested again">
              ✗ reject
            </button>
          </div>
        </div>
      ))}
    </div>
  );
}

// ── SmarttagsSettings — module settings panel ────────────────────────────

function SmarttagsSettings({ api }: { api: ChairPhotoAPI }) {
  const [modelPath, setModelPath] = useState("");
  const [saved, setSaved] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [deleteError, setDeleteError] = useState("");
  const [training, setTraining] = useState(false);
  const [trainStatus, setTrainStatus] = useState("");
  const [modelStatus, setModelStatus] = useState<SmarttagsModelStatus | null>(null);

  const refreshModelStatus = useCallback(() => {
    smarttagsModelStatus(api).then(setModelStatus).catch(() => setModelStatus(null));
  }, [api]);

  // Load the model path setting + model presence on mount.
  useEffect(() => {
    api.getSetting("smarttags.model_path").then((v) => { if (v) setModelPath(v); }).catch(() => {});
    refreshModelStatus();
  }, [api, refreshModelStatus]);

  const handleSave = async () => {
    await api.setSetting("smarttags.model_path", modelPath.trim());
    setSaved(true);
    setTimeout(() => setSaved(false), 2000);
    refreshModelStatus();
  };

  const handleDownloadSuccess = (s: SmarttagsModelStatus) => {
    setModelStatus(s);
    api.showToast(s.ready ? "Smart Tagging model downloaded." : "Model still not ready.");
  };

  const handleTrainClassifiers = async () => {
    setTraining(true);
    setTrainStatus("");
    try {
      const r = await smarttagsTrainClassifiers(api);
      setTrainStatus(
        r.examined === 0
          ? "No tag has enough confirmed indexed photos yet."
          : `${r.trained} trained, ${r.skipped} already fresh (${r.examined} eligible).`
      );
    } catch (e) {
      setTrainStatus(String(e));
    } finally {
      setTraining(false);
    }
  };

  const handleDeleteIndex = async () => {
    setDeleting(true);
    setDeleteError("");
    try {
      await smarttagsDeleteIndex(api);
      api.showToast("Smart Tagging index deleted. Run Index again to rebuild.");
    } catch (e) {
      setDeleteError(String(e));
    } finally {
      setDeleting(false);
    }
  };

  return (
    <div>
      <h3>Smart Tagging</h3>
      <div className="iptc-row">
        <label className="iptc-label">Model</label>
        {modelStatus ? (
          <span className="term-note">
            {modelStatus.ready
              ? `Ready (${modelStatus.model.custom ? "custom" : "default"}): ${modelStatus.model.path}`
              : `Not available — ${modelStatus.model.detail ?? "missing"}`}
          </span>
        ) : (
          <span className="term-note">Checking…</span>
        )}
      </div>
      {modelStatus && !modelStatus.ready && !modelStatus.model.custom && (
        <div className="iptc-actions">
          <ModelDownloadButton api={api} onSuccess={handleDownloadSuccess} />
        </div>
      )}
      <div className="iptc-row">
        <label className="iptc-label">Model path</label>
        <input
          className="tag-input"
          value={modelPath}
          onChange={(e) => setModelPath(e.target.value)}
          placeholder="Path to a CLIP vision ONNX model (blank = pinned default, downloadable above)"
        />
      </div>
      <div className="iptc-row">
        <span className="term-note">
          CLIP embeddings are computed from cached preview images and stored locally.
          The index is not shared and never leaves this machine.
        </span>
      </div>
      <div className="iptc-actions">
        <button className="chip chip-on" onClick={handleSave}>
          Save Smart Tagging settings
        </button>
        <span className="iptc-status">{saved ? "Saved" : ""}</span>
      </div>

      <div className="iptc-row">
        <label className="iptc-label">Classifiers</label>
        <span className="term-note">
          Per-tag classifiers refine the similarity suggestions for tags with many
          confirmed photos. Retrain after a batch of accepts/rejects.
        </span>
      </div>
      <div className="iptc-actions">
        <button className="chip chip-on" onClick={handleTrainClassifiers} disabled={training}>
          {training ? "Training…" : "Train classifiers"}
        </button>
        <span className="iptc-status">{trainStatus}</span>
      </div>

      <div className="iptc-row">
        <label className="iptc-label">Index management</label>
      </div>
      <div className="iptc-actions">
        <button className="chip chip-danger" onClick={handleDeleteIndex} disabled={deleting}>
          {deleting ? "Deleting…" : "Delete index"}
        </button>
        {deleteError && <span className="iptc-error">{deleteError}</span>}
      </div>
    </div>
  );
}

export const smartTaggingModule: ChairPhotoModule = {
  id: "smarttags",
  name: "Smart Tagging",
  version: "0.1.0",
  description: "Suggest tags based on visual similarity using local CLIP embeddings.",
  backendFeature: "smarttags",
  // Every `smarttags_*` command the module's helpers call (#48). `smarttags_download_model`
  // is the only one that touches the network, and it fetches the pinned CLIP model.
  permissions: {
    commands: [
      "smarttags_accept_suggestion",
      "smarttags_delete_index",
      "smarttags_download_model",
      "smarttags_index_cancel",
      "smarttags_index_photos",
      "smarttags_index_status",
      "smarttags_load_suggestions",
      "smarttags_model_status",
      "smarttags_reject_suggestion",
      "smarttags_suggest_tags",
      "smarttags_train_classifiers",
    ],
  },
  onLoad(api) {
    api.registerPanel({
      id: "smarttags-similar",
      label: "Similar tags",
      slot: "inspector",
      render: () => <SimilarTagsPanel api={api} />,
    });
    api.registerSettingsPanel(() => <SmarttagsSettings api={api} />);
  },
};
