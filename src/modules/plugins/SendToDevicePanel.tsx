// Shared UI for the LocalSend/Snapchat modules: a reusable panel that sends the current
// selection (≥1 photo, the active photo's version applied to it) to a LocalSend device on
// the LAN — a device list with Refresh (re-discover), a manual IP:port favorite, an optional
// PIN, a version picker (defaults to the active version), and a Send button with progress.
//
// It is a transfer surface, not a publisher: by default it records nothing. The Snapchat
// module passes `onSent` (to record the publication, host-stamped with its marker) and a
// `preflight` aspect check (rendered as a non-blocking warning above Send). The LocalSend
// target passes neither — same UI, pure transfer.
//
// Bundled first-party plugin: imports only from the module contract (../registry), core
// command wrappers (../api), and host hooks (../host), per the module isolation rule.
// Mirrors publishing.tsx. The localsend_* commands and the localsend:progress subscription
// are owned here and go through ChairPhotoAPI, never through core api.ts or Tauri directly.

import { useCallback, useEffect, useRef, useState } from "react";
import type { ChairPhotoAPI, Photo } from "../registry";
import { listVersions, PhotoVersion } from "../api";
import { useHostSelection } from "../host";

// ── Backend commands (owned by this shared publishing/transfer surface) ───────
// Per the module contract, a module's own commands go through `ChairPhotoAPI.invoke`
// rather than core's `api.ts`. They live here rather than in localsend.tsx because this
// panel is shared by the LocalSend and Snapchat modules — the same reason SmugMugAlbum
// lives in publishing.tsx.

/**
 * A LocalSend device, as discovered (UDP multicast) or a manual-IP favorite. Pass a
 * discovered object straight back to `localsendSend`. The camelCase fields mirror the
 * backend DTO. A manual favorite is `{ alias: "Manual", ip, port: 53317, protocol: "",
 * fingerprint: "" }`.
 */
interface LocalSendDevice {
  alias: string;
  deviceModel?: string | null;
  deviceType?: string | null;
  ip: string;
  port: number;
  /** `""` means "not announced — the backend probes it"; only a manual entry uses that. */
  protocol: "http" | "https" | "";
  fingerprint: string;
}

/** Result of a send: photos actually transferred vs. skipped (original offline / render
 *  failed). A network/handshake failure rejects the whole call with a string error. */
interface SendResult {
  sent: number;
  failed: number;
}

interface LocalSendProgress {
  done: number;
  total: number;
}

/**
 * Discover LocalSend devices on the LAN (UDP multicast listen + announce). `timeoutMs`
 * bounds the listen window (default 2500 in the backend). Requires the `localsend`
 * backend feature.
 */
const localsendDiscover = (api: ChairPhotoAPI, timeoutMs?: number) =>
  api.invoke<LocalSendDevice[]>("localsend_discover", { timeoutMs: timeoutMs ?? null });

/**
 * Send the given photos to a LocalSend device. `versionId` applies only to its own photo
 * (the others are sent unedited), matching export. Renders full-res JPEG(s) to a temp dir
 * and deletes them after. Streams `localsend:progress`. Requires the `localsend` feature.
 */
const localsendSend = (
  api: ChairPhotoAPI,
  photoIds: number[],
  versionId: number | null,
  device: LocalSendDevice,
  pin?: string,
) =>
  api.invoke<SendResult>("localsend_send", {
    photoIds,
    versionId: versionId ?? null,
    device,
    pin: pin ?? null,
  });

export interface SendToDevicePanelProps {
  api: ChairPhotoAPI;
  /** Called once per photo after a successful send (e.g. Snapchat records a publication).
   *  The active photo's version is passed for the active photo, null for the others. */
  onSent?: (photoId: number, versionId: number | null) => void;
  /** Optional pre-flight check on the active photo + its selected version; a non-null string
   *  is shown as a non-blocking warning notice above Send (the user can still send). */
  preflight?: (photo: Photo, version: PhotoVersion | null) => string | null;
}

