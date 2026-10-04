# Windows and macOS builds (possible future feature)

**Status: possible future feature, no plans.** Owner and Claude, 2026-10-04. Assessed by reading
the code at `feature/gpui` 144ae76; no build for either platform was attempted (no Windows or
macOS target or toolchain on this machine). ChairPhoto ships for Linux only.

## What is already portable

- GPUI runs on Linux, Windows and macOS (macOS was Zed's first platform).
- SQLite, LibRaw (built from source by `crates/core`, so it needs the platform's C toolchain),
  ONNX Runtime, and the external tools (exiftool, exiv2, ffmpeg, ImageMagick) all exist for
  both platforms.
- Most Unix-specific code is gated with a fallback: 73 `cfg(unix)` sites, 22 `cfg(not(unix))`
  alternatives (counted with grep). The no-overwrite placement uses `renameat2` on Linux only
  and falls back to a hard link elsewhere; `rustix` is a `cfg(unix)` dependency; LocalSend's
  interface lookup is Unix-only with a fallback.

## Windows

1. **Single instance — the one hard blocker.** `crates/app/src/single_instance.rs` uses Unix
   domain sockets (`std::os::unix::net`) with no fallback, so `chairphoto-app` does not compile
   for Windows. Needs a named-pipe / named-mutex version; it also forwards `chairphoto://` links
   to the running instance.
2. **Desktop integration.** `desktop.rs` writes a freedesktop `.desktop` entry and edits
   `mimeapps.list`; Windows registers the URL handler in the registry.
3. **Paths.** `~/Pictures/Raw`, `~/.local/share`, `~/.cache` and a required `HOME` need the
   Pictures / AppData equivalents; volumes are drive letters and UNC paths (`\\nas\photos`).
4. **File semantics.** Rename-over, locking and deleting open files differ on NTFS; the
   import, sidecar and cache code relies on POSIX behaviour, has fallbacks, and would need
   testing there.
5. **Tests/CI.** Many tests are `cfg(unix)`; a Windows CI runner would be needed.
6. **Packaging.** MSI/MSIX installer, shipping or locating the external tools, code signing.

## macOS — closer than Windows

macOS is Unix, so most of the Unix code (the socket-based single instance, rustix, LocalSend's
`getifaddrs`) applies as is. What differs:

1. **Peer credentials — a compile blocker.** `single_instance.rs` `peer_uid` uses Linux's
   `SO_PEERCRED` / `libc::ucred`, which macOS lacks; macOS uses `getpeereid` (or
   `LOCAL_PEERCRED`). A small, contained change.
2. **No `renameat2`.** Placement takes the hard-link fallback (fine on APFS); macOS's own
   `renamex_np(RENAME_EXCL)` would be the native equivalent.
3. **Desktop integration.** The `chairphoto://` handler is declared in the `.app` bundle's
   `Info.plist`, not a `.desktop` file; `desktop.rs` would be skipped.
4. **Paths.** XDG fallbacks (`~/.local/share`) work but aren't native; macOS expects
   `~/Library/Application Support` and `~/Library/Caches`. Volumes mount under `/Volumes`.
5. **GPU / models.** CUDA (`faces-cuda`) doesn't exist; ONNX Runtime's CoreML provider would
   be the accelerated path, CPU otherwise.
6. **Packaging.** An `.app` bundle, ideally a universal (Apple Silicon + Intel) binary, code
   signing and notarization (needs an Apple Developer account), and the external tools via
   Homebrew or bundled.

## Cheap first step (if picked up)

Add the target and a CI runner (GitHub Actions has Windows and macOS runners) that only runs
`cargo check --workspace`. That lists every compile error without anyone needing the
hardware.
