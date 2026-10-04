//! Process-wide serialization of env-var mutations in tests; see the rules above
//! `test_env_helpers` in `app/mod.rs`. One `ENV_LOCK` per test binary: the core's unit tests
//! compile this file once (the Tauri shell's did too, via `#[path]`, until #165).

use std::sync::Mutex;

/// Process-wide lock for env-var mutations. **One static for the whole crate.**
pub static ENV_LOCK: Mutex<()> = Mutex::new(());

pub struct EnvGuard {
    pub key: &'static str,
    pub original: Option<std::ffi::OsString>,
    pub _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvGuard {
    pub fn set(key: &'static str, value: &str) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let original = std::env::var_os(key);
        std::env::set_var(key, value);
        EnvGuard { key, original, _lock: lock }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}
