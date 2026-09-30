//! Temp-directory fixture shared by the integration tests in this directory.
//!
//! This is the `tests/` sibling of `src/test_support.rs` — see that file's module docs for
//! the bug this fixes (constant-keyed temp paths collide across concurrent `cargo test`
//! processes) and for why it isn't literally the same module: `tests/*.rs` link a *normal*
//! (non-`cfg(test)`) build of the library, so a `cfg(test)` item defined there is invisible
//! here. This is a small, independent, near-identical copy instead. If this fixture's shape
//! ever changes, update both.
//!
//! Not itself a test binary: unlike a hypothetical `tests/common.rs`, Cargo's `tests/*.rs`
//! auto-discovery does not match a `mod.rs` nested under a subdirectory, so this file is
//! only compiled where a test file opts in with `mod common;`.
#![allow(dead_code)]

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A test's own temp directory, removed on drop. Derefs to `Path`, so it stands in for the
/// `PathBuf` these fixtures used to return — `dir.join(...)`, `&dir` where `&Path` is
/// expected, etc. all keep working unchanged at call sites.
#[derive(Debug)]
pub struct TestTmpDir(PathBuf);

impl TestTmpDir {
    /// Create `<temp>/chairphoto-test-<tag>-<pid>-<seq>/`.
    pub fn new(tag: &str) -> Self {
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
    pub fn into_subpath(self, relative: &str) -> TestSubPath {
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
pub struct TestSubPath {
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
