---
title: "Face Tagging"
description: "Local face detection and recognition, and how faces become people tags."
tags:
  - chairphoto/module
  - chairphoto/tagging
aliases:
  - "Faces"
  - "Face recognition"
---

# Face Tagging

Detects faces, recognizes who each one is, and turns that into ordinary person tags — entirely
on your machine. Unlike AI Tagging there is no cloud option at all: no image and no embedding
ever leaves the computer.

The catalog has an advantage most face taggers lack. Years of manual tagging mean thousands of
photos already record *who is in the photo* — they just lack face regions saying *which face*.
Those existing tags are used as weak labels to bootstrap recognition, so you are not naming
clusters from scratch.

Optional, behind the `faces` Cargo feature, with plugin-owned `faces__*` tables. ChairPhoto is
fully distributable without it.

## Model stack

Inference runs on ONNX Runtime through the `ort` crate.

| Stage | Model | License | Notes |
|---|---|---|---|
| Detection + 5 landmarks | **YuNet** (OpenCV Zoo) | Apache-2.0 | ~350 KB, real-time on CPU |
| Embedding (512-d) | **AuraFace-v1** (fal.ai) | Apache-2.0 | ArcFace-style, trained on commercially usable data, 99.65% LFW |

Both are Apache-2.0 and therefore redistribution-safe. The higher-accuracy InsightFace
`buffalo_l` weights are **non-commercial only**, so they are never bundled and never
auto-downloaded; if they are ever offered it will be as an explicit download-it-yourself opt-in.

Models are not vendored. They download on first enable into `<app_data_dir>/models/` from pinned
official sources with SHA-256 verification. Missing models degrade to a "models not downloaded"
state rather than crashing — the same contract as the external binary dependencies.

## Pipeline

1. Render the photo's cached **2048 px preview** — 512 px thumbnails are too small for faces.
   Always resolve through `resolve_photo_path`, never `photos.path`.
2. **Detect** — YuNet returns a bounding box, five landmarks, and a confidence.
3. **Align** — similarity transform from those landmarks to the canonical 112×112 crop.
4. **Embed** — AuraFace produces a 512-d vector, L2-normalized.
5. **Match** — cosine similarity against known people, above a tunable threshold.

Per photo on a Ryzen 3900X: detection 10–30 ms, plus a few milliseconds per face to align and
embed. A 100k-photo library indexes in a few hours of background CPU time. Embeddings for
~200k faces come to roughly 400 MB, so a plain table with a brute-force cosine scan is enough —
no vector index.

## Data model

`faces__faces` holds one row per detected face:

- `photo_id`, `bbox` (x/y/w/h **normalized 0–1** against the oriented full image, so it survives
  any resolution change), `landmarks`, `detect_confidence`
- `embedding` — 512 f32 little-endian, about 2 KB per face
- `person_tag_id`, `state` (`unassigned` / `suggested` / `confirmed` / `rejected` / `ignored`),
  `match_confidence`, `source` (`detect` / `seed` / `match` / `manual` / `xmp`)

A face marked **ignored** — a photobomber, a face in a background crowd — is excluded from
centroids and suggestions but kept, so re-indexing cannot resurrect it.

`faces__clusters` tracks unnamed clusters for faces matching no known person.

**A person is a tag.** `person_tag_id` points at a normal hierarchical tag under a people root
you choose (`faces.people_root`, default `People`). There is no separate person table, so
confirming a face goes through the existing `assign_tag` path and inherits XMP keyword export
and cross-catalog merge by tag UUID for free.

## Seeding and matching

1. **Auto-seed.** A photo with exactly one detected face and exactly one person tag assigns that
   tag as `confirmed`, `source = seed`. Marked as machine-derived, so it stays auditable and
   revocable.
2. **Per-person centroids** are computed from confirmed faces and updated incrementally. A person
   with many faces may carry several sub-centroids to cover changes in appearance over time.
3. **Constrained match.** When a photo has N faces and M person tags, the assignment is solved
   optimally (Hungarian algorithm, cost = 1 − cosine to each centroid). The photo's own tags
   constrain the search space, which is what makes this markedly more accurate than open-set
   matching.
4. **Open match.** Faces in untagged photos go to the nearest centroid above threshold as
   `suggested`; confirming also assigns the person tag to the photo.
5. **Clustering.** Faces below threshold join the nearest existing cluster within threshold, or
   start a new one. Deliberately incremental rather than batch DBSCAN, because clusters have to
   keep evolving as photos arrive. Naming a cluster creates or binds a person tag and confirms
   its faces.
6. **Rejection memory.** Rejecting a suggestion records the (face, person) pair so it is never
   proposed again.

Everything except auto-seeding is a suggestion you confirm — the same non-destructive contract as
AI Tagging.

