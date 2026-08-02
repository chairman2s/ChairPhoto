// Shared UI for the Flickr/SmugMug publishing modules: an OAuth settings panel (enter the
// app key/secret, connect via the OOB verifier flow) and a per-photo publish panel (pick the
// version, fill title/description, publish, then record the publication so it shows under the
// inspector's "Published to" panel and the published:* filter facets).
//
// These are bundled first-party plugins, so they import only from the module contract
// (../registry) and the command wrappers (../api), per the module isolation rule.

import { useCallback, useEffect, useState } from "react";
import type { ChairPhotoAPI } from "../registry";
import { listVersions, openExternal, PhotoVersion } from "../api";

/**
 * An upload-target album, as used by `PublishService.listAlbums`/`createAlbum`.
 * Part of the shared publishing contract (SmugMug is the only service using it today),
 * so it lives here rather than in core `api.ts` or in one service's module.
 */
export interface SmugMugAlbum {
  uri: string;
  name: string;
}

/** Per-service plumbing the shared panels call (the backend command wrappers). */
export interface PublishService {
  /** Module id / publication marker, e.g. "flickr". */
  id: string;
  /** Display name, e.g. "Flickr". */
  name: string;
  /** Where to register the developer app (shown as a hint). */
  signupUrl: string;
  beginAuth: () => Promise<string>;
  completeAuth: (verifier: string) => Promise<void>;
  connected: () => Promise<boolean>;
  /** Upload the selected version; returns the published page/image URL (Flickr returns
   *  its photo page URL, SmugMug the image URI). Recorded on the publication so the
   *  importer can recognise chairphoto's own uploads later. `tags` is the service's own
   *  tag format (empty when the service has no `suggestTags`). */
  publish: (
    photoId: number,
    versionId: number | null,
    title: string,
    body: string,
    albumUri: string,
    tags: string,
  ) => Promise<string>;
  /** Services with native tags (Flickr): suggest a prefill for the Tags field from the
   *  photo's catalog keywords, in the service's own tag format. Absence hides the field. */
  suggestTags?: (photoId: number) => Promise<string>;
  /** SmugMug only: list upload-target albums. */
  listAlbums?: () => Promise<SmugMugAlbum[]>;
  /** SmugMug only: create a new album and return it. */
  createAlbum?: (name: string) => Promise<SmugMugAlbum>;
}

export function OAuthSettings({ api, svc }: { api: ChairPhotoAPI; svc: PublishService }) {
  const [key, setKey] = useState("");
  const [secret, setSecret] = useState("");
  const [verifier, setVerifier] = useState("");
  const [authUrl, setAuthUrl] = useState("");
  const [connected, setConnected] = useState(false);
  const [status, setStatus] = useState("");
  /** Max long edge in pixels. Empty / "0" = full resolution (no resize). */
  const [maxLongEdge, setMaxLongEdge] = useState("");

  useEffect(() => {
    api.getSetting("api_key").then((v) => setKey(v ?? "")).catch(() => {});
    api.getSetting("api_secret").then((v) => setSecret(v ?? "")).catch(() => {});
    api.getSetting("max_long_edge").then((v) => setMaxLongEdge(v ?? "")).catch(() => {});
    svc.connected().then(setConnected).catch(() => {});
  }, []);

  const saveKeys = async () => {
    await api.setSetting("api_key", key.trim());
    await api.setSetting("api_secret", secret.trim());
    // Persist the max long edge: store as-is (empty = full resolution).
    await api.setSetting("max_long_edge", maxLongEdge.trim());
  };

  const connect = async () => {
    setStatus("");
    try {
      await saveKeys();
      const url = await svc.beginAuth();
      setAuthUrl(url);
      try {
        await openExternal(url);
      } catch {
        /* fall back to the shown link */
      }
      setStatus("Authorize in your browser, then paste the verifier code below.");
    } catch (e) {
      setStatus(String(e));
    }
  };

  const finish = async () => {
    setStatus("");
    try {
      await svc.completeAuth(verifier.trim());
      setVerifier("");
      setAuthUrl("");
      setConnected(await svc.connected());
      setStatus("Connected.");
    } catch (e) {
      setStatus(String(e));
    }
  };

  return (
    <div className="iptc-panel">
      <div className="field">
        <label>{svc.name} API key</label>
        <input className="folder-input" value={key} onChange={(e) => setKey(e.target.value)} />
      </div>
      <div className="field">
        <label>{svc.name} API secret</label>
        <input
          className="folder-input"
          type="password"
          value={secret}
          onChange={(e) => setSecret(e.target.value)}
        />
      </div>
      <div className="field">
        <label>Max long edge (px)</label>
        <input
          className="folder-input"
          type="number"
          min={0}
          step={1}
          placeholder="0 = full resolution"
          value={maxLongEdge}
          onChange={(e) => setMaxLongEdge(e.target.value)}
        />
      </div>
      <span className="term-note">
        Register an app at {svc.signupUrl} to get a key + secret. Stored only in your local
        catalog. Max long edge: leave empty or 0 for full resolution; set e.g. 2048 to cap
        uploads so the longer dimension does not exceed that size (never upscales).
      </span>
      <div className="row" style={{ marginTop: 8 }}>
        <button className="chip" onClick={() => saveKeys().then(() => setStatus("Saved."))}>
          Save keys
        </button>
        <button className="chip chip-on" onClick={connect}>
          {connected ? "Reconnect" : "Connect"}
        </button>
        <span className="iptc-status">{connected ? "Connected ✓" : "Not connected"}</span>
      </div>
      {authUrl && (
        <div className="field" style={{ marginTop: 8 }}>
          <span className="term-note">
            If the browser didn't open,{" "}
            <a href={authUrl} target="_blank" rel="noreferrer">
              open this authorize link
            </a>
            .
          </span>
          <div className="row" style={{ marginTop: 6 }}>
            <input
              className="folder-input"
              placeholder="Paste verifier code"
              value={verifier}
              onChange={(e) => setVerifier(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") finish();
              }}
            />
            <button className="chip" onClick={finish} disabled={!verifier.trim()}>
              Finish
            </button>
          </div>
        </div>
      )}
      {status && <div className="modal-sub">{status}</div>}
    </div>
  );
}

