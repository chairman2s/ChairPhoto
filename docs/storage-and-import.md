---
title: "Storage, Import & Sync"
description: "Volumes, import, backup, catalog merge and the reconcile queue."
tags:
  - chairphoto/core
  - chairphoto/library
aliases:
  - "Storage"
  - "Import"
  - "Sync"
  - "Merge"
---

# Storage, Import & Sync

ChairPhoto manages a RAW library that physically lives in more than one place — a NAS
archive, a fast local disk, a memory card, a travel laptop — while presenting it as one
catalog. This document covers how photos are located, how they move between tiers, how
machines stay in sync, and how data is exported.

## Purpose

chairphoto must manage a large RAW library that physically lives in more than one
place — a NAS archive, a fast local working disk, a memory card, a travel laptop —
while presenting it as one coherent catalog. This doc defines how photos are
located, how they move between tiers, how machines stay in sync, and how data is
exported. The overriding goal is **never lose an original**, while keeping culling
and tagging fast regardless of where the files are (or whether the NAS is even
reachable).

## Core idea: logical identity vs physical location

A photo's **identity** is its UUID (already implemented: assigned at first import,
written to the catalog and the XMP sidecar). A photo's **locations** are physical
instances of that identity. One photo (one UUID) may exist simultaneously as:

- the original on the **NAS** (canonical archive),
- a fast **local** working copy,
- (transiently) the file on a **memory card**,
- an outbound **export** copy (laptop / hand-off).

So the model is **"a photo has a set of locations, each with a role,"** not "a photo
has a path." All path resolution goes through a **resolver** that returns the best
*available* copy. This is core, because everything — the image protocol, editor
launch, export — asks "where is this file?"

### Identity that fails to reach the disk

The catalog half of the identity cannot fail; the disk half can. Storage is mounted
read-only, an existing sidecar does not parse, the volume goes offline mid-scan, or
the sidecar already carries a *different* `xmp:Identifier` that must not be
overwritten (XMP safety: when uncertain, preserve). A row whose sidecar never
received the UUID has lost its portable identity — a merge or a re-import can no
longer recognise the file, and the catalog is then the only copy of that fact.

So the operation is **upsert (or relocate) the row AND bind the sidecar identity, or
record a retryable repair** — never "log it and continue". `catalog/identity.rs`
owns it: `bind_sidecar_identity` is pure filesystem work (safe off the catalog lock),
`record_sidecar_identity` writes the outcome, and everything that creates or re-points
a row — the local scan, the NAS in-place scan, card ingest, the bundle indexer,
`relocate_photo` — goes through `ensure_sidecar_identity`. Failures land in
`pending_sidecar_identity` with the reason and an attempt count.

`repair_pending_identity` retries the queue (planned under the lock, sidecar IO off
it, recorded under it) and clears what succeeds. Unreachable files stay queued — an
unmounted volume is a normal state. So does an identity **conflict**: the file keeps
the identifier it has, and the divergence stays visible for a human rather than being
resolved by clobbering somebody else's identity. A sidecar failure never aborts a
scan: one unwritable file must not cost the user the other 99,999 rows.

### How a sidecar is written, and when it is refused

