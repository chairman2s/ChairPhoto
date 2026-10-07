# Recognising the same photo across machines and cards (discussion)

**Status: discussion, not scheduled.** Owner and Claude, 2026-10-04. Nothing here is built.
Related: `docs/plans/laptop-checkout.md`, #248, #249, #247.

## 1. A content-derived identity

**Idea (owner):** a photo's identity should come out the same wherever it is imported, so a
card imported on five machines that know nothing of each other ends up with one identity.

**How:** UUID v5 under a fixed ChairPhoto namespace, over the SHA-256 of the original file's
bytes. Deterministic, so every machine computes the same value. ChairPhoto already does this
for re-minted legacy identities (`catalog::legacy_photo_identity`, `LEGACY_IDENTITY_NAMESPACE`).

**Why the bytes:** originals are never modified (AGENTS.md), so the hash is stable for the
photo's life. Card ingest already reads every byte while copying, so hashing there is nearly
free; in-place import needs one read; backup verification already computes SHA-256
(`photo_locations.verified_hash`). Capture time + serial alone would fail for cameras without
a serial, phones and screenshots.

**What it would solve:** #249 (a card imported on both machines) disappears; #183/#185-style
duplicates become a second location of one photo; the laptop round trip no longer depends on
an identity surviving the trip.

**Catches:**
- Existing photos have random v4 identities already written to sidecars and used as merge
  keys; rewriting them is a large, risky migration. Safer: keep every identity, add a content
  fingerprint per photo, and let merge and bundle import match by UUID first, then by
  fingerprint. New imports could later derive their UUID from it.
- Software that writes into originals (some tools edit JPEG/DNG metadata in place) changes the
  hash, so such a file looks new elsewhere. ChairPhoto itself never does this.
- Two byte-identical copies on one machine get one identity; the second becomes a location of
  the same photo, which changes today's duplicate handling.
- AGENTS.md's "UUID v4 on first import" rule would change, and the namespace could never
  change once used.

**Leaning:** start with the fingerprint as a second match key (low risk); derived UUIDs later,
if at all.

## 2. RAW on one card, camera JPEG on the other

**Requirement (owner):** the camera writes RAW to one card and JPEG to the other. The JPEG card
is rarely imported, but when it is, ChairPhoto must understand each JPEG is the same photo as
a RAW already in the library.

**What exists today (read from the code, not tested for this two-card case):**
`Catalog::pair_raw_jpeg_stacks` (`catalog/mod.rs`) stacks a JPEG under the RAW with the same
folder and the same file stem, case-insensitive, and runs after each scan
(`scanner/mod.rs`), after bundle import and in the v18 migration. Card ingest files by EXIF
capture date into `YYYY/MM/DD` and keeps camera file names, so a camera's `DSC01234.JPG`
normally lands beside `DSC01234.ARW` and is stacked under it.

**Gaps to decide:**
- The JPEG still becomes its own row with its own UUID, import batch and backup; it is hidden
  under the RAW, not recognised as "already imported". The import dialog shows it as new.
- Pairing needs the same folder *and* stem. It breaks if the camera numbers the two cards
  differently, files were renamed, or the date folder differs (no EXIF date → file mtime).
  Matching by capture time (with sub-second) + camera serial, as #246 already does, would be
  more robust.
- Culling/tags/IPTC live on the RAW; should the JPEG inherit them, or stay a hidden
  derivative?
- Options: (a) keep importing it as a stacked derivative but show it as "pairs with an
  existing RAW" in the dialog and pair by capture + serial; (b) record it as a companion /
  alternate rendition of the RAW's photo instead of a separate photo; (c) offer to skip
  JPEGs whose RAW is already imported.

## 3. The camera JPEG as the preview source (undecided idea)

**Idea (owner, not decided):** when the camera's own JPEG of a RAW has been imported, use it
for that photo's thumbnail and preview instead of the small JPEG embedded in the RAW.

**Why:** it is full resolution with the camera's own colours and picture style (closer to what
was seen on the camera), it already exists, and it decodes fast — a cold loupe open would
not wait for RAW work.

**Conditions it would need:**
- **Edits win.** Only for photos with no Develop edit; an edited photo shows its render.
- **Same framing.** Only when the JPEG's aspect matches the RAW's (an in-camera 16:9 crop of
  a 3:2 RAW must not be used). Face boxes are stored in the preview's frame and the #154
  cross-check compares aspects, so a different shape would refuse or misplace face regions.
  Matching capture time and serial is not enough on its own.
- **A different look.** In-camera DRO/HDR/creative styles can make the JPEG differ from the
  RAW; possibly a preference ("use camera JPEG for previews when available").
- **Offline.** A JPEG only on an unmounted NAS falls back to the RAW's embedded preview.
- **Reliable pairing first.** Depends on §2's pairing by capture time + serial, and fits its
  option (b): the JPEG as another rendition of the RAW's photo, preferred for previews while
  the photo is unedited.
