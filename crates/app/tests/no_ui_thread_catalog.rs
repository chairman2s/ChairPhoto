//! Static guard: catalog-locking core calls never run on the UI thread.
//!
//! `with_catalog*` takes the catalog mutex, which a worker can hold for seconds (the Tauri
//! shell once froze the window 2.2 s on it). Every call under `crates/app/src` must therefore
//! sit in a function that hands the work to an off-thread executor (`Runner`, GPUI's background
//! executor, a spawned thread), or be listed in `ALLOWED` with the reason it is safe.
//!
//! This is a textual scan, deliberately conservative: it looks back from each call to the
//! enclosing `fn` for an off-thread marker. It cannot prove the closure itself is the one
//! handed over, so review the call when adding an allowance.

use std::fs;
use std::path::{Path, PathBuf};

const CALLS: &[&str] = &["with_catalog(", "with_catalog_as(", "with_catalog_identified(", "catalog.lock()"];

const MARKERS: &[&str] = &[
    "background_executor",
    "Runner::get",
    ".run(",
    ".spawn(",
    "spawn_blocking",
    "thread::spawn",
    "cx.spawn",
];

/// (file suffix, trimmed source line prefix, why this is safe).
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "darkroom/session/rails.rs",
        "with_catalog_as(state,",
        "the `work` closure of `run_op`, which runs it on `Runner` (rails.rs `Runner::get(cx).run(move || work(&state))`)",
    ),
    (
        "darkroom/session/rails.rs",
        "move |state| with_catalog_as(state,",
        "the `work` closure of `run_op`, which runs it on `Runner`",
    ),
    (
        "loupe/cull.rs",
        "if let Err(e) = with_catalog_as(app,",
        "`save_cursor`, only called inside `background_executor().spawn` (cull.rs, both callers)",
    ),
    (
        "modules/ai_tagging/state.rs",
        "with_catalog_identified(app,",
        "the `work` closure of `run_off`, which runs it on `Runner`",
    ),
    ("modules/ai_tagging/state.rs", "with_catalog_as(app,", "the `work` closure of `run_off`, which runs it on `Runner`"),
    ("modules/ai_tagging/state.rs", "move |app| with_catalog_as(app,", "the `work` closure of `run_off`, which runs it on `Runner`"),
    (
        "modules/map/state.rs",
        "with_catalog_identified(app,",
        "`read_points`, documented as a worker's job and only called from the module's off-thread `run`",
    ),
    (
        "modules/map/state.rs",
        "if let Err(e) = with_catalog_as(&app,",
        "the `then` callback of `MachinePrefs::modify_durably`, which runs it on `Runner` off the UI thread",
    ),
    (
        "preferences/mod.rs",
        "with_catalog_as(&self.state,",
        "`Scope::catalog`; a `Scope` is only handed to the `work` closures of `Ctx::run*`, which run on `Runner`",
    ),
];

const LOOK_BACK: usize = 80;

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "tests") {
                continue;
            }
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let name = path.file_name().unwrap().to_string_lossy();
            if name == "tests.rs" || name.ends_with("_tests.rs") {
                continue;
            }
            out.push(path);
        }
    }
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

fn is_fn_header(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("fn ")
        || t.starts_with("pub fn ")
        || t.starts_with("pub(crate) fn ")
        || t.starts_with("pub(super) fn ")
        || t.starts_with("async fn ")
        || t.starts_with("pub async fn ")
}

/// Violations in one file's source: `(line number, line)` for each call with no off-thread
/// marker between the enclosing `fn` and the call, and no allowance.
fn violations(file: &str, src: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = src.lines().collect();
    // Inline `#[cfg(test)]` modules sit at the end of the file by convention.
    let end = lines.iter().position(|l| l.trim() == "#[cfg(test)]").unwrap_or(lines.len());
    let mut found = Vec::new();
    for i in 0..end {
        let line = lines[i];
        if is_comment(line) || !CALLS.iter().any(|c| line.contains(c)) {
            continue;
        }
        // A `use` or a definition is not a call.
        if line.trim_start().starts_with("use ") || line.contains("fn with_catalog") {
            continue;
        }
        let mut safe = false;
        for j in (i.saturating_sub(LOOK_BACK)..=i).rev() {
            let l = lines[j];
            if !is_comment(l) && MARKERS.iter().any(|m| l.contains(m)) {
                safe = true;
                break;
            }
            if j != i && is_fn_header(l) {
                break;
            }
        }
        let allowed = ALLOWED.iter().any(|(f, p, _)| file.ends_with(f) && line.trim_start().starts_with(p));
        if !safe && !allowed {
            found.push((i + 1, line.trim().to_string()));
        }
    }
    found
}

#[test]
fn catalog_calls_run_off_the_ui_thread() {
    let mut files = Vec::new();
    rs_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    assert!(files.len() > 20, "scan found only {} files", files.len());
    let mut bad = Vec::new();
    let mut calls = 0;
    for f in &files {
        let src = fs::read_to_string(f).unwrap();
        calls += src.lines().filter(|l| !is_comment(l) && CALLS.iter().any(|c| l.contains(c))).count();
        for (n, l) in violations(&f.to_string_lossy(), &src) {
            bad.push(format!("{}:{n}: {l}", f.display()));
        }
    }
    assert!(calls > 20, "scan saw only {calls} catalog calls; the patterns are stale");
    assert!(
        bad.is_empty(),
        "catalog-locking calls with no off-thread executor in their function (run them on \
         `Runner`/`cx.background_executor()`, or allow-list with a reason):\n{}",
        bad.join("\n")
    );
}

// ---- the scanner itself ----

#[test]
fn scanner_flags_a_bare_call_and_passes_a_spawned_one() {
    let bare = "fn on_click() {\n    let v = with_catalog(&state, |c| c.x());\n}\n";
    assert_eq!(violations("a.rs", bare).len(), 1);
    let spawned = "fn on_click(cx: &mut App) {\n    cx.background_executor().spawn(async move {\n        with_catalog(&state, |c| c.x())\n    });\n}\n";
    assert!(violations("a.rs", spawned).is_empty());
    let locked = "fn f() {\n    let g = self.catalog.lock();\n}\n";
    assert_eq!(violations("a.rs", locked).len(), 1);
}

#[test]
fn scanner_does_not_borrow_a_previous_functions_marker() {
    let src = "fn a(cx: &App) {\n    Runner::get(cx).run(|| ());\n}\nfn b() {\n    with_catalog_as(&s, f, |c| c.x());\n}\n";
    assert_eq!(violations("a.rs", src).len(), 1);
}
