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

const CALLS: &[&str] = &[
    "with_catalog(",
    "with_catalog_as(",
    "with_catalog_identified(",
    "with_catalog_from(",
    "with_catalog_blocking(",
    "catalog.lock()",
];

const MARKERS: &[&str] = &[
    "background_executor",
    "Runner::get",
    "spawn_blocking",
    "thread::spawn",
];

/// `.run(` / `.spawn(` hand work off only when given a closure or a GPUI context (`self.run(cx,
/// move |app| ..)`, `Runner::get(cx).run(move || ..)`); a bare `child.spawn()` or `x.run(1)` is
/// not an executor. `cx.spawn` is deliberately not a marker: it runs on the UI thread.
const GENERIC_MARKERS: &[&str] = &[".run(", ".spawn("];

fn is_marker(line: &str) -> bool {
    MARKERS.iter().any(|m| line.contains(m))
        || (GENERIC_MARKERS.iter().any(|m| line.contains(m))
            && (line.trim_end().ends_with('(')
                || ["cx", "move |", "move ||", "|| ", "|_|", "async"].iter().any(|h| line.contains(h))))
}

/// (file suffix, enclosing fn, trimmed source line prefix, why this is safe).
const ALLOWED: &[(&str, &str, &str, &str)] = &[
    (
        "darkroom/session/rails.rs",
        "switch_now",
        "with_catalog_as(state,",
        "the `work` closure of `run_op`, which runs it on `Runner` (rails.rs `Runner::get(cx).run(move || work(&state))`)",
    ),
    (
        "darkroom/session/rails.rs",
        "toggle_cover",
        "move |state| with_catalog_as(state,",
        "the `work` closure of `run_op`, which runs it on `Runner`",
    ),
    (
        "loupe/cull.rs",
        "save_cursor",
        "if let Err(e) = with_catalog_as(app,",
        "`save_cursor`, only called inside `background_executor().spawn` (cull.rs, both callers)",
    ),
    (
        "modules/ai_tagging/state.rs",
        "reload_settings",
        "with_catalog_identified(app,",
        "the `work` closure of `run_off`, which runs it on `Runner`",
    ),
    ("modules/ai_tagging/state.rs", "save", "with_catalog_as(app,", "the `work` closure of `run_off`, which runs it on `Runner`"),
    ("modules/ai_tagging/state.rs", "follow_photo", "move |app| with_catalog_as(app,", "the `work` closure of `run_off`, which runs it on `Runner`"),
    (
        "modules/map/state.rs",
        "read_points",
        "with_catalog_identified(app,",
        "`read_points`, documented as a worker's job and only called from the module's off-thread `run`",
    ),
    (
        "modules/map/state.rs",
        "migrate_consent",
        "if let Err(e) = with_catalog_as(&app,",
        "the `then` callback of `MachinePrefs::modify_durably`, which runs it on `Runner` off the UI thread",
    ),
    (
        "preferences/mod.rs",
        "catalog",
        "with_catalog_as(&self.state,",
        "`Scope::catalog`; a `Scope` is only handed to the `work` closures of `Ctx::run*`, which run on `Runner`",
    ),
    ("modules/ai_tagging/state.rs", "write", "move |app| with_catalog_as(app,", "the `work` closure of `run_off`, which runs it on `Runner`"),
    (
        "modules/tag_graph/mod.rs",
        "catalog_source",
        "with_catalog(&app,",
        "the `GraphSource` closure, documented as blocking and run on a background thread",
    ),
    (
        "modules/mod.rs",
        "get",
        "with_catalog_as(&self.app,",
        "`ModuleSettings::get`, blocking by contract; every caller in the app runs it inside `Runner`",
    ),
    ("modules/mod.rs", "set", "with_catalog_as(&self.app,", "`ModuleSettings::set`, blocking by contract; every caller runs it inside `Runner`"),
    ("modules/mod.rs", "set_all", "with_catalog_as(&self.app,", "`ModuleSettings::set_all`, blocking by contract; callers run it inside `Runner`"),
    ("shell/state.rs", "read_lists", "with_catalog_identified(state,", "`read_lists`, only called inside `background_executor().spawn`"),
    ("shell/state.rs", "read_counts", "let pending = with_catalog(state,", "`read_counts`, only called inside `background_executor().spawn`"),
    ("shell/state.rs", "read_counts", "let identity_debt = with_catalog(state,", "`read_counts`, only called inside `background_executor().spawn`"),
    ("shell/state.rs", "read_counts", "let trash = with_catalog(state,", "`read_counts`, only called inside `background_executor().spawn`"),
    ("model.rs", "read_summary", "with_catalog_identified(state,", "`read_summary`, only called inside `background_executor().spawn`"),
    ("model.rs", "resolve_link", "let (from, photo) = with_catalog_identified(state,", "`resolve_link`, only called inside `background_executor().spawn`"),
    ("model.rs", "resolve_link", "let tags = with_catalog(state,", "`resolve_link`, only called inside `background_executor().spawn`"),
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
    fn_name(line).is_some()
}

