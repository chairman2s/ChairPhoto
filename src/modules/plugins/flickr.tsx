// Flickr publishing module: upload the selected version of a photo to Flickr via its
// official OAuth 1.0a API, and record the publication (marker "flickr"). Settings hold the
// user's app key/secret + the OAuth tokens; see docs/flickr.md.

import { useState } from "react";
import type { ChairPhotoAPI, ChairPhotoModule } from "../registry";
import { thumbnailUrl } from "../api";
import { OAuthSettings, PublishPanel, type PublishService } from "./publishing";

// ── Backend commands (owned by this module) ───────────────────────────────────
// Per the module contract, a module's own commands go through `ChairPhotoAPI.invoke`
// rather than core's `api.ts`, so the command names travel with the module.

/** One matched entry from `flickr_import_published`. */
interface FlickrImportMatch {
  catalogId: number;
  catalogPath: string;
  flickrId: string;
  flickrUrl: string;
  /** Unix timestamp of the Flickr upload (the real historical posting date). */
  publishedAt: number;
  /** Small (240 px) Flickr thumbnail URL; null when Flickr omits it. */
  thumbUrl: string | null;
}

/** One catalog candidate inside an ambiguous match (for manual resolution). */
interface FlickrImportCandidate {
  catalogId: number;
  catalogPath: string;
  /** ISO-like capture time ("YYYY-MM-DDTHH:MM:SS"), or null if unknown. */
  captureTime: string | null;
}

/** One ambiguous entry from `flickr_import_published`. */
interface FlickrImportAmbiguous {
  flickrId: string;
  flickrUrl: string;
  /** Unix timestamp of the Flickr upload — needed to construct a plan entry on resolution. */
  publishedAt: number;
  title: string;
  reason: string;
  /** Small (240 px) Flickr thumbnail URL; null when Flickr omits it. */
  thumbUrl: string | null;
  /** Post-collapse candidate set (up to 10) for manual resolution. */
  candidates: FlickrImportCandidate[];
}

/** Return type of `flickr_import_published` (dry-run preview). */
interface FlickrImportResult {
  matchedCount: number;
  ambiguousCount: number;
  unmatchedCount: number;
  /** Up to 50 matches for display. */
  matches: FlickrImportMatch[];
  /**
   * The full (uncapped) match set — pass this verbatim to `flickrImportApply`.
   * Use `plan.length` (not `matchedCount`) for the "Import N" button label.
   */
  plan: FlickrImportMatch[];
  /** Up to 50 ambiguous items that need manual attention. */
  ambiguous: FlickrImportAmbiguous[];
}

/** Begin Flickr OAuth; returns the authorize URL to open (then paste the verifier). */
const flickrBeginAuth = (api: ChairPhotoAPI) => api.invoke<string>("flickr_begin_auth");

/** Finish Flickr OAuth with the verifier code the user pasted. */
const flickrCompleteAuth = (api: ChairPhotoAPI, verifier: string) =>
  api.invoke<void>("flickr_complete_auth", { verifier });

const flickrConnected = (api: ChairPhotoAPI) => api.invoke<boolean>("flickr_connected");

/** Upload the selected version to Flickr; returns the photo's page URL.
 *  `tags` is Flickr's tags format: space-separated, multi-word tags quoted. */
const postToFlickr = (
  api: ChairPhotoAPI,
  photoId: number,
  versionId: number | null,
  title: string,
  description: string,
  tags: string,
) =>
  api.invoke<string>("post_to_flickr", {
    photoId,
    versionId: versionId ?? null,
    title,
    description,
    tags,
  });

/** Suggested Flickr tags for a photo — its export keywords in Flickr's tags format. */
const flickrSuggestTags = (api: ChairPhotoAPI, photoId: number) =>
  api.invoke<string>("flickr_suggest_tags", { photoId });

/**
 * Fetch the user's Flickr photostream and match against the local catalog.
 * Returns a preview plan — does NOT write any publications.
 * Pass the returned `plan` to `flickrImportApply` to apply.
 * Never writes to Flickr (read-only API calls).
 */
