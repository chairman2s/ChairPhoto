# Product: RAW foundation — Develop renders the real file

## Problem

"When I open a keeper in Develop, the picture I'm adjusting isn't the picture I'll get.
It's the little JPEG the camera tucked inside the RAW — 8 bits, the camera's own contrast
baked in, the sky already white. I pull Exposure down and the sky stays white, because the
detail was thrown away before ChairPhoto ever looked at the file. Then the export is made
from the real RAW and quietly bent to match what I saw. I can't adjust things on an image
that doesn't really look like the result. In Develop I want to work on the best possible
quality, always — no 'fast' mode, no half-size, no setting to get wrong. The Library can
flip through proxies; the Darkroom can't."

## Success metric

**What you see is what you export: 0 exports that differ from the Develop view.**
Measured in-app on every edited export of a supported RAW: at 100% the exported pixels
are compared exactly against the Develop render of the same version at 100%; at Fit the
export is scaled to the view's size and compared within the tolerance of resampling only.
Today every export differs beyond that (it is a different source, tone-matched after the
fact); the target is a count of zero differing exports, month after month.

Supporting number, the reason the metric matters: on the set of keepers with clipped
highlights, pulling Exposure or Highlights down recovers sky and cloud detail in every
frame where the RAW still holds it — today it recovers it in none.

## Announcement — the blog post before the feature

Develop now works on your **actual photograph**. Open a keeper and ChairPhoto decodes the
RAW itself — all 14 bits the sensor recorded, in linear light — and every slider, the tone
strip, the proof sheet and the loupe render from that, not from the camera's preview JPEG.
Pull the exposure down and a blown sky gives back its clouds; warm the white balance and
the colours move the way the sensor saw them, not the way the camera's JPEG guessed. The
first look appears instantly from the camera's preview, marked *preparing*, and swaps to the
real thing a moment later; step through a set and the next photos are already prepared.
What you see at 100% is exactly what the export writes, pixel for pixel, and Fit is that
same picture scaled to your screen, because both are the same rendering of the same file.
If a camera is newer than ChairPhoto's decoder, Develop says so on the photo and works on
the camera's preview instead — no guessing, no silent fallback. Nothing changes for the
Library — it stays fast on proxies — and your originals, as always, are never touched.

## Screens

- `mockups/01-preparing.html` — the Darkroom stage in its states: the camera preview
  shown at once with a quiet *Preparing full quality* pill, then the developed RAW with a
  source badge (*RAW · 14-bit · 67 MP*). Two honest exceptions carry their own badge: a
  JPEG-only photo (*JPEG · 8-bit*, the file's full quality is simply what you get) and a
  RAW the decoder does not support yet (*camera preview · RAW not supported yet*, with the
  camera named). No user setting anywhere.
- `mockups/02-headroom.html` — why it matters: the same blown sky at as-shot, with
  Exposure −1 on the camera JPEG (still white), and with Exposure −1 on the RAW (clouds
  back), the tone strip's whites mass shrinking to match. Also the sensor-clipping
  overlay: where the RAW itself is clipped, which is the only "gone for good".
