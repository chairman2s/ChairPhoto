//! Tauri commands — the bridge between the React frontend and the Rust catalog.
//!
//! Every command takes the shared [`AppState`] (a mutex-guarded open catalog) and
//! returns a serializable value or a string error. Errors are stringified here so
//! the frontend gets a plain message; richer typing can come later if needed.
//!
//! The commands themselves live in the domain submodules below; this file keeps only
//! the state and the helpers more than one domain needs. The imports here are the
//! shared surface the submodules pick up through their `use super::*` — keep them
//! broad enough to serve the submodules, not just this file's own code.

use crate::catalog::{
    Album, Catalog, IptcFields, MetadataEntry, Photo, PhotoLocation, PhotoVersion, PickState,
    Publication, SmartAlbum, Tag, TagGroup, TagTerm, TagWithCount, Volume,
};
use crate::scanner::ScanResult;
use crate::thumbnails::{preview_bytes, thumbnail_bytes};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

// ── Domain submodules ────────────────────────────────────────────────────────
//
// Each holds one domain's `#[tauri::command]`s plus the helpers only that domain
// uses. Anything shared by two or more domains stays in this file (`with_catalog`,
// `AppState`, `app_data_dir`, …). Commands are re-exported flat, so `lib.rs` keeps
// referring to them as `commands::<name>` and the frontend is unaffected.

mod ai;
mod albums;
mod appearance;
mod burst;
mod catalog;
#[cfg(feature = "collage")]
mod collage;
mod culling;
mod develop;
mod editing;
mod export;
mod external;
#[cfg(feature = "faces")]
mod faces;
#[cfg(feature = "flickr")]
mod flickr;
mod graph;
mod images;
mod indexing;
#[cfg(feature = "instagram")]
mod instagram;
#[cfg(feature = "localsend")]
mod localsend;
#[cfg(feature = "map")]
mod map;
// The host-mediated `api.fetch` proxy (#49). Feature-gated because it is the only core
// command needing an HTTP client, and `reqwest` is an optional dependency.
#[cfg(feature = "module-fetch")]
mod net;
mod photos;
mod publications;
// Shared publish/transfer helpers. Compiled for LocalSend and Instagram too: they render
// through the same job-scoped temp directories (`publishing::JobTempDir`).
#[cfg(any(feature = "flickr", feature = "smugmug", feature = "instagram", feature = "localsend"))]
pub(crate) mod publishing;
mod scan;
mod settings;
#[cfg(feature = "slideshow")]
mod slideshow;
#[cfg(feature = "smarttags")]
mod smarttags;
#[cfg(feature = "smugmug")]
mod smugmug;
mod storage;
mod tags;

pub use ai::*;
pub use albums::*;
pub use appearance::*;
pub use burst::*;
pub use culling::*;
pub use catalog::*;
#[cfg(feature = "collage")]
pub use collage::*;
pub use develop::*;
pub use editing::*;
pub use export::*;
pub use external::*;
#[cfg(feature = "faces")]
pub use faces::*;
#[cfg(feature = "flickr")]
pub use flickr::*;
pub use graph::*;
pub use images::*;
pub use indexing::*;
// The application state, job-ownership protocol and shared helpers live in `crate::app`
// (Tauri-free, so a native frontend can link them). Re-exported flat so the domain
// submodules keep picking them up through their `use super::*`, `jobs::` paths included.
pub use crate::app::*;
#[cfg(feature = "instagram")]
pub use instagram::*;
#[cfg(feature = "localsend")]
pub use localsend::*;
#[cfg(feature = "map")]
pub use map::*;
#[cfg(feature = "module-fetch")]
pub use net::*;
pub use photos::*;
pub use publications::*;
pub use scan::*;
pub use settings::*;
#[cfg(feature = "slideshow")]
pub use slideshow::*;
#[cfg(feature = "smarttags")]
pub use smarttags::*;
#[cfg(feature = "smugmug")]
pub use smugmug::*;
pub use storage::*;
pub use tags::*;

