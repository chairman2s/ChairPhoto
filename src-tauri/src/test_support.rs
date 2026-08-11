//! Shared temp-directory fixture for this crate's unit tests (the `#[cfg(test)] mod tests`
//! blocks scattered across `src/`).
//!
//! A fixture that builds its temp path from a constant — a tag string, a hash of a literal,
//! or a bare filename — resolves to the *same* directory in every `cargo test` process on
//! the machine. Two worktrees, or a targeted run beside a full one, is enough to trigger it:
//! one process's cleanup deletes a directory another process is still writing into, and the
//! failure surfaces far from its actual cause (see issue #45; #29 and #41 each fixed one
//! instance of this before the rest were swept).
//!
//! `TestTmpDir` fixes both halves of the bug:
//! - The path is keyed on the caller's tag *and* the process id *and* a per-process atomic
//!   counter, so two fixtures in the same test, the same fixture called twice, and the same
//!   fixture in two concurrent `cargo test` processes never collide.
//! - Cleanup runs in `Drop`, not at the end of the happy path, so a panicking test still
//!   leaves nothing behind. (Cleanup only at the end of the happy path is how `/tmp` on this
//!   machine reached 7.9 GB of leftovers before #41.)
//!
//! ## Why this file, and not one shared by `tests/` too
//!
//! `tests/*.rs` integration tests link a *normal* (non-`cfg(test)`) build of this crate as
//! their dependency — only the crate's own unit-test binary is built with `--cfg test`. A
//! `#[cfg(test)]` item here is therefore invisible from `tests/`: verified directly while
//! implementing #45 (referencing a `cfg(test)` probe function from an integration test
//! failed to resolve, "configured out"). Making it visible there needs either a Cargo
//! feature plus a self-referential `[dev-dependencies]` entry (prototyped and confirmed
//! working, but it touches `Cargo.toml`/`Cargo.lock`/`lib.rs` and widens what the
//! `--all-features`/`--no-default-features` checks must stay clean over, for a mechanism
//! most Rust reviewers won't have seen before), or a separate crate in a new workspace
//! (strictly more manifest surface for no more benefit). Two small, independent copies —
//! this one and `tests/common/mod.rs` — cost one duplicated ~25-line struct instead, and
//! need no manifest change at all. If this fixture's shape ever changes, update both.
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A test's own temp directory, removed on drop. See the module docs for why both the
/// keying and the `Drop` matter. Derefs to `Path`, so it stands in for the `PathBuf` these
/// fixtures used to return — `dir.join(...)`, `&dir` where `&Path` is expected, etc. all
/// keep working unchanged at call sites.
#[derive(Debug)]
pub(crate) struct TestTmpDir(PathBuf);

impl TestTmpDir {
    /// Create `<temp>/chairphoto-test-<tag>-<pid>-<seq>/`.
    pub(crate) fn new(tag: &str) -> Self {
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "chairphoto-test-{tag}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Consume this fixture directory into a `TestSubPath` at `relative`, so a helper that
    /// hands back a *subdirectory* (e.g. a catalog root nested under the fixture dir, next
    /// to the catalog file) can still return just that one path while keeping the whole
    /// fixture directory alive — and cleaned up on drop — through it.
    pub(crate) fn into_subpath(self, relative: &str) -> TestSubPath {
        let path = self.0.join(relative);
        TestSubPath {
            path,
            _dir: self,
        }
    }
}

impl Deref for TestTmpDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TestTmpDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestTmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A path inside a `TestTmpDir`, carrying the fixture directory's guard along with it.
/// Derefs to the subpath; dropping this drops the *whole* fixture directory (the subpath
/// included), not just the subpath itself.
#[derive(Debug)]
pub(crate) struct TestSubPath {
    path: PathBuf,
    _dir: TestTmpDir,
}

impl Deref for TestSubPath {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestSubPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}
