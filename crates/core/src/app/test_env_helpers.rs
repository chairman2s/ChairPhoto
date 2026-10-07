//! Process-wide serialization of env-var mutations in tests; see the rules above
//! `test_env_helpers` in `app/mod.rs`. One `ENV_LOCK` per test binary: the core's unit tests
//! compile this file once (the Tauri shell's did too, via `#[path]`, until #165).

use std::sync::Mutex;

/// Process-wide lock for env-var mutations. **One static for the whole crate.**
pub static ENV_LOCK: Mutex<()> = Mutex::new(());

pub struct EnvGuard {
    vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
    pub _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvGuard {
    pub fn set(key: &'static str, value: &str) -> Self {
        Self::set_all(&[(key, value)])
    }

    /// Set several env vars at once, under one `ENV_LOCK` acquisition. Needed whenever a
    /// test wants more than one var changed: the lock is a plain, non-reentrant `Mutex`, so
    /// two separate `set` calls live at once in the same test would deadlock (the second
    /// blocks forever on a lock its own first guard already holds).
    pub fn set_all(vars: &[(&'static str, &str)]) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Isolate before saving, so the drop restores the isolated directories: a guard taken
        // before the process's first `isolate` would save the developer's own `XDG_DATA_HOME`
        // (often `~/.local/share` itself), and putting it back after a test inside the guard
        // isolated would undo the isolation for every later test in the process.
        crate::test_home::isolate();
        let saved = vars
            .iter()
            .map(|(key, value)| {
                let original = std::env::var_os(key);
                std::env::set_var(key, value);
                (*key, original)
            })
            .collect();
        EnvGuard { vars: saved, _lock: lock }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, original) in &self.vars {
            match original {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Restoring after isolation ─────────────────────────────────────────────────

    /// A guard's drop never puts back the real home's data or cache directory, even when the
    /// guard is the first thing in the process to touch them and the developer's shell exports
    /// `XDG_DATA_HOME=~/.local/share`. Run alone (`cargo test -p chairphoto-core --lib
    /// a_dropped_guard`) it is that first guard; in the full run an earlier test may have
    /// isolated already.
    #[test]
    fn a_dropped_guard_leaves_the_isolated_directories() {
        let guard = EnvGuard::set_all(&[("XDG_DATA_HOME", "/nonexistent-a"), ("XDG_CACHE_HOME", "/nonexistent-b")]);
        // What a test inside the guard does by resolving either directory.
        crate::test_home::isolate();
        drop(guard);
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Resolved through the real functions: passing `test_home::check` is the assertion.
        let _ = crate::thumbnails::cache_dir();
        let _ = crate::app::app_data_dir().unwrap();
    }
}
