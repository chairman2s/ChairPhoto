# Arch Linux packaging

`PKGBUILD` builds ChairPhoto from a tagged GitHub release. It is kept in-tree so the
dependency list stays in step with the code; the AUR copy is a mirror of these files, not a
separate source of truth.

## Layout

| File | Purpose |
|---|---|
| `PKGBUILD` | The package recipe. |
| `chairphoto.desktop` | Launcher entry, plus the `chairphoto://` scheme registration. |

## Why `onnxruntime-cpu` is a hard dependency

Face tagging and Smart Tagging run inference through `ort`. Left alone, `ort` downloads its
own ONNX Runtime during the build — fine for development, wrong for a distro package, which
must build from declared dependencies rather than an unpinned network fetch.

`src-tauri/Cargo.toml` enables ort's `pkg-config` feature. `ort-sys` probes for
`libonnxruntime` *before* its download path and returns early when it finds a usable one, so
the change is additive: a machine with `onnxruntime-cpu` installed links the system library
and downloads nothing, and a machine without one behaves exactly as before.

Two details make this worth guarding rather than trusting:

- ort **ignores** a system library whose minor version is below the C API level it targets
  (24, from its default `api-24` feature) and quietly downloads instead.
- The fallback is silent. A missing `.pc` file produces a working package that ignored its
  own dependency.

So `build()` checks for `libonnxruntime` and its version explicitly and fails the build
rather than letting either case slide.

`onnxruntime-cuda` exists in `extra` too, but CUDA support is a compile-time feature
(`faces-cuda`, `smarttags-cuda`), so GPU inference needs a separate package variant rather
than a swapped dependency.

## Cutting a release

1. Make sure `package.json`, `src-tauri/Cargo.toml`, and `src-tauri/tauri.conf.json` all
   carry the same version — calendar versioning, unpadded month (`2026.8.0`, never
   `2026.08.0`, which is not valid semver).
2. Tag and push:
   ```bash
   git tag -a v2026.8.0 -m "ChairPhoto 2026.8.0"
   git push origin v2026.8.0
   ```
3. Create the GitHub release for that tag so the source tarball URL resolves.
4. Set `pkgver` in `PKGBUILD` to match, then fill in the checksums:
   ```bash
   cd packaging
   updpkgsums          # from pacman-contrib
   ```
   `sha256sums` ships as `SKIP` placeholders; a release package must carry real hashes.
5. Build and install locally to verify:
   ```bash
   makepkg -si
   ```
6. Lint before publishing:
   ```bash
   namcap PKGBUILD
   namcap chairphoto-*.pkg.tar.zst
   ```

## Testing without a release

To exercise the recipe against the working tree, build a tarball shaped like GitHub's and
point the recipe at it:

```bash
VER=$(python3 -c "import json;print(json.load(open('../package.json'))['version'])")
git -C .. archive --format=tar.gz --prefix="ChairPhoto-$VER/" -o "/tmp/chairphoto-$VER.tar.gz" HEAD
cp "/tmp/chairphoto-$VER.tar.gz" .
makepkg -si --skipchecksums   # picks the local tarball over the release URL
```

Note this packages committed content only — `git archive` ignores uncommitted changes.

## Updating the package

`pkgrel` increments when the recipe changes but the source does not (a corrected dependency,
a fixed install path). It resets to `1` on every `pkgver` bump.
