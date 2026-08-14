//! Non-destructive editing commands: the per-photo edit record, render (single and
//! batch), the .cube LUT library, and named photo versions.
//!
//! Originals are never modified — see `docs/editing.md` and `plugins/edit/`.

use super::*;
#[cfg(feature = "edit")]
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};

/// Read a photo's edit record (opaque JSON), or null if it has none.
#[tauri::command]
pub fn get_edit_record(state: State<'_, AppState>, photo_id: i64) -> Result<Option<String>, String> {
    with_catalog(&state, |c| c.get_edit_record(photo_id))
}

/// Replace a photo's edit record. An empty value clears it. Core validates only that
/// the value is well-formed JSON; editing modules own its contents.
#[tauri::command]
pub fn set_edit_record(
    state: State<'_, AppState>,
    photo_id: i64,
    edit_json: String,
) -> Result<(), String> {
    with_catalog(&state, |c| c.set_edit_record(photo_id, &edit_json))
}

/// Render a photo's preview proxy with an edit record applied (crop + tone), returning
/// a base64 data URL (`data:image/jpeg;base64,…`) for the loupe/editor. Operates on the
/// embedded preview — never the original file. Errors if the `edit` feature is compiled
/// out. `maxEdge` (0 = full) caps the result size for fast live preview. `hiRes` renders
/// from the native-size preview tier instead of the fast 2048 one — the loupe requests
/// it when zooming into a version (a cropped render of the 2048 proxy can be smaller
/// than the window, leaving nothing to zoom into).
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
        // Gather path candidates under a brief lock (pure SQL), then stat + decode +
        // render off the lock so a slow/offline NAS can't serialize the app.
        let state = app.state::<AppState>();
        let candidates = {
            let guard = state.catalog.lock().map_err(|e| e.to_string())?;
            let catalog = guard.as_ref().ok_or("No catalog is open")?;
            catalog.photo_path_candidates(photo_id).map_err(|e| e.to_string())?
        };
        let health = state.volume_health.clone();
        let bytes = tauri::async_runtime::spawn_blocking(move || {
            // OriginalRequired: an edit render needs the real original, so a
            // cached-unreachable flag must never stand in for a stat.
            let path = crate::volume_health::pick_existing(
                &candidates,
                &health,
                crate::catalog::ResolveMode::OriginalRequired,
            )
            .ok_or_else(|| format!("no reachable copy of photo {photo_id}"))?;
            let jpeg = if hi_res.unwrap_or(false) {
                crate::thumbnails::zoom_bytes(&path)?
            } else {
                crate::thumbnails::preview_bytes(&path)?
            };
            crate::plugins::edit::render_jpeg(&jpeg, &edit_json, max_edge)
        })
        .await
        .map_err(|e| e.to_string())??;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(format!("data:image/jpeg;base64,{b64}"))
    }
}

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
) -> Result<Vec<Option<String>>, String> {
    #[cfg(not(feature = "edit"))]
    {
        let _ = (&app, photo_id, &edit_jsons, max_edge);
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
        tauri::async_runtime::spawn_blocking(move || {
            // OriginalRequired: an edit render needs the real original, so a
            // cached-unreachable flag must never stand in for a stat.
            let path = crate::volume_health::pick_existing(
                &candidates,
                &health,
                crate::catalog::ResolveMode::OriginalRequired,
            )
            .ok_or_else(|| format!("no reachable copy of photo {photo_id}"))?;
            let jpeg = crate::thumbnails::preview_bytes(&path)?;
            let mut img = image::load_from_memory(&jpeg).map_err(|e| e.to_string())?;
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

// --- LUTs (user-supplied .cube files for the editor, see docs/editing.md) --

/// List the `.cube` LUT filenames available in the app's luts folder.
#[tauri::command]
pub async fn list_luts() -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let dir = luts_dir()?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.to_lowercase().ends_with(".cube") {
                out.push(name);
            }
        }
        out.sort();
        Ok(out)
    })
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
        tauri::async_runtime::spawn_blocking(move || {
            let src = PathBuf::from(&path);
            let name = src
                .file_name()
                .ok_or("not a file path")?
                .to_string_lossy()
                .to_string();
            if !name.to_lowercase().ends_with(".cube") {
                return Err("only .cube LUTs are supported".to_string());
            }
            let text = std::fs::read_to_string(&src).map_err(|e| e.to_string())?;
            crate::plugins::edit::cube::CubeLut::parse(&text)
                .map_err(|e| format!("invalid LUT: {e}"))?;
            std::fs::write(luts_dir()?.join(&name), text).map_err(|e| e.to_string())?;
            Ok(name)
        })
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
    tauri::async_runtime::spawn_blocking(move || {
        std::fs::remove_file(luts_dir()?.join(&file)).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// --- photo versions (crop/exposure variants, see docs/editing.md) ---------

#[tauri::command]
pub fn list_versions(state: State<'_, AppState>, photo_id: i64) -> Result<Vec<PhotoVersion>, String> {
    with_catalog(&state, |c| c.list_versions(photo_id))
}

#[tauri::command]
pub fn create_version(
    state: State<'_, AppState>,
    photo_id: i64,
    name: String,
) -> Result<i64, String> {
    with_catalog(&state, |c| c.create_version(photo_id, &name))
}

#[tauri::command]
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
    // Save + gather everything the monochrome refresh needs under one brief lock.
    let (photo_id, any_bw, stored_gray, candidates) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        catalog
            .set_version_edit(version_id, edit_json)
            .map_err(|e| e.to_string())?;
        let photo_id = catalog.version_photo_id(version_id).map_err(|e| e.to_string())?;
        let any_bw = catalog
            .list_versions(photo_id)
            .map_err(|e| e.to_string())?
            .iter()
            .any(|v| crate::plugins::edit::is_bw(&v.edit_json));
        let stored = catalog.is_grayscale(photo_id).map_err(|e| e.to_string())?;
        let cands = catalog.photo_path_candidates(photo_id).map_err(|e| e.to_string())?;
        (photo_id, any_bw, stored, cands)
    };
    let gray = if any_bw {
        true
    } else if !stored_gray {
        // Not B&W by edit and already not flagged — nothing can change.
        return Ok(());
    } else {
        // The flag was set but no version is B&W anymore: fall back to the
        // pixel-derived signal (the photo itself may still be monochrome).
        let health = state.volume_health.clone();
        // OriginalRequired: the outcome is PERSISTED (`set_grayscale` + auto-tags), so it
        // must not be decided by a cached reachability flag — or by a decode failure.
        // `None` here covers BOTH ways "could not tell" happens: no reachable copy
        // (`pick_existing` fails) and a reachable copy that fails to decode
        // (`thumbnail_bytes` fails). Both are "could not tell", not "not grayscale", so
        // both skip the write below and leave the stored flag alone (AGENTS.md:
        // missing/unmounted storage is normal, never evidence the row is wrong). Only an
        // actual decoded verdict is ever persisted.
        let outcome = tauri::async_runtime::spawn_blocking(move || {
            crate::volume_health::pick_existing(
                &candidates,
                &health,
                crate::catalog::ResolveMode::OriginalRequired,
            )
            .and_then(|p| thumbnail_bytes(&p).ok())
            .map(|t| crate::thumbnails::is_grayscale_jpeg(&t))
        })
        .await
        .map_err(|e| e.to_string())?;
        match outcome {
            Some(g) => g,
            None => return Ok(()),
        }
    };
    if gray != stored_gray {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("No catalog is open")?;
        catalog.set_grayscale(photo_id, gray).map_err(|e| e.to_string())?;
        catalog.apply_auto_tags().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub fn delete_version(state: State<'_, AppState>, version_id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.delete_version(version_id))
}

#[tauri::command]
pub fn duplicate_version(state: State<'_, AppState>, version_id: i64) -> Result<i64, String> {
    with_catalog(&state, |c| c.duplicate_version(version_id))
}

#[tauri::command]
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
    with_catalog_blocking(&state, move |c| grid_version_counts(c, &photo_ids)).await
}

pub(crate) fn grid_version_counts(
    c: &Catalog,
    photo_ids: &[i64],
) -> crate::catalog::Result<Vec<(i64, i64)>> {
    c.version_counts(photo_ids)
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
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(catalog);
        AppState {
            volume_health: std::sync::Arc::new(health),
            ..state
        }
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
