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

The pass runs as a background job with its own catalog connection, so a match over a large
library does not hold the catalog against the UI. `faces_run_matching` returns the job id as
soon as the run has started; progress arrives as `faces:match_progress {done, total, phase, job}`
and the counters as a terminal `faces:match_done`. It is cancellable, stopping at the next face
and again at the next phase boundary, and a catalog switch stops it the same way. Stopping is a
stop, not a rollback: the pass is idempotent, so suggestions already written stay and a re-run
recomputes the rest.

## Indexing

A background worker with its own catalog connection, bounded parallelism,
`faces:progress {done, total, job}` events, abort-safe, and resumable through a persistent queue.
Triggered by an explicit "Index faces" action.

Indexing and matching are separate jobs with separate abort flags and status slots, so
cancelling one does not stop the other.

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

**The frame.** Face boxes are stored normalized in one canonical frame, the **display frame**:
the photo as its own metadata orients it (the EXIF-oriented preview the indexer detects on),
**without** the non-destructive `user_rotation`. The loupe draws the picture with the user
rotation applied, so the overlay turns each box by it for display and turns a box drawn on the
rotated picture back before storing it. The user rotation lives only in the catalog — the
original is never rewritten and the sidecar carries no orientation of ours — so the export
ignores it too: other tools see the file unturned, and so must its regions.

MWG regions use a different frame, the **stored frame**: MWG 2.0 § 5.9 requires region
coordinates "relative to the stored image, prior to the application of the Exif Orientation
tag", and `AppliedToDimensions` is the stored image's size. A scan records each photo's EXIF
Orientation (`photos.exif_orientation`, 1–8, from exiftool's `EXIF:Orientation`; schema v26
fills it for photos scanned earlier from the metadata they already stored). The export turns
each box from the display frame into the stored frame by that orientation — all eight,
mirrors included — and the import (`read_face_regions_in`) turns a region back before matching
it to the detections. **An unknown orientation is never guessed:** the boxes are written and
read as they are.

**Structure.** `mwg-rs:AppliedToDimensions` records the stored pixel size — the photo's recorded
`width` / `height` (EXIF `ExifImageWidth` / `ExifImageHeight`) — never swapped for the EXIF
Orientation or a user rotation. A photo with no recorded size gets no `AppliedToDimensions`
(there is no `1×1` stand-in).

**An `AppliedToDimensions` already in the sidecar is never rewritten** (#145): the regions
other tools wrote are normalized against it, so changing it would move them. ChairPhoto writes
`AppliedToDimensions` only into a `Regions` that has none, and otherwise puts its boxes into the
frame the sidecar declares (`region_target`; the import reads by the same rule):

| Declared size | Known orientation | ChairPhoto's boxes go in |
|---|---|---|
| none, or ChairPhoto's old `1×1` | any | the stored frame |
| the stored image's aspect (resized or not) | any | the stored frame |
| the aspect swapped | 5–8 (turned a quarter) | the display frame the size describes |
| anything else, or a size without a usable `w`/`h` | any | **refused** |
| any, with the photo's size unknown | 5–8 | **refused** — which frame is meant cannot be told |
| any | unknown | as they are |

A refused write fails with an error naming the sidecar and leaves it byte for byte as it was;
a refused frame imports nothing.

Each region in the `RegionList`
carries `mwg-rs:Name` (the person tag's leaf name), `mwg-rs:Type="Face"`, and an `mwg-rs:Area`
whose `x`/`y` are the rectangle's normalized **center** — MWG stores centers, not corners — with
`w`/`h` as the size. Stored bboxes are top-left-normalized, so the writer converts corner→center
and the reader converts back.

**Merge safety is binding.** The RegionList may already contain regions written by other tools.
The writer edits the existing `Regions` in place, never rebuilds it. **Every region ChairPhoto
writes carries its marker** (#135), and a catalog replaces or removes only regions carrying
*its own* marker.

**The marker format is stable** — it is on disk in users' sidecars, and changing it needs a
legacy rule of its own. It is a `chairphoto:FaceId` struct field
(`https://chairphoto.local/ns/1.0/`) of the region, whose value is

```
<catalog UUID>/<face id>
```

— the writing catalog's identity, exactly as `settings.catalog_uuid` holds it (a UUID v4,
lowercase and hyphenated, minted once on the catalog's first open; `catalog::CATALOG_UUID_KEY`),
a `/`, and the face's `faces__faces.id` in canonical decimal (no sign, no leading zero:
`/007` is not `/7`). Face ids are `AUTOINCREMENT`, so a catalog never reuses one. A region whose marker names another catalog — a second catalog over the same
folders, or this catalog's predecessor before a rebuild — or whose value is anything but exactly
this form is **foreign**: never removed, never re-marked.

A copy of a catalog file — a sync between two machines, a restored backup — shares the
original's identity *and* its face-id counter, so the two copies write the same markers for
different faces. A marker is therefore this catalog's only for a **face it knows on that
photo**: one in the set being written, or one of the photo's faces that has left it (rejected,
ignored, unnamed). A marker with this catalog's identity but a face id it does not know on that
photo came from a copy and is foreign like any other (review N1). And a region of a face that is
still in the set but no longer recognisably that face (step 1) is kept as it is, not removed.
Deleting a drawn box writes the photo's regions first, while its id is still known.

Each write sends the photo's whole confirmed set, and for each existing region, in this order:

1. **This catalog's marker, with the id of a face in the set** (and still that face's name or
   place): moved to the face's box and renamed to its person. A renamed person or a
   re-detected box no longer leaves a stale copy behind.
