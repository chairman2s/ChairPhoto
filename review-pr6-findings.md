# Review findings — PR #6

`feat: make ONNX Runtime optional, and document the module capability gaps`
Reviewed at `010027c`. CI green (Backend 5m0s, Frontend 18s).

Self-review: the same session wrote this code, so these are the defects found by
re-examining it, not an independent assessment.

All findings below are resolved as of `203afec` unless their **Status** says otherwise.
Each finding keeps its original wording; status lines were added afterwards.
See [Resolution](#resolution) at the end.

---

## 1. The probe never checks the API version ort actually requests

**Severity:** high — leaves the change's stated goal partly unmet
**Where:** `src-tauri/src/plugins/onnx.rs:39`, `:114`
**Status:** fixed in `6e78000`. `get_api(MIN_MINOR)` is called and a null return rejected.
Adding the check risked rejecting a *good* runtime, so it was verified the other way too:
the probe still accepts the installed 1.27.1 and real inference still detects 4 faces.

`probe_at` validates the runtime's *version string* and stops there. ort does more:

```rust
let api = ((*base).GetApi)(ort_sys::ORT_API_VERSION);
NonNull::new(api.cast_mut()).expect("Failed to initialize ORT API")
```

A runtime reporting `1.27` that returns null for API 24 passes the probe and then panics
inside ort. That is the exact failure class `plugins::onnx` exists to prevent — the probe
answers "is a new-enough library present?" when the question that matters is "will ort's
initialisation succeed?".

The `get_api` field is declared in the `OrtApiBase` struct for precisely this and is never
called. No dead-code warning fires because rustc skips that lint for `#[repr(C)]` types, so
nothing pointed at it.

**Fix:** call `get_api(MIN_MINOR)` after the version check and reject a null return, with an
error naming the API version that was refused.

---

## 2. The probe unloads ONNX Runtime, then ort loads it again

**Severity:** medium — latent stability risk, one-line fix
**Where:** `src-tauri/src/plugins/onnx.rs:96-129`
**Status:** fixed in `6e78000`. The handle is leaked on success via `std::mem::forget`, and
the comment that defended the drop is replaced with why it is kept.

`lib` is dropped at the end of `probe_at`, which calls `dlclose`. Nothing else holds a
reference at that point, so the refcount reaches zero and the library may be fully unloaded —
running its destructors — before ort maps it afresh moments later.

Unload/reload cycles are a known hazard for libraries with atexit handlers and thread-local
destructors, both of which ONNX Runtime has. The probe does not create a session, so no
thread pools exist yet, which makes this lower risk than it could be — but it is risk taken
for no benefit.

The doc comment currently defends the drop:

> Unloading and reloading the same shared object is cheap because the OS keeps it mapped for
> the process.

That is only true while another reference exists. At probe time none does, so the comment
asserts something false about the situation it describes.

**Fix:** `std::mem::forget(lib)` — a deliberate, bounded leak of one handle per process.
ort's subsequent `dlopen` then becomes a refcount bump rather than a remap, so this is both
safer and faster. Replace the comment with why the handle is intentionally kept.

---

## 3. `MIN_MINOR` duplicates the `api-24` Cargo feature with nothing linking them

**Severity:** medium — silent future breakage
**Where:** `src-tauri/src/plugins/onnx.rs:31`, `src-tauri/Cargo.toml` (ort feature list)
**Status:** properly fixed in `203afec`. The first attempt (`6e78000`) did not work and was
described too generously here as a "tripwire": `assert_eq!(MIN_MINOR, 24)` only fires when
someone edits *the constant*, which is the harmless direction. The dangerous direction —
`Cargo.toml` moving to `api-25` while the constant stays — passed straight through it. It
could not detect the drift at all, under a name and doc comment claiming it did.

ort already exposes the value: `pub const MINOR_VERSION: u32 = ort_sys::ORT_API_VERSION`
(`ort/src/lib.rs:86`), computed from whichever `api-*` feature is enabled. `MIN_MINOR` is now
derived from it, so the drift class is gone rather than watched for, and the misleading test
is deleted. The derived value resolves to 24, identical to the literal it replaced.

That trade introduces a smaller risk in its place: the floor is no longer readable in the
file, so a bad derivation would be invisible, and a floor of `0` would accept every runtime
while every test still passed. The probe test now prints the floor and asserts it is at least
17, the oldest API ort supports — checking the derivation rather than restating the value.

The probe's floor is a hand-written constant. ort's requirement comes from its `api-24`
feature. Upgrade ort to `api-25` and the probe keeps accepting 1.24 runtimes, ort rejects
them, and the indefinite hang returns for exactly the users this change protects.

The two live in different files and there is no compile-time relationship, so nothing fails
when they drift.

**Fix:** name `MIN_MINOR` explicitly in the `Cargo.toml` comment beside the ort feature list.
The comment currently points at `plugins::onnx` in general, which is not specific enough to
survive a hurried version bump. A stronger option is deriving the floor from the enabled
`api-*` feature via `cfg!`, at the cost of a small ladder.

---

## 4. A malformed version string is reported as "too old"

**Severity:** low — misleading diagnosis
**Where:** `src-tauri/src/plugins/onnx.rs:117-127`
**Status:** fixed in `6e78000`. An unparseable version now reports that it could not be
read, separately from being below the floor.

`unwrap_or(0)` turns any unparseable version into `0`, which then fails the `< MIN_MINOR`
check. A user with an unusual build gets told to upgrade a runtime that may be current.

**Fix:** distinguish "could not parse version X" from "version X is below the floor".

---

## 5. Accepted risk — CI cannot exercise the probe's success path

**Severity:** informational; no fix proposed
**Where:** `src-tauri/src/plugins/onnx.rs:155`, `.github/workflows/ci.yml`
**Status:** unchanged and still accepted. CI continues to prove only that the probe rejects
bad runtimes. The `get_api` check added for finding 1 widens what an untested success path
now covers, which makes this slightly more consequential than when first written.

`probe_accepts_an_installed_runtime` skips on CI because runners have no ONNX Runtime, and
apt offers no package to install one. Every ONNX-dependent test skips there for the same
reason.

So CI proves the probe *rejects* bad runtimes and never proves it *accepts* good ones. A
regression that made it reject valid runtimes — a mistyped symbol, a broken version parse —
would keep CI green and disable face tagging and Smart Tagging for every user who has the
runtime installed. The probe now gates whether those modules work at all, so it is the single
point whose success path is untested.

Worth recording rather than fixing: installing ONNX Runtime on the runner means fetching a
binary, which cuts against the build's current hermeticity.

---

## Packaging note

`check()` inverting to assert *absence* of linkage is correct and guards something no other
step would catch. It does trade away incidental coverage: the previous assertion proved the
binary could reach a runtime, so the package could not ship with inference wholly unwired.
The new one cannot. A package can now build, pass `check()`, and ship with faces
non-functional.

Inherent to making the dependency optional rather than a defect, but it means the packaging
step no longer says anything about whether inference works.

`optdepends` also carries no version constraint while the code requires ≥ 1.24. This degrades
correctly — a 1.23 runtime produces the probe's clear error rather than a hang — so it is
cosmetic, but stating the minimum in the optdepends text would save a support round trip.

**Status:** the version note is fixed in `6e78000` (`onnxruntime-cpu: … (1.24 or newer)`).
The coverage trade is **not** fixed and is inherent: the package can still build, pass
`check()`, and ship with inference non-functional. `check()` now asserts only that the
runtime is *not* linked.

---

## What holds up

- Both failure paths are asserted unconditionally and need no runtime to run.
- Errors name the fix (`Install onnxruntime (Arch: onnxruntime-cpu)`) rather than just the
  symptom.
- The deliberate wording split between "could not be loaded" and the model managers' "not
  available" prevents a test from passing for the wrong reason, and is commented where it
  matters.
- The change follows the existing optional-tool pattern (`exiftool`, `ffmpeg`, ImageMagick)
  rather than inventing a new one.
- Verified in both runtime conditions, which is what caught the CI failure the first attempt
  missed.

---

## Suggested order

1. **#1** and **#2** before merge — #1 leaves the goal partly unmet, #2 is a one-line
   improvement to a latent risk.
2. **#3** with them; it is a comment and prevents a silent future regression.
3. **#4** whenever.
4. **#5** decide and record; no code change proposed.

---

## Codex review addendum

Reviewed by Codex on 2026-08-07 at `010027c56d0a618acc83c668681e02965668d09c`
against base `2ee763cfb068f233f2661727fd21291d95bd7dbe`.

This addendum records the PR-review findings from a separate pass. It does not supersede
the self-review above.

### Standards findings

1. `packaging/README.md:14` is stale after the PR: it still says `onnxruntime-cpu` is a
   hard dependency, and `packaging/README.md:32` still describes the removed
   `pkg-config` / build-time `libonnxruntime` guard. That contradicts the new
   `optdepends` recipe behavior in `packaging/PKGBUILD:47`.

2. `src-tauri/Cargo.toml:57` still says the `ort` `download-binaries` strategy fetches a
   CUDA-enabled ONNX Runtime for `faces-cuda`, but this PR disables default `ort`
   features and uses `load-dynamic`.

3. Low-severity duplication smell: the runtime preflight and test skip logic are repeated
   in the faces and smarttags paths:
   `src-tauri/src/plugins/faces/engine.rs:324`,
   `src-tauri/src/plugins/smarttags/embed.rs:284`,
   `src-tauri/tests/faces_engine.rs:25`, and
   `src-tauri/src/plugins/smarttags/embed.rs:384`. This is not blocking unless the pattern
   grows.

### Spec finding

1. `packaging/PKGBUILD:111` says it guards against `libonnxruntime` appearing in
   `NEEDED`, but it uses `ldd | grep`. That checks loader resolution output, not the ELF
   dynamic section directly. The PR body specifically claims a `NEEDED` invariant, so this
   should use `readelf -d ... | grep NEEDED` or equivalent.

### Verification

- `cargo check --all-features --all-targets` passed.
- `cargo check --no-default-features` passed.
- `cargo test` passed when rerun outside the sandbox. The sandboxed run failed only because
  local loopback binding for map mock-server tests was denied at
  `src-tauri/src/plugins/map/geocode.rs:501`.
- Targeted missing-runtime tests with
  `ORT_DYLIB_PATH=/nonexistent/libonnxruntime.so` passed/skipped cleanly without hanging.
- `git diff --check 2ee763cfb068f233f2661727fd21291d95bd7dbe...HEAD` passed.

### Status of the Codex findings

All addressed in `6e78000` except the duplication smell.

1. **Stale `packaging/README.md`** — fixed. The section headed "Why `onnxruntime-cpu` is a hard
   dependency" argued the reverse of what the recipe now does; it is rewritten as "…is an
   *optional* dependency", the deleted `pkg-config` guard description is gone, and a GPU
   subsection was added.
2. **Stale `Cargo.toml:57`** — fixed. It claimed `download-binaries` fetches a CUDA-enabled
   runtime. The comment now records the actual behaviour, which this PR improved without
   anyone noticing: `onnxruntime-cuda` provides the same soname and conflicts with
   `onnxruntime-cpu`, so one binary runs against either and the installed package decides.
3. **Duplication smell** — **not fixed, deliberately.** Two lines at two call sites, and the two
   skip helpers live in different scopes (integration test vs unit-test module), so unifying
   costs more than it saves. Worth revisiting if a third ONNX consumer appears.
4. **Spec finding, `ldd` vs `NEEDED`** — fixed. `check()` reads `readelf -d … | grep NEEDED`.
   Because an assertion that always passes is indistinguishable from one whose invariant holds,
   it was checked against a positive control: the same pipeline detects `libraw`, which *is*
   linked.

### Found by the follow-up sweep, in neither review

**`README.md` was stale and is the most user-visible of the five files.** Its requirements table
listed exiftool, exiv2, ffmpeg, ImageMagick and LibRaw but not ONNX Runtime, and the Arch
install line omitted it — so a reader could install everything documented and still have face
tagging fail. Fixed in `6e78000`: added to the requirements table, both install sections, and
the modules table (which now distinguishes runtime cost from build-time cost).

Neither review listed it. It surfaced only by grepping every ONNX mention repo-wide rather
than reviewing the files already under change — which is the lesson the Codex findings taught,
since all three of theirs were the same class.

---

## Resolution

Fixed in `6e78000`, across five files:

| File | Addresses |
|---|---|
| `src-tauri/src/plugins/onnx.rs` | self-review 1, 2, 3 (redone in `203afec`), 4 |
| `src-tauri/Cargo.toml` | Codex 2, self-review 3 |
| *(follow-up)* `203afec` | self-review 3, properly |
| `packaging/PKGBUILD` | Codex spec, packaging note |
| `packaging/README.md` | Codex 1 |
| `README.md` | follow-up sweep |

Deliberately unresolved: self-review 5 (CI blind spot), the `check()` coverage trade, and
Codex 3 (duplication).

An error was caught during the fixing itself: the first `optdepends` line advertised
`onnxruntime-cuda` as the GPU option, but this package does not build `faces-cuda`, so that
runtime would satisfy the dependency and still run on CPU. Corrected before commit.

**Verification at `6e78000`:** `cargo check --all-features --all-targets`, `cargo check
--no-default-features`, and `cargo test` run twice — with the runtime present and with
`ORT_DYLIB_PATH` pointed at a nonexistent file — all exit 0, 524 passed, 0 failed, no warnings.
`makepkg` exits 0 with `check()` executing; namcap reports only the known `ld-linux` false
positive. CI green on `6e78000` (run 31171578795).

### Follow-up review pass (`203afec`)

A second Codex pass confirmed findings 1, 2, 4, the packaging spec finding, and the doc
cleanups, and rejected the fix for finding 3 as not catching the drift it claimed to. That was
correct — see finding 3's status. Fixed by deriving from `ort::MINOR_VERSION`.

That pass explicitly did not rerun `makepkg` or the full test suite, so both were rerun here
after the change: `cargo test` in both runtime conditions (523 passed, 0 failed — one fewer
than `6e78000`, the deleted tripwire), plus a full package build.

Two review passes have now each caught a class the other missed. The self-review went deep on
the new file and ignored what the change invalidated elsewhere; the follow-up caught that blast
radius and then caught a fix that asserted the wrong invariant under a confident name. The
recurring failure is not missing detail — it is stating a conclusion more strongly than the
evidence supports.