/// The name in a fn header line, tolerating `pub`, `pub(..)` (incl. `pub(in ..)`), `const`,
/// `async`, `unsafe` and `extern "C"` in front of `fn`.
fn fn_name(line: &str) -> Option<&str> {
    let mut t = line.trim_start();
    loop {
        if let Some(r) = t.strip_prefix("pub(") {
            t = r.split_once(')')?.1.trim_start();
            continue;
        }
        let mut stripped = false;
        for q in ["pub ", "const ", "async ", "unsafe ", "default "] {
            if let Some(r) = t.strip_prefix(q) {
                t = r.trim_start();
                stripped = true;
            }
        }
        if let Some(r) = t.strip_prefix("extern ") {
            let r = r.trim_start();
            t = if r.starts_with('"') { r[1..].split_once('"')?.1.trim_start() } else { r };
            stripped = true;
        }
        if !stripped {
            break;
        }
    }
    let rest = t.strip_prefix("fn ")?;
    let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    (end > 0).then(|| &rest[..end])
}

/// Line indices covered by `#[cfg(test)] mod .. { .. }`, brace-matched. A `#[cfg(test)]` on a
/// field, item or `use` is not a module and hides nothing.
fn test_module_lines(lines: &[&str]) -> Vec<bool> {
    let mut skip = vec![false; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == "#[cfg(test)]" {
            let mut j = i + 1;
            while j < lines.len() && (lines[j].trim().is_empty() || lines[j].trim_start().starts_with("#[")) {
                j += 1;
            }
            let head = lines.get(j).map(|l| l.trim_start()).unwrap_or("");
            let is_mod = head.starts_with("mod ") || head.starts_with("pub mod ") || head.starts_with("pub(crate) mod ");
            if is_mod && !head.trim_end().ends_with(';') {
                let mut depth = 0i32;
                let mut opened = false;
                let mut k = j;
                while k < lines.len() {
                    for c in lines[k].chars() {
                        match c {
                            '{' => {
                                depth += 1;
                                opened = true;
                            }
                            '}' => depth -= 1,
                            _ => {}
                        }
                    }
                    if opened && depth <= 0 {
                        break;
                    }
                    k += 1;
                }
                let last = k.min(lines.len() - 1);
                for s in skip.iter_mut().take(last + 1).skip(i) {
                    *s = true;
                }
                i = last;
            }
        }
        i += 1;
    }
    skip
}

/// Name of the nearest fn header at or above line `i`.
fn enclosing_fn<'a>(lines: &[&'a str], i: usize) -> Option<&'a str> {
    (0..=i).rev().find_map(|j| if is_comment(lines[j]) { None } else { fn_name(lines[j]) })
}

/// Violations in one file's source: `(line number, line)` for each call with no off-thread
/// marker between the enclosing `fn` and the call, and no allowance.
fn violations(file: &str, src: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = src.lines().collect();
    let skip = test_module_lines(&lines);
    let mut found = Vec::new();
    for i in 0..lines.len() {
        let line = lines[i];
        if skip[i] || is_comment(line) || !CALLS.iter().any(|c| line.contains(c)) {
            continue;
        }
        // A `use` or a definition is not a call.
        if line.trim_start().starts_with("use ") || line.contains("fn with_catalog") {
            continue;
        }
        let mut safe = false;
        for j in (i.saturating_sub(LOOK_BACK)..=i).rev() {
            let l = lines[j];
            if !is_comment(l) && is_marker(l) {
                safe = true;
                break;
            }
            if j != i && is_fn_header(l) {
                break;
            }
        }
        let encl = enclosing_fn(&lines, i);
        let allowed = ALLOWED
            .iter()
            .any(|(f, func, p, _)| file.ends_with(f) && encl == Some(*func) && line.trim_start().starts_with(p));
        if !safe && !allowed {
            found.push((i + 1, format!("[in fn {}] {}", encl.unwrap_or("catalog"), line.trim())));
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

#[test]
fn scanner_scans_code_after_a_cfg_test_field_or_item() {
    let src = "struct S {\n    #[cfg(test)]\n    probe: u8,\n}\nfn on_click() {\n    with_catalog(&state, |c| c.x());\n}\n";
    assert_eq!(violations("a.rs", src).len(), 1);
}

#[test]
fn scanner_skips_only_the_cfg_test_module() {
    let src = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {\n        with_catalog(&s, |c| c.x());\n        if true {\n        }\n    }\n}\nfn b() {\n    with_catalog(&s, |c| c.y());\n}\n";
    let v = violations("a.rs", src);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].0, 11);
}

#[test]
fn scanner_reads_every_fn_qualifier_and_new_calls() {
    for h in ["unsafe fn f() {", "const fn f() {", "pub(in crate::a) fn f() {", "pub async fn f() {", "pub(crate) unsafe fn f() {"] {
        assert!(is_fn_header(h), "{h}");
    }
    let src = "fn ok() { Runner::get(cx).run(|| ()); }\nunsafe fn b() {\n    with_catalog_from(&s, f, |c| c.x());\n}\n";
    assert_eq!(violations("a.rs", src).len(), 1);
    let blocking = "const fn b() {\n    with_catalog_blocking(&s, |c| c.x());\n}\n";
    assert_eq!(violations("a.rs", blocking).len(), 1);
}

#[test]
fn an_allowance_is_tied_to_its_function() {
    let src = "fn other() {\n    with_catalog_as(&self.state, id, f)\n}\n";
    assert_eq!(violations("preferences/mod.rs", src).len(), 1);
    let src = "fn catalog() {\n    with_catalog_as(&self.state, id, f)\n}\n";
    assert!(violations("preferences/mod.rs", src).is_empty());
}

#[test]
fn a_generic_run_or_spawn_is_not_an_off_thread_marker() {
    let src = "fn f() {\n    child.spawn();\n    let _ = x.run(1);\n    with_catalog(&s, |c| c.x());\n}\n";
    assert_eq!(violations("a.rs", src).len(), 1);
}
