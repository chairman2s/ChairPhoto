# Review findings — PR #6

`feat: make ONNX Runtime optional, and document the module capability gaps`
Reviewed at `010027c`. CI green (Backend 5m0s, Frontend 18s).

Self-review: the same session wrote this code, so these are the defects found by
re-examining it, not an independent assessment.

Nothing here is fixed. Four findings plus one accepted risk.

---

## 1. The probe never checks the API version ort actually requests

**Severity:** high — leaves the change's stated goal partly unmet
**Where:** `src-tauri/src/plugins/onnx.rs:39`, `:114`

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

`unwrap_or(0)` turns any unparseable version into `0`, which then fails the `< MIN_MINOR`
check. A user with an unusual build gets told to upgrade a runtime that may be current.

**Fix:** distinguish "could not parse version X" from "version X is below the floor".

---

## 5. Accepted risk — CI cannot exercise the probe's success path

**Severity:** informational; no fix proposed
**Where:** `src-tauri/src/plugins/onnx.rs:155`, `.github/workflows/ci.yml`

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
