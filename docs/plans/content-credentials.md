# Content Credentials (C2PA / Content Authenticity Initiative) (discussion)

**Status: discussion, not scheduled.** Owner and Claude, 2026-10-04. Nothing here is built.
Facts about the standard and camera support below are from memory and must be checked against
the current C2PA spec and vendor docs before any work starts.

## Background

The Content Authenticity Initiative (CAI, led by Adobe) promotes **Content Credentials**, built
on the open **C2PA** standard. A C2PA *manifest* is signed metadata embedded in (or linked
from) a file: who or what made it, what was done to it (crop, colour, AI generation…), which
earlier files ("ingredients") it came from, and a cryptographic hash binding it to the pixels.
Anyone can verify that the file is unchanged since signing and read its history.

Several recent cameras can sign at capture (some only with a firmware update or a licence),
and some platforms read and display the credentials. The CAI publishes open-source SDKs,
including a Rust one (`c2pa-rs`), which would fit ChairPhoto's core.

## Where it touches ChairPhoto

1. **Preserve (already mostly true).** Originals are never modified, so a camera-signed
   original stays valid in the library, in backups and in *Hand-off* exports (RAW + XMP copied
   as-is). Check: no path rewrites an original's bytes; bundle import/export copies them
   byte-for-byte.
2. **Show.** Read and verify an original's manifest and show it in the inspector: "Signed at
   capture by <camera>", valid / tampered / unknown signer. Possibly a library filter
   ("has Content Credentials").
3. **Export with credentials.** A *Show off* JPEG today is the embedded preview, with no
   manifest. A rendered export could carry a new manifest naming the original as its
   ingredient and listing the edits (crop, exposure, colour…), so a viewer can trace it back
   to the signed capture.
4. **Identity.** A signed original's manifest carries its own identifiers and a content hash.
   That could feed the content fingerprint idea (`docs/plans/photo-identity.md` §1) when
   present — never as the only key, since most photos have no manifest.
5. **AI features.** C2PA can record "do not train / do not mine" preferences, and edits made
   with AI tools are meant to be declared. Relevant if AI tagging, OpenJev or any generative
   edit ever changes pixels; tagging alone doesn't.

## Open questions and catches

- **Signing needs a certificate.** For exports to show as trusted, ChairPhoto (or the user)
  needs a signing certificate from a CA that verifiers trust. A self-signed local certificate
  verifies as "unknown signer". Who holds the key, and where (keyring)?
- **Privacy.** A manifest can name the signer (the user) and include capture details. Signing
  exports must be opt-in per export, with what goes in the manifest shown first. Verifying
  against online trust lists or revocation would contact the network, which ChairPhoto never
  does by default — bundle a trust list or make online checks an explicit opt-in.
- **Platforms strip metadata.** Some upload paths (Instagram, Flickr…) drop embedded
  manifests; the C2PA "soft binding" / cloud manifest options would need their own decision.
- **RAW support.** Which RAW formats carry manifests, and whether the SDK reads them, needs
  checking per camera.
- **Edits are non-destructive.** The edit list for an export's manifest would come from the
  develop history; it should be honest about what it can describe.

## Likely first step (when picked up)

Read-only: detect and verify manifests on originals and show them in the inspector, with no
network access. Signing exports comes after a decision on certificates and privacy.