2. **This catalog's marker, same Name and center within `AREA_EPSILON = 0.02` of a face in the
   set** (a face id that changed): taken over the same way.
3. **This catalog's marker, matched by nothing, for a face of this photo that has left the
   set:** removed. This is how a **rejected or ignored** face, or one whose person was removed,
   leaves the sidecar.
4. **Anything else — unmarked, or another catalog's:** foreign, and **always kept**. When its
   Name and center (within `AREA_EPSILON`) match a face in the set it is that face already in
   the file — a Lightroom region ingested earlier, say: only its `Area` coordinates
   (`stArea:x/y/w/h/unit`) are updated, in the form they are written in, and no marked copy is
   appended. Its marker (if any), `mwg-rs:Rotation`, `Type`, extensions and foreign attributes
   such as `digiKam:Confidence` stay, and it is never marked as ours, so rejecting the face
   later never removes it.

Each existing region is claimed by at most one face and each face claims at most one region;
where several regions match a face by Name + Area, the **closest** center wins, not the first in
the file (#147). The faces that claimed none are appended, marked. **When in doubt, the
region is preserved.** Foreign attributes and children of `Regions`, `AppliedToDimensions`
and the list survive. A write that changes nothing in the
regions (an empty set and nothing of ours, or the set as the file already has it) leaves the
sidecar untouched and creates none.

**Regions written before the marker existed** are recognised against the catalog's record of
what the old writer exported: `faces__legacy_regions`, taken once — when a catalog that already
has faces first opens the faces tables after the upgrade; the one call whose `faces__once` claim
row is new takes it, in the same savepoint, so two connections opening at once never take it
twice or bring back rows already spent — with the name and display-frame box
each face had then (the old writer did not convert frames). The old writer exported a photo's
whole confirmed set after every face verb on it and never otherwise, so the record holds the
confirmed, named faces it can tell were exported:

- a face a verb confirmed (accepted, assigned, named — every source but `seed` and `xmp`);
- an auto-seeded face (`seed`; the matching pass exported nothing) only on a photo a verb
  touched — a verb-confirmed face, an ignored face or a remembered rejection there;
- never a face confirmed from another tool's region (`xmp`).

Whether a write succeeded was never recorded (an offline photo's was skipped), and a seed made
after the verb is still counted, so the record can over-count. That is why only an unmarked
region in **exactly the old writer's shape** can be taken for its: `rdf:li
rdf:parseType="Resource"` holding exactly `mwg-rs:Name`, `mwg-rs:Type` = `Face` and an
`mwg-rs:Area rdf:parseType="Resource"` of exactly `stArea:x/y/w/h` and `stArea:unit` =
`normalized`, all as plain elements, with no other attribute, field or child. A region another
tool wrote at the same place under the same name — digiKam's nested `rdf:Description` with
`digiKam:Confidence`, a Lightroom region with `mwg-rs:Rotation` — has another shape and stays
foreign. Such an old-shaped region matching a recorded face by Name + Area is adopted (moved and
marked) while the face is in the set, and removed once it is not. A photo's record is spent by
its first write that reaches the sidecar, so a region another tool writes later under the same
name and place is never taken for ours. Pre-marker regions of a face rejected *before* the
upgrade are not on the record and stay — the catalog no longer knows they were ours.

The old writer wrote display-frame boxes, so until a photo's pre-marker regions are converted
another tool — or a later import, by a rebuilt or second catalog — reads them in the wrong place
on a turned photo. **Every face index run converts them first** (`convert_legacy_regions`,
before it indexes anything): it writes each photo still on the record through the normal region
write, which adopts and marks (or removes) the old regions in the stored frame and spends the
record. It shares the index job's abort flag, ownership and catalog connection, and the record
is its queue: an abort, an offline original or a failed write leaves that photo's rows for the
next run, and rows of photos no longer in the catalog are dropped. A photo whose sidecar
*refuses* the write for its own layout or frame — a `Regions` this writer cannot read, or an
`AppliedToDimensions` of another frame — is tried once and then set aside (`faces__legacy_refused`,
counted as `refused`): the same write would be refused on every run until the file changes. It
keeps its record, so a face verb that later writes the photo still adopts or removes its old
regions, and that write clears the refusal. The unprefixed `parseType`, `about` and MWG
struct fields that a pre-#138 build's xmltree left behind are no longer refused. Every sidecar
write restores them first, when it can do so unambiguously, and backs the file up (#143; see
`docs/storage-and-import.md`, "Sidecars damaged by releases before #138"). A photo set aside
for that damage by an earlier build stays set aside until a face verb writes it. Its progress goes out through
the index job's own status and `faces:progress` — photos converted of photos to convert, `0/n`
to `n/n` — before the index's own count starts again from `0`; there is no phase label, which
would need a new field in both front ends. It logs a summary of what it did. It is part of the index
job rather than a job of its own to keep this branch small: the record is a one-time backlog,
indexing is the faces job a user runs, and it already owns the faces worker. A catalog that is
never indexed again keeps its record until a face verb touches each photo.

A sidecar may be rooted at `x:xmpmeta` or, as the XMP spec allows, at a bare `rdf:RDF`, which
is read and written in place (#147); a file rooted at anything else is not written. The writer
and reader accept `Regions` in any top-level `rdf:Description`, struct values written
with `rdf:parseType="Resource"`, as a nested `rdf:Description` or as attributes, and a list in an
`rdf:Bag` or `rdf:Seq`. A `Regions` in any other layout (two of them, another container, a
reference) is **not written**: the write fails with an error and the sidecar is left as it was,
because a write that cannot see a foreign region would delete it.

**Writes** fire from the same hooks as keyword export — `faces_accept`, `faces_accept_person`,
`faces_assign`, `faces_reject`, `faces_ignore`, `faces_name_cluster` — each writing the photo's
full current confirmed set right after the verb, so the sidecar stays in sync. The batch confirm
writes only the photos it actually changed, after its transaction commits: a sidecar that cannot
be written (offline volume) must not roll back a confirmation the catalog already recorded. An
offline photo's write is skipped, not queued: its sidecar catches up at the next face write for
that photo.

**Reads** happen during indexing: existing `mwg-rs:Regions` are parsed and IoU-matched
(≥ 0.5, greedy best-first, one-to-one) against the photo's still-unassigned detections. A named
match confirms the face (`state = 'confirmed'`, `source = 'xmp'`), finding or creating the person
tag under `<people_root>/<region name>`. Labels written by digiKam, Lightroom, Picasa or a
previous ChairPhoto run are therefore ingested for free.

## Interface

- **Loupe overlay** — face rectangles on the photo, each with a chip showing the assigned or
  suggested name and confirm / reject / reassign / ignore actions.
- **Inspector panel** — the active photo's face list with the same per-face actions. With more
  than one photo selected, a suggested face also offers *confirm on N*: `faces_accept_person`
  confirms that person across the whole selection in one transaction. It **accepts suggestions,
  it never creates them** — a photo where the matcher never suggested that person is reported
  back, not force-assigned, because inventing a face region for a person the detector never
  proposed there would fabricate data. The result is reported by bucket (confirmed /
  already confirmed / had no suggestion), so a count never claims more than happened.
- **People view** — a wall of named people with face crops as avatars and photo counts, plus
  unnamed clusters waiting to be named. Clicking a person filters to their photos, and a review
  queue supports bulk confirmation. A review verdict applies only while the face is still
  suggested as the person the queue showed, so a list read before a re-run of matching never
  confirms someone the user did not see. Clusters can be named together as one person (a merge),
  and a cluster's faces can be named apart or ignored (a split); only faces still pending a
  decision change, and a cluster that a matching run has since regrouped names nothing (cluster
  ids are never reused). Clusters are rebuilt from scratch by every matching run, so merging or
  splitting is done by *naming* — the only durable form. In the GPUI app these writes wait while
  a matching run is going, as UX only: a run regroups the clusters the view shows. The guarantee
  is in the core — every seed, suggestion and cluster write of the matcher re-checks in its own
  `UPDATE` that the face is still undecided, so a confirm, naming, assignment or ignore made
  during a run (from any view, or the Tauri UI) is never overwritten.
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