const flickrImportPublished = (api: ChairPhotoAPI) =>
  api.invoke<FlickrImportResult>("flickr_import_published");

/**
 * Apply a previewed import plan: upsert publications with real historical dates.
 * Takes the `plan` array from `flickrImportPublished` verbatim.
 * Returns the number of rows upserted.
 */
const flickrImportApply = (api: ChairPhotoAPI, plan: FlickrImportMatch[]) =>
  api.invoke<number>("flickr_import_apply", { plan });

/** Bind this module's commands to the host API handle given at load time. */
function makeService(api: ChairPhotoAPI): PublishService {
  return {
    id: "flickr",
    name: "Flickr",
    signupUrl: "flickr.com/services/apps/create",
    beginAuth: () => flickrBeginAuth(api),
    completeAuth: (verifier) => flickrCompleteAuth(api, verifier),
    connected: () => flickrConnected(api),
    publish: (photoId, versionId, title, description, _albumUri, tags) =>
      postToFlickr(api, photoId, versionId, title, description, tags),
    suggestTags: (photoId) => flickrSuggestTags(api, photoId),
  };
}

// ── Shared thumbnail size constants ───────────────────────────────────────────

const THUMB_SIZE = 200; // px — Flickr small (240px source) and catalog thumbs
const MATCH_THUMB_SIZE = 80; // px — smaller thumbs in the matches list

// ── Small catalog thumbnail (thumb:// asset protocol) ─────────────────────────

function CatalogThumb({
  catalogId,
  size,
}: {
  catalogId: number;
  size: number;
}) {
  return (
    <img
      src={thumbnailUrl(catalogId)}
      alt=""
      draggable={false}
      style={{
        width: size,
        height: size,
        objectFit: "cover",
        borderRadius: 4,
        flexShrink: 0,
        background: "rgba(30,41,59,0.8)",
      }}
      onError={(e) => {
        (e.currentTarget as HTMLImageElement).style.visibility = "hidden";
      }}
    />
  );
}

// ── Small Flickr thumbnail (live.staticflickr.com, CSP is null so loads fine) ─

function FlickrThumb({
  thumbUrl,
  title,
  size,
}: {
  thumbUrl: string | null;
  title: string;
  size: number;
}) {
  if (!thumbUrl) {
    // Fallback: dark box with the title text
    return (
      <div
        style={{
          width: size,
          height: size,
          borderRadius: 4,
          flexShrink: 0,
          background: "rgba(30,41,59,0.8)",
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          fontSize: 9,
          color: "rgba(255,255,255,0.4)",
          padding: 4,
          textAlign: "center",
          overflow: "hidden",
          lineHeight: 1.3,
        }}
      >
        {title || "no preview"}
      </div>
    );
  }
  return (
    <img
      src={thumbUrl}
      alt={title}
      draggable={false}
      style={{
        width: size,
        height: size,
        objectFit: "cover",
        borderRadius: 4,
        flexShrink: 0,
        background: "rgba(30,41,59,0.8)",
      }}
      onError={(e) => {
        (e.currentTarget as HTMLImageElement).style.visibility = "hidden";
      }}
    />
  );
}

// ── Tiny side label ("ON FLICKR" / "IN CATALOG") for the import lists ─────────

function SideTag({ children, color }: { children: string; color?: string }) {
  return (
    <div
      style={{
        fontSize: 9,
        fontWeight: 600,
        textTransform: "uppercase",
        letterSpacing: 0.6,
        color: color ?? "rgba(255,255,255,0.45)",
        marginBottom: 3,
      }}
    >
      {children}
    </div>
  );
}

// ── Import-published panel ─────────────────────────────────────────────────────

