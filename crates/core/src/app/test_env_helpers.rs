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
