# ChairPhoto

A catalog-first photo organizer for people with a lot of photos and a NAS.

ChairPhoto keeps a SQLite catalog of your library, never modifies your originals, and
writes everything it knows into XMP sidecars so your work survives the catalog. It is a
desktop application built with Tauri — a Rust backend doing all I/O and image work, and a
React frontend that only displays.

> **Status: early.** This is a personal project released in the hope it's useful to
> someone else. It works on the author's machine and library; expect rough edges.

## What it does

- **Catalog & culling** — virtualized grid over large libraries, ratings, colour labels,
  flags, and a hierarchical tag vocabulary with facets and smart albums.
- **Non-destructive editing** — crop, tone, film looks and `.cube` LUTs, saved as named
  versions. Originals are never touched.
- **RAW** — full-resolution decode via LibRaw for the editor and export; embedded previews
  for fast browsing.
- **Metadata that outlives the catalog** — every photo gets a UUID written to both the
  catalog and its XMP sidecar. The XMP writer merges into the existing document and never
  clobbers namespaces belonging to darktable, RawTherapee, or anything else.
- **Multi-machine** — portable catalog bundles and a laptop⇄desktop catalog merge that
  matches photos by UUID rather than path.
- **Local AI, optional** — face detection and recognition, and CLIP-based smart tagging,
  both running locally through ONNX Runtime. Nothing leaves your machine unless you
  choose a cloud provider.
- **Hand-off** — open a photo in darktable / RawTherapee / ART / RapidRAW and get the
  rendered result back, stacked under the original.
- **Publishing** — LAN transfer via LocalSend. Flickr and SmugMug are supported through their
  official APIs but are not built by default; enable them with `--features flickr,smugmug`.

Most of this lives in **modules** you can turn off. See [Modules](#modules).

## Platform

Developed and tested on **Linux**. Tauri itself is cross-platform, but ChairPhoto's system
dependencies and packaging have not been exercised on macOS or Windows — reports welcome.

## Requirements

ChairPhoto shells out to a few system tools rather than bundling them. **Missing tools
degrade one feature; they never crash the app** — but you'll want them.

| Tool | Needed for | Without it |
|---|---|---|
| **exiftool** | EXIF/IPTC/XMP extraction, RAW preview fallback | Metadata gaps on scan |
| **exiv2** | Primary embedded-preview extractor for RAW | Slower/failed RAW thumbnails |
| **ffmpeg** | Video poster frames, slideshow `.mp4` render | No video thumbs, no slideshow |
| **ImageMagick** *(with the libheif delegate)* | HEIF/HEIC (iPhone) decode | HEIC tiles show "no preview" |
| **ONNX Runtime** *(1.24 or newer)* | Face tagging and Smart Tagging inference | Those two modules report the runtime is missing; everything else is unaffected |
| **LibRaw** | Full-resolution RAW decode (build-time link) | Build fails unless you disable the `raw` feature |

Building additionally needs a Rust toolchain, Node.js, and the usual Tauri Linux
dependencies (`webkit2gtk-4.1`, `gtk3`, `librsvg`), plus `clang`/`libclang` for the LibRaw
bindings.

### Arch Linux

```bash
sudo pacman -S --needed \
  perl-image-exiftool exiv2 ffmpeg imagemagick libheif libraw \
  clang pkgconf webkit2gtk-4.1 gtk3 librsvg \
  rust nodejs npm

# Only for face tagging / Smart Tagging. onnxruntime-cuda substitutes for the CPU build
# on an NVIDIA GPU, but the GPU is only used in a `faces-cuda`/`smarttags-cuda` build.
sudo pacman -S --needed onnxruntime-cpu
```

### Debian / Ubuntu

```bash
sudo apt install \
  libimage-exiftool-perl exiv2 ffmpeg imagemagick libheif1 libraw-dev \
  clang pkg-config libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev \
  nodejs npm
# Rust via https://rustup.rs
# ONNX Runtime (face tagging / Smart Tagging) is not packaged by Debian; install a
# 1.24+ build from https://github.com/microsoft/onnxruntime/releases and point
# ORT_DYLIB_PATH at its libonnxruntime.so, or skip those two features.
```

*(Arch package names verified on the development machine; Debian names are the
equivalents and may differ by release.)*

## Build & run

```bash
npm install
npm run tauri dev      # development, hot reload
npm run tauri build    # production build
```

Checks:

```bash
npm test                                    # frontend tests (vitest)
npx tsc --noEmit                            # frontend typecheck
cd src-tauri
cargo test                                  # backend tests
cargo check --all-features --all-targets
cargo check --no-default-features           # verifies feature gating still holds
```

The tree is warning-clean under every feature combination. Please keep it that way.

## Modules

Optional features are Cargo features on the backend and modules on the frontend, so you
can build only what you want:

```bash
# a lean build with no RAW, no local AI, no browser automation
cargo build --no-default-features --features edit,collage,slideshow
```

| Feature | What it adds | Extra cost |
|---|---|---|
| `raw` | Full-res RAW decode | LibRaw + libclang at build time |
| `edit` | Crop/tone render engine | none |
| `faces` | Local face detect + recognise | ONNX Runtime at runtime + model download |
| `smarttags` | Local CLIP tag suggestions | ONNX Runtime at runtime + ~350 MB model |
| `ai` | Vision-model tag suggestions (Ollama or cloud) | network for cloud providers |
| `map` | Geofences + reverse geocoding | network for the geocoder |
| `flickr`, `smugmug` | Publishing via official APIs | — |
| `instagram` | Posts an export by driving Chrome | Chrome/Chromium at runtime |
| `localsend` | Send to a LAN device | — |
| `collage`, `slideshow` | Mosaic render; slideshow video | ffmpeg for slideshow |

`faces-cuda` and `smarttags-cuda` additionally run inference on an NVIDIA GPU; both fall
back to CPU rather than failing.

## Writing a module

Modules load through a small, stable host API (`ChairPhotoAPI` / `ChairPhotoModule`) and
can add panels and actions without touching core. The API is additive-only within a major
version, and a module declares the oldest host it supports via `minHostVersion`.

**Licensing for module authors:** a module you distribute must be GPL-3.0, because it runs
inside ChairPhoto. The **external service** a module talks to does not have to be open
source, and may cost money — the GPL stops at the network boundary. See
[`MODULE_LICENSING.md`](MODULE_LICENSING.md).

## License

GPL-3.0-only. See [`LICENSE`](LICENSE).

In short: you may use, modify, and redistribute ChairPhoto, including commercially, but
versions you distribute must also be open source under the same license. This is
deliberate — the project should stay open no matter who builds on it.