/// The Tauri shell's [`EventSink`]: each [`CoreEvent`] becomes the webview event of the
/// same name, with its payload serialized as its own type (so the wire format is exactly
/// what the frontend's listeners were written against).
///
/// A newtype rather than an impl on `AppHandle` itself: `EventSink` is the core crate's
/// trait and `AppHandle` is Tauri's type, so the orphan rule needs a local type between them.
pub struct WebviewEvents<R: tauri::Runtime>(pub tauri::AppHandle<R>);

impl<R: tauri::Runtime> EventSink for WebviewEvents<R> {
    fn send(&self, event: CoreEvent) {
        SendCoreEvent::send(&self.0, event);
    }
}

/// `app.send(CoreEvent::…)` for the commands that hold an `AppHandle` — the same emission as
/// [`WebviewEvents`], as a shell-local trait because the core's `EventSink` cannot be
/// implemented on Tauri's type from this crate (the orphan rule).
pub trait SendCoreEvent {
    fn send(&self, event: CoreEvent);
}

impl<R: tauri::Runtime> SendCoreEvent for tauri::AppHandle<R> {
    fn send(&self, event: CoreEvent) {
        struct Emit<'a, R: tauri::Runtime>(&'a tauri::AppHandle<R>);
        impl<R: tauri::Runtime> crate::app::events::EventVisitor for Emit<'_, R> {
            fn visit<T: serde::Serialize + Clone>(&self, name: &'static str, payload: &T) {
                let _ = tauri::Emitter::emit(self.0, name, payload);
            }
        }
        event.visit(&Emit(self));
    }
}

// The env-var lock the command tests share (`use super::test_env_helpers::EnvGuard`). It is
// the core's file, compiled once more into this crate's test binary: `#[cfg(test)]` items do
// not cross crates, and this binary is its own process, so it needs its own lock anyway.
#[cfg(test)]
#[path = "../../../crates/core/src/app/test_env_helpers.rs"]
pub(crate) mod test_env_helpers;

#[cfg(test)]
mod thread_rules {
    /// Source scan: every `#[tauri::command]` in `commands/` whose body takes the catalog
    /// lock (`with_catalog(`, `with_catalog_blocking(`, `state.catalog.lock()`) must not run on
    /// the main thread — it is either an `async fn` or marked `#[tauri::command(async)]`.
    /// See `with_catalog`'s doc for the freeze this prevents. The scan is deliberately
    /// syntactic: a bare `#[tauri::command]` directly above a `pub fn` whose body (up to
    /// the next line that is exactly `}`) mentions the lock is a failure.
    #[test]
    fn commands_that_take_the_catalog_lock_never_run_on_the_main_thread() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/commands");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = src.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if line.trim() != "#[tauri::command]" {
                    continue;
                }
                // Skip attribute/doc lines to the signature.
                let mut j = i + 1;
                while j < lines.len() && (lines[j].starts_with("#[") || lines[j].starts_with("///")) {
                    j += 1;
                }
                let Some(sig) = lines.get(j) else { continue };
                if !sig.starts_with("pub fn ") {
                    continue; // async fn: fine
                }
                let name = sig["pub fn ".len()..].split('(').next().unwrap_or("?");
                // Body: up to the first line that is exactly "}".
                let end = lines[j..].iter().position(|l| *l == "}").map(|k| j + k).unwrap_or(lines.len());
                let body = lines[j..end].join("\n");
                if body.contains("with_catalog(") || body.contains("with_catalog_blocking(") || body.contains(".catalog.lock()") {
                    offenders.push(format!("{}:{} {name}", path.file_name().unwrap().to_string_lossy(), i + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "sync commands taking the catalog lock on the main thread — mark them \
             #[tauri::command(async)] or make them async fn:\n{}",
            offenders.join("\n")
        );
    }
}
