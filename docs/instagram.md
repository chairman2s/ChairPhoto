---
title: "Instagram publishing module"
description: "Post to Instagram by driving Chrome, supervised by default."
tags:
  - chairphoto/module
  - chairphoto/publishing
---

# Instagram publishing module

Posts the selected version to Instagram and records the result via the
[`publications`](publications.md) model. Unlike [Flickr](flickr.md) and
[SmugMug](smugmug.md), there is **no API key** — Instagram has no public "post a local
file" endpoint, so ChairPhoto drives a real **Chrome** browser instead.

The module is in the **default build** (`instagram` Cargo feature). It needs Chrome or
Chromium present at runtime.

## How posting works

**Supervised by default.** With the auto-publish toggle off — the default — ChairPhoto
opens the create dialog, attaches the image, types the caption, and then **stops before
Share**. The composed post is left on screen and you click Share yourself. Only with the
toggle on does it click Share for you.

Every attempt reports one of three outcomes:

| outcome | meaning |
|---------|---------|
| **posted** | Composed and shared, with the confirmation seen. |
| **awaitingReview** | Composed and left on screen for you to review and Share. |
| **needsLogin** | Not logged in. The Chrome window is open at the login page — log in and retry. |

A publication is recorded **only on a confirmed post**. After a supervised run ChairPhoto
asks whether you completed it, because it cannot see a click it did not make.

## Signing in

There is no key or token to paste. ChairPhoto launches Chrome with its **own persistent
profile directory** and connects to it over the DevTools Protocol, so:

- Your Instagram login lives in that profile and **persists between posts**.
- The Chrome window is launched detached and **survives after ChairPhoto is done** — which
  is what makes a supervised post possible.
- Your normal browser profile is untouched.

Chrome is located by trying the usual executable names (`google-chrome-stable`,
`google-chrome`, and the Chromium equivalents). A clear error is returned when none is
found.

## The image and the caption

The selected version is rendered through the export path and **resized to 1080 px wide**.
Every aspect Instagram supports — square, 4:5 portrait, 1.91:1 landscape, 9:16 story — is
1080 wide, so this matches what Instagram actually displays and avoids a second
recompression on their side.

`build_instagram_caption` prefills the caption from the photo's IPTC title and its export
keywords rendered as `#hashtags` — the same keyword set described in
[taxonomy.md](taxonomy.md). The prefill stops as soon as you edit the field, so your typing
is never overwritten.

## Where it lives

- `src-tauri/src/instagram/mod.rs` — the Chrome automation.
- `src-tauri/src/commands/instagram.rs` — `post_to_instagram` and
  `build_instagram_caption`, both gated on the `instagram` feature. The render goes to a
  temp JPEG.
- `src/modules/plugins/instagram.tsx` — the publish target in the unified Publish dialog:
  version picker, caption box, auto-publish toggle, and the post-run confirmation.

The module declares `publicationMarker: "instagram"`, so the host stamps the marker and
posts appear under "Published to → Instagram" and the `published:instagram` facet — the
same contract Flickr and SmugMug use.

## Why it is built this way

Instagram's CSS class names are obfuscated and rotate, so the automation clicks by
**visible text** ("Next", "Share") and **ARIA labels** ("New post", "Write a caption…")
rather than selectors. Setting the file input is done over the DevTools Protocol, because
that is the one step page JavaScript is not allowed to do.

## Limits

- **This is browser automation against a moving target.** Instagram can change its
  composer at any time and break the flow; expect occasional selector tuning. That
  fragility is the reason supervised posting is the default.
- Chrome or Chromium must be installed.
- One photo per post — no carousels, no Reels, no Stories. For stories, see the Snapchat
  approach in [localsend.md](localsend.md): send to the phone and post by hand.
- Because a supervised post is completed by you outside the app, ChairPhoto has to ask
  whether it succeeded rather than observing it.