Every sidecar write (`xmp::document::SidecarDocument`, issue #149) reads the file, changes
only what that writer owns, and replaces the whole file at once: a temp file in the same
folder (a dotfile ending `.chairphoto-tmp`, unique to that write), synced, then renamed over
the sidecar. A reader, another tool or a crash sees the old sidecar or the new one, never a
mix. A temp file left by a write killed before its rename stays until it is a day old; the
next successful write to its folder after that has it removed, on a background thread (each
folder is checked once per run, only regular files with names of that exact pattern are
touched, and a folder ChairPhoto may write but not list is never checked). Writers of one
sidecar in one process take turns, and an IPTC save and a geocode fill store and write in
one order. A write is **refused, leaving the sidecar as it was**, when:

- the original or its folder is missing — an unmounted volume; no folder is ever created.
  On Unix the original's folder is held open from the read to the rename, and the temp file
  is created and renamed through that handle, so a volume that goes away after the last
  check cannot redirect the write into the empty mount point (#155);
- the sidecar exists but this process may not write it (permissions, ownership, ACL);
- **the folder is not writable**, even if the sidecar itself is (a share that grants
  modify but not create, a mount ACL). The rename needs a new file in the folder, and
  ChairPhoto keeps failing safe here rather than falling back to a non-atomic in-place
  write (decided in the #149 review, F3).

A refused write stays as debt, not as a silent success. An identity or import-batch field
lands in `pending_sidecar_identity` and the repair pass retries it once the folder is
writable. A refused IPTC write after the catalog stored the values leaves those fields owed
in `pending_sidecar_iptc`; the next save or the repair pass writes them (see IPTC that fails
to reach the disk). The save reports the sidecar pending, a single-photo geocode returns an
error saying so, and geocode-all does not count that photo as filled.
Face-region and GPS writes log the failure, and the catalog stays authoritative.

### Sidecars damaged by releases before #138

Every release before #138 parsed and wrote sidecars with `xmltree` 0.11, which drops attribute
prefixes. Each write it made turned `rdf:parseType="Resource"` into `parseType="Resource"`,
turned `rdf:about` into both an unprefixed `about` (with the original value) and an empty
`rdf:about=""` (the writer re-inserted one after parsing), and an attribute-form MWG `AppliedToDimensions` or `Area`
(`stDim:w`, `stArea:x`, …) into no-namespace `w`, `x`, …. The same happened to every other
prefixed attribute in the file, foreign ones included (`digiKam:Confidence`). On such a file
the face-region writer refuses every write ("mwg-rs:Regions is not a struct") and the reader
finds no regions.

When a writer opens a sidecar that carries `chairphoto:LastWrite`, which means a ChairPhoto
release wrote the damage, `SidecarDocument::open` restores those known attributes in memory
before the writer runs (`xmp/repair.rs`, #143). The table below lists every attribute it
restores; nothing else is touched:

| Unprefixed | On | Restored as |
|---|---|---|
| `about` | `rdf:Description` | `rdf:about`, replacing an empty or equal `rdf:about` |
| `parseType` | a property element or `rdf:li` | `rdf:parseType` |
| `w`, `h`, `unit` | `mwg-rs:AppliedToDimensions` | `stDim:` |
| `x`, `y`, `w`, `h`, `unit` | `mwg-rs:Area` | `stArea:` |
| `Name`, `Type`, `Rotation` | a region's nested `rdf:Description` (digiKam's form) | `mwg-rs:` |
| `lang` | an `rdf:li` of an `rdf:Alt` | `xml:lang` |

The first two are how RDF/XML itself reads an unqualified `about` or `parseType`. The MWG
fields have no other meaning on those elements, and a Lang Alt item's `lang` can only be
`xml:lang`. The last two rows are skipped, rather than treated as ambiguous, where the prefixed
counterpart is already there. Attributes whose namespace is lost for good,
such as `Confidence`, stay as they are. The repair runs only when it is unambiguous. If any
element already carries the attribute that would be restored (`parseType` beside
`rdf:parseType`, `x` beside `stArea:x`, `about` beside a different non-empty `rdf:about`),
nothing is repaired and the write is refused as
before. A repair counts as a first write for the backup rule: the damaged file is copied to
`<sidecar>.chairphoto-backup` first, unless a backup already exists, and an existing backup is
never replaced. Readers (face regions, GPS, identifier, IPTC presence) apply the same repair in
memory to a sidecar with `chairphoto:LastWrite`, writing nothing and taking no backup, so face
import reads the regions of a damaged sidecar before any write. The file itself is healed by the
first ChairPhoto write to it.

### The repair pass is a job

The queue reached 74,488 rows on the 100k harness shape, and every row is a sidecar parse
and rewrite — a network round trip each on a NAS. So the pass is a background job like face
indexing or Smart Tagging (`JobRegistry::identity`, `commands/jobs.rs`): the command returns
a **job id**, progress arrives as `identity:repair_progress`, the result as
`identity:repair_done`, `identity_repair_cancel` stops it before its next copy, and
`identity_repair_status` lets the debt panel re-attach after a remount. A newer pass or a
catalog switch trips it, so no pass outlives the catalog it was started against.

It plans the queue **one keyset page at a time**, ordered by the queue's primary key. Keyset
rather than `LIMIT/OFFSET` because the pass deletes the rows it binds: an offset window
slides left under every page turn and skips exactly as many rows as the previous page
repaired.

**Who owns a queue row.** The pass and `resolve_identity_conflict` write the same rows from
two connections. The rule is that the pass owns a row only while the row is still the one it
planned:

- each plan carries the row's version (`attempts`, `last_attempt_at`);
- the value *and* version are re-read immediately before the sidecar IO, so a page-old plan
  never acts on data a resolution has changed, and a row resolved or dismissed since is
  skipped without the file being touched;
- the record is a compare-and-set on that version and on `dismissed_at = 0`, and the pass
  only ever UPDATEs or DELETEs — never INSERTs, so a retry that cannot find its row has
  nothing to say.

A row somebody else decided is counted `superseded` and reported separately: it is not a
failure, and it is not an outcome the pass produced. Re-reading the *value* is the part no
compare-and-set can replace — Adopt rewrites `photos.uuid` and leaves the photo's other
copies' queue rows untouched when they carry no identifier, so a stale plan would write the
photo's previous identity into one of those sidecars.

**A locked catalog is waited out, not fatal** (#182). Another connection can hold the write
lock longer than the 5 s busy timeout — the bundle importer keeps one transaction open across
its whole index phase. A catalog step of the pass (or of the bulk resolution below) that meets
such a lock is retried after growing pauses (`catalog/busy.rs`, about 20 s in all, cut short
by the abort flag). Only the catalog step is retried, never the sidecar IO before it, so the
record is the same compare-and-set the first attempt would have made. A row still locked after
that is left exactly as it was — still queued, or its IPTC still owed — counted `busy`, and
the pass carries on with the next. Its sidecar may already hold what the pass wrote; the next
pass reads that and records it bound. Only a page read that stays locked ends the pass, and
under WAL a read does not wait for a writer.

A resolution deliberately does **not** stop a running pass. Killing a 74k-row pass because
one row got a decision is a worse trade than dropping that row's result, and the per-row
ownership above is what makes coexistence safe.

### IPTC that fails to reach the disk

Authored IPTC has the same two halves, but not the same shape of debt. An IPTC save writes
only the managed fields whose catalog value changed (#144: a field the catalog never changed
keeps whatever another tool wrote), and it writes after the catalog commit. So a sidecar
write that fails after the store — read-only storage, an unparseable sidecar, a volume
unmounting mid-save — would leave values in the catalog that no later write carries: a re-save
of the same values changes nothing, and the next save writes only its own change.

The debt is therefore a **set of fields per photo**, in `pending_sidecar_iptc`
(`catalog/iptc_owed.rs`, #148):

- **Owed in the store.** `Catalog::set_iptc` ORs the fields it changed into the photo's
  `owed` mask and bumps its `generation`, in the transaction (a savepoint) that stores the
  values. It is the only way to change an existing row's IPTC, so no store can skip it.
- **Written as owed ∪ changed.** `set_iptc` returns the write that pays everything owed,
  with the catalog's values now: a cleared field is removed from the sidecar, a field never
  owed is not touched. The path is resolved before the store, so an unreachable original
  still fails the save closed with nothing stored. A volume that goes away after the store
  fails the write (the sidecar document refuses a missing original or folder, #149), so the
  fields stay owed.
- **Cleared by compare-and-set.** `settle_iptc_write` clears the mask only while
  `generation` is still the one the write read, and only for the photo with that UUID. A
  newer store keeps its fields owed. A superseded write that *succeeded* owes again each
  field it wrote with a value the catalog no longer holds: those older values may have
  reached the disk after the newer write's, and only another write can prove otherwise. A
  field it wrote with the current value is right whichever write landed last, so it is not
  owed again, and when nothing is left owed the settle reports written. Rows stay at `owed = 0` rather than being deleted, so
  a photo's generation only grows.
- **Retried by the repair pass.** After the identity queue, `run_identity_repair` drains the
  photos owing IPTC under the same job, abort flag and progress (`iptcWritten`,
  `iptcUnreachable`, `iptcFailed` in its summary; `iptcOwed` in the panel's summary). The
  per-photo record writes the sidecar of the copy the resolver picks, as a save does. The
  title-bar "identity debt" chip and menu badge count copies owing identity plus photos owing
  IPTC, and the panel's Start is enabled by either.
- **Listed, dismissed, retried one at a time (#153).** The identity-debt panel lists the
  photos owing IPTC (`list_owed_iptc_page`: id, UUID, path, owed fields, last error,
  generation; paged) with **Retry** and **Dismiss** per row (`app::iptc_owed`). Retry writes the
  photo's owed fields as a save does: it takes the sidecar's write turn, reads what is owed
  now with the turn held, and writes and settles through the save's compare-and-set; an
  unreachable original is recorded on the row and stays owed. Dismiss clears the owed set
  without writing — for a photo kept on read-only media — by compare-and-set on the UUID and
  generation the row was read with, so a store since then (whose debt the user has not seen)
  or another photo that took the id is never dismissed. The catalog keeps its values; a later
  save owes only what it changes. Both are bound to the photo's UUID and to the catalog
  their row was read from (`with_catalog_as`): the GPUI panel binds them to the catalog of
  the page it drew, and the identity queue's `list_pending_identity` /
  `summarize_pending_identity` / `resolve_identity_conflict` are bound the same way,
  closing on `catalog:switched`. A byte copy of the catalog has the same ids, UUIDs and
  generations; only the catalog identity tells them apart. Each re-reads the panel's and
  the title bar's counts.
- **Reported honestly.** A save that reached only the catalog answers `pending` with the
  reason (`IptcSaveOutcome`, returned by `Catalog::set_iptc` and shown by both
  inspectors as "Saved to catalog; sidecar pending (…)"); `unchanged` when nothing was owed.
- **Bundle import** stores each new photo's manifest IPTC through `set_iptc_carried`, which
  owes only the fields the sidecar beside the extracted file has no value for. A value that
  sidecar does carry may be another tool's edit the source catalog never imported, so the
  import leaves it, as a save would (#144). The importer writes the owed fields right after
  its transaction commits, and anything that fails stays owed; a sidecar that does not parse
  owes every field. A catalog merge's insert of
  a new row (`catalog/merge.rs`, typically a metadata-only photo with no original here)
  carries the source's IPTC without owing it.

No migration backfills the table: which writes failed before v25 is unknown, and owing every
photo's IPTC would rewrite every sidecar in the library on the next pass.

### Resolving a conflict

A conflict is not repairable by retrying, so it needs a person. The decision is made per
**copy** — resolving one copy says nothing about the same photo's other copies — through
`Catalog::resolve_identity_conflict` (`resolve_identity_conflict` over IPC, the identity
debt panel in the UI). There is no default: an action is required, and the three outcomes
are CONTEXT.md § Identity's, verbatim.

| Decision | Changes the catalog | Changes the file |
|---|---|---|
| **Adopt** — the catalog takes the identity in the sidecar | `photos.uuid` | never |
| **Overwrite** — the sidecar takes the catalog's identity | no | `xmp:Identifier`, after a backup |
| **Dismiss** — stop retrying this copy | `dismissed_at` only | no |

Dismiss (undone by Restore) keeps the queue row for the record while taking it out of the
repair pass and out of the debt count. That is what lets a catalog whose only remaining
debts are conflicts reach a clean terminal state instead of reporting the same
unresolvable copies on every pass forever.

Both file-reading decisions re-read the sidecar at decision time rather than trusting the
queue row's recorded reason, which describes the last attempt, not the file now.

**Adopt is refused if another photo already holds that identity**, naming the photo that
does. `photos.uuid` is `UNIQUE`, so the write would fail regardless — but as an opaque
constraint error, and resolving one conflict must not manufacture another.

**Only a UUID is an identity.** A sidecar `xmp:Identifier` that is not a non-nil UUID in the
hyphenated form (`catalog::is_photo_identity`) belongs to another tool: a DAM asset id, say,
which several files may share. A scan or bundle import never adopts it as `photos.uuid` or
uses it to re-home a row; the file gets its own minted UUID, and the foreign value stays in
the sidecar as a conflict. Adopt refuses it; Overwrite (after the backup) or Dismiss resolve
it. A value spelled as a URI (`urn:uuid:<uuid>`) is foreign for the same reason —
`is_photo_identity` requires the bare hyphenated form — so a bulk Overwrite replaces it like
any other single non-UUID value, after the same backup (#222 N2); ChairPhoto itself never
writes that spelling.

**Non-UUID conflicts can be resolved in bulk** (#150). A DAM-managed library can hold them by
the thousand — schema v23 queues every re-minted row's copies — so Overwrite and Dismiss
also run over all of them at once as a job (`Catalog::run_resolve_foreign_conflicts`, owned
by `JobRegistry::identity_resolve` through `app::identity::claim_resolve_foreign_conflicts`;
over IPC `resolve_foreign_identity_conflicts`, `identity_resolve_cancel`,
`identity_resolve_status`, with `identity:resolve_progress` and the terminal
`identity:resolve_done`). It acts only on un-dismissed copies whose recorded conflict is a
non-UUID, and decides each exactly as a single resolution does: the queue row is re-read,
Overwrite re-reads the sidecar and backs it up first, and a copy that is no longer a non-UUID
conflict — resolved meanwhile, its sidecar now carrying a UUID or nothing, its file
unreachable — is skipped, never acted on. A UUID conflict names another photo's identity and
is never resolved in bulk. Overwrite replaces every `xmp:Identifier` value while a conflict
records only the first, so a bulk Overwrite also skips a sidecar holding more than one value
(a Bag a DAM appended to, a second `rdf:Description`): another photo's UUID may sit beside the
DAM id (`xmp::read_identifiers`). The run is its own job family, so it neither stops nor is stopped
by a repair pass (each queue row has an owner); a newer run, Cancel or a catalog switch stops
it before its next copy, a copy whose record stays locked through every retry is left queued
and counted `busy` (#182; an Overwrite counted there may already have written the sidecar, which
a later run skips and the repair pass binds), and a front end's start is bound to the catalog it read
(`CATALOG_CHANGED` otherwise). The GPUI identity-debt panel does not offer it yet.

Before #141 a scan did adopt such a value, so an older catalog can hold rows whose
`photos.uuid` is a DAM id. Schema v23 (#146) re-mints each of them, keeps the old value in
`photo_legacy_identifiers`, and queues every copy it records as a conflict; it writes no
sidecar. Until that conflict is resolved the sidecar's foreign value is the file's only link
to its row, so a scan re-homes a moved file onto the row holding it as a legacy identifier —
only when no other row holds it and every primary copy the row records is gone (present
storage, no file; an unmounted volume does not count). A backup, cache or export copy still in
place does not hold it back: it is the same row's copy, not evidence of another photo. The
scanned file must also be the row's size: an offloaded photo has no primary copy left to be
"gone", and an export or derivative carrying the same DAM id must not take it over, while an
original is never modified and keeps its size wherever it is moved or restored.
Otherwise the file is a different photo and gets its own row, as above.

This size guard is a deliberate trade-off (#146), not a loss: a file another tool rewrites in
place — a DAM or digiKam writing metadata into a JPEG or DNG, changing its byte size — and
that is then moved before the next scan no longer comes home, because its size no longer
matches. It gets a new row instead, the pre-#146 duplicate, and nothing is deleted. Only a
modify-then-move *between* scans hits this: a rescan at the file's unchanged path refreshes
`photos.size` first, so the guard only ever sees it stale when the file has also moved.

A scan that mints a UUID for a file whose sidecar holds such a foreign value records it the
same way (#150), so a file catalogued after #141 re-homes under the same guards when it
moves. The value is recorded whichever way the scan matched the row, so a row catalogued
before #150 gains it at its next rescan. A legacy value has at most one owner: a scan, merge
or bundle import never records a value another row already holds — the first holder is
v23's re-mint or the first file seen carrying it, and a later one is a duplicate or another
file sharing a DAM id. Two owners would make every scan refuse both.

**A re-home never leaves a file behind** (#150). A scan or bundle import that finds a file
carrying an identity some row already holds re-homes that row onto the file only when the
row's own copy is gone: its primary location on the file's volume, and, for a file under the
catalog root, the file at its `photos.path`. If that copy is still in place, the new file is
another copy carrying the same identity — a bundle's copy at another relative path, a
duplicate made outside ChairPhoto, the other half of a v24 case collision — so it gets a row
of its own with a minted UUID, and its sidecar's identity is queued as a conflict for a
person (Overwrite or Dismiss). Re-homing it would leave the original with no row: the next
scan would catalogue it afresh without its ratings, tags and faces, or the two files would
take turns owning the row. A row whose primary copies are all on other volumes still gains a
file found on a volume indexed in place as another location; that moves nothing. A bundle
whose photo lands as such a separate copy puts none of its data on the copy's row (#185): its
culling, IPTC, edits, versions and tags go onto the existing row, which merge matches by
identity, the way a bundle photo already in the library does (see "Photo (existing)" below).
The copy's row is a file of this import (its batch, its queued backup) and nothing more.

"Still in place" is a question about the file, not the name (#184). On a case-insensitive
filesystem (APFS and HFS+ by default, exFAT and vfat drives, a casefold directory) the old
name of a case-only rename, `IMG.ARW` → `img.arw`, still opens the renamed file, so testing
the recorded path with `exists()` kept the file apart from its own row. The guard therefore
asks whether the recorded path opens the very file being scanned (same device and inode;
the canonical path off Unix) and, if it does, whether the recorded name — and each folder
name it does not share with the scanned path — is still in its folder's listing. A name the
filesystem only folds onto the renamed file is not, so the row re-homes; a second hard link
is, so it stays a copy of its own. An unreadable listing counts as listed. The legacy-identifier
re-home (`every_primary_copy_is_gone`) still tests `exists()`.

**A re-minted legacy identity is a UUID v5, not v4.** This is the one exception to "a UUID v4
on first import": a photo imported fresh still gets a random v4, but a value that already
served as a photo's identity is re-minted as `catalog::legacy_photo_identity` — UUID v5 of the
value under the fixed `catalog::LEGACY_IDENTITY_NAMESPACE`, which must never change. Two
catalogs that each held a photo as `dam:asset/1` migrate independently and must still agree on
its identity, because identity is the merge key and a bundle does not carry the legacy value; a
random v4 per catalog would split that photo in two at the next merge. For the same reason
merge and bundle import map an old bundle's non-UUID id through the same function
(`catalog::photo_identity_for`) and record it as the row's legacy identifier: no path stores a
non-UUID `photos.uuid` any more. An empty or whitespace-only value is no identity at all, not
a legacy one: v23 gives such a row a random v4, merge never matches it to another catalog's,
and no lookup finds it. Nor is a blank-uuid bundle photo matched to the photo at its path,
which can be a different one (#150): the importer renames a different-size collision to
` (n)`, so the user's own photo is what sits at the bundle's path. The importer tells merge
which row it indexed each such original into, and the bundle's tags land there. A blank-uuid
photo with no original in the bundle is inserted with a v4 if its path is free, and otherwise
skipped and counted (`MergeSummary::photos_skipped`). If v23 finds the v5 already held by another row (a migrated
catalog's bundle merged in first), the two rows claim one photo and it cannot tell which is
right, so that row gets a v4 and its copies stay queued as conflicts. The legacy value itself
is never a merge key or a deep-link target. Settings keyed by the photo's uuid (today only the
Obsidian module's note record, `obsidian.note.<uuid>`) move to the new identity in the same
transaction, in v23 and in v24 alike; Adopt does not move them. If the new identity already
has such a record, the one with content wins (#150): a blank record (what Forget leaves) is
replaced by the real one or dropped beside it, and two real records both stay — the new
key's is the one shown, the old one is logged — so no record with content is lost.

A UUID is one identity in either case. `photos.uuid` holds it lowercase, as ChairPhoto mints
it (`catalog::canonical_photo_identity`): a scan, a bundle import, a merge, Adopt and a deep
link all canonicalise before they store or look up, and schema v24 lowercased the rows an
older scan stored as the sidecar spelled them. A sidecar that spells the identity upper-case
is bound as it is and never rewritten for the case alone. If lowercasing a row would give it
another row's identity, v24 re-mints it instead and queues its copies as conflicts.

**Adopt changes what merge matches on.** Identity is the merge key (`merge_photo` looks up
`photos WHERE uuid = ?`), so a catalog that has already been merged or bundled elsewhere
holds the *previous* identity for that photo:

- A bundle already written keeps the old UUID. Re-merging it creates a SECOND row for the
  same photo rather than matching the adopted one; the two are then independent rows with
  independent state. Adopt is therefore right when the sidecar is authoritative (a photo
  re-imported into a fresh catalog, which is the case it exists for) and wrong as a way to
  "tidy up" a photo whose identity has already travelled.
- Any `chairphoto://<uuid>` deep link (Obsidian notes, the tag/photo links) still points at
  the old identity and stops resolving.
- The photo's other copies were bound to the old identity, so Adopt re-reads each of them
  (never writes) and re-queues the ones that now diverge, rather than leaving the catalog
  believing they are bound.

**Overwrite** goes through `xmp::overwrite_identifier`, the one writer permitted to destroy
an identifier it did not write. It preserves the sidecar first even when the file carries
`chairphoto:LastWrite` — which the ordinary backup-once rule would skip, and which is
exactly the case here, since a file duplicated after import carries both our stamp and
somebody else's identity. An existing backup is never replaced: the earliest state we ever
saw outranks the current one.

## Volumes (named storage locations)

Locations never store absolute paths. They reference a **named volume** + a relative
path. A volume (e.g. `NAS-Photos`, `LocalScratch`) has:

- a base path **per machine** (the NAS mounts at different points on desktop vs laptop),
- a runtime **reachability** state (mounted? online?).

Remapping the NAS to a new mount point updates one volume record, not every photo.
This is also what makes catalogs portable between machines.

## Storage lifecycle (Pattern C)

Photos move through states; transitions are **gated on NAS reachability**:

```
card → [LOCAL ONLY, not backed up]   ← only copy is on this disk; AT RISK
            │  (NAS reachable + verified copy made)
            ▼
       [BACKED UP]   = local original + verified NAS original
            │  (need local space; only allowed from BACKED UP)
            ▼
       [ARCHIVED]    = NAS original only; browsable via local cached preview
```

Existing NAS-resident photos (the current library) simply start at **ARCHIVED**.
A laptop with no NAS lives in the top state: imports land local, are fully usable,
and the backup transition is **deferred** until the NAS reappears.

### Per-photo status (derived from its locations)

- **Local only — not backed up** → at-risk; show a visible indicator
- **Backed up** → safe (local + verified NAS)
- **Archived** → NAS-only, browse via cached preview
- **Offline** → NAS-only *and* NAS unreachable → browse/cull/tag still work; edit/export blocked
- **Missing** → no known copy anywhere

`Catalog::photo_storage_status` (+ a batch `photo_storage_statuses` for the grid)
derives this from the photo's locations' volume kinds and reachability. A backup
*record* counts as backed-up even while the NAS is unmounted (reachability only
chooses Archived vs Offline when there's no local copy). The grid shows two icons
per tile — local (▣) and NAS (☁, dimmed when offline).

### A copy is the image plus its declared companions

A photo's edit state does not live in the image. darktable and Lightroom write develop
history into `<raw>.xmp`, RawTherapee into `.pp3`, ART into `.arp`, and RapidRAW into
`.rrdata` — all beside the original. Backup used to copy the image alone, so a photo could
be reported **BACKED UP** while every edit decision made on it existed in exactly one
place (issue #80).

The set is **declared, never guessed** (`crates/core/src/companions.rs`). An integration names
its extension; the catalog does not sweep arbitrary neighbouring files, because backup must
not behave differently depending on what happens to share a folder with the photo.

Each kind also declares what its *presence* means, which is a separate question from whether
it travels:

| | carried | presence implies an edit |
|---|---|---|
| `.xmp` | yes | only by content — chairphoto writes this file too |
| `.pp3`, `.arp` | yes | yes |
| `.rrdata` | yes | **no** — RapidRAW writes it when it *opens* a photo |

Both sidecar shapes are handled: appended (`DSC1.ARW.xmp`) and basename (`DSC1.xmp`,
darktable's alternate mode). Destinations are derived from the destination *image*, so a
copy whose `relative_path` differs between volumes still lands correctly rather than leaving
an orphan.

Three rules govern carrying:

- **Backup and restore carry companions** through the same hash-verified atomic rename the
  image uses, recording each in `photo_location_companions` against the exact
  `photo_locations.id` — not (photo, volume), since one volume can hold several roles for
  the same photo. What is recorded is the **source** mtime at carry time, which is the
  reference point for answering "has the local file moved on since we copied it".
- **Offload carries before it deletes.** Invariant 1 covers edit state too: freeing the
  local image must not strand the history beside it. Companions go home first, then the
  local ones are freed with the image, and `restore` brings them back.
- **Offload deletes only what home holds byte for byte (#255).** Re-hashing the backup
  proves home is intact, not that it holds what is here. So offload checks each local file
  **after moving it to a hidden name** in its folder (`.<name>.chairphoto-offload-<pid>-<n>`,
  #256): companions first, then the image, each re-hashed there — the image against the
  verified backup hash, a companion against the hash the carry confirmed at home — and
  freed only with exactly the companions the carry confirmed, never a fresh listing. Then
  it looks at every name it emptied once more, and only then deletes the hidden files. A
  write through the photo's name either landed before the move (the moved file holds it and
  its hash says so) or comes after it and makes a new file at that name (the last look finds
  it); what is deleted is the hidden file, which no other writer knows by name, so nothing
  written after its check is deleted. A local JPEG or DNG rewritten in place after its
  backup, a sidecar edited after the carry, a companion that appeared after it, or any new
  file at an emptied name refuses the photo: every moved file goes back under its name
  (never replacing a file there — one that cannot go back, because a new file took its name
  or the rename failed, is never deleted, confirmed or not: it stays under its hidden name
  beside the photo, and the refusal names it and why; where the filesystem has neither a no-replace rename nor
  hard links, a file goes back by a plain rename once its name is seen free), nothing is
  deleted, and a queued offload is kept `failed`
  with the reason ("changed since its backup — refusing to offload; the copy at home is the
  earlier version"). A crash between the move and the delete leaves the file under its
  hidden name; the next backup, offload or restore plan that looks in that folder (once per
  folder per run, off the catalog lock) puts every such file whose process is no longer
  running back under its name, again never replacing one — a file left by a crash is never
  deleted, since it may hold the only copy of a local change. An empty one is not put back
  (it is most likely a name the offload claimed and never filled; putting it back would make
  a 0-byte original); it is removed only when a non-empty file holds its name. A second
  operation in a folder whose sweep is still running waits for it to finish. The cost is one sequential
  read of each local copy, on a local disk, beside the NAS read of the backup offload
  already made; size or mtime would be cheaper and are not content checks (an in-place
  rewrite can keep the size, and `exiftool -P` keeps the mtime). Back up does not replace a
  verified backup that is present, so such a photo stays local until the owner decides how
  a changed original reaches home.
- **Divergence refuses; it never resolves.** A companion present on both sides with
  different contents is two unreconciled edits. Backup leaves it untouched and does not
  claim it as carried; offload refuses outright. Choosing a side would silently destroy
  work.

Carrying is idempotent — an identical file already at the destination is adopted rather
than rewritten — so a companion placed there by any other means is absorbed on the next
pass instead of being re-copied or causing a conflict.

**A sidecar backup is not a companion.** `<sidecar>.chairphoto-backup` — the copy the XMP
safety rule takes before ChairPhoto's first write — is **per copy** by construction: each
copy's sidecar had its own pre-ChairPhoto state, and the NAS copy already has its own. So
offload neither carries it (two backups for one photo is exactly what the divergence rule
refuses to offload over) nor deletes it (that would destroy the only record of the earlier
sidecar, during a routine space-freeing operation, for a few KB). It is left in place and
**reported**, so the one file left in an otherwise emptied folder is something the verb
said rather than something the user discovers (#82).

**Delete takes it, and reports it separately.** Emptying the trash removes the image, its
companions and the catalog row, so nothing is left for the backup to be the earlier state
*of* — and a file with no row and nothing beside it is the orphan the delete path already
refuses to create everywhere else. It is counted in `sidecar_backups_deleted` rather than in
`files_deleted`: a sidecar backup is not a companion, and folding it into the tally of
destroyed originals would inflate that number with a file the user never knew about (#84).

Order is part of the rule. The backups are taken **last**, only once every image and
companion this delete is responsible for is confirmed absent — a delete that failed on the
image leaves a photo that still exists, and the record of its earlier sidecar is then still
a record of something. A backup that cannot be removed is itself a failure and the photo
keeps its row: a few KB against every original already gone, but the row is what makes the
leftover findable and the delete retryable.

A backup **stranded by an earlier offload** — sitting where the local copy used to be, with
its location row dropped — is still in reach, because `photo_path_candidates` always ends
with the catalog-root path. That fallback, not the location rows, is what a later delete
walks to find it.

`verified_hash` deliberately stays a hash of the **image only**. The image is immutable, so
a changed hash means bit rot; companions are mutable by design (darktable rewrites `.xmp` on
every edit, and so does chairphoto on IPTC/GPS/face writes), so hashing them would report
ordinary work as corruption.

### Safety: a second axis

`StorageStatus` answers *can I display this photo now* and drives the grid badge, so it is
on the hot path and does not care whether anything was hash-verified. **`SafetyStatus`**
answers *would I lose it*, is read by one panel, and is deliberately a separate enum rather
than more variants on the first — overloading one would drag verification onto the grid's
hot path.

| bucket | meaning |
|---|---|
| `Missing` | no copy recorded anywhere |
| `AtRisk` | no copy at home |
| `Unverified` | a copy at home, never hash-verified |
| `Stale` | verified at home, but a companion has moved on locally |
| `Safe` | verified at home, companions carried and current |

A photo lands in the first bucket it matches. `Stale` exists only because a copy is the
image *plus* its companions: pixels safe at home with the develop history on one disk is
not safe, and without the bucket the summary would call issue #80's exact situation `Safe`.

**`AtRisk` is home-possession, not copy count** — see `CONTEXT.md`, which now says so in the
vocabulary. An `export` copy never counts toward home: it is a one-way hand-off, even when
the user pointed the export at a backup-kind disk.

**Every query on this axis is pure SQL.** No volume is stat-ed, so an unmounted NAS cannot
make the panel hang — the same reason `photo_statuses` moved its reachability check off the
catalog lock. That constraint is what forces freshness to be *recorded* rather than
measured on read: the scanner notes how each carried companion looks while it is already
walking that file (`note_companion_freshness`), and the summary reads the note. Only *other*
copies' rows are refreshed — a file cannot be evidence that it has not diverged from itself.

Two consequences the UI must carry rather than hide:

- **`Stale` is a floor, not a total**, while any carried companion has not been seen since
  it was carried. The summary reports that count alongside it.
- **The catalog speaks only for volumes it can see.** Redundancy inside a device, and any
  off-site backup, are invisible; a photo reported safe is safe as far as this catalog
  knows, and the panel says exactly that.

The counting query is one grouped pass over `photo_locations`, measured at 0.24 s against a
165,093-photo catalog versus 0.56 s for four correlated sub-selects per photo. The grid
filter needs a per-row `EXISTS` instead, so the rule has two spellings; they share what they
can and a test pins the rest, because a panel that reports one number and then lists a
different set is worse than either number alone.

### The key performance principle

**Cull / browse / tag run entirely off the local preview cache** (already built).
They never touch the original, so they are fast and work even when the NAS is
offline. **Only editing and export need the original**, fetched on demand. This is
what makes NAS slowness/absence largely invisible in daily use.

## Reconcile queue

Because actions can't run while the NAS is away, pending operations are queued and
drained when the NAS volume is detected:

- `backup(photo)` — copy local → NAS, hash-verify, mark Backed up
- `offload(photo)` — only after verified backup; frees local space. It first keeps an
  **offline thumbnail** of each frame so the grid still shows the photo while home is away
  (`thumbnails::ensure_persistent_thumb`; every thumbnail rendered from a reachable original
  refreshes it too). The file is keyed by the catalog's UUID and the photo's UUID —
  `<cache>/chairphoto/persist-v2/<catalog uuid>/<photo uuid>.jpg` — never by photo id, which
  another catalog reuses for another photo, and never by the per-open `CatalogIdentity`, which
  a restart changes (#258). The id-keyed files before #258 (`persist/<id>.jpg`) are migrated
  once, to the catalog opened at start-up (`thumbnails::adopt_id_keyed_thumbs`, off the UI
  thread): each is copied to that catalog's photo of the id unless the photo already has its
  own, and `persist/` is removed only once every copy has landed (a failed or interrupted run
  leaves it for the next start). Nothing records which catalog wrote a file, so another
  catalog's photo can be adopted — the same tile the old layout showed — until the next
  render of the reachable original replaces it. An offload whose thumbnail keys cannot be
  read fails before deleting anything. After that migration, each start cleans the store up
  for the start-up catalog (`thumbnails::prune_offline_thumbs`, off the UI thread, review of
  #258 N2): that catalog's files for photos it no longer has (removed, or re-minted under a
  new UUID) once they are 30 days old, and the directories of other catalogs not opened for
  a year (each start and each catalog switch marks that catalog's directory with `.opened`;
  a directory without one goes by its newest file). Conservative on purpose: an archive
  catalog opened once a year with every original on an unmounted NAS has nothing else to
  show. For the same reason the first kind is skipped when another catalog in the
  recent-catalogs list shares the open one's UUID (a copy of the file shares it, and the
  photos the copy dropped are the original's) or cannot be read
  (`app::catalogs::orphans_are_its_own`). Never through a symlink — the `.opened` marker
  included — and only names the store writes.
- `restore(photo)` — pull an archived original back to local (e.g. to edit it)

**The ops and verification**: `catalog/lifecycle.rs` + the `app/storage.rs` service bodies
(`backup_photo_as` / `offload_photo_as` / `restore_photo_as`, run on a worker). SHA-256 (`photo_locations.verified_hash`,
schema v10); each op is plan → pure file IO off-thread → record-under-lock, and the plan
itself is split like the path resolver (#85): candidate rows are gathered in pure SQL
under the catalog lock and their existence is statted off it, so a NAS copy never blocks
the UI — and a slow or unmounted NAS never holds the catalog lock while a plan checks it.
The backup target is the single backup volume and
restore lands on the single local volume (multi-volume selection is future). The `pending_operations`
queue and automatic draining on NAS reappearance are not implemented; the ops are
invoked directly per photo.

Reconcile is **prompt-then-go**: surface "240 photos from Trip 2026-06 aren't backed
up — back up now?" rather than acting silently. (Decision.)

`reconcile_now` drains the queue op-by-op via the plan→IO→record
split (off the UI thread). Trigger (owner decision): **on app launch + window focus** —
when a backup volume is reachable and ops are pending, the drain runs **in the
background** (non-blocking; a status line shows "Backing up N to NAS…", a topbar
"Back up (N)" indicator shows the count and triggers it manually). The earlier blocking
"prompt-then-go" dialog was dropped — it looped on the focus that dismissing it caused,
and the owner asked for silent background backup. Backup entry (owner decision):
**both** — every imported photo is auto-enqueued, and a manual per-photo "Back up"
(run-or-queue) sits in the inspector alongside Offload / Restore (shown by status).

## Trash and delete

**Trash** hides a photo everywhere, reversibly, without touching a byte. It is
`photos.trashed_at` plus one predicate in `photos_visible` — which is the whole of "every
query must learn about trash", because everything that lists or counts photos for the user
already reads that view. The grid, album counts, library stats, tag counts and the safety
summary all pick it up at once.

It is **not** `pick_state = 'reject'`: reject is a verdict on a photo that stays in the
library and keeps appearing, while trash hides it. Two-pass culling uses one, deletion
flows use the other. It is **not** a sidecar field either — no user culling state reaches
an in-library sidecar today, so trash would be the first, and would mark the file trashed
for every foreign tool that reads that sidecar on the strength of a reversible local
decision.

**Trashing a stack takes the whole stack.** Frames are hidden from the grid by
`stack_parent_id IS NULL`, so hiding a master alone would leave its frames hidden by one
predicate and their master hidden by another: the trash would list one photo and the rest
would be reachable from nowhere. The nullable timestamp already chosen for ordering doubles
as the group key — a master stamps its untrashed frames with the same value, and restoring
clears exactly that value, so a frame trashed separately keeps its own and stays put. Its
one seam is whole-second resolution: a master and an unrelated frame of the same stack
trashed inside one second would restore together.

A trashed frame stops counting toward its master's stack badge; an *offline* one still
counts. The difference is that one is a decision about the photo and the other is a fact
about a disk.

### A storage verb acts on the moment, not the file

A stack is how a burst is stored, so a tile is a *moment*: trash, back up, and offload all
take the master **and its frames**. They did not always — trash started cascading in
cluster B while offload and backup still took one row, so the same tile behaved two ways
and offloading a 7-frame burst freed the keeper alone (#82).

The cascade lives in `plan_offload` / `plan_backup` / `plan_restore`, so every caller of the
service bodies in `crates/core/src/app/storage.rs` inherits it: the inspector buttons, the
Library's Retrieve from NAS, the reconcile drain, and the age-based `apply_offload_policy`
sweep. (The
sweep also has to de-duplicate: a frame is eligible in its own right and its master's
offload has already freed it, so without that it would count the same frame twice.)

Two conditions keep the cascade honest:

- **Every frame is gated on its own copies.** Invariant 2 is decided per frame: a frame
  without its own verified backup stays local rather than being freed on the strength of
  the master's. Backup likewise skips a frame with no local copy to send.
- **What was skipped is reported**, with the reason, the way `empty_trash` reports what it
  refused: *"Freed 4 of 7 — no verified backup — refusing to offload"*.

The sweep inherits one more thing, and it is worth stating plainly: **`offload_age_days`
selects moments, not photos.** The cutoff picks which photos are candidates, but the cascade
that follows applies no age test, so a frame imported inside the retention window is freed
when its master falls outside it. A burst is one moment; splitting it across two disks to
honour the cutoff exactly would be the worse answer. Nothing is at risk either way, because
every frame is still gated on its own verified backup — but a user who set the policy to keep
recent work on fast local storage can find yesterday's frame on the NAS, and that is the
behaviour, not a bug (#87).

When reconcile completes only part of a stack, it replaces the completed master's queue
row with one row per skipped frame, retried independently, so the completed master is not
destructively replayed. A frame that refused on its own account is `failed` with its reason.
A frame left only because the drain was superseded (a catalog switch or a newer drain) is
`pending` again — an interruption is not a failure, and only pending rows are counted by the
reconcile check and the queue chip, so a failed row would never be retried by itself. For
the same reason an op the trip stopped before it did anything (an offload before its named
photo) keeps its pending row untouched.

Ownership is the service layer's (`crates/core/src/app/storage.rs`), not a lock held across
the work. A verb the user started on ids read from one catalog runs its plan, its file IO and
its record on a connection of its own to that catalog (`backup_photo_as` and its siblings), so
a switch mid-copy cannot record it into the catalog switched to. A drain or offload-policy
sweep also holds the reconcile generation (`storage::ReconcileClaim`): a switch or a newer
drain trips it, and the claimed work then starts no further stack member — those are
reported skipped and requeued as above — and no further queued op. Offload, the verb that
deletes, re-checks the flag before every member's delete, the named photo's included. A copy
or delete already under way is indivisible and is recorded on that claimed connection.

**One storage operation per photo at a time (#254).** User verbs run on the blocking pool
beside each other and beside a drain or the offload-policy sweep. Every backup, offload and
restore the service layer runs therefore claims, in `AppState::storage_claims`
(`storage::StorageClaims`, keyed by catalog file and photo id), the photo it was named on
**and the frames it will take** before it plans, and holds the claim until it has recorded.
Nothing waits on a claim: a verb whose named photo is held fails with "a storage operation
on this photo is already in progress"; a held frame is left and reported with that reason;
a drain whose op's photo is held leaves the op `pending` and untouched (`DrainSummary::busy`),
and a held frame of a stack op is requeued `pending`, not failed — both are retried by the
next drain. The claim is per photo rather than one global gate because a drain can run for
hours, and a user's Offload of an unrelated photo must not wait behind it. Its mutex is a
leaf in the `app::jobs` lock order. The inspector also disables its storage buttons for a
photo while one of them runs, so a double-click starts one run. The `Catalog::*_photo` sync
wrappers do not claim; they are for tests and single-threaded callers.

Empty Trash and Relocate claim too (#256). Emptying the trash claims each photo just before
its delete, reads where its copies are under that claim (not when the run listed the
photos, which can be minutes earlier), and releases it after the delete: a photo a storage
operation holds is reported failed with the in-progress reason and keeps its row and files,
so it can be retried; a copy a backup or restore made just before is deleted with the rest
rather than outliving its photo's row; and a verb that starts after the delete finds
nothing to copy. Relocate claims the photo in the same catalog lock hold that re-points its
row, and holds the claim until the moved file's identity is recorded; a held photo is
refused and left pointing where it was — otherwise an offload's commit could drop the
re-pointed row by id and leave the moved file with none.

An IPTC sidecar write — a save, the debt panel's Retry, a geocode fill — claims its photo
too, from before it opens the sidecar until it has settled (#256): an offload deletes the
local sidecar once it has confirmed it at home, and a write landing after that check would
leave a newer sidecar beside a freed image, untracked, with the debt settled. Whichever
claims first goes ahead. A save that meets a claimed photo stores in the catalog and leaves
the fields owed, reported "sidecar pending (a storage operation on this photo is already in
progress)"; the next save or the repair pass writes them, to wherever the photo then
resolves. An offload that meets a write is refused as in progress. The identity-repair pass,
face-region and GPS writes do not claim. Each replaces the sidecar by a rename, so it
either lands before the offload moves the sidecar aside — and fails its re-hash — or makes a
new file at its name, which the offload keeps. In that second case the new file is built
without the moved sidecar (the writer found none), so it lacks every field ChairPhoto does
not own — another tool's keywords and history: the offload refuses, puts the image back,
and keeps the old sidecar beside it under its hidden name, which the refusal names, for the
user to merge by hand. Nothing is deleted, but the photo's own sidecar name now holds the
thinner file. A write that lands after the offload's last look is beside a freed image,
untracked (the photo is recorded archived). Both windows are one small file's hash wide.

Two guards do not depend on the claim. Offload's commit drops exactly the local location
rows it planned from, by id, so a row added after the plan (a restore) is never dropped with
them. And every lifecycle copy (`copy_and_verify`, used for images and companions) writes its
own temp file (`.<name>.chairphoto-part-<pid>-<n>`, created exclusively) and places it
**without replacing** whatever is at the destination — `renameat2(RENAME_NOREPLACE)`, else a
hard link, else an exclusive create, the import's own placement. Two writers can never
write into one file. A destination that already exists is accepted only when it hashes to
the source (another writer placed the same bytes); otherwise the copy fails and that file is
left untouched. This also means a Restore no longer overwrites a local file that differs
from the backup: it fails and names the file instead. On a filesystem with neither a
no-replace rename nor hard links (an exFAT or FAT backup drive) the destination is claimed by
an exclusive create and the verified temp copied into it; that second copy is hashed too, and
one that does not match is removed (the copy created it) and the copy fails (#256). A crash
during that second copy can leave a short file at the destination, which later copies refuse
as "already exists with different contents" until it is removed by hand; on every other
filesystem a crash leaves at most the hidden temp file, which scans skip. The next copy into
a folder (once per folder per run) removes the temp files there whose process is no longer
running on this machine and that have gone an hour unwritten — the hour because a backup
folder can be shared with another machine whose copy is still writing; a temp file only ever
holds bytes that exist elsewhere.

Restore is the same rule pointing the other way: a stack that leaves as seven frames comes
back as seven. It brings home only the frames that are *away* — a frame already local is
left alone, because copying the backup over it would replace a file the user may have
edited since.

Pressing a verb on a *frame* acts on that frame alone. Stacks are one level deep, so a
frame has nothing under it — the same asymmetry restore has, where bringing a child back
does not bring back its master.

**Delete** is the only path in the app that destroys an original, and it is gated twice:
an explicit confirmation the backend requires rather than assumes, and **every known copy
reachable**. Reachability rather than role, because master-ness is an advisory claim and an
advisory claim cannot guard a verb with no undo. Every copy rather than just home, because
deleting what is in reach while a disconnected disk holds one leaves a survivor that
nothing points at. A volume missing from the reachability map counts as unreachable —
failing open would destroy originals on the strength of an absent map entry.

Companions are deleted with the image. Leaving them would strand sidecars on the one path
where nothing can be recovered afterwards.

## Safety invariants (non-negotiable)

1. **Never delete the last verified copy** of a photo — including the companions that
   carry its edit state.
2. **Never offload** anything not verified-backed-up.
3. **Hash-verify** the NAS copy before marking safe or deleting anything local — and the
   local copy against it: a local file that no longer matches its backup is never deleted.
4. On a NAS-less machine, offload of un-backed-up photos is **unavailable**; they
   stay local and flagged at-risk.
5. If local fills up with **no NAS**, chairphoto may auto-evict only the
   **regenerable preview/thumbnail cache** — never an original. It warns instead.

### Trust hierarchy

- **Home (NAS + desktop catalog)** — canonical, permanent, **only grows**.
- **Local working disk** — fast, disposable cache; evictable only once home has a
  verified copy.
- **Laptop / hand-off exports** — outbound copies; creating or deleting them never
  affects home.

**Nothing ever leaves home.** The only delete operation (offload) removes a *local
cache* copy after the NAS (= home) holds a verified original, so the original never
actually leaves home.

## Import

Two modes over the same core location model:

- **Import in place (reference)** — e.g. existing NAS images. Create the photo with a
  single location on the NAS volume, role = primary. Files are **not moved**. (This is
  essentially today's scan-in-place.)
- **Import from card (ingest)** — `scanner::ingest_from_card` copies
  each supported image from the card into `<dest>/YYYY/MM/DD/` (date from EXIF capture
  time, falling back to file mtime), **keeping camera filenames**, then indexes the
  copies, groups them in one import batch, and auto-enqueues NAS backup. Destination
  default `~/Pictures/Raw` (must be under the catalog root). Collisions (#246, owner
  decision 2026-10-04): a file of the same name and size already there is the same photo,
  already imported and skipped, only when its EXIF capture time (`DateTimeOriginal` with
  `SubSecTimeOriginal`) and camera serial (`SerialNumber`, `InternalSerialNumber`, each
  compared when both files carry it) agree. A file with no `DateTimeOriginal` (a camera's
  video) has its QuickTime `CreateDate` as its capture time (read as
  `-QuickTime:CreateDate`), compared only with the other file's; an all-zero date is no
  capture time. A still's EXIF or XMP `CreateDate` is not read: a still with no
  `DateTimeOriginal` has no capture time. When neither file has a
  capture time (a PNG, a stripped JPEG) their contents are compared by streamed SHA-256
  instead. Anything else —
  another size, another sub-second, another body — is a different photo, copied as ` (n)`
  with its own row and UUID; nothing is ever overwritten (`same_photo::create_new_file`):
  the copy is written to a hidden temporary file in the same folder
  (`.<name>.chairphoto-part-…`, never indexed), synced, and then given its name without
  replacing anything — `renameat2(RENAME_NOREPLACE)` on Linux, else a hard link — so a file
  that appears there after the name was found free sends the copy on to the next free name,
  and a crash mid-copy leaves at most the hidden temporary file, never a short original at
  a library name. On a filesystem with neither (exFAT, FAT) the name is claimed by an
  exclusive create and the temporary file copied in: still no overwrite, but without that
  crash guarantee. A name is free only when
  nothing is at it and nothing at its sidecar's name (`<name>.xmp`) either: a sidecar with
  no original beside it (another tool's, or one whose original was removed) belongs to some
  other photo, and a new file placed beside it would adopt its identity and metadata. Nor is
  a name free that a catalog row holds (#247) — by its logical path, or by one of its
  locations (any role) under that location's own volume base, its file there or not
  (missing storage is normal): indexing matches
  by path, so a new file there would take that row's identity, rating and tags. The catalog
  is read once per date folder per import (`scanner::free_name::CatalogNames`), on the
  import's own connection to the catalog it started against, never the one open since.
  **One exception re-links instead of minting** (L-f of the third #246 review): a name whose
  file is gone, held by the logical path of exactly one row, goes to the arriving file that
  *is* that row's photo — for a card's file, its stamp against the capture metadata the row
  stores must prove it without contents to compare (the row's file is gone): #246's rule
  says the same capture **and** a sub-second or a serial is present, and equal, on both
  sides (`same_photo::same_capture_without_contents`). The same second with a serial
  missing on either side (the catalog's `-fast2` extraction skips MakerNotes serials) and
  no sub-second on both could be another body's shot, so it re-links nothing, and neither
  does no capture time; for a bundle's original, the bundle gives
  it the row's identity — and only when the sidecar at that name, if any, carries the row's
  identity and no other (one of another identity, of none, or that does not parse keeps the
  name taken, and is left untouched). The file is placed at that name, even past a free
  plain name, and indexing re-links the row: missing cleared, its rating, tags and edits
  kept, no second row. So a photo deleted outside the app and imported again from its card
  comes back to its row. Every other arriving file goes on to the next free ` (n)` with a
  row of its own: a different capture is never attached to an old row. File mtime is never evidence (a
  copy changes it). The ` (n)` names an earlier import gave are checked too (every one in
  the folder, past a gap in the numbers or with the plain name gone), so importing a
  card again skips every file. Each date folder is listed once per import
  (`same_photo::FolderListings`), not once per file: card ingest plans every file before
  copying any, and a bundle's unpack records each name it places. One photo met twice in a run (the same file in two folders
  of the card) is the same rule against the file already copied: it is copied once. Every
  earlier match counts, so a third meeting is skipped against the second when the first
  failed to copy. The metadata comes from one exiftool pass per 150 colliding
  files (`scanner::same_photo`), over both sides of each pair, so a re-import reads a few KB
  per file rather than hashing the card. The import dialog's "already imported" flag uses
  the same rule and the same plan, so a second meeting of one photo on the card is flagged
  as the copy will skip it. Owner decisions: Year/Month/Day tree, keep filenames. UI: topbar "Import card" dialog with an
  optional **Import name** that labels the batch (defaults to the source folder); the batch
  keeps its stable UUID underneath. (Cross-volume "import once" by UUID is handled by bundle merge.)

**Stopping an import.** Cancel, a newer import and a catalog switch all trip the import's
abort flag, which is checked between files in every phase. During the copy (or a bundle's
unpack) the import stops before the next file and indexes nothing; the copies stay in the
library folder for the next rescan (a bundle: import it again). During indexing
(`scanner::index_ingested_abortable`, `bundle::importer::index_bundle_abortable`) it stops
before the next copy, and what it indexed so far is committed whole, as if the import had
held only those files: rows and identity sidecars, the import batch (and its UUID in their
sidecars), queued backups, auto-tags and geofence tags; for a bundle, the merge runs over
the manifest narrowed to those photos, so the originals not yet indexed are not inserted as
metadata-only rows. Indexing always writes the catalog the import started against (its own
connection to that file, opened before the copy, which reads the names that catalog's rows
hold through it), never one opened since. The report says how many were indexed;
the rest wait for a rescan (card) or a second import of the bundle, which matches what is
already there by UUID.

### Import batches ("negative film roll")

Every ingest creates an **import batch** with a permanent unique ID — think of it as
a negative film roll. All photos from that ingest belong to it forever.

- Batches are **auto-created, immutable**, and shown in their **own list**, separate
  from user albums.
- The batch UUID is written to each photo's XMP as `chairphoto:ImportBatch` for
  permanence and portability, beside the photo UUID in `xmp:Identifier`. The writes are
  merge-safe and preserve foreign sidecar content. If either sidecar field cannot be
  written (read-only storage, malformed XMP, offline volume), the catalog records a
  retryable row in `pending_sidecar_identity` instead of logging and forgetting it;
  `repair_pending_identity` retries both fields.
- The batch is the natural unit for: culling on a trip, "not backed up" status,
  NAS reconcile, and merging home.

`import_batches` table + `photos.import_batch_id` (schema v9);
`catalog/batches.rs` (create / assign-immutably / list with counts); each scan that
imports new photos creates one batch (source = scanned folder) and assigns only the
new photos; the library query carries a `batchId` filter; a read-only Batches sidebar
section + a filter-bar chip. **Batch UUID in XMP sidecar** — the scanner, card ingest,
and bundle importer write `chairphoto:ImportBatch` (merge-safe, beside the
`chairphoto:LastWrite` field) so the batch survives catalog loss and merge; failed
writes are queued for the same repair pass as missing `xmp:Identifier` values.

## Organizational axes (sidebar)

Four independent axes:

1. **Tags** — hierarchical, describe *content* (`Birds/Owls`). Already implemented.
2. **Import batches** — auto, immutable, per-ingest, separate list.
3. **Albums** — manual curated collections; photos from anywhere. (`albums` +
   `album_photos` junction.) Ordered membership, sidebar section,
   add-from-selection; album viewing composes with the culling filter via
   the library query's `albumId`. Deleting an album never deletes photos.
4. **Smart albums** — rule-based, auto-populated from metadata. currently a list of AND
   conditions over fields (camera, lens, ISO, date, rating, label, pick, tag, batch).
   Nested AND/OR is not implemented.

Import batches and albums share the same "show me this set of photos" plumbing but
are distinct concepts (honoring "don't mix them in the album list").

## Catalog topology & merge

**Decision: (b) desktop-main + laptop-satellite.** Catalogs are **per-machine and
local** (required, since the laptop must work with no NAS — the catalog can't live on
the NAS). The desktop holds the permanent main catalog; the laptop is a satellite.

A **merge** is two independent halves:

1. **Metadata merge** — bring the laptop's records into the desktop catalog
   (photos by UUID, batches, tags, ratings, labels, picks, edits, albums). Fast,
   pure database rows.
2. **Physical reconcile** — copy the trip's originals from the laptop's local disk to
   the NAS, hash-verify, mark Backed up. This is the lifecycle queue with the
   laptop's files as source.

### Additive-only

The laptop only **adds new import batches**; it does not check out existing library
photos. Therefore merge is **additive** — the desktop gains photos it has never seen,
and a photo it already has keeps every value it holds (a bundle fills only its blanks and
adds its edits as new versions, #185), so **no edit conflicts are possible**. The one shared structure is the **tag
taxonomy**, resolved by matching tags on normalized full path (assignments union);
albums merge by name. All non-destructive.

Transport: **bundle file** (a `.chairphoto` export the desktop imports), unit =
import batch. Live-network sync is not implemented.

### Bundle format

A bundle is a single zip archive (`<name>.chairphoto`):

```
<name>.chairphoto  (zip)
├── manifest.json              ← BundleManifest (batch, photos, taxonomy)
├── originals/<relative_path>  ← RAW/JPEG originals, keyed by catalog-relative path
├── originals/<path>.xmp       ← each original's XMP sidecar (carries UUID + ImportBatch)
└── previews/<uuid>.jpg        ← cached JPEG previews (best-effort, optional)
```

**Identity is always UUID, never a path**: photos are keyed by `photos.uuid`, tags by
`tags.uuid`, the batch by `import_batches.uuid`. Paths are hints only.

The merge engine (`catalog/merge.rs`) is **pure-DB, no file I/O**:

| Operation | Strategy |
|-----------|----------|
| Batch | Insert idempotently by uuid; re-merge of the same bundle is a no-op |
| Tag taxonomy | Resolve by tag uuid first, then normalized full_path; create missing ancestors; never overwrite existing uuid / exportable flag |
| Tag terms | `INSERT … ON CONFLICT DO NOTHING` — adds missing terms, never modifies existing |
| Photo (new) | Insert with full state (rating, label, pick, IPTC, edit record, versions) |
| Photo (existing) | **Its own values win; blanks are filled** (owner decision on #185, 2026-10-04). Rating 0, an empty label and a pick of "none" take the bundle's. The bundle's edit record (as a version named "Imported edit"; a blank one is no edit and adds none) and its versions are added as **new versions** after the photo's own, unless the photo already has those settings (its edit record or a version with an equal JSON value), so a re-merge adds none; the photo's edit record and versions are not changed. Blank IPTC fields are filled by the importer, not the pure-DB merge (below). A row the importer created for this bundle moments before is not filled again. |
| Photo (new identity, path taken) | **Kept apart** — no row holds its identity but another photo, under another identity, holds its path (the same capture imported separately on each side, #246): neither inserted (`photos.path` is UNIQUE; before #185 the whole merge failed here) nor merged onto that photo. Counted in `MergeSummary::photos_kept_apart`. The same holds when the importer found the photo's original in the library at another name (a ` (n)` one) under another identity and its own path is free: the importer passes those identities to `merge_bundle_into`, so no metadata-only row is inserted at a path where no file of the photo is. |
| Tag assignments | `INSERT OR IGNORE` union — new assignments added, none removed |

The importer (`bundle/importer.rs`) runs in three phases:
1. **Parse** — open the zip, validate `format_version`.
2. **Copy** (off the catalog lock) — extract `originals/` into `<root>/YYYY/MM/DD/`;
   a collision is decided by card ingest's rule (#246): the same name, size and capture →
   already imported, skip; anything else → rename with ` (n)` suffix; never overwrite. A
   name a catalog row holds is not free either, its file gone or not (#247, as for a card),
   unless that row has the bundle photo's identity and no sidecar there says otherwise: the
   original then goes back to the row's name and the index phase re-links the row. The
   bundle's side is read from the original's bytes in memory (the manifest carries no
   capture time or serial), never written anywhere to be compared: one exiftool process
   reads the bytes from stdin and the library files from their paths, with one set of
   arguments, and the stamps decide first, as for a card (`same_photo::find_in_library`).
   Only where they do not say "the same capture" are the contents compared, streamed and
   stopping at the first differing byte — byte-identical is always the same photo — so a
   re-import of RAWs that carry a capture time reads their headers, not every library
   copy whole. Re-importing a bundle the library already holds writes nothing to the
   library's disk.
   A file found already there is not touched by this phase, its sidecar included: the
   index phase binds an identity to it — the row's, through `ensure_sidecar_identity` —
   and only when no row of **another** identity holds the file. One that does is the
   owner's photo (the same capture imported separately on each side): it is neither
   upserted nor bound, its sidecar never receives the bundle's identity (even when it
   lacks one, as identity debt), and the bundle's photo is kept apart (below).
   Writes a UUID sidecar beside each original so the index phase can match by identity:
   the bundle's own sidecar, or a fresh identity sidecar, each only as a new file — a file
   already at the sidecar's name (the original's name was chosen with it free, so one there
   now appeared meanwhile) is left as it is, never replaced.
3. **Index** (secondary connection, off the main lock) — `upsert_photo_with_identity` for
   each extracted file, giving a row created for the bundle's own photo the bundle's full
   state; run `merge_bundle_into`, which fills in what an existing photo lacks; assign
   newly-created photos to the batch; write the batch UUID sidecar; fill an existing
   photo's blank IPTC; apply auto-tags; reconcile missing.

   **A bundle photo the library already has** (#185) — its file skipped as the same
   capture (#246), or its identity held by a row whose own file is still in place (its
   copy then gets a row of its own, as above) — has its data put on the row merge matches
   by identity, never on a second row, as the table above says. Its IPTC goes through the rules for an existing row's IPTC (AGENTS.md, "XMP
   safety"): the original's path is resolved first (an unreachable original is not
   filled), the store is `Catalog::set_iptc` under the sidecar's write turn
   (`xmp::lock::WriteOrder`), held through the sidecar write and the compare-and-set
   settle, and a failed write stays owed. A field is filled only when neither the row nor
   the sidecar beside its original has a value — a value in the sidecar the catalog never
   imported is the photo's own too — and not at all when that sidecar does not parse. (Not
   `set_iptc_carried`: that is for values arriving beside a sidecar of their own, and this
   sidecar is the existing photo's.) Such a photo is **not** queued for backup and **not**
   put in the bundle's batch: it was not added by this import, and batch membership is
   immutable ("All photos from that ingest belong to it forever" — it belongs to the batch
   it arrived with). A version it gains owes the monochrome refresh any version write owes
   (docs/editing.md): a B&W version sets the flag.

**Batch UUID in XMP sidecar** (`chairphoto:ImportBatch`): every imported photo's
XMP sidecar carries the batch UUID alongside the photo UUID. This makes the batch
membership survive catalog loss or a catalog merge on a second machine — a re-scan can
reconstruct which batch each photo belongs to from the sidecar alone.

## Export (one-way)

`export::export_photos` resolves each photo's best original via the resolver
and writes to a destination folder by preset — **Hand-off** (RAW + its XMP sidecar) and
**Show off** (JPEG, currently the embedded full-size preview; edited-JPEG rendering
awaits the editor module). Unreachable originals are counted and reported ("N skipped —
connect the NAS"), never silently dropped. Destination collisions get a " (n)" suffix
(the sidecar stays paired). File I/O runs off the UI thread. *Full bundle* (RAW +
previews + catalog metadata) is the merge format; per-language /
hierarchical keyword assembly is covered in docs/taxonomy.md.

**Export is one-way** ("show off" / hand RAWs to someone). Exported
albums on the laptop are **read-only reference**; only new import batches merge back.
Editing an exported photo on the laptop and merging those edits back is the
checkout/two-way-sync case — **deferred**.

Two flavors:

- **Interchange export** (other people / other software): RAW files + **XMP
  sidecars**. Metadata travels inside the sidecars (`dc:subject`,
  `lr:hierarchicalSubject`, `xmp:Rating`, `xmp:Label` — already written, Lightroom/
  darktable read them natively). No chairphoto catalog needed on the far end.
- **chairphoto bundle** (your own laptop): RAW + previews + catalog metadata in a zip;
  the desktop imports with `import_bundle` and everything — photos, ratings, tags,
  versions, IPTC — lands instantly. **Shipped.**

Selectable contents → presets: *Hand-off to editor* (RAW + XMP), *Show off* (JPEG, or
RAW + previews), *Full bundle* (everything).

**Availability dependency**: exporting RAW originals requires them to be reachable.
If the NAS is offline, only previews/JPEGs can be exported — chairphoto must say so
up front ("18 of 50 originals are offline — connect the NAS to include RAWs") rather
than silently exporting a partial set.

## Catalog switching

chairphoto supports multiple named catalogs (one open at a time). Each catalog is an
independent `.chairphoto` SQLite file; switching replaces the in-memory catalog handle
with a different one and resets all frontend state.

### Commands

| Command | Description |
|---------|-------------|
| `create_catalog(name, catalog_path, root)` | Create a new catalog file at `catalog_path` rooted at `root`. Returns an error if the file already exists. Records the new catalog in the recent list. |
| `open_catalog(catalog_path, root)` | Open an existing catalog file. Returns an error if the file does not exist. Records in the recent list. |
| `switch_catalog(catalog_path, root, create, name)` | Safe teardown + reinit: abort any in-flight scan, close the current catalog (WAL flushed), open (or create) the new one, emit `catalog:switched`. |
| `list_recent_catalogs` | Return up to 20 recently-opened catalogs ordered by last-opened (most recent first). |

### Recent-catalog registry

A `recent_catalogs.json` file under `app_data_dir` (`$XDG_DATA_HOME/chairphoto/`)
tracks up to 20 recently-opened catalogs in JSON:

```json
[
  {
    "name": "Main",
    "catalogPath": "/home/user/.local/share/chairphoto/default.chairphoto",
    "root": "/home/user/Pictures/Raw",
    "lastOpened": 1751500000
  }
]
```

Recording the same `catalogPath` again deduplicates the entry and promotes it to the
front of the list (most-recent-first). The list is capped at 20 entries.

### Stored root vs supplied root

A catalog's library root is **persisted in the `settings` table** on first open
(`catalog_root` key). When the same file is opened again, it **adopts the stored root**
regardless of the `root` argument supplied to `Catalog::open`. To change the root after
the fact, call `set_library_root` (which runs an explicit `UPDATE`).

This behavior is intentional: the catalog stays self-contained and portable — moving the
catalog file to a different machine and opening it automatically recovers the original
root setting rather than silently using whatever the caller passed.

### Switch lifecycle

`switch_catalog` performs a safe handoff in four steps:

1. **Abort every in-flight job** — trips the installed abort generation of every family in
   `AppState::jobs` (scan, face indexing, face matching, sharpness, pHash, trash, import,
   reconcile, Smart Tagging, identity repair, develop) and clears every queryable status
   slot, so a running job exits at its
   next cancellation point and stops being reachable as a slot's owner. The scan and the
   identity repair pass each run on their own `open_secondary` connection, so the mutex is
   not held and the swap is not blocked by either.
2. **Close the current catalog** — drops the `Option<Catalog>` under the mutex. This
   releases the WAL write lock and flushes pending writes before the new catalog is opened.
3. **Open (or create) the new catalog** — off the async executor, so the UI thread is
   never stalled.
4. **Emit `catalog:switched`** — the GPUI app resets all session state (selection, filters,
   albums, scan progress) and re-queries the new catalog.

A *fresh* abort generation is installed for each family after the new catalog is open, so
subsequent jobs start un-aborted while the old workers keep the flag they were given (and
stay aborted). Steps 1–2 and the publish in step 3 are the two phases of one ownership
transition; both, and every job start, live in `commands/jobs.rs`, which also carries the
backend-wide lock order (catalog → abort generations → status slots). `set_library_root`
runs the same transition — it replaces the catalog handle exactly as a switch does.

### Invariants

- **No cross-catalog writes**: an aborted scan stops at the first cancellation point; any
  writes already committed are durable in the *old* catalog only and never appear in the
  new one (they are separate SQLite files). A back-up drain (`storage::ReconcileClaim`)
  runs every op on its own connection to the catalog it claimed, so the op in flight at a
  switch finishes there and the drain then stops. A write keyed by ids a front end read
  earlier (Trash today) carries the `CatalogIdentity` it read them with and goes through
  `with_catalog_as`. That write fails closed once another catalog is open, even before
  `catalog:switched` reaches the UI.
- **No dangling state**: between step 2 (close) and step 4 (emit `catalog:switched`) the
  `Option<Catalog>` holds `None`. Any command that calls `with_catalog` during this window
  returns `"No catalog is open"` rather than touching a stale connection.
- **Root follows the catalog**: each catalog remembers its own library root; opening a
  different catalog automatically switches the effective root. Volume health caches are
  invalidated on switch.

## Core vs module split

- **Core**: the location model (photos have N locations on named volumes, with roles
  + availability), the **resolver**, import-batch identity, and the catalog/merge
  primitives. Path resolution is fundamental, so this must be core.
- **Module**: policy and packaging — ingest-from-card rules, auto-backup scheduling,
  verification, offload/eviction, and the export presets/packaging (zip, folder, JPEG
  rendering). JPEG rendering for export leans on the editing module
  (exposure/crop → JPEG).

## Data model

- `volumes` — id, name, per-machine base path, (runtime reachability not stored).
- `photo_locations` — photo_id, volume_id, relative_path, role
  (`primary`/`local_cache`/`backup`/`export`), verified_hash, state.
- `import_batches` — id, uuid, imported_at, source_label, note, count.
  Photos gain `import_batch_id` (and the batch id is mirrored to XMP).
- `albums` — id, name; `album_photos` — album_id, photo_id (manual M:N).
- `smart_albums` — id, name, rule definition (AND conditions).
- `pending_operations` — kind (backup/offload/restore), photo_id, target, status.
- `pending_sidecar_iptc` (v25) — photo_id, `owed` (a bitmask of managed IPTC fields whose
  bit numbering is persisted), `generation`, attempts, error, timestamps: catalog IPTC the
  sidecar has not received (see IPTC that fails to reach the disk). No value columns —
  `photos.iptc_*` is the source of truth.
- `pending_sidecar_identity` — photo_id, field (`identifier` or `import_batch`), attempts,
  error, timestamps, `dismissed_at`: sidecar identity fields that are in SQLite but not yet
  on disk. No value column — `photos.uuid` and `import_batches.uuid` are the sources of
  truth. A non-zero `dismissed_at` is a human's "stop retrying this copy" (see Resolving a
  conflict): the row stays for the record, and leaves both the repair pass and the debt
  count.

## Storage model

- **Volume kinds.** Each volume is **`local`** (fast working disk, e.g.
  `~/Pictures/Raw`) or **`backup`** (NAS/remote, e.g. the ZimaCube at
  `~/ZimaCube/Gallery`). The default catalog-root volume is `local`. Multiple `backup`
  volumes are allowed (a photo is "backed up" if any backup holds a verified copy).
  Implemented.
- **Local-cache eviction = rolling time window.** Local keeps the most recent **X
  months** (default **12**, configurable); the NAS keeps that window **plus everything
  older, forever**. Photos older than the window are offloaded from local (verified NAS
  copy retained) → **Archived**, still browsable via the cached preview. New imports
  land local and are backed up to the NAS. (Implements the Pattern C lifecycle; the
  `Year/month/day` folder layout makes the cutoff a simple date compare.) Offload itself
  is the backup / offload / restore lifecycle.
- **One photo, shown once, prefers local.** A photo is a single catalog row (identity =
  UUID) with N locations; the resolver already prefers a local copy over the NAS copy,
  so a photo present on both shows **once** and reads from local — no duplicates. Making
  this automatic across the two folders (whose relative paths differ) means the
  multi-volume scan/ingest matches the same photo across volumes **by its XMP UUID**,
  not by path (card ingest; underpins bundle merge).
- **Restore (temporary local copy).** Pulling older NAS folders back to local for fast
  editing (e.g. a 2015 shoot) is the `restore` lifecycle op, the inverse of
  offload.
