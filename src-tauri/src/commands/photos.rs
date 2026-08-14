//! Per-photo commands: listing and faceting, the culling verbs (rating, colour
//! label, pick/reject), IPTC fields, user rotation, and RAW+JPEG stacking.

use super::*;
use tauri::State;

/// One window of the library view, plus how many photos match in total.
///
/// The whole filter/sort/window state travels as a single typed [`PhotoQuery`] (issue
/// #10) instead of eleven positional arguments that TypeScript, this signature, and the
/// SQL builder each had to spell identically. Omit `window` to get every matching row.
///
/// [`PhotoQuery`]: crate::catalog::PhotoQuery
#[tauri::command]
pub async fn list_photos(
    state: State<'_, AppState>,
    query: crate::catalog::PhotoQuery,
) -> Result<crate::catalog::PhotoPage, String> {
    with_catalog_blocking(&state, move |c| c.photo_page(&query)).await
}

/// Distinct camera models / lenses present in the catalog, for the filter-bar dropdowns.
/// `kind` is "camera" or "lens".
#[tauri::command]
pub fn distinct_photo_values(
    state: State<'_, AppState>,
    kind: String,
) -> Result<Vec<String>, String> {
    with_catalog(&state, |c| c.distinct_photo_values(&kind))
}

/// Assemble a reach-hashtag bundle from a tag group (for the export dialog's preview
/// and copy button). Core export-to-disk; platform destinations are module-based.
#[tauri::command]
pub fn assemble_hashtag_bundle(
    state: State<'_, AppState>,
    group_id: i64,
    limit: Option<usize>,
) -> Result<Vec<String>, String> {
    with_catalog(&state, |c| c.assemble_hashtag_bundle(group_id, limit))
}

/// All import batches, newest first, each with a photo count.
#[tauri::command]
pub fn list_import_batches(
    state: State<'_, AppState>,
) -> Result<Vec<crate::catalog::ImportBatch>, String> {
    with_catalog(&state, |c| c.list_import_batches())
}

/// The derived facets the UI can offer as filter chips.
#[tauri::command]
pub async fn list_facets(state: State<'_, AppState>) -> Result<Vec<crate::catalog::Facet>, String> {
    with_catalog_blocking(&state, move |c| Ok(c.available_facets())).await
}

#[tauri::command]
pub fn get_photo(state: State<'_, AppState>, photo_id: i64) -> Result<Photo, String> {
    with_catalog(&state, |c| c.get_photo(photo_id))
}

/// One photo by its stable uuid — resolves a chairphoto://<uuid> deep link.
#[tauri::command]
pub fn get_photo_by_uuid(state: State<'_, AppState>, uuid: String) -> Result<Photo, String> {
    with_catalog(&state, |c| c.get_photo_by_uuid(&uuid))
}

/// The photo's current absolute path on disk (best available copy), for "Reveal in Files".
/// Errors if no copy is reachable (e.g. the NAS is unmounted).
#[tauri::command]
pub async fn photo_path(state: State<'_, AppState>, photo_id: i64) -> Result<String, String> {
    // `require_photo_path` stats each candidate copy, which can block on an offline NAS —
    // so this must not run on the main thread.
    let path = with_catalog_blocking(&state, move |c| c.require_photo_path(photo_id)).await?;
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn set_rating(
    state: State<'_, AppState>,
    photo_id: i64,
    rating: i64,
) -> Result<Photo, String> {
    with_catalog(&state, |c| c.set_culling(photo_id, Some(rating), None, None))
}

#[tauri::command]
pub fn set_label(
    state: State<'_, AppState>,
    photo_id: i64,
    label: String,
) -> Result<Photo, String> {
    with_catalog(&state, |c| c.set_culling(photo_id, None, Some(&label), None))
}

#[tauri::command]
pub fn set_pick_state(
    state: State<'_, AppState>,
    photo_id: i64,
    pick_state: PickState,
) -> Result<Photo, String> {
    with_catalog(&state, |c| {
        c.set_culling(photo_id, None, None, Some(pick_state))
    })
}

/// Read a photo's authored IPTC fields.
#[tauri::command]
pub fn get_iptc(state: State<'_, AppState>, photo_id: i64) -> Result<IptcFields, String> {
    with_catalog(&state, |c| c.get_iptc(photo_id))
}

/// Save a photo's authored IPTC fields to the catalog AND write them to the photo's
/// XMP sidecar (merge-safe — preserves darktable/other data). The sidecar is written
/// next to the original file resolved by the location resolver.
#[tauri::command]
pub async fn set_iptc(
    state: State<'_, AppState>,
    photo_id: i64,
    fields: IptcFields,
) -> Result<(), String> {
    // Persist under the catalog lock, then write the sidecar off it. The XMP write is a
    // read-modify-write of an XML document — far too slow to hold the catalog mutex (or
    // the main thread) across.
    let stored = fields.clone();
    let original = with_catalog_blocking(&state, move |c| {
        c.set_iptc(photo_id, &stored)?;
        c.require_photo_path(photo_id)
    })
    .await?;
    tauri::async_runtime::spawn_blocking(move || crate::xmp::write_iptc(&original, &fields))
        .await
        .map_err(|e| e.to_string())?
}

/// Apply a non-destructive orientation correction to a photo: rotate the *displayed*
/// image by `delta` degrees clockwise (±90 for the buttons, 180 to flip). The override is
/// stored in the catalog and composed on top of the file's EXIF orientation at render time
/// (see protocol.rs) — the original file is never modified. Returns the new absolute
/// rotation (0/90/180/270).
#[tauri::command]
pub fn rotate_photo(
    state: State<'_, AppState>,
    photo_id: i64,
    delta: i64,
) -> Result<i64, String> {
    with_catalog(&state, |c| {
        let current = c.photo_rotation(photo_id)?;
        c.set_photo_rotation(photo_id, current + delta)
    })
}

/// The photos stacked under `photo_id` (e.g. a camera JPEG under its RAW).
#[tauri::command]
pub fn list_stack_children(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<Vec<crate::catalog::Photo>, String> {
    with_catalog(&state, |c| c.list_stack_children(photo_id))
}

/// Stack `childId` under `parentId` (the child is hidden from the grid, grouped under
/// the master). Used to manually pair a derivative (e.g. JPEG) with its master (RAW).
#[tauri::command]
pub fn stack_photo(
    state: State<'_, AppState>,
    child_id: i64,
    parent_id: i64,
) -> Result<(), String> {
    with_catalog(&state, |c| c.set_stack_parent(child_id, parent_id))
}

/// Remove a photo from its stack — it returns to the main grid as a top-level photo.
#[tauri::command]
pub fn unstack_photo(state: State<'_, AppState>, child_id: i64) -> Result<(), String> {
    with_catalog(&state, |c| c.unstack(child_id))
}

/// Stack every derivative JPEG under its sibling RAW (same folder + filename stem).
/// Idempotent; returns the number newly stacked. Runs automatically on the v18 migration
/// and after each scan — this command lets the user re-run it on demand.
#[tauri::command]
pub fn pair_raw_jpeg_stacks(state: State<'_, AppState>) -> Result<usize, String> {
    with_catalog(&state, |c| c.pair_raw_jpeg_stacks())
}

/// All stored EXIF/IPTC/XMP metadata for a photo (grouped), for the metadata panel.
#[tauri::command]
pub fn get_photo_metadata(
    state: State<'_, AppState>,
    photo_id: i64,
) -> Result<Vec<MetadataEntry>, String> {
    with_catalog(&state, |c| c.get_photo_metadata(photo_id))
}

