---
title: "LocalSend — send photos to a device on the LAN"
description: "Send photos to a device on the LAN, and the Snapchat flow built on it."
tags:
  - chairphoto/module
  - chairphoto/publishing
aliases:
  - "Snapchat"
  - "Send to device"
---

# LocalSend — send photos to a device on the LAN

Send a selected photo or version from ChairPhoto to a [LocalSend](https://localsend.org)
device — a phone or laptop — over the local network, using LocalSend's **documented HTTP
protocol** rather than scraping. ChairPhoto **sends**; it never accepts a photo from the
network. Discovery does bind a short-lived inbound port to hear peers' register replies
(see *Discovery* below), but that port answers `register` and refuses uploads.

This also underpins the **Snapchat** module: no good API posts to a Snapchat story, so the
workflow is *send the photo to the phone via LocalSend, then post it by hand from the
phone* — and ChairPhoto records it as published to Snapchat.

## Where it lives

A **publish target**, "Device (LocalSend)", in the unified **Publish** dialog. It sends the
current selection of one or more photos.

Rendering is `render_localsend_jpegs` (`commands/localsend.rs`) — **not**
`commands::publishing::render_export_jpeg`, the SmugMug/Flickr helper. The two are siblings
built from the same `export` primitives (`resolve_originals`, `upload_file_name`,
`JobTempDir`, `write_item_jpeg`), not one calling the other, so they share behaviour by
construction rather than by delegation:

- **Shared.** Version-picking (the selected version where it matches), and EXIF/GPS carried
  into the render — `write_item_jpeg` re-encodes, which strips metadata, then copies EXIF+GPS
  and embeds keywords/rating/IPTC back via exiftool. Best-effort: without `exiftool` the
  pixels still send and the metadata copy is logged and skipped.
- **Different.** One output path per resolvable photo, rather than one render for one photo.
  Always full resolution — the publish path applies a per-module long-edge limit
  (`write_item_jpeg_with_long_edge`); LocalSend never downscales, and lets the device decide.
  A photo whose render fails, or whose filename would collide with an earlier render in the
  same send, is skipped and counted as failed rather than aborting the whole send or being
  sent under another photo's name.

Unlike the service targets it is a **transfer, not a publication**, so it does **not** call
`api.recordPublication` — the publish-target contract lets a target's `render()` do
whatever it needs.

The send UI is a reusable `SendToDevicePanel` exported from the LocalSend plugin: device
list with Refresh, manual IP, optional PIN, version picker, Send with progress, an optional
`onSent` callback, and an optional `preflight(photo, version)` returning a non-blocking
warning. The Snapchat module renders the exact same flow and differs only in what happens
on success — mirroring how `publishing.tsx` is shared by Flickr and SmugMug.

## Protocol (LocalSend v2)

- **Discovery** — UDP multicast on `224.0.0.167:53317`. Each device periodically sends an
  announcement JSON `{ alias, version:"2.0", deviceModel, deviceType, fingerprint, port,
  protocol:"http"|"https", download }`; replying with our own announcement makes us visible
  too. When multicast is blocked, a manual `IP:port` is the fallback.

  Discovery is **two-sided**: a peer that hears our announcement replies with `POST
  /api/localsend/v2/register` to the `port` we advertised, and only falls back to a UDP
  `announce:false` datagram if that fails. So for the duration of one discovery pass
  ChairPhoto binds an **ephemeral** TCP port and advertises that — not `53317`, which
  another LocalSend-speaking process on the same machine may hold. It answers `register`
  and refuses every other route, `prepare-upload` and `upload` explicitly with `403`.
  The listener is released when the pass ends. See
  [`docs/adr/0001-localsend-register-listener.md`](adr/0001-localsend-register-listener.md)
  for why an inbound port is justified in a local-first app, and `localsend/register.rs`.
- **Subnet sweep** — some devices never participate in multicast at all, and no amount of
  listening finds them. Measured: an iPhone running LocalSend v2.2 answers `ping`, accepts TCP
  on `:53317` in 6–49 ms and returns HTTP 200 from `/info`, while multicast discovery sees only
  the desktop app on the sending machine. So each pass also sweeps the local subnet: a TCP
  connect to `:53317` on every host address, then `GET /api/localsend/v2/info` on the few that
  answer. Peers found this way are merged into the same result and deduped by fingerprint, so a
  device that answers both paths still appears once.

  It runs **on every pass, alongside multicast**, not as a fallback when multicast finds
  nothing — on a machine that also runs the LocalSend desktop app, multicast never finds
  nothing, so a fallback would never fire and the phone would stay invisible. It is bounded to
  finish inside the discovery window described below, and the same deadline cuts it off if it
  ever runs long, so it does not lengthen a Refresh. See `localsend/scan.rs`.
- **HTTP base** `http(s)://<ip>:<port>/api/localsend/v2/…`:
  - `POST /prepare-upload` — body `{ info:<our device info>, files:{ <fileId>:{ id,
    fileName, size, fileType } } }` → `{ sessionId, files:{ <fileId>:<token> } }`. A
    PIN-protected receiver returns `401`; the client retries with `?pin=NNNN`.
  - `POST /upload?sessionId=&fileId=&token=` — raw file bytes as the body → `200`.
  - `POST /cancel?sessionId=` — abort.
- **TLS** — the device's announced `protocol` is honoured. For `https`, which on a LAN
  means self-signed, the `reqwest` client both accepts the peer's certificate **and presents
  one of its own**. Some peers require a client certificate and abort the handshake with
  `certificate required` without it — observed on LocalSend mobile v2.2, where
  `danger_accept_invalid_certs` alone is not enough because that governs only how we treat
  *their* certificate. Peers that do not require one ignore it (verified against the desktop
  app v2.1), so it is presented unconditionally rather than negotiated. The certificate is
  self-signed, generated once per installation and stored under the app data directory
  (`localsend-client-identity.pem`, `0600`); it authenticates nothing, since every
  certificate in this protocol is self-signed and LocalSend pins by fingerprint. See
  `localsend/identity.rs`. ChairPhoto generates its own random `fingerprint` for sending.

## Implementation

All I/O is in Rust. `src-tauri/src/localsend/mod.rs`, behind a `localsend` Cargo feature,
reuses `reqwest` and does UDP through tokio:

- `discover(timeout_ms) -> Vec<Device>` — multicast listen and announce, plus a concurrent
  subnet sweep, yielding `Device { alias, deviceModel, deviceType, ip, port, protocol,
  fingerprint }`. The three sources (UDP announcement, register POST, sweep) feed one channel
  and one dedupe map, so each peer yields one `Device` however many ways it was heard.
- `send_files(device, [paths], pin?)` — prepare-upload then per-file upload, emitting a
  `localsend:progress` event stream like card import.

Two feature-gated commands:

```
localsend_discover() -> [{ alias, deviceModel, deviceType, ip, port, protocol, fingerprint }]
localsend_send(photoIds, versionId?, device, pin?) -> { sent, failed }
```

`SendToDevicePanel.tsx` owns the module's backend surface: the `localsend_discover` and
`localsend_send` wrappers go through `ChairPhotoAPI.invoke`, and the `localsend:progress`
stream through `ChairPhotoAPI.onEvent` — not through core `api.ts`, and never through
Tauri directly. `onEvent` is an optional host-API member, so the panel guards for hosts
that predate it, in which case progress simply does not display.

Unit tests cover the discovery-JSON parse, the prepare-upload body and response shapes, the
device-info builder, the host's `satisfies` semver helper, and the Snapchat aspect helper.
The discovery-socket tests (issue #39) are the exception: they bind the real well-known UDP
port, join the real multicast group, and send/receive real datagrams over loopback — see the
`#[cfg(test)] mod tests` doc comments in `src-tauri/src/localsend/mod.rs` for what that costs
(serialized against each other and against a co-resident LocalSend desktop app) and how it's
kept hermetic (loopback only, never the LAN).

The sweep keeps that hermeticity: its subnet arithmetic and info-body parsing are pure
functions with no network at all, and the end-to-end tests drive it against a loopback stub on
an ephemeral port. The multicast tests pass an explicitly empty target list, so `cargo test`
never sweeps the developer's real network. One test (`discover_returns_a_device_found_only_by_
the_subnet_sweep`) exists purely to fail if the sweep is ever dropped from `discover_on` —
without it, the feature could be deleted from the wiring and every other test would stay
green.

## Snapchat

Snapchat is a thin module layered on LocalSend: send to phone, then record.

- `plugins/snapchat.tsx` declares `publicationMarker: "snapchat"`, `backendFeature:
  "localsend"` (it reuses `localsend_send`), and `requires: [{ id: "localsend", version:
  "^0.1.0" }]`.
- Its publish target renders the shared `SendToDevicePanel` with `onSent={() =>
  api.recordPublication(photoId, versionId)}`. Because the host stamps the *calling*
  module's marker, the record lands as **snapchat** — so after you send to the phone and
  post the story by hand, the photo appears under "Published to → Snapchat" and the
  `published:snapchat` facet, exactly like the API-based publishers.
- **Aspect pre-flight.** Snapchat stories are vertical **9:16 (1080×1920)**, so Snapchat
  passes a `preflight` that returns a **non-blocking warning** when the effective aspect is
  not within about ±3% of 9:16 — for example *"This looks like 3:2 landscape — Snapchat
  stories are vertical 9:16 (1080×1920). Make a 9:16 crop (Versions → edit) for best
  results."* You can still send. The effective aspect is the selected version's crop when
  present, read generically from the version's `editJson` (any `crop` with `w`/`h`, or an
  `aspect` like "9:16"), else `photo.width / photo.height`. Resolution is not enforced,
  since Snapchat downscales — only the shape is flagged.

The LocalSend target passes no `onSent` and no `preflight`: pure transfer, no record, no
Snapchat-specific check. That and the marker are the only differences between the two
modules.

## Module dependencies (`requires`)

`ChairPhotoModule` (`src/modules/registry.ts`) carries a version-aware `requires`:

```ts
requires?: { id: string; version?: string }[];   // version = semver range; omit = any
// e.g. snapchat: requires: [{ id: "localsend", version: "^0.1.0" }]
```

Modules already carry a `version` such as "0.1.0", and a dependency's range is matched
against it. Enforcement is in `src/modules/host.ts`:

- `enableModule(id)` first recursively enables each required module, dependencies before
  dependents so `onLoad` order is correct. It **refuses**, and reports why, if a required
  module is missing, its `backendFeature` is not compiled, or its version does not satisfy
  the requested range.
- `disableModule(id)` cascade-disables any enabled module that requires it, with a toast,
  so there is never an orphaned dependent.
- `persistEnabled`/`initHost` enable in dependency order on load.
- `components/ModulesPanel.tsx` shows "Requires: `<name>` `<range>`" and disables the
  toggle when a dependency is unavailable or version-incompatible.

Version matching uses a small in-repo `satisfies(version, range)` — no new dependency —
covering what simple `MAJOR.MINOR.PATCH` module versions need: omitted or `*` for any,
exact `X.Y.Z`, `>=X.Y.Z`, and npm-style caret `^X.Y.Z`. For `X > 0` the caret means same
major and greater-or-equal; `^0.Y.Z` means same major **and minor** and greater-or-equal,
since a 0.x bump is treated as breaking. See [plugin-system.md](plugin-system.md).

## Limits

- A real transfer can only be verified against an actual LocalSend device on the same
  network, which has not been available in development. The implementation follows the
  published spec and should be sanity-checked against the current LocalSend protocol
  version on first real send.
- The protocol can drift between LocalSend versions; this implementation targets v2.
- Multicast is blocked on some networks. The subnet sweep covers most of what that used to
  cost; manual IP remains the last resort, for a peer on a non-default port or on another
  subnet.
- The sweep probes the well-known port only. A peer that has moved off `:53317` is found by
  multicast or by manual IP, not by the sweep.

## Privacy posture

ChairPhoto is local-first, and discovery is the one place it speaks to the network without
being asked to send something. What that involves, precisely:

- **Multicast announcement** — the alias `ChairPhoto`, a randomly generated per-run
  fingerprint, and a port. Group-addressed, so it reaches the local link only.
- **Subnet sweep** — a TCP connection attempt to `:53317` on each address in the interface's
  own subnet, and nothing else until a host answers. A host that is absent or running something
  unrelated receives one connect and is never contacted again; only a host that returns a valid
  LocalSend `/info` body becomes a device.
- **Where it will not go.** The sweep is confined to the local link by construction. Loopback
  and point-to-point interfaces are excluded, so a VPN or tunnel subnet is never swept, and any
  network wider than a /24 is declined outright rather than sampled — a decision logged with
  its reason, not silent. No catalog data, photo, or filename is sent at any point in
  discovery.
- **Sending is still separate and still explicit.** Discovery finding a device does nothing on
  its own; photos leave only when you pick that device and send.
