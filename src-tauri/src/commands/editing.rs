//! Non-destructive editing commands: the per-photo edit record, render (single and
//! batch), the .cube LUT library, and named photo versions.
//!
//! Originals are never modified — see `docs/editing.md` and `plugins/edit/`.

use super::*;
#[cfg(feature = "edit")]
use crate::media::{render_edit_bytes, working_image};
use tauri::{AppHandle, Manager, State};

/// Read a photo's edit record (opaque JSON), or null if it has none.
#[tauri::command(async)]
pub fn get_edit_record(state: State<'_, AppState>, photo_id: i64) -> Result<Option<String>, String> {
    with_catalog(&state, |c| c.get_edit_record(photo_id))
}

/// Replace a photo's edit record. An empty value clears it. Core validates only that
/// the value is well-formed JSON; editing modules own its contents.
#[tauri::command(async)]
pub fn set_edit_record(
    state: State<'_, AppState>,
    photo_id: i64,
    edit_json: String,
) -> Result<(), String> {
    with_catalog(&state, |c| c.set_edit_record(photo_id, &edit_json))
}

/// Render a photo's preview proxy with an edit record applied (crop + tone), returning
/// a base64 data URL (`data:image/jpeg;base64,…`) — the loupe window's renderer and the
/// legacy Develop view. The Darkroom stage does not use this: it loads the same render
/// through the native `edit://` protocol (see [`render_edit_bytes`]). Operates on the
/// embedded preview — never the original file. Errors if the `edit` feature is compiled
/// out. `maxEdge` (0 = full) caps the result size for fast live preview. `hiRes` renders
/// from the native-size preview tier instead of the fast 2048 one — the loupe requests it
/// when zooming into a version (a cropped render of the 2048 proxy can be smaller than
/// the window, leaving nothing to zoom into).
#[tauri::command]
pub async fn render_edit(
    app: AppHandle,
    photo_id: i64,
    edit_json: String,
    max_edge: u32,
    hi_res: Option<bool>,
) -> Result<String, String> {
    #[cfg(not(feature = "edit"))]
    {
        let _ = (&app, photo_id, &edit_json, max_edge, hi_res);
        Err("Editing backend not included in this build".into())
    }
    #[cfg(feature = "edit")]
    {
        let job = crate::image_pool::EditJob {
            photo_id,
            edit_json,
            max_edge,
            hi_res: hi_res.unwrap_or(false),
            base_only: false,
            source: crate::plugins::edit::SourceToken::Preview,
            clip: false,
            catalog_epoch: 0,
        };
        // Render and base64-wrap on a blocking worker: both are CPU work, neither belongs
        // on the async thread.
        crate::app::spawn_blocking(move || {
            let bytes = render_edit_bytes(&app.state::<AppState>(), &job)?;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok(format!("data:image/jpeg;base64,{b64}"))
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[cfg(feature = "edit")]
use crate::app::editing::parse_working_token;

/// Render a photo's preview proxy with *several* edit records in one call — the
/// preset browser's thumbnail strip. The proxy is decoded and pre-scaled **once**
/// (presets never crop, so a shared downscale is valid), then each record renders on
/// a copy: N thumbnails cost roughly one preview render. Per-record failures yield
/// `None` instead of failing the batch. All decode work runs on a blocking worker.
#[tauri::command]
pub async fn render_edit_batch(
    app: AppHandle,
    photo_id: i64,
    edit_jsons: Vec<String>,
    max_edge: u32,
    source: Option<String>,
) -> Result<Vec<Option<String>>, String> {
    #[cfg(not(feature = "edit"))]
    {
        let _ = (&app, photo_id, &edit_jsons, max_edge, &source);
        Err("Editing backend not included in this build".into())
    }
    #[cfg(feature = "edit")]
    {
        use image::GenericImageView;
        let state = app.state::<AppState>();
        let candidates = {
            let guard = state.catalog.lock().map_err(|e| e.to_string())?;
            let catalog = guard.as_ref().ok_or("No catalog is open")?;
            catalog.photo_path_candidates(photo_id).map_err(|e| e.to_string())?
        };
        let health = state.volume_health.clone();
        crate::app::spawn_blocking(move || {
            // OriginalRequired: an edit render needs the real original, so a
            // cached-unreachable flag must never stand in for a stat.
            let path = crate::volume_health::pick_existing(
                &candidates,
                &health,
                crate::catalog::ResolveMode::OriginalRequired,
            )
            .ok_or_else(|| format!("no reachable copy of photo {photo_id}"))?;
            if let Some(token) = parse_working_token(source.as_deref())? {
                // Working image: every record renders through the framed-base cache (one
                // geometry per batch, so one downscale) — no shared pre-scale needed.
                let image = working_image(&token)?;
                return Ok(edit_jsons
                    .iter()
                    .map(|ej| {
                        crate::plugins::edit::render_proxy(
                            crate::plugins::edit::RenderSource::Working { token: token.clone(), image: image.clone() },
                            ej,
                            max_edge,
                            crate::plugins::edit::RenderOpts::default(),
                        )
                        .and_then(|out| crate::plugins::edit::encode_jpeg(&out, 82))
                        .ok()
                        .map(|bytes| {
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            format!("data:image/jpeg;base64,{b64}")
                        })
                    })
                    .collect());
            }
            let jpeg = crate::thumbnails::preview_bytes(&path)?;
            let mut img = crate::plugins::edit::decode_proxy_cached(&jpeg)?;
            // Pre-scale once to ~2× the thumbnail edge; each per-record render then
            // only pushes a few hundred kilopixels through the look pipeline.
            if max_edge > 0 {
                let (w, h) = img.dimensions();
                let target = max_edge.saturating_mul(2);
                if w.max(h) > target {
                    img = img.thumbnail(target, target);
                }
            }
            Ok(edit_jsons
                .iter()
                .map(|ej| {
                    crate::plugins::edit::render_image(img.clone(), ej, max_edge)
                        .and_then(|out| crate::plugins::edit::encode_jpeg(&out, 82))
                        .ok()
                        .map(|bytes| {
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            format!("data:image/jpeg;base64,{b64}")
                        })
                })
                .collect())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

/// Share of pixels per Darkroom tone-strip zone — 8 equal gamma-luma bands of the
/// photo's **rendered working state**, so the strip always describes the print it sits
/// under (docs/plans/darkroom). Same lock-then-worker shape as `render_edit`.
#[tauri::command]
pub async fn edit_zone_masses(
    app: AppHandle,
    photo_id: i64,
    edit_json: String,
    source: Option<String>,
) -> Result<[f32; 8], String> {
    #[cfg(not(feature = "edit"))]
    {
        let _ = (&app, photo_id, &edit_json, &source);
        Err("Editing backend not included in this build".into())
    }
    #[cfg(feature = "edit")]
    {
        crate::app::spawn_blocking(move || {
            crate::app::editing::zone_masses(&app.state::<AppState>(), None, photo_id, &edit_json, source.as_deref())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

/// Classical auto-tone suggestion for the Darkroom's proof sheet: a percentile analysis
/// of the proxy's luma histogram → an edit-json *fragment* with only
/// `tone.ev/contrast/highlights/shadows` (docs/plans/darkroom). A learned model can
/// replace the internals later without this surface changing.
#[tauri::command]
pub async fn suggest_auto_tone(
    app: AppHandle,
    photo_id: i64,
    source: Option<String>,
    base_json: Option<String>,
) -> Result<String, String> {
    #[cfg(not(feature = "edit"))]
    {
        let _ = (&app, photo_id, &source, &base_json);
        Err("Editing backend not included in this build".into())
    }
    #[cfg(feature = "edit")]
    {
        let state = app.state::<AppState>();
        let candidates = {
            let guard = state.catalog.lock().map_err(|e| e.to_string())?;
            let catalog = guard.as_ref().ok_or("No catalog is open")?;
            catalog.photo_path_candidates(photo_id).map_err(|e| e.to_string())?
        };
        let health = state.volume_health.clone();
        crate::app::spawn_blocking(move || {
            let path = crate::volume_health::pick_existing(
                &candidates,
                &health,
                crate::catalog::ResolveMode::OriginalRequired,
            )
            .ok_or_else(|| format!("no reachable copy of photo {photo_id}"))?;
            // On the RAW engine the analysis reads what the stage shows as-shot: the working
            // image through the record's base (`base_json`: engine, display transform and
            // camera match, no adjustments) — the fragment's EV then means linear stops on
            // the picture being developed, not on the camera's JPEG.
            let rgb = if let Some(token) = parse_working_token(source.as_deref())? {
                let image = working_image(&token)?;
                crate::plugins::edit::render_proxy(
                    crate::plugins::edit::RenderSource::Working { token, image },
                    base_json.as_deref().unwrap_or(r#"{"engine":2}"#),
                    1024,
                    crate::plugins::edit::RenderOpts::default(),
                )?
                .to_rgb8()
            } else {
                let jpeg = crate::thumbnails::preview_bytes(&path)?;
                crate::plugins::edit::decode_proxy_cached(&jpeg)?.to_rgb8()
            };
            let a = crate::plugins::edit::auto_tone_for(&rgb);
            Ok(serde_json::json!({
                "tone": {
                    "ev": a.ev,
                    "contrast": a.contrast,
                    "highlights": a.highlights,
                    "shadows": a.shadows,
                }
            })
            .to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

// --- LUTs (user-supplied .cube files for the editor, see docs/editing.md) --

/// List the `.cube` LUT filenames available in the app's luts folder.
#[tauri::command]
pub async fn list_luts() -> Result<Vec<String>, String> {
    crate::app::spawn_blocking(|| crate::app::editing::list_luts_in(&luts_dir()?))
        .await
        .map_err(|e| e.to_string())?
}

/// Validate a `.cube` file (by fully parsing it) and copy it into the luts folder.
/// Returns the bare filename an edit record can reference.
#[tauri::command]
pub async fn import_lut(path: String) -> Result<String, String> {
    #[cfg(not(feature = "edit"))]
    {
        let _ = path;
        Err("Editing backend not included in this build".into())
    }
    #[cfg(feature = "edit")]
    {
        crate::app::spawn_blocking(move || crate::app::editing::import_lut_into(&luts_dir()?, std::path::Path::new(&path)))
            .await
            .map_err(|e| e.to_string())?
    }
}

/// Delete a LUT from the luts folder. Edit records referencing it keep rendering
/// (without the LUT) — missing files are non-fatal by design.
#[tauri::command]
pub async fn delete_lut(file: String) -> Result<(), String> {
    if file.contains('/') || file.contains('\\') || file.is_empty() {
        return Err("bad LUT filename".into());
    }
    crate::app::spawn_blocking(move || {
        std::fs::remove_file(luts_dir()?.join(&file)).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// --- photo versions (crop/exposure variants, see docs/editing.md) ---------

#[tauri::command(async)]
pub fn list_versions(state: State<'_, AppState>, photo_id: i64) -> Result<Vec<PhotoVersion>, String> {
    with_catalog(&state, |c| c.list_versions(photo_id))
}

#[tauri::command(async)]
pub fn create_version(
    state: State<'_, AppState>,
    photo_id: i64,
    name: String,
) -> Result<i64, String> {
    with_catalog(&state, |c| c.create_version(photo_id, &name))
}

#[tauri::command(async)]
pub fn rename_version(
    state: State<'_, AppState>,
    version_id: i64,
    name: String,
) -> Result<(), String> {
    with_catalog(&state, |c| c.rename_version(version_id, &name))
}

/// Save a version's edit record. With the `edit` feature on, this also refreshes the
/// photo's monochrome flag (H6): a B&W develop (bw mixer / saturation -1) on *any*
/// version marks the photo monochrome; when the last B&W version is edited away, the
/// flag falls back to the pixel-derived signal (recomputed from the cached thumbnail
/// off the lock). The auto-tag refresh only runs when the flag actually changes, so
/// debounced slider saves stay cheap.
#[tauri::command]
pub async fn set_version_edit(
    app: AppHandle,
    version_id: i64,
    edit_json: String,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    #[cfg(not(feature = "edit"))]
    {
        with_catalog(&state, |c| c.set_version_edit(version_id, &edit_json))
    }
    #[cfg(feature = "edit")]
    {
        set_version_edit_in_state(&state, version_id, &edit_json).await
    }
}

/// The testable body of [`set_version_edit`] under the `edit` feature: takes `&AppState`
/// directly (rather than a Tauri-wrapped `State`) so tests can drive it without a live app.
#[cfg(feature = "edit")]
async fn set_version_edit_in_state(
    state: &AppState,
    version_id: i64,
    edit_json: &str,
) -> Result<(), String> {
    let edit_json = edit_json.to_string();
    write_version_then_refresh_monochrome(state, version_id, move |c| c.set_version_edit(version_id, &edit_json)).await
}

/// Any write that changes a version's settings, followed by the monochrome refresh every
/// such write owes (H6), on a blocking worker (`app::editing::write_version_then_refresh_monochrome`).
#[cfg(feature = "edit")]
async fn write_version_then_refresh_monochrome<T: Send + 'static>(
    state: &AppState,
    version_id: i64,
    write: impl FnOnce(&crate::catalog::Catalog) -> crate::catalog::Result<T> + Send + 'static,
) -> Result<T, String> {
    let state = state.clone();
    crate::app::spawn_blocking(move || {
        crate::app::editing::write_version_then_refresh_monochrome(&state, None, version_id, write)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Make a version the photo's cover — the look its Library thumbnail shows — or clear it
/// with `null`. Returns the new cover token (`"version:rev"`), which the grid puts in the
/// thumbnail URL. Only a reference is stored; the thumbnail is rendered from the settings.
#[tauri::command(async)]
pub fn set_cover_version(
    state: State<'_, AppState>,
    photo_id: i64,
    version_id: Option<i64>,
) -> Result<Option<String>, String> {
    with_catalog(&state, |c| c.set_cover_version(photo_id, version_id))
}

/// A version's edit history (the Darkroom's History panel).
#[tauri::command(async)]
pub fn version_history(state: State<'_, AppState>, version_id: i64) -> Result<crate::catalog::VersionHistory, String> {
    with_catalog(&state, |c| c.version_history(version_id))
}

/// Save a version's settings as a history step (the Darkroom's autosave). `amend`
/// replaces the current step — the same control still moving. Settings only.
#[tauri::command]
pub async fn commit_version_edit(
    app: AppHandle,
    version_id: i64,
    edit_json: String,
    label: String,
    amend: bool,
) -> Result<crate::catalog::VersionHistory, String> {
    let state = app.state::<AppState>();
    #[cfg(not(feature = "edit"))]
    {
        with_catalog(&state, |c| c.commit_version_edit(version_id, &edit_json, &label, amend))
    }
    #[cfg(feature = "edit")]
    {
        write_version_then_refresh_monochrome(&state, version_id, move |c| {
            c.commit_version_edit(version_id, &edit_json, &label, amend)
        })
        .await
    }
}

/// Step the version to history step `seq` (undo, redo, or a click in the History panel).
/// Returns the step's settings and the history.
#[tauri::command]
pub async fn goto_version_step(
    app: AppHandle,
    version_id: i64,
    seq: i64,
) -> Result<(String, crate::catalog::VersionHistory), String> {
    let state = app.state::<AppState>();
    #[cfg(not(feature = "edit"))]
    {
        with_catalog(&state, |c| c.goto_version_step(version_id, seq))
    }
    #[cfg(feature = "edit")]
    {
        write_version_then_refresh_monochrome(&state, version_id, move |c| c.goto_version_step(version_id, seq)).await
    }
}

#[tauri::command(async)]
pub fn delete_version(state: State<'_, AppState>, version_id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.delete_version(version_id))
}

#[tauri::command(async)]
pub fn duplicate_version(state: State<'_, AppState>, version_id: i64) -> Result<i64, String> {
    with_catalog(&state, |c| c.duplicate_version(version_id))
}

#[tauri::command(async)]
pub fn reorder_versions(
    state: State<'_, AppState>,
    photo_id: i64,
    ordered_ids: Vec<i64>,
) -> Result<(), String> {
    with_catalog(&state, |c| c.reorder_versions(photo_id, &ordered_ids))
}

#[tauri::command]
pub async fn version_counts(
    state: State<'_, AppState>,
    photo_ids: Vec<i64>,
) -> Result<Vec<(i64, i64)>, String> {
    with_catalog_blocking(&state, move |c| c.version_counts(&photo_ids)).await
}

// --- publications (where a photo was posted + which version, see docs/publications.md) ---

#[cfg(all(test, feature = "edit"))]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, LocationRole, VolumeKind};
    use crate::volume_health::VolumeHealth;
    use std::time::Duration;

    fn temp_catalog(tag: &str) -> (Catalog, crate::test_support::TestTmpDir, std::path::PathBuf) {
        let dir = crate::test_support::TestTmpDir::new(&format!("set-version-edit-{tag}"));
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("t.chairphoto"), &root).unwrap();
        (catalog, dir, root)
    }

    fn state_with(catalog: Catalog, health: VolumeHealth) -> AppState {
        let mut state = AppState::default();
        *state.catalog.lock().unwrap() = Some(catalog);
        state.volume_health = std::sync::Arc::new(health);
        state
    }

    /// A photo whose only copy lives on a separate "NAS" volume, with a version that has
    /// no B&W edit, and `photos.grayscale` pre-seeded `true` (as if a real B&W develop had
    /// set it earlier) — the exact state `set_version_edit_in_state`'s pixel-derived
    /// fallback runs from. Mirrors `photo_on_detachable_nas` in
    /// `catalog/locations.rs` (same fixture technique, reused rather than reinvented).
    fn stale_grayscale_photo_on_nas(
        catalog: &Catalog,
        dir: &crate::test_support::TestTmpDir,
        root: &std::path::Path,
    ) -> (i64, i64, std::path::PathBuf, i64) {
        let id = catalog
            .upsert_photo(&root.join("archive/a.jpg"), None, 1, 1)
            .unwrap()
            .id;
        let nas_base = dir.join("nas");
        let nas_file = nas_base.join("archive/a.jpg");
        std::fs::create_dir_all(nas_file.parent().unwrap()).unwrap();
        std::fs::write(&nas_file, b"not-actually-decoded-while-online").unwrap();
        let nas = catalog.add_volume("NAS", &nas_base, VolumeKind::Backup).unwrap();
        catalog
            .add_location(id, nas, "archive/a.jpg", LocationRole::Backup)
            .unwrap();

        catalog.set_grayscale(id, true).unwrap();
        catalog.apply_auto_tags().unwrap();
        let version_id = catalog.create_version(id, "V1").unwrap();
        (id, nas, nas_base, version_id)
    }

    /// Write a small, solidly-coloured JPEG (well above the `is_grayscale_jpeg` chroma
    /// threshold) so a decode of it is unambiguously "not grayscale".
    fn write_colour_jpeg(path: &std::path::Path) {
        let img = image::RgbImage::from_pixel(32, 32, image::Rgb([200, 30, 30]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut bytes, 90,
            ))
            .unwrap();
        std::fs::write(path, bytes.into_inner()).unwrap();
    }

    /// A history commit is a settings write like any other: committing a B&W develop marks
    /// the photo monochrome and tags it, and stepping back to colour — the original still
    /// reachable and decoding as colour — clears both. Through the same refresh as a save.
    #[test]
    fn history_commits_and_steps_refresh_the_monochrome_flag() {
        let (catalog, _dir, root) = temp_catalog("history-bw");
        let file = root.join("c.jpg");
        write_colour_jpeg(&file);
        let id = catalog.upsert_photo(&file, None, 1, 1).unwrap().id;
        let v = catalog.create_version(id, "V1").unwrap();
        let state = state_with(catalog, VolumeHealth::with_ttl(Duration::MAX));
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tagged = |state: &AppState| {
            let guard = state.catalog.lock().unwrap();
            let c = guard.as_ref().unwrap();
            (
                c.is_grayscale(id).unwrap(),
                c.get_photo_tags(id).unwrap().iter().any(|t| t.full_path == "Treatment/Black & White"),
            )
        };

        let bw = r#"{"bw":{"enabled":true,"r":0.3,"g":0.6,"b":0.1}}"#;
        let h = rt
            .block_on(write_version_then_refresh_monochrome(&state, v, move |c| c.commit_version_edit(v, bw, "B&W Neutral", false)))
            .unwrap();
        assert_eq!(h.head, Some(1));
        assert_eq!(tagged(&state), (true, true), "a B&W commit marks and tags the photo");

        rt.block_on(write_version_then_refresh_monochrome(&state, v, move |c| c.goto_version_step(v, 0)))
            .unwrap();
        assert_eq!(tagged(&state), (false, false), "stepping back to colour clears both");
    }

    /// **Offline original.** The photo's only copy sits on a volume renamed away (a
    /// genuinely unreachable NAS, not merely a stale reachability flag — see the module
    /// doc on `pick_existing`: `OriginalRequired` always re-verifies a cached-unreachable
    /// candidate, so only an actually-missing file makes it return `None`). Forced, not
    /// waited for, following the `#9` fixture technique in `volume_health.rs` /
    /// `catalog/locations.rs`.
    ///
    /// Recomputing must leave both `photos.grayscale` and the monochrome auto-tag exactly
    /// as they were — "could not tell" is not "not grayscale" (AGENTS.md: missing/unmounted
    /// storage is normal, never evidence the row is wrong).
    #[test]
    fn recompute_leaves_grayscale_and_autotags_when_original_is_offline() {
        let (catalog, dir, root) = temp_catalog("offline");
        let (photo_id, nas, nas_base, version_id) = stale_grayscale_photo_on_nas(&catalog, &dir, &root);
        assert!(
            catalog.get_photo_tags(photo_id).unwrap().iter().any(|t| t.full_path == "Treatment/Black & White"),
            "sanity: the photo starts tagged monochrome"
        );

        // Force the offline condition: rename the NAS mount away, let a refresh cache it
        // unreachable, and leave it detached (a real unmounted NAS, not a restored one).
        let detached = dir.join("nas-detached");
        std::fs::rename(&nas_base, &detached).unwrap();
        let health = VolumeHealth::with_ttl(Duration::MAX);
        health.refresh(&[(nas, nas_base.to_string_lossy().to_string())]);
        assert_eq!(health.reachable(nas), Some(false), "sanity: cached unreachable");

        let state = state_with(catalog, health);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(set_version_edit_in_state(&state, version_id, "{}"))
            .unwrap();

        let guard = state.catalog.lock().unwrap();
        let catalog = guard.as_ref().unwrap();
        assert!(
            catalog.is_grayscale(photo_id).unwrap(),
            "an unreachable original must not clear the stored grayscale flag"
        );
        assert!(
            catalog.get_photo_tags(photo_id).unwrap().iter().any(|t| t.full_path == "Treatment/Black & White"),
            "the monochrome auto-tag must survive an offline recompute untouched"
        );
    }

    /// **Reachable but undecodable original.** The NAS is up and the file is right where
    /// the catalog says it is, but its bytes are not a decodable image (a damaged file, an
    /// unsupported format `image` can't parse and ImageMagick can't rescue). This is the
    /// second failure mode the fix also has to cover — `pick_existing` succeeds but
    /// `thumbnail_bytes` fails — and it must be treated exactly like "could not tell", not
    /// "not grayscale".
    #[test]
    fn recompute_leaves_grayscale_when_reachable_original_fails_to_decode() {
        let (catalog, _dir, root) = temp_catalog("undecodable");
        std::fs::create_dir_all(root.join("archive")).unwrap();
        let path = root.join("archive/broken.jpg");
        std::fs::write(&path, b"this is not a jpeg, magick and image both give up").unwrap();
        let photo_id = catalog.upsert_photo(&path, None, 1, 1).unwrap().id;
        catalog.set_grayscale(photo_id, true).unwrap();
        catalog.apply_auto_tags().unwrap();
        let version_id = catalog.create_version(photo_id, "V1").unwrap();

        let state = state_with(catalog, VolumeHealth::with_ttl(Duration::MAX));
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(set_version_edit_in_state(&state, version_id, "{}"))
            .unwrap();

        let guard = state.catalog.lock().unwrap();
        let catalog = guard.as_ref().unwrap();
        assert!(
            catalog.is_grayscale(photo_id).unwrap(),
            "a reachable-but-undecodable original must not clear the stored grayscale flag"
        );
        assert!(
            catalog.get_photo_tags(photo_id).unwrap().iter().any(|t| t.full_path == "Treatment/Black & White"),
            "the monochrome auto-tag must survive an undecodable recompute untouched"
        );
    }

    /// **The positive path.** A reachable, decodable, genuinely colourful original must
    /// still clear a stale `true` flag — the fix must not "fix" this bug by never writing.
    #[test]
    fn recompute_clears_grayscale_for_a_genuinely_colour_photo() {
        let (catalog, _dir, root) = temp_catalog("colour");
        std::fs::create_dir_all(root.join("archive")).unwrap();
        let path = root.join("archive/colour.jpg");
        write_colour_jpeg(&path);
        let photo_id = catalog.upsert_photo(&path, None, 1, 1).unwrap().id;
        catalog.set_grayscale(photo_id, true).unwrap();
        catalog.apply_auto_tags().unwrap();
        let version_id = catalog.create_version(photo_id, "V1").unwrap();

        let state = state_with(catalog, VolumeHealth::with_ttl(Duration::MAX));
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(set_version_edit_in_state(&state, version_id, "{}"))
            .unwrap();

        let guard = state.catalog.lock().unwrap();
        let catalog = guard.as_ref().unwrap();
        assert!(
            !catalog.is_grayscale(photo_id).unwrap(),
            "a genuinely colour photo must still have its stale grayscale flag cleared"
        );
        assert!(
            !catalog.get_photo_tags(photo_id).unwrap().iter().any(|t| t.full_path == "Treatment/Black & White"),
            "the monochrome auto-tag must be removed once the flag correctly clears"
        );
    }
}