## Indexing

A background worker with its own catalog connection, bounded parallelism,
`faces:progress {done, total}` events, abort-safe, and resumable through a persistent queue.
Triggered by an explicit "Index faces" action.

Parallelism follows the `indexing.speed` preference:

- **`background`** (default) — at most 2 concurrent detect/embed workers with 2 intra-op threads
  each, so a re-index leaves the desktop responsive.
- **`full`** — scales to roughly N/2 intra-op threads per worker and finishes as fast as the
  hardware allows, at the cost of responsiveness during the run. Measured about 1.5× faster than
  `background` on a 24-thread Ryzen 9 3900X.

## XMP face regions

Confirmed faces are written to the sidecar as **MWG Regions** (`mwg-rs:Regions`), the Metadata
Working Group schema that digiKam, Lightroom and Picasa all understand. The codec is in
`crate::xmp` (`write_face_regions` / `read_face_regions`); the catalog-side wiring is in
`plugins/faces/regions.rs`.

**Structure.** `mwg-rs:AppliedToDimensions` records the photo's **oriented** pixel size — EXIF
dimensions with the non-destructive `user_rotation` applied, so a 90°/270° rotation swaps the
axes. That is the reference frame for the normalized areas. Each region in the `RegionList`
carries `mwg-rs:Name` (the person tag's leaf name), `mwg-rs:Type="Face"`, and an `mwg-rs:Area`
whose `x`/`y` are the rectangle's normalized **center** — MWG stores centers, not corners — with
`w`/`h` as the size. Stored bboxes are top-left-normalized, so the writer converts corner→center
and the reader converts back.

**Merge safety is binding.** The RegionList may already contain regions written by other tools.
The writer rebuilds it from the foreign regions it did not write plus ChairPhoto's current
confirmed set. A region counts as ours — and is therefore replaced — only when its `Name` matches
one we are writing *and* its center area is within `AREA_EPSILON = 0.02`. **When in doubt, the
region is preserved.** Re-writing updates only ChairPhoto's own regions and can never duplicate
or clobber a foreign face.

**Writes** fire from the same hooks as keyword export — `faces_accept`, `faces_assign`,
`faces_reject`, `faces_ignore`, `faces_name_cluster` — each writing the photo's full current
confirmed set, so the sidecar stays in sync.

**Reads** happen during indexing: existing `mwg-rs:Regions` are parsed and IoU-matched
(≥ 0.5, greedy best-first, one-to-one) against the photo's still-unassigned detections. A named
match confirms the face (`state = 'confirmed'`, `source = 'xmp'`), finding or creating the person
tag under `<people_root>/<region name>`. Labels written by digiKam, Lightroom, Picasa or a
previous ChairPhoto run are therefore ingested for free.

## Interface

- **Loupe overlay** — face rectangles on the photo, each with a chip showing the assigned or
  suggested name and confirm / reject / reassign / ignore actions.
- **People view** — a wall of named people with face crops as avatars and photo counts, plus
  unnamed clusters waiting to be named. Clicking a person filters to their photos, and a review
  queue supports bulk confirmation.
- **Settings** — people root, model download status, similarity threshold, and index actions
  with progress.

## GPU acceleration

Optional NVIDIA inference behind the additive `faces-cuda` feature, which implies `faces` and
enables `ort/cuda`. Off by default so the standard build stays CPU-only and portable.

**It cannot crash on a machine without CUDA.** The engine registers the CUDA execution provider
explicitly per session (`engine::try_register_cuda`) rather than through
`with_execution_providers`, precisely so a registration failure is observable. If the runtime,
driver, GPU or **cuDNN 9** is missing, registration returns an error, the reason is logged, and
inference continues on CPU. `engine::active_ep()` reports where inference actually ran, so the UI
can be honest about it.

Setting `faces.force_cpu = "true"` skips CUDA registration even in a `faces-cuda` build — useful
when the GPU is needed elsewhere or to compare the two directly.

At runtime the provider needs the CUDA runtime libraries **and a complete cuDNN 9** on the loader
path. A partial cuDNN install can abort ONNX Runtime during CUDA init; that is an install
problem, not a ChairPhoto one.

Measured on an RTX 3080 against a four-face fixture: identical detections, embeddings agreeing to
cosine ≈ 0.9999, and 702 ms → 462 ms per image (about 1.5×). The modest gain reflects tiny models
and un-optimized CPU-side preprocessing dominating a debug build.

## Settings

| Key | Purpose |
|---|---|
| `faces.people_root` | Tag branch holding person identities, default `People` |
| `faces.match_threshold` | Cosine threshold for matching and clustering, default `0.45` |
| `faces.force_cpu` | Skip CUDA registration in a `faces-cuda` build |
| `indexing.speed` | `background` (default) or `full` |
