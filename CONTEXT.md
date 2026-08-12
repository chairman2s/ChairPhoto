# ChairPhoto — Ubiquitous Language

Glossary of canonical terms. Definitions only — no implementation details.
Design docs live in `docs/`.

## Storage & topology

- **Home** — the canonical archive of originals (in practice: the NAS). Only grows.
  A photo is *safe* when home holds a verified copy. Home is a *place*, never a device.
- **Master** — the device currently acting as custodian of home: it can reach the NAS,
  runs backup/verify, and holds the complete catalog. Master-ness is derived from
  verified home access — any device that gains it can be promoted if the current
  master is declared dead. There is exactly one master at a time.
- **Satellite** — any non-master device (e.g. a travel laptop). May hold a full or
  partial copy of the library, and may originate photos (card ingest in the field).
- **Unreachable** — a device that cannot currently be contacted (offline, traveling,
  powered down). Transient. Implies nothing about the device's health or role.
- **Dead** — a device the user has permanently declared broken/unrecoverable. A human
  declaration, never inferred from unreachability. Only death of the master justifies
  promoting another device.
- **At risk** — the state of a photo that exists on exactly one disk (typically a
  satellite after card ingest, before home holds a verified copy). Priority one of
  any sync design is shrinking the time a photo spends at risk.
- **Original** — the camera file (RAW/JPEG) as first ingested. Never modified,
  never leaves home outbound without an explicit user action per operation.

## Identity

- **Photo identity** — the UUID a photo is given at first import and keeps forever.
  Unique across the catalog: no two photos share one. It is what catalog merge matches
  on, never the path.
- **Copy** — one file of a photo at one location. A photo may have several. Each copy
  carries its own sidecar, so identity is discharged per copy, not per photo.
- **Bound** — a copy whose sidecar carries the photo's identity. The settled state.
- **Identity debt** — a copy whose sidecar does not yet carry it. Normal and transient:
  an unreachable volume owes just as much as a failed write. Debt is per copy.
- **Conflict** — a copy whose sidecar carries a *different* photo's identity. Not an
  error and not repairable by retrying: the file is left alone, because overwriting it
  would destroy whatever that identifier belongs to.
- **Adopt** — the catalog takes the identity already in the copy's sidecar. Changes the
  catalog, never the file. Refused if another photo already holds that identity.
- **Overwrite** — the copy's sidecar takes the catalog's identity. Changes the file, and
  destroys the identifier that was there. Backs the sidecar up first.
- **Dismiss** — stop retrying this copy. Changes neither the catalog nor the file; the
  record is kept, and the copy stops counting as debt.

## Removing things

- **Evict** — remove locally cached bytes from a non-home device (e.g. free laptop
  space). Allowed only once home holds a verified copy. Pure cache management: never
  synchronized, never affects any other device, never touches home.
- **Trash** — a per-photo metadata state ("in trash"). Hides the photo everywhere;
  reversible; synchronizes like any other metadata. Touches no bytes anywhere.
- **Delete** — destroy an original at home. Only possible on the master, only from
  the trash, only by explicit manual confirmation. Never synchronized, never
  triggered by another device. Satellites have no delete capability at all.

## Metadata

- **Rating** — the catalog owner's single 0–5 star value per photo. One value per
  photo, not per person. Any per-reviewer scoring is a separate concept — such values
  are not Ratings and never overwrite one.
