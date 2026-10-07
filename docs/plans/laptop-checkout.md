# Laptop checkout and return (discussion)

**Status: discussion, not scheduled.** Owner and Claude, 2026-10-04. Nothing here is built.
The owner will come back to it; turn it into a wayfinder map with tickets then.

Today a bundle only carries *new* photos from the laptop to the main computer, and merge is
additive (`docs/storage-and-import.md`, "Catalog topology & merge"). Taking existing library
photos out to the laptop and bringing changes back was deferred. This note records the shape
discussed for that.

## The round trip

1. **Checkout (main → laptop).** Pick an album or selection, *Send to laptop*, and choose:
   - **Full** — RAWs included; everything works on the laptop.
   - **Light** — previews only; small and fast.
2. **On the laptop.** Each checked-out photo records where its original lives: the origin
   catalog's UUID plus a readable name ("Original on *Main*"). A Light photo is a photo whose
   only original is on another machine — "not here", a normal state like unmounted storage,
   not an error.
3. **Return (laptop → main).**
   - Photos checked out from the main computer come back as **metadata only** (a few KB each).
   - Photos new on the laptop (shot on the trip) still carry their RAWs.
   - One return bundle can hold both.
4. **Transport: LocalSend.** ChairPhoto can already *send* over LocalSend (`docs/localsend.md`);
   the new part is the main computer *receiving* a `.chairphoto` file and offering to import
   it ("Import 312 updates from *Laptop*?"). A USB stick or the NAS remain fallbacks.

## Decisions so far

- **Light mode disables Develop** for those photos. Culling, rating, tags, captions and faces
  work on previews; develop edits need the RAW (no smart-preview proxies for now).
- **Edits come back as new versions** of the main computer's photo; its current edit and
  versions are untouched (same rule as #185), so edits never conflict.
- **Rating, label, pick, IPTC and similar fields use the checkout snapshot**, not fill-blanks:
  the checkout records each photo's values at that moment. On return, per field:
  - only the laptop changed it → apply the laptop's value;
  - only the main computer changed it → keep the main computer's;
  - both changed it → **ask the user**: a conflict list showing the snapshot value, the main
    computer's and the laptop's, with a per-field choice and "apply to all".
- **Several machines are supported**, though the owner uses one main computer:
  - the origin marker is a catalog UUID, never "the" main computer;
  - each checkout records which machine it went to and its own snapshot, so a photo out on
    two laptops is compared against each laptop's own snapshot;
  - when the second laptop returns, the first laptop's changes count as "main changed it", so
    a real disagreement between laptops surfaces as a conflict question;
  - the origin always points at the catalog holding the RAW, even through a laptop → laptop
    checkout.
- **Deleting on the laptop** drops only the laptop's copy; it never deletes the original on the
  main computer. Rejecting is a rating and travels back like any rating.
- **Importing the same return twice** is a no-op (merge is already idempotent by UUID).

## Open questions

- Should the laptop show "N photos changed since last sync" to remind you to send back?
- What happens to a checkout that is never returned — does the main computer show photos as
  "out on *Laptop*", and can a checkout be cancelled from either side?
- Light previews: which tier is sent (the 2048 px preview?) and are faces/Smart Tagging
  allowed to run on them on the laptop?
- Does a Full checkout's RAW on the laptop count as a backup copy for safety status, or never?

## Likely tickets (when picked up)

1. Checkout export with Full / Light.
2. Origin marker and the "original on another machine" location.
3. Checkout snapshot storage (per photo, per destination machine).
4. Metadata-only return bundle (plus RAWs for laptop-new photos).
5. Conflict dialog on return.
6. LocalSend receive and bundle auto-import.
