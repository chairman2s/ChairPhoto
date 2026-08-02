//! "Develop in an external editor" round-trip.
//!
//! No RAW editor offers a synchronous "open a file and return the result" API, but
//! darktable / RawTherapee / ART all support a **sidecar + headless-CLI** round-trip:
//! launch the GUI on the original → the editor writes its develop sidecar
//! (`.xmp` / `.pp3` / `.arp`) next to the RAW → we render the developed JPEG with the
//! editor's `*-cli` and adopt it as a **stacked child** of the original (so it groups under
//! the RAW in the grid + inspector Stack section).
//!
//! darktable 5.6+ AI restore (neural denoise/upscale) is different: it writes a NEW
//! DNG/TIFF next to the original instead of editing a sidecar. While a darktable session
//! is open we watch the folder for those outputs and stack them as they appear — see
//! [`run_gui_watching_ai_outputs`].
//!
//! Runs on a dedicated catalog connection (like scans) so the shared connection keeps
//! serving reads while an interactive edit session is open. See the approved plan.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::catalog::Catalog;
use crate::commands::AppState;
use tauri::{AppHandle, Emitter, Manager, Runtime, State};

/// A supported external develop editor and its default binaries.
struct Editor {
    key: &'static str,
    label: &'static str,
    default_gui: &'static str,
    default_cli: &'static str,
    sidecar_ext: &'static str,
}

const EDITORS: &[Editor] = &[
    Editor { key: "darktable",   label: "darktable",   default_gui: "darktable",   default_cli: "darktable-cli",   sidecar_ext: "xmp" },
    Editor { key: "rawtherapee", label: "RawTherapee", default_gui: "rawtherapee", default_cli: "rawtherapee-cli", sidecar_ext: "pp3" },
    Editor { key: "art",         label: "ART",         default_gui: "ART",         default_cli: "ART-cli",         sidecar_ext: "arp" },
];

fn editor(key: &str) -> Option<&'static Editor> {
    EDITORS.iter().find(|e| e.key == key)
}

