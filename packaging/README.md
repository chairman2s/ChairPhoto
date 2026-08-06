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

## Why `options=(!lto)`

makepkg turns LTO on globally (`OPTIONS=(... lto)` with `LTOFLAGS="-flto=auto"`). Two
dependencies compile C or assembly through the `cc` crate — `libsqlite3-sys`, which builds
the bundled SQLite, and `ring` — so they pick up `-flto=auto` and emit bitcode. rustc then
links with `-fuse-ld=lld` and no `-flto`, so the LTO codegen step never runs and those
objects contribute no symbols:

```
ld.lld: error: undefined symbol: sqlite3_step
ld.lld: error: undefined symbol: ring_core_0_17_14__aes_nohw_encrypt
```

Pure-Rust dependencies link fine, which is why only these two appear. This failure only
shows up under `makepkg` — the flags come from `/etc/makepkg.conf`, not from the project, so
an ordinary `cargo build` never reproduces it.

## Signing, and switching to a verified source

The guidelines ask that sources be verified with PGP signatures wherever possible. The
`sha256sums` in `PKGBUILD` already prove *integrity* — that the tarball has not changed since
the hash was written — but not *authenticity*, that it came from this project at all. A signed
tag closes that, and also defends against a force-pushed tag silently changing what a release
points at.

The signing key exists and the repository is configured to sign tags:

```
F811994B5376B3AF01DD2896589E06E08AD289E9   ed25519, expires 2028-08-05
```

`git config --local tag.gpgsign true` is set, so `git tag` signs by default. Commit signing is
deliberately left off — it prompts for the passphrase on every commit and buys nothing here.

**This is not active yet.** `v2026.8.0` was tagged before the key existed, and a recipe that
demands a signed tag cannot build an unsigned one. Retagging it would be a force-push, which
the guidelines specifically warn against. So the switch below applies at the **next** release,
whose tag will be signed.

### The switch

Apply all of this together, never piecemeal — a signed source with an unsigned tag fails, and
a git source with the tarball's directory layout fails too.

```diff
 makedepends=(
   'cargo'
   'clang'
+  'git'
   'nodejs>=24'
   'npm'
   'pkgconf'
 )

+# git rev-parse "v$pkgver" — the *tag object* hash, not the commit it points at. Pinning the
+# object means a force-pushed tag changes the hash and fails the build instead of silently
+# altering what gets packaged.
+_tag=0000000000000000000000000000000000000000
+validpgpkeys=('F811994B5376B3AF01DD2896589E06E08AD289E9')
+
-source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz"
+source=("$pkgname::git+$url.git?signed#tag=$_tag"
         "$pkgname.desktop")
-sha256sums=('9243bce8e428bf534228c598f0b5e78cb932684adf4fa91965d583bd1ebb2ebf'
+# SKIP is correct for a VCS source: integrity comes from the pinned tag object and its
+# signature, which `?signed` makes makepkg verify against validpgpkeys.
+sha256sums=('SKIP'
             'f886deeefb89b0ac96497bf02e6307539175fbb99a498ba3f40ed8ff14e10f00')
+
+# Guards against bumping pkgver without updating _tag: the version is derived from the tag
+# that was actually checked out.
+pkgver() {
+  cd "$pkgname"
+  git describe --tags | sed 's/^v//'
+}
```

Then replace every `cd "ChairPhoto-$pkgver"` with `cd "$pkgname"` — makepkg names a git
checkout after the `name::` prefix, not `<repo>-<version>` as with a tarball.

Naming the source `$pkgname::` keeps that directory stable across releases; without it the
clone is named after the repository (`ChairPhoto`), which is capitalised differently.

### Publishing the public key

Anyone verifying the tag needs the public key. Export it with:

```bash
gpg --export --armor F811994B5376B3AF01DD2896589E06E08AD289E9
```

Adding it to GitHub is optional and only affects the "Verified" badge on tags. The `gh` CLI
needs a scope the current token lacks:

```bash
gh auth refresh -s admin:gpg_key
gh gpg-key add <(gpg --export --armor F811994B5376B3AF01DD2896589E06E08AD289E9)
```

The key expires 2028-08-05. Extend it with `gpg --edit-key` before then, or signature
verification on new releases starts failing.

## Cutting a release

1. Make sure `package.json`, `src-tauri/Cargo.toml`, and `src-tauri/tauri.conf.json` all
   carry the same version — calendar versioning, unpadded month (`2026.8.0`, never
   `2026.08.0`, which is not valid semver).
2. Tag and push. `-s` signs it; `tag.gpgsign` makes that the default, but being explicit
   documents the intent:
   ```bash
   git tag -s v2026.8.1 -m "ChairPhoto 2026.8.1"
   git verify-tag v2026.8.1        # confirm before pushing
   git push origin v2026.8.1
   ```
3. Create the GitHub release for that tag so the source tarball URL resolves.
4. Set `pkgver` in `PKGBUILD` to match, then refresh the checksums:
   ```bash
   cd packaging
   updpkgsums          # from pacman-contrib
   ```
   Every source needs a real hash. The one exception is a VCS source, where `SKIP` is correct
   because integrity comes from the pinned tag object instead — see the section above.
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