export function PublishPanel({ api, svc }: { api: ChairPhotoAPI; svc: PublishService }) {
  const photoId = api.getActivePhotoId();
  const [versions, setVersions] = useState<PhotoVersion[]>([]);
  const [versionId, setVersionId] = useState<number | null>(api.getActiveVersionId());
  const [title, setTitle] = useState("");
  const [description, setDescription] = useState("");
  const [tags, setTags] = useState("");
  const [tagsTouched, setTagsTouched] = useState(false);
  const [albums, setAlbums] = useState<SmugMugAlbum[]>([]);
  const [albumUri, setAlbumUriState] = useState("");
  const [newAlbum, setNewAlbum] = useState("");
  const [creating, setCreating] = useState(false);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState("");

  const reload = useCallback(() => {
    if (photoId == null) {
      setVersions([]);
      return;
    }
    listVersions(photoId).then(setVersions).catch(() => setVersions([]));
    setVersionId(api.getActiveVersionId());
  }, [photoId, api]);

  useEffect(() => {
    reload();
  }, [reload]);

  // A new photo gets a fresh prefill even if the previous one's tags were hand-edited.
  useEffect(() => {
    setTagsTouched(false);
  }, [photoId]);

  // Prefill Tags from the photo's catalog keywords until the user edits the field
  // (same pattern as the Instagram caption prefill).
  useEffect(() => {
    if (photoId == null || !svc.suggestTags || tagsTouched) return;
    let alive = true;
    svc.suggestTags(photoId)
      .then((t) => alive && setTags(t))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [photoId, tagsTouched]);

  // Select an album and remember it as the default for next time.
  const selectAlbum = (uri: string) => {
    setAlbumUriState(uri);
    api.setSetting("last_album", uri).catch(() => {});
  };

  const cacheAlbums = (list: SmugMugAlbum[]) =>
    api.setSetting("albums_cache", JSON.stringify(list)).catch(() => {});

  // On mount: show the cached album list immediately (no click). Only hit the network if
  // there's no cache yet; the user can Refresh to re-fetch.
  useEffect(() => {
    if (!svc.listAlbums) return;
    let alive = true;
    (async () => {
      const [cached, last] = await Promise.all([
        api.getSetting("albums_cache").catch(() => null),
        api.getSetting("last_album").catch(() => null),
      ]);
      let list: SmugMugAlbum[] = [];
      if (cached) {
        try {
          list = JSON.parse(cached);
        } catch {
          /* ignore corrupt cache */
        }
      }
      if (!alive) return;
      if (list.length) {
        setAlbums(list);
        setAlbumUriState(last && list.some((a) => a.uri === last) ? last : list[0].uri);
      } else {
        try {
          const a = await svc.listAlbums!();
          if (!alive) return;
          setAlbums(a);
          setAlbumUriState(last && a.some((x) => x.uri === last) ? last : a[0]?.uri ?? "");
          cacheAlbums(a);
        } catch (e) {
          if (alive) setStatus(String(e));
        }
      }
    })();
    return () => {
      alive = false;
    };
  }, []);

  const refreshAlbums = async () => {
    if (!svc.listAlbums) return;
    setStatus("Refreshing albums…");
    try {
      const a = await svc.listAlbums();
      setAlbums(a);
      if (!a.some((x) => x.uri === albumUri)) selectAlbum(a[0]?.uri ?? "");
      cacheAlbums(a);
      setStatus("");
    } catch (e) {
      setStatus(String(e));
    }
  };

  const createAlbum = async () => {
    if (!svc.createAlbum || !newAlbum.trim()) return;
    setCreating(true);
    setStatus("Creating album…");
    try {
      const a = await svc.createAlbum(newAlbum.trim());
      const next = [a, ...albums.filter((x) => x.uri !== a.uri)];
      setAlbums(next);
      cacheAlbums(next);
      selectAlbum(a.uri);
      setNewAlbum("");
      setStatus(`Created “${a.name}” ✓`);
    } catch (e) {
      setStatus(String(e));
    } finally {
      setCreating(false);
    }
  };

  const publish = async () => {
    if (photoId == null) return;
    setBusy(true);
    setStatus("Publishing…");
    try {
      const publishedUrl = await svc.publish(
        photoId,
        versionId,
        title.trim(),
        description.trim(),
        albumUri,
        svc.suggestTags ? tags.trim() : "",
      );
      // Store the remote URL on the record when the service returned one — the Flickr
      // importer matches its own uploads by the photo id embedded in it.
      await api.recordPublication(
        photoId,
        versionId,
        /^https?:\/\//.test(publishedUrl) ? publishedUrl : undefined,
      );
      api.showToast(`Published to ${svc.name}.`);
      setStatus(`Published to ${svc.name} ✓`);
    } catch (e) {
      setStatus(String(e));
    } finally {
      setBusy(false);
    }
  };

  if (photoId == null) return <div className="panel-empty">Select a photo</div>;

  return (
    <div className="iptc-panel">
      <div className="field">
        <label>Version</label>
        <select
          className="folder-input"
          value={versionId ?? ""}
          onChange={(e) => setVersionId(e.target.value ? Number(e.target.value) : null)}
        >
          <option value="">Original</option>
          {versions.map((v) => (
            <option key={v.id} value={v.id}>
              {v.name}
            </option>
          ))}
        </select>
      </div>
      <div className="field">
        <label>Title</label>
        <input className="folder-input" value={title} onChange={(e) => setTitle(e.target.value)} />
      </div>
      <div className="field">
        <label>Description</label>
        <textarea
          className="folder-input"
          rows={2}
          style={{ resize: "vertical", fontFamily: "inherit" }}
          value={description}
          onChange={(e) => setDescription(e.target.value)}
        />
      </div>
      {svc.suggestTags && (
        <div className="field">
          <label>Tags</label>
          <textarea
            className="folder-input"
            rows={2}
            style={{ resize: "vertical", fontFamily: "inherit" }}
            value={tags}
            onChange={(e) => {
              setTags(e.target.value);
              setTagsTouched(true);
            }}
          />
          <span className="term-note">
            Prefilled from the photo's tags. Space-separated; quote multi-word tags
            ("northern lights"). Shown as real {svc.name} tags, not #hashtags.
          </span>
        </div>
      )}
      {svc.listAlbums && (
        <div className="field">
          <label>Album</label>
          <div className="row">
            <select
              className="folder-input"
              value={albumUri}
              onChange={(e) => selectAlbum(e.target.value)}
            >
              {albums.length === 0 && <option value="">(no albums yet)</option>}
              {albums.map((a) => (
                <option key={a.uri} value={a.uri}>
                  {a.name}
                </option>
              ))}
            </select>
            <button className="chip" onClick={refreshAlbums} title="Re-fetch albums from SmugMug">
              Refresh
            </button>
          </div>
          {svc.createAlbum && (
            <div className="row" style={{ marginTop: 6 }}>
              <input
                className="folder-input"
                placeholder="New album name"
                value={newAlbum}
                onChange={(e) => setNewAlbum(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") createAlbum();
                }}
              />
              <button
                className="chip"
                onClick={createAlbum}
                disabled={creating || !newAlbum.trim()}
              >
                {creating ? "Creating…" : "+ New"}
              </button>
            </div>
          )}
        </div>
      )}
      <div className="iptc-actions">
        <button
          className="chip chip-on"
          onClick={publish}
          disabled={busy || (!!svc.listAlbums && !albumUri)}
        >
          {busy ? "Publishing…" : `Publish to ${svc.name}`}
        </button>
        <span className="iptc-status">{status}</span>
      </div>
    </div>
  );
}