/// Resolve an editor command: an explicit Preferences setting wins; otherwise the default
/// binary if it's on `PATH`. `None` = not available (don't offer it / can't run it).
fn resolved_cmd(catalog: &Catalog, key: &str, which: &str, default: &str) -> Option<String> {
    if let Ok(Some(v)) = catalog.get_setting(&format!("editor.{key}.{which}")) {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    on_path(default).then(|| default.to_string())
}

/// Whether a command is runnable: an absolute/relative path is checked directly; a bare
/// name is looked up with `which` (same approach the rest of the backend uses).
fn on_path(cmd: &str) -> bool {
    if cmd.contains('/') {
        return Path::new(cmd).exists();
    }
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn sidecar_candidates(raw: &Path, key: &str) -> Vec<PathBuf> {
    crate::scanner::sidecars::develop_sidecars(raw, key)
}

/// Newest mtime (ns) across an editor's candidate sidecars, or `None` if none exist.
fn latest_sidecar_mtime(raw: &Path, key: &str) -> Option<u128> {
    sidecar_candidates(raw, key)
        .into_iter()
        .filter_map(|p| std::fs::metadata(&p).ok())
        .filter_map(|m| m.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .max()
}

fn existing_sidecar(raw: &Path, key: &str) -> Option<PathBuf> {
    sidecar_candidates(raw, key).into_iter().find(|p| p.is_file())
}

// --- darktable AI-restore outputs (5.6+) ------------------------------------
//
// darktable's neural-restore tools (AI denoise/upscale) don't edit in place: they write a
// NEW file next to the original — `<stem>_<suffix>[_<n>].dng` for raw denoise, `.tif` for
// the RGB tasks (suffixes from _task_suffix() in darktable src/libs/neural_restore.c).
// While a darktable session is open on a photo we watch its folder for these and adopt
// each one as a stacked child, same as a rendered develop result.

const DT_AI_SUFFIXES: &[&str] = &["raw-denoise", "denoise", "upscale-2x", "upscale-4x", "restore"];

/// Whether `candidate` is a darktable AI-restore output derived from `raw`:
/// `<raw stem>_<ai suffix>[_<n>].dng|.tif|.tiff` (case-insensitive extension).
fn is_dt_ai_output(raw: &Path, candidate: &Path) -> bool {
    let (Some(stem), Some(name)) = (
        raw.file_stem().and_then(|s| s.to_str()),
        candidate.file_name().and_then(|s| s.to_str()),
    ) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    let Some(body) = lower
        .strip_suffix(".dng")
        .or_else(|| lower.strip_suffix(".tif"))
        .or_else(|| lower.strip_suffix(".tiff"))
    else {
        return false;
    };
    let Some(rest) = body.strip_prefix(&format!("{}_", stem.to_ascii_lowercase())) else {
        return false;
    };
    DT_AI_SUFFIXES.iter().any(|s| {
        rest == *s
            || rest
                .strip_prefix(s)
                .and_then(|t| t.strip_prefix('_'))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// All AI-restore outputs currently next to `raw`.
fn dt_ai_outputs(raw: &Path) -> Vec<PathBuf> {
    let Some(parent) = raw.parent() else { return Vec::new() };
    let mut found: Vec<PathBuf> = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_dt_ai_output(raw, p))
        .collect();
    found.sort();
    found
}

/// Run the darktable GUI on `raw` and, while it's open, watch the folder for AI-restore
/// outputs. Each new output is adopted as a stacked child as soon as its size is stable
/// across two polls (darktable writes the file progressively). Keeps watching briefly
/// after the GUI exits so a job finishing right at quit isn't missed. Returns the ids of
/// the adopted children (may be empty).
fn run_gui_watching_ai_outputs<R: Runtime>(
    app: &AppHandle<R>,
    gui: &str,
    raw: &Path,
    photo_id: i64,
    db_path: &Path,
    root: &Path,
) -> Result<Vec<i64>, String> {
    let before: HashSet<PathBuf> = dt_ai_outputs(raw).into_iter().collect();
    let mut child = Command::new(gui)
        .arg(raw)
        .spawn()
        .map_err(|e| format!("couldn't launch {gui}: {e}"))?;

    let mut sizes: HashMap<PathBuf, u64> = HashMap::new();
    let mut adopted: Vec<i64> = Vec::new();
    let mut done: HashSet<PathBuf> = HashSet::new();
    let mut catalog: Option<Catalog> = None; // opened lazily on first import
    let mut exited_at: Option<Instant> = None;

    loop {
        if exited_at.is_none() && child.try_wait().ok().flatten().is_some() {
            exited_at = Some(Instant::now());
        }
        for path in dt_ai_outputs(raw) {
            if before.contains(&path) || done.contains(&path) {
                continue;
            }
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            match sizes.get(&path) {
                Some(&prev) if prev == size && size > 0 => {
                    if catalog.is_none() {
                        catalog =
                            Some(Catalog::open_secondary(db_path, root).map_err(|e| e.to_string())?);
                    }
                    match adopt_stacked_child(catalog.as_ref().unwrap(), &path, photo_id) {
                        Ok(id) => {
                            adopted.push(id);
                            emit(app, "stacked", "darktable");
                        }
                        Err(e) => eprintln!(
                            "external-edit: couldn't import AI output {}: {e}",
                            path.display()
                        ),
                    }
                    done.insert(path);
                }
                _ => {
                    sizes.insert(path, size);
                }
            }
        }
        if let Some(t) = exited_at {
            let pending = sizes.keys().any(|p| !done.contains(p));
            // Grace window after exit: catch a file that appeared as the GUI closed, and
            // let an in-flight write stabilize — but never hang on a stuck one.
            if (!pending && t.elapsed() > Duration::from_secs(2))
                || t.elapsed() > Duration::from_secs(20)
            {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
    Ok(adopted)
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DevelopProgress {
    phase: String, // waiting | rendering | stacked | done | nochange | error
    editor: String,
}

fn emit<R: Runtime>(app: &AppHandle<R>, phase: &str, editor: &str) {
    let _ = app.emit(
        "develop:progress",
        DevelopProgress { phase: phase.into(), editor: editor.into() },
    );
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableEditor {
    key: String,
    label: String,
    /// The GUI command is runnable (so we can offer "Edit in …").
    gui: bool,
    /// The CLI command is runnable (so we can auto-render the result).
    cli: bool,
    sidecar: String,
}

/// Which editors are configured/available, for the inspector "Edit in…" menu + Preferences.
#[tauri::command]
pub fn available_editors(state: State<'_, AppState>) -> Result<Vec<AvailableEditor>, String> {
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_ref().ok_or("No catalog is open")?;
    Ok(EDITORS
        .iter()
        .map(|ed| AvailableEditor {
            key: ed.key.into(),
            label: ed.label.into(),
            gui: resolved_cmd(catalog, ed.key, "gui", ed.default_gui).is_some(),
            cli: resolved_cmd(catalog, ed.key, "cli", ed.default_cli).is_some(),
            sidecar: ed.sidecar_ext.into(),
        })
        .collect())
}

/// Resolve the pieces needed to run a develop round-trip under a brief catalog lock, so the
/// long GUI/CLI work below never holds the shared connection.
struct Resolved {
    db_path: PathBuf,
    root: PathBuf,
    raw: PathBuf,
    gui: Option<String>,
    cli: Option<String>,
}

fn resolve(app: &AppHandle, photo_id: i64, ed: &Editor) -> Result<Resolved, String> {
    let state = app.state::<AppState>();
    let guard = state.catalog.lock().map_err(|e| e.to_string())?;
    let catalog = guard.as_ref().ok_or("No catalog is open")?;
    let raw = catalog.require_photo_path(photo_id).map_err(|e| e.to_string())?;
    Ok(Resolved {
        db_path: catalog.db_path().to_path_buf(),
        root: catalog.root().to_path_buf(),
        raw,
        gui: resolved_cmd(catalog, ed.key, "gui", ed.default_gui),
        cli: resolved_cmd(catalog, ed.key, "cli", ed.default_cli),
    })
}

/// Launch the editor GUI on a photo's original and wait for it to close. If the develop
/// sidecar was created/updated, render the developed JPEG via the editor CLI and adopt it as
/// a stacked child (returns its new photo id). Returns `None` if nothing changed (e.g. the
/// editor was already open and handed off, or no edit was made) — use `import_developed` then.
#[tauri::command]
pub async fn develop_in_editor(
    app: AppHandle,
    photo_id: i64,
    editor_key: String,
) -> Result<Option<i64>, String> {
    let ed = editor(&editor_key).ok_or("unknown editor")?;
    let r = resolve(&app, photo_id, ed)?;
    let gui = r
        .gui
        .ok_or_else(|| format!("{} is not configured — set its path in Preferences", ed.label))?;
    let before = latest_sidecar_mtime(&r.raw, ed.key);

    let app2 = app.clone();
    let key = editor_key.clone();
    let (raw, db_path, root, cli) = (r.raw, r.db_path, r.root, r.cli);
    let result = tauri::async_runtime::spawn_blocking(move || -> Result<Option<i64>, String> {
        emit(&app2, "waiting", &key);
        // Wait for the interactive session to finish. Editors may exit non-zero; we key off
        // the sidecar changing, not the exit code. For darktable, additionally watch the
        // folder during the session: its AI restore tools (5.6+) write a new DNG/TIFF next
        // to the original, which we adopt into the stack as it appears.
        let ai_imported = if key == "darktable" {
            run_gui_watching_ai_outputs(&app2, &gui, &raw, photo_id, &db_path, &root)?
        } else {
            Command::new(&gui)
                .arg(&raw)
                .status()
                .map_err(|e| format!("couldn't launch {gui}: {e}"))?;
            Vec::new()
        };

        let changed = match (before, latest_sidecar_mtime(&raw, &key)) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(b), Some(a)) => a > b,
        };
        if !changed {
            // No develop edit — but AI outputs may still have been produced and stacked.
            if let Some(&first) = ai_imported.first() {
                emit(&app2, "done", &key);
                return Ok(Some(first));
            }
            emit(&app2, "nochange", &key);
            return Ok(None);
        }
        let cli = cli.ok_or_else(|| {
            format!("edited, but no CLI is configured to render {}'s result", key)
        })?;
        emit(&app2, "rendering", &key);
        let catalog = Catalog::open_secondary(&db_path, &root).map_err(|e| e.to_string())?;
        let id = render_and_stack(&catalog, &cli, &key, &raw, photo_id)?;
        emit(&app2, "done", &key);
        Ok(Some(id))
    })
    .await
    .map_err(|e| e.to_string())?;

    if result.is_err() {
        emit(&app, "error", &editor_key);
    }
    result
}

/// Render the developed result from the CURRENT sidecar (without relaunching the GUI) and
/// adopt it as a stacked child. The manual fallback for when the editor was already open, or
/// to re-render after further edits. Errors if no develop sidecar exists.
#[tauri::command]
pub async fn import_developed(
    app: AppHandle,
    photo_id: i64,
    editor_key: String,
) -> Result<i64, String> {
    let ed = editor(&editor_key).ok_or("unknown editor")?;
    let r = resolve(&app, photo_id, ed)?;
    let has_sidecar = existing_sidecar(&r.raw, ed.key).is_some();
    // darktable may have produced AI-restore outputs without a develop edit (see the
    // watcher above) — those count as an importable result too.
    let has_ai = ed.key == "darktable" && !dt_ai_outputs(&r.raw).is_empty();
    if !has_sidecar && !has_ai {
        return Err(format!("no {} sidecar next to this photo yet — edit it first", ed.label));
    }
    let cli = if has_sidecar {
        Some(r.cli.ok_or_else(|| {
            format!("no CLI configured to render {}'s result", ed.label)
        })?)
    } else {
        None
    };
    let (raw, db_path, root, key) = (r.raw, r.db_path, r.root, editor_key.clone());
    let app2 = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || -> Result<i64, String> {
        emit(&app2, "rendering", &key);
        let catalog = Catalog::open_secondary(&db_path, &root).map_err(|e| e.to_string())?;
        // Adopt any AI-restore outputs first (idempotent for already-indexed ones), so
        // "Import result" works after a handoff to an already-open darktable instance.
        let mut ai_first: Option<i64> = None;
        if key == "darktable" {
            for path in dt_ai_outputs(&raw) {
                match adopt_stacked_child(&catalog, &path, photo_id) {
                    Ok(id) => {
                        ai_first.get_or_insert(id);
                        emit(&app2, "stacked", &key);
                    }
                    Err(e) => eprintln!(
                        "external-edit: couldn't import AI output {}: {e}",
                        path.display()
                    ),
                }
            }
        }
        let id = match cli {
            Some(cli) => render_and_stack(&catalog, &cli, &key, &raw, photo_id)?,
            None => ai_first.ok_or("no importable result found")?,
        };
        emit(&app2, "done", &key);
        Ok(id)
    })
    .await
    .map_err(|e| e.to_string())?;
    if result.is_err() {
        emit(&app, "error", &editor_key);
    }
    result
}

/// Run the editor CLI to render `raw` (+ its sidecar) to a fresh `<stem>-<editor>.jpg` next
/// to the original, then index that JPEG and stack it under `raw_photo_id`.
fn render_and_stack(
    catalog: &Catalog,
    cli: &str,
    key: &str,
    raw: &Path,
    raw_photo_id: i64,
) -> Result<i64, String> {
    let out = unique_output_path(raw, key)?;
    run_cli(cli, key, raw, &out)?;
    if !out.is_file() {
        return Err(format!("{cli} produced no output file"));
    }
    adopt_stacked_child(catalog, &out, raw_photo_id)
}

/// Index `out` on the RAW's volume (writes a UUID sidecar, merge-safe), fill in
/// EXIF/dimensions + external-editor attribution, and group it under `raw_photo_id`.
/// Idempotent: re-adopting an already-indexed file only re-asserts the stack parent.
fn adopt_stacked_child(catalog: &Catalog, out: &Path, raw_photo_id: i64) -> Result<i64, String> {
    let (id, created, _unchanged) = crate::scanner::upsert_external_one(catalog, out)?;
    if created {
        let mut meta = crate::metadata::extract_batch(&[out.to_path_buf()]);
        if let Some(m) = meta.remove(out) {
            let _ = catalog.set_photo_metadata(id, &m.promoted, &m.entries);
        }
        let editors = crate::scanner::sidecars::detect_external_editors_joined(out);
        let _ = catalog.set_external_editors(id, &editors);
    }
    catalog.set_stack_parent(id, raw_photo_id).map_err(|e| e.to_string())?;
    Ok(id)
}

/// `<parent>/<stem>-<editor>.jpg`, bumping `-2`, `-3`… if a file already exists.
fn unique_output_path(raw: &Path, key: &str) -> Result<PathBuf, String> {
    let parent = raw.parent().ok_or("photo has no parent folder")?;
    let stem = raw
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("photo has no filename")?;
    let mut candidate = parent.join(format!("{stem}-{key}.jpg"));
    let mut n = 2;
    while candidate.exists() {
        candidate = parent.join(format!("{stem}-{key}-{n}.jpg"));
        n += 1;
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ai_output_matcher() {
        let raw = Path::new("/photos/DSC01234.ARW");
        let hit = |name: &str| is_dt_ai_output(raw, &Path::new("/photos").join(name));

        // the documented darktable 5.6 suffixes, DNG + TIFF, with collision counters
        assert!(hit("DSC01234_raw-denoise.dng"));
        assert!(hit("DSC01234_raw-denoise_1.dng"));
        assert!(hit("DSC01234_denoise.tif"));
        assert!(hit("DSC01234_upscale-2x.tif"));
        assert!(hit("DSC01234_upscale-4x_12.tiff"));
        assert!(hit("DSC01234_restore.dng"));
        assert!(hit("DSC01234_raw-denoise.DNG")); // extension case-insensitive

        // not ours
        assert!(!hit("DSC01234.ARW"));
        assert!(!hit("DSC01234-darktable.jpg")); // our own rendered result
        assert!(!hit("DSC01234_raw-denoise.jpg")); // wrong extension
        assert!(!hit("DSC09999_raw-denoise.dng")); // different stem
        assert!(!hit("DSC01234_something.dng")); // unknown suffix
        assert!(!hit("DSC01234_raw-denoise_x.dng")); // non-numeric counter
        assert!(!hit("DSC012345_raw-denoise.dng")); // stem is a prefix, not equal
    }
}

fn run_cli(cli: &str, key: &str, raw: &Path, out: &Path) -> Result<(), String> {
    let mut cmd = Command::new(cli);
    match key {
        // darktable-cli <input> [<xmp>] <output> [options]; core options after --core.
        // --library :memory: lets it render even while the GUI holds the library lock.
        "darktable" => {
            cmd.arg(raw);
            if let Some(sc) = existing_sidecar(raw, key) {
                cmd.arg(sc);
            }
            cmd.arg(out)
                .args(["--out-ext", "jpg", "--hq", "true"])
                .args(["--core", "--library", ":memory:"]);
        }
        // rawtherapee-cli / ART-cli: -s uses the sidecar; -c must be last (input).
        "rawtherapee" | "art" => {
            cmd.args(["-Y", "-o"])
                .arg(out)
                .args(["-j95", "-s", "-c"])
                .arg(raw);
        }
        _ => return Err(format!("unknown editor: {key}")),
    }
    let output = cmd
        .output()
        .map_err(|e| format!("couldn't run {cli}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{cli} render failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(())
}