const DEFAULT_PORT = 53317;

export function SendToDevicePanel({ api, onSent, preflight }: SendToDevicePanelProps) {
  // Rendered inside PublishDialog (via the LocalSend/Snapchat publish targets), which
  // (issue #16) now subscribes only to contributions — it no longer re-renders on
  // selection changes, so this panel needs its own subscription to pick up the active
  // photo/selection/version.
  useHostSelection();
  const photoId = api.getActivePhotoId();
  const selected = api.getSelectedPhotos();
  const photoIds = selected.length ? selected.map((p) => p.id) : photoId != null ? [photoId] : [];

  const [versions, setVersions] = useState<PhotoVersion[]>([]);
  const [versionId, setVersionId] = useState<number | null>(api.getActiveVersionId());
  const [devices, setDevices] = useState<LocalSendDevice[]>([]);
  const [selectedFingerprint, setSelectedFingerprint] = useState<string>("");
  const [manualIp, setManualIp] = useState("");
  const [manualPort, setManualPort] = useState(String(DEFAULT_PORT));
  const [pin, setPin] = useState("");
  const [scanning, setScanning] = useState(false);
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState<{ done: number; total: number } | null>(null);
  const [status, setStatus] = useState("");
  /** The latest manual IP, readable from inside an in-flight scan. The scan below starts on
   *  mount and resolves seconds later; its closure would otherwise still hold the empty
   *  string it captured, and auto-select a discovered device over an address the user has
   *  typed in the meantime — sending to the wrong device with no visible cause. */
  const manualIpRef = useRef("");
  /** Guards the mount scan against React StrictMode's deliberate double-invoke in dev, which
   *  would otherwise fire two overlapping discovery passes on every open. */
  const scannedOnMount = useRef(false);

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

  // Stream the per-file progress while a send is in flight. `onEvent` is an optional
  // host-API member, so guard for hosts predating it: without it the transfer still
  // works, it just shows no per-file progress.
  useEffect(() => {
    if (!api.onEvent) return;
    const sub = api.onEvent<LocalSendProgress>("localsend:progress", (p) => setProgress(p));
    return () => {
      sub.then((stop) => stop()).catch(() => {});
    };
  }, [api]);

  const discover = async () => {
    setScanning(true);
    setStatus("Scanning the network…");
    try {
      const found = await localsendDiscover(api);
      setDevices(found);
      // Auto-select the first device only when the user has not typed an address. They are
      // mutually exclusive inputs (`chosenDevice` prefers a selected device), so claiming the
      // selection here would override a manual IP entered while this scan was in flight.
      if (
        found.length &&
        !manualIpRef.current.trim() &&
        !found.some((d) => d.fingerprint === selectedFingerprint)
      ) {
        setSelectedFingerprint(found[0].fingerprint);
      }
      setStatus(found.length ? "" : "No devices found — enter an IP below.");
    } catch (e) {
      setStatus(String(e));
    } finally {
      setScanning(false);
    }
  };

  // Scan once when the panel opens. Without this the device list is empty until the user
  // finds Refresh, which reads as "discovery is broken" — it was the first thing to go wrong
  // when this panel was tested against a real device. Deliberately not re-run on any
  // dependency: a pass takes about five seconds and re-scanning mid-interaction would fight
  // the user's selection. Refresh stays the way to scan again.
  useEffect(() => {
    if (scannedOnMount.current) return;
    scannedOnMount.current = true;
    void discover();
    // `discover` is recreated each render; depending on it would re-run this on every
    // keystroke. The ref above is what makes "once" true, not the dependency list.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const activeVersion = versionId != null ? versions.find((v) => v.id === versionId) ?? null : null;
  const activePhoto = selected.find((p) => p.id === photoId) ?? null;
  const warning = preflight && activePhoto ? preflight(activePhoto, activeVersion) : null;

  // The device to send to: a discovered device by fingerprint, else the manual favorite.
  const chosenDevice = (): LocalSendDevice | null => {
    const picked = devices.find((d) => d.fingerprint === selectedFingerprint);
    if (picked) return picked;
    const ip = manualIp.trim();
    if (!ip) return null;
    const port = Number(manualPort) || DEFAULT_PORT;
    // Empty protocol = "unknown, probe it". A manually typed address carries no announced
    // scheme, and the two share port 53317: hardcoding "http" here sent plain HTTP at the TLS
    // listener every current LocalSend build runs, and also skipped the client certificate,
    // which `client_for` attaches only for https. The backend resolves it (see
    // `localsend::probe_protocol`).
    return { alias: "Manual", ip, port, protocol: "", fingerprint: "" };
  };

  const send = async () => {
    const device = chosenDevice();
    if (device == null || photoIds.length === 0) return;
    setBusy(true);
    // Only seed the counter when progress events can actually arrive; on a host without
    // `onEvent` it would otherwise sit frozen at 0/N for the whole transfer.
    setProgress(api.onEvent ? { done: 0, total: photoIds.length } : null);
    setStatus("Sending…");
    try {
      const res = await localsendSend(api, photoIds, versionId, device, pin.trim() || undefined);
      if (onSent) {
        for (const id of photoIds) onSent(id, id === photoId ? versionId : null);
      }
      const failedNote = res.failed ? `, ${res.failed} skipped` : "";
      api.showToast(`Sent ${res.sent} to ${device.alias}${failedNote}.`);
      setStatus(`Sent ${res.sent}${failedNote} ✓`);
    } catch (e) {
      setStatus(String(e));
    } finally {
      setBusy(false);
      setProgress(null);
    }
  };

  if (photoIds.length === 0) return <div className="panel-empty">Select a photo</div>;

  const canSend = !busy && chosenDevice() != null;

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
        <label>Device</label>
        <div className="row">
          <select
            className="folder-input"
            value={selectedFingerprint}
            onChange={(e) => setSelectedFingerprint(e.target.value)}
          >
            {devices.length === 0 && <option value="">(none found — use manual IP)</option>}
            {devices.map((d) => (
              <option key={d.fingerprint} value={d.fingerprint}>
                {d.alias}
                {d.deviceModel ? ` (${d.deviceModel})` : ""} — {d.ip}
              </option>
            ))}
          </select>
          <button className="chip" onClick={discover} disabled={scanning} title="Re-discover devices on the LAN">
            {scanning ? "Scanning…" : "Refresh"}
          </button>
        </div>
        <div className="row" style={{ marginTop: 6 }}>
          <input
            className="folder-input"
            placeholder="Manual IP (e.g. 192.168.1.42)"
            value={manualIp}
            onChange={(e) => {
              const next = e.target.value;
              setManualIp(next);
              manualIpRef.current = next;
              if (next.trim()) setSelectedFingerprint("");
            }}
          />
          <input
            className="folder-input"
            style={{ maxWidth: 80 }}
            placeholder="Port"
            value={manualPort}
            onChange={(e) => setManualPort(e.target.value)}
          />
        </div>
        <span className="term-note">
          Devices are found over the local network. If discovery is blocked, type the
          device's IP shown in its LocalSend app.
        </span>
      </div>

      <div className="field">
        <label>PIN (optional)</label>
        <input
          className="folder-input"
          placeholder="If the receiver requires a PIN"
          value={pin}
          onChange={(e) => setPin(e.target.value)}
        />
      </div>

      {warning && (
        <div className="modal-sub" role="status" style={{ color: "var(--warn, #c98a00)" }}>
          {warning}
        </div>
      )}

      {progress && (
        <div className="modal-sub">
          Sending {progress.done}/{progress.total}…
        </div>
      )}

      <div className="iptc-actions">
        <button className="chip chip-on" onClick={send} disabled={!canSend}>
          {busy ? "Sending…" : `Send ${photoIds.length > 1 ? `${photoIds.length} photos` : "photo"}`}
        </button>
        <span className="iptc-status">{status}</span>
      </div>
    </div>
  );
}
