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

A **publish target**, "Device (LocalSend)", in the unified **Publish** dialog. It reuses
the per-photo, version-picking, full-res EXIF-preserving render path
(`render_export_jpeg`) built for SmugMug, and sends the current selection of one or more
photos.

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
- **HTTP base** `http(s)://<ip>:<port>/api/localsend/v2/…`:
  - `POST /prepare-upload` — body `{ info:<our device info>, files:{ <fileId>:{ id,
    fileName, size, fileType } } }` → `{ sessionId, files:{ <fileId>:<token> } }`. A
    PIN-protected receiver returns `401`; the client retries with `?pin=NNNN`.
  - `POST /upload?sessionId=&fileId=&token=` — raw file bytes as the body → `200`.
  - `POST /cancel?sessionId=` — abort.
- **TLS** — the device's announced `protocol` is honoured. For `https`, which on a LAN
  means self-signed, a cert-accepting `reqwest` client is used; LocalSend itself pins by
  fingerprint. ChairPhoto generates its own random `fingerprint` for sending.

## Implementation

All I/O is in Rust. `src-tauri/src/localsend/mod.rs`, behind a `localsend` Cargo feature,
reuses `reqwest` and does UDP through tokio:

- `discover(timeout_ms) -> Vec<Device>` — multicast listen and announce, yielding
  `Device { alias, deviceModel, deviceType, ip, port, protocol, fingerprint }`.
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
- Multicast is blocked on some networks — the manual-IP path is the fallback.
