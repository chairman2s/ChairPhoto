# ChairPhoto

A catalog-first photo organizer for people with a lot of photos and a NAS.

ChairPhoto keeps a SQLite catalog of your library, never modifies your originals, and
writes everything it knows into XMP sidecars so your work survives the catalog. It is a
native desktop application written in Rust: a core that does all I/O and image work, and a
[GPUI](https://www.gpui.rs/) front end over it — no webview, no IPC, no browser engine.

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

Most of this lives in **modules** you can turn off, or leave out of the build entirely. See
[Modules](#modules).

## Platform

Developed and tested on **Linux**. ChairPhoto's system dependencies and packaging have not
been exercised on macOS or Windows — reports welcome.

**Hyprland / Omarchy.** Omarchy makes every window slightly translucent by default, which
mixes the wallpaper into the tones you are judging in Develop. Keep ChairPhoto opaque by
adding this to `~/.config/hypr/hyprland.lua` (the same rule is in `packaging/omarchy/`):

```lua
o.window("^chairphoto$", { tag = "-default-opacity", opacity = "1 1" })
```

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
| **LibRaw** (vendored) | Full-resolution RAW decode — a pinned git submodule compiled into the binary (`git submodule update --init`) | Build fails unless you disable the `raw` feature |

At build time ChairPhoto additionally needs a Rust toolchain and `clang`/`libclang` (for the
LibRaw bindings, plus zlib). At run time the GPUI front end needs a Vulkan driver, Wayland or
X11 client libraries, `libxkbcommon`, and fontconfig/freetype for text layout — no browser
engine of any kind.

### Arch Linux

```bash
sudo pacman -S --needed \
  perl-image-exiftool exiv2 ffmpeg imagemagick libheif \
  clang pkgconf zlib \
  vulkan-icd-loader libxkbcommon libxkbcommon-x11 libxcb wayland fontconfig freetype2 \
  rust

# Install your GPU's Vulkan driver too, e.g. nvidia-utils or vulkan-radeon.

# Only for face tagging / Smart Tagging. onnxruntime-cuda substitutes for the CPU build
# on an NVIDIA GPU, but the GPU is only used in a `faces-cuda`/`smarttags-cuda` build.
sudo pacman -S --needed onnxruntime-cpu
```

### Debian / Ubuntu

```bash
sudo apt install \
  libimage-exiftool-perl exiv2 ffmpeg imagemagick libheif1 zlib1g-dev \
  clang pkg-config \
  libvulkan-dev libxkbcommon-dev libxkbcommon-x11-dev libxcb1-dev libxcb-xkb-dev \
  libwayland-dev libfontconfig-dev libfreetype-dev
# Rust via https://rustup.rs
# ONNX Runtime (face tagging / Smart Tagging) is not packaged by Debian; install a
# 1.24+ build from https://github.com/microsoft/onnxruntime/releases and point
# ORT_DYLIB_PATH at its libonnxruntime.so, or skip those two features.
```

*(Arch package names verified on the development machine; Debian names are the
equivalents — also used by CI, `.github/workflows/ci.yml` — and may differ by release.)*

## Build & run

```bash
cargo build --release -p chairphoto-app --bin chairphoto-gpui
cargo run --release -p chairphoto-app --bin chairphoto-gpui
```

It opens your real catalog by default (`~/Pictures/Raw`, changeable in Preferences). To try
it without touching your own library, point it at throwaway data directories instead:

```bash
XDG_DATA_HOME=/tmp/cp-data XDG_CACHE_HOME=/tmp/cp-cache \
  cargo run --release -p chairphoto-app --bin chairphoto-gpui
```

That isolates the catalog database and caches ChairPhoto keeps under
`$XDG_DATA_HOME`/`$XDG_CACHE_HOME` (both default to `~/.local/share` / `~/.local/cache`),
which is the same mechanism this project's own agents use to avoid ever touching the real
library during development.

Checks:

```bash
cargo test --workspace                      # all Rust crates
cargo check --workspace --all-features --all-targets
cargo check --workspace --no-default-features   # verifies feature gating still holds
```

Tests that need something this machine lacks (a RAW fixture, ONNX Runtime, a model, a free
LocalSend port) skip and print `SKIPPED: <test> — <why>`; run
`cargo test --workspace -- --nocapture` to see which ran.

The Rust side is a Cargo workspace at the repository root, with build output in `target/`:

| Crate | Package | Role |
|---|---|---|
| `crates/core` | `chairphoto-core` | Catalog, import, decode, XMP, jobs and module backends. No UI dependency. |
| `crates/model` | `chairphoto-model` | UI logic with no I/O (library session, editing, presets, tag graph layout, deep links, …), shared by the GPUI views and tested on its own. |
| `crates/app` | `chairphoto-app` | The GPUI front end — the only front end (binary `chairphoto-gpui`). |

Feature names are the same in every crate that forwards them.

The tree is warning-clean under every feature combination. Please keep it that way.

## Modules

Optional features are Cargo features, compiled into the binary, with a runtime on/off toggle
in the Modules preferences panel — so you can build only what you want:

```bash
# a lean build with no RAW, no local AI, no browser automation
cargo build -p chairphoto-app --no-default-features --features edit,collage,slideshow
```

| Feature | What it adds | Extra cost |
|---|---|---|
| `raw` | Full-res RAW decode | the vendored LibRaw submodule, a C++ compiler, zlib and libclang at build time |
| `edit` | Crop/tone render engine (the Darkroom) | none |
| `faces` | Local face detect + recognise | ONNX Runtime at runtime + model download |
| `smarttags` | Local CLIP tag suggestions | ONNX Runtime at runtime + ~350 MB model |
| `ai` | Vision-model tag suggestions (Ollama or cloud) | network for cloud providers |
| `map` | Geofences + reverse geocoding | network for the geocoder |
| `tag-graph` | Visual tag graph (Communities view) | none (bundles tiny-skia for the edge raster) |
| `flickr`, `smugmug` | Publishing via official APIs | — |
| `instagram` | Posts an export by driving Chrome | Chrome/Chromium at runtime |
| `localsend` | Send to a LAN device | — |
| `collage`, `slideshow` | Mosaic render; slideshow video | ffmpeg for slideshow |

`raw`, `edit`, `ai`, `instagram`, `collage`, `slideshow`, `localsend`, `map`, `faces`,
`smarttags` and `tag-graph` are on by default; `flickr` and `smugmug` are opt-in.

`faces-cuda` and `smarttags-cuda` additionally run inference on an NVIDIA GPU; both fall
back to CPU rather than failing.

Known, deliberate limitations: video shows its poster frame with *Play in system player*
instead of playing inline, and the tag graph has no "Photo ↔ tag" view — Communities only.

## Writing a module

All modules are first-party Rust, compiled into the app through the `Module` trait
(`crates/app/src/modules/mod.rs`) and registered in `modules::bundled()`. There is no
third-party or externally loaded module today; see `docs/plugin-system.md` § "Why no
third-party modules" for what that used to look like and why it was dropped at the GPUI
cutover. Adding a module means contributing Rust to this repository (or carrying a patch on
your own fork) — see `docs/plugin-system.md` for the trait, and
[`MODULE_LICENSING.md`](MODULE_LICENSING.md) for what the GPL requires and does not require
of a module that talks to an external service.

## License

GPL-3.0-only. See [`LICENSE`](LICENSE).

In short: you may use, modify, and redistribute ChairPhoto, including commercially, but
versions you distribute must also be open source under the same license. This is
deliberate — the project should stay open no matter who builds on it.