function ImportPublishedPanel({ api }: { api: ChairPhotoAPI }) {
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState("");
  // `plan` stores the full (uncapped) match list from the last preview so we can
  // send it verbatim to flickrImportApply without re-fetching the photostream.
  const [plan, setPlan] = useState<FlickrImportResult | null>(null);
  // Manually resolved ambiguous entries: flickrId → constructed FlickrImportMatch.
  const [resolved, setResolved] = useState<Map<string, FlickrImportMatch>>(new Map());

  const preview = async () => {
    setBusy(true);
    setStatus("Fetching photostream…");
    setPlan(null);
    setResolved(new Map());
    try {
      const result = await flickrImportPublished(api);
      setPlan(result);
      setStatus(
        `${result.matchedCount} matched · ${result.ambiguousCount} ambiguous · ${result.unmatchedCount} not in catalog`,
      );
    } catch (e) {
      setStatus(String(e));
    } finally {
      setBusy(false);
    }
  };

  // Pick a candidate for a given ambiguous entry.
  const resolve = (ambig: FlickrImportAmbiguous, candidate: FlickrImportCandidate) => {
    const match: FlickrImportMatch = {
      catalogId: candidate.catalogId,
      catalogPath: candidate.catalogPath,
      flickrId: ambig.flickrId,
      flickrUrl: ambig.flickrUrl,
      publishedAt: ambig.publishedAt,
      thumbUrl: ambig.thumbUrl,
    };
    setResolved(prev => new Map(prev).set(ambig.flickrId, match));
  };

  // Remove a previously resolved entry.
  const unresolve = (flickrId: string) => {
    setResolved(prev => {
      const next = new Map(prev);
      next.delete(flickrId);
      return next;
    });
  };

  const apply = async () => {
    if (!plan) return;
    // Full auto-matched plan + manually resolved entries.
    const resolvedMatches = Array.from(resolved.values());
    const fullPlan: FlickrImportMatch[] = [...plan.plan, ...resolvedMatches];
    if (fullPlan.length === 0) return;
    setBusy(true);
    setStatus("Importing…");
    try {
      const applied = await flickrImportApply(api, fullPlan);
      setPlan(null);
      setResolved(new Map());
      api.showToast(`Imported ${applied} Flickr publication(s).`);
      setStatus(`Done — ${applied} publication(s) recorded.`);
    } catch (e) {
      setStatus(String(e));
    } finally {
      setBusy(false);
    }
  };

  // The button label and count include auto-matched + manually resolved.
  const applyCount = (plan ? plan.plan.length : 0) + resolved.size;

  return (
    <div className="iptc-panel" style={{ marginTop: 16 }}>
      <div className="field">
        <label style={{ fontWeight: 600 }}>Import published from Flickr</label>
        <span className="term-note">
          Matches photos in your Flickr photostream to the local catalog by capture time
          and title, then records them as publications with their real historical upload
          dates. Never writes to Flickr.
        </span>
      </div>
      <div className="row" style={{ marginTop: 8, gap: 8 }}>
        <button className="chip" onClick={preview} disabled={busy}>
          {busy && !plan ? "Fetching…" : "Preview"}
        </button>
        <button
          className="chip chip-on"
          onClick={apply}
          disabled={busy || !plan || applyCount === 0}
        >
          {busy && plan
            ? "Importing…"
            : plan
              ? `Import ${applyCount} publication${applyCount !== 1 ? "s" : ""}`
              : "Import"}
        </button>
      </div>
      {status && <div className="modal-sub" style={{ marginTop: 6 }}>{status}</div>}

      {/* Auto-matched entries — small catalog thumb per row for visual confirmation */}
      {plan && plan.matches.length > 0 && (
        <div style={{ marginTop: 8 }}>
          <div className="term-note" style={{ marginBottom: 4 }}>
            Matches — Flickr photo → your catalog file (up to 50 shown):
          </div>
          <div
            style={{
              maxHeight: 340,
              overflowY: "auto",
              fontSize: 11,
              lineHeight: 1.6,
            }}
          >
            {plan.matches.map((m: FlickrImportMatch) => (
              <div
                key={m.flickrId}
                style={{ display: "flex", gap: 8, alignItems: "center", marginBottom: 4 }}
              >
                <FlickrThumb thumbUrl={m.thumbUrl} title="" size={MATCH_THUMB_SIZE} />
                <span style={{ opacity: 0.4, fontSize: 14 }}>→</span>
                <CatalogThumb catalogId={m.catalogId} size={MATCH_THUMB_SIZE} />
                <div style={{ minWidth: 0 }}>
                  <div style={{ opacity: 0.55, fontSize: 10 }}>
                    {new Date(m.publishedAt * 1000).toLocaleDateString()}
                  </div>
                  <a href={m.flickrUrl} target="_blank" rel="noreferrer" style={{ fontSize: 11 }}>
                    {m.catalogPath.split("/").pop()}
                  </a>
                </div>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Manually resolved entries — pair view: Flickr thumb + chosen catalog thumb */}
      {resolved.size > 0 && (
        <div style={{ marginTop: 8 }}>
          <div className="term-note" style={{ marginBottom: 4, color: "#50a050" }}>
            Manually resolved ({resolved.size}) — Flickr photo → your catalog file:
          </div>
          <div style={{ fontSize: 11 }}>
            {Array.from(resolved.values()).map((m: FlickrImportMatch) => (
              <div
                key={m.flickrId}
                style={{ display: "flex", gap: 8, alignItems: "center", marginBottom: 6 }}
              >
                <FlickrThumb thumbUrl={m.thumbUrl} title="" size={MATCH_THUMB_SIZE} />
                <span style={{ opacity: 0.4, fontSize: 14 }}>→</span>
                <CatalogThumb catalogId={m.catalogId} size={MATCH_THUMB_SIZE} />
                <div style={{ minWidth: 0, flex: 1 }}>
                  <a href={m.flickrUrl} target="_blank" rel="noreferrer">
                    {m.catalogPath.split("/").pop()}
                  </a>
                </div>
                <button
                  className="chip"
                  style={{ fontSize: 10, padding: "0 5px", flexShrink: 0 }}
                  onClick={() => unresolve(m.flickrId)}
                >
                  Undo
                </button>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Ambiguous entries — visual resolution UI */}
      {plan && plan.ambiguous.length > 0 && (
        <div style={{ marginTop: 8 }}>
          <div className="term-note" style={{ marginBottom: 4, color: "#c08030" }}>
            Ambiguous — pick the right file or skip (up to 50 shown):
          </div>
          <div
            style={{
              maxHeight: 560,
              overflowY: "auto",
            }}
          >
            {plan.ambiguous.map((a: FlickrImportAmbiguous) => {
              const isResolved = resolved.has(a.flickrId);
              const resolvedMatch = resolved.get(a.flickrId);
              return (
                <div
                  key={a.flickrId}
                  style={{
                    marginBottom: 12,
                    borderLeft: "2px solid #c08030",
                    paddingLeft: 8,
                  }}
                >
                  {/* Flickr side — thumbnail + title + date + link */}
                  <SideTag color="#7ba8d8">On Flickr</SideTag>
                  <div style={{ display: "flex", gap: 8, alignItems: "flex-start", marginBottom: 6 }}>
                    <a
                      href={a.flickrUrl}
                      target="_blank"
                      rel="noreferrer"
                      title="Open on Flickr (full image)"
                      style={{ flexShrink: 0 }}
                    >
                      <FlickrThumb thumbUrl={a.thumbUrl} title={a.title} size={THUMB_SIZE} />
                    </a>
                    <div style={{ minWidth: 0 }}>
                      <div style={{ fontWeight: 500, fontSize: 12, marginBottom: 2 }}>
                        {a.title || <em style={{ opacity: 0.5 }}>untitled</em>}
                      </div>
                      <div style={{ opacity: 0.55, fontSize: 10, marginBottom: 2 }}>
                        {new Date(a.publishedAt * 1000).toLocaleDateString()}
                      </div>
                      <a
                        href={a.flickrUrl}
                        target="_blank"
                        rel="noreferrer"
                        style={{ fontSize: 10, opacity: 0.6 }}
                      >
                        flickr.com ↗
                      </a>
                    </div>
                  </div>

                  {/* Resolved state: pair view */}
                  {isResolved && resolvedMatch && (
                    <div style={{ display: "flex", gap: 8, alignItems: "center" }}>
                      <FlickrThumb thumbUrl={a.thumbUrl} title={a.title} size={MATCH_THUMB_SIZE} />
                      <span style={{ opacity: 0.4, fontSize: 14 }}>→</span>
                      <CatalogThumb catalogId={resolvedMatch.catalogId} size={MATCH_THUMB_SIZE} />
                      <div style={{ flex: 1, fontSize: 11, opacity: 0.7 }}>
                        {resolvedMatch.catalogPath.split("/").pop()}
                      </div>
                      <button
                        className="chip"
                        style={{ fontSize: 10, padding: "0 6px", flexShrink: 0 }}
                        onClick={() => unresolve(a.flickrId)}
                      >
                        Undo
                      </button>
                    </div>
                  )}

                  {/* Unresolved state: candidate cards */}
                  {!isResolved && a.candidates.length > 0 && (
                    <>
                      <SideTag>In your catalog — click the file that matches</SideTag>
                      <div style={{ display: "flex", flexWrap: "wrap", gap: 8 }}>
                        {a.candidates.map((c: FlickrImportCandidate) => (
                        <button
                          key={c.catalogId}
                          onClick={() => resolve(a, c)}
                          style={{
                            background: "none",
                            border: "1px solid rgba(255,255,255,0.15)",
                            borderRadius: 6,
                            padding: 4,
                            cursor: "pointer",
                            textAlign: "left",
                            width: THUMB_SIZE + 8,
                            flexShrink: 0,
                            transition: "border-color 0.15s, background 0.15s",
                          }}
                          onMouseEnter={(e) => {
                            e.currentTarget.style.borderColor = "rgba(255,255,255,0.5)";
                            e.currentTarget.style.background = "rgba(255,255,255,0.06)";
                          }}
                          onMouseLeave={(e) => {
                            e.currentTarget.style.borderColor = "rgba(255,255,255,0.15)";
                            e.currentTarget.style.background = "none";
                          }}
                        >
                          <CatalogThumb catalogId={c.catalogId} size={THUMB_SIZE} />
                          <div
                            style={{
                              fontSize: 9,
                              marginTop: 3,
                              opacity: 0.7,
                              overflow: "hidden",
                              textOverflow: "ellipsis",
                              whiteSpace: "nowrap",
                            }}
                          >
                            {c.catalogPath.split("/").pop()}
                          </div>
                          <div style={{ fontSize: 9, opacity: 0.45, marginTop: 1 }}>
                            {c.captureTime
                              ? // WebKitGTK only reliably parses the T-separated ISO form.
                                new Date(c.captureTime.replace(" ", "T")).toLocaleString()
                              : "no time"}
                          </div>
                        </button>
                        ))}
                      </div>
                    </>
                  )}
                </div>
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}

// ── Module ────────────────────────────────────────────────────────────────────

export const flickrModule: ChairPhotoModule = {
  id: "flickr",
  name: "Flickr",
  version: "0.1.0",
  description: "Publish photos to Flickr via the official API; records which version was posted.",
  backendFeature: "flickr",
  publicationMarker: "flickr",
  // The OAuth 1.0a handshake, the upload, and the published-photo import (#48). Every one
  // of these talks to flickr.com from Rust, which is exactly what the user is approving.
  permissions: {
    commands: [
      "flickr_begin_auth",
      "flickr_complete_auth",
      "flickr_connected",
      "flickr_import_apply",
      "flickr_import_published",
      "flickr_suggest_tags",
      "post_to_flickr",
    ],
  },
  onLoad(api) {
    const service = makeService(api);
    api.registerSettingsPanel(() => (
      <>
        <OAuthSettings api={api} svc={service} />
        <ImportPublishedPanel api={api} />
      </>
    ));
    api.registerPublishTarget({
      id: "flickr",
      label: "Flickr",
      render: () => <PublishPanel api={api} svc={service} />,
    });
  },
};
