//! Flickr publishing commands — OAuth 1.0a token dance, photo upload, native-tag
//! prefill, and importing an existing Flickr photostream back into the catalog.
//!
//! Gated on the `flickr` Cargo feature; see `docs/flickr.md` and `flickr/`.

use super::publishing::*;
use super::*;
use tauri::{AppHandle, State};

/// Begin Flickr OAuth: returns the authorize URL the user opens (then pastes a verifier).
/// Stashes the request token/secret in settings for the completion step.
#[cfg(feature = "flickr")]
#[tauri::command]
pub async fn flickr_begin_auth(app: AppHandle) -> Result<String, String> {
    let (key, secret) = read_app_keys(&app, "flickr")?;
    let rt = crate::flickr::begin_auth(&key, &secret).await?;
    write_setting(&app, "flickr.request_token", &rt.token)?;
    write_setting(&app, "flickr.request_secret", &rt.secret)?;
    Ok(rt.authorize_url)
}

/// Finish Flickr OAuth with the verifier the user pasted; stores the access token/secret.
#[cfg(feature = "flickr")]
#[tauri::command]
pub async fn flickr_complete_auth(app: AppHandle, verifier: String) -> Result<(), String> {
    let (key, secret) = read_app_keys(&app, "flickr")?;
    let req_token = read_setting(&app, "flickr.request_token")?;
    let req_secret = read_setting(&app, "flickr.request_secret")?;
    if req_token.is_empty() {
        return Err("Start with Connect before entering the verifier.".into());
    }
    let at = crate::flickr::complete_auth(&key, &secret, &req_token, &req_secret, verifier.trim()).await?;
    write_setting(&app, "flickr.access_token", &at.token)?;
    write_setting(&app, "flickr.access_secret", &at.secret)?;
    // The NSID lets post_to_flickr build canonical page URLs for publication records.
    write_setting(&app, "flickr.user_nsid", at.user_nsid.as_deref().unwrap_or(""))?;
    write_setting(&app, "flickr.request_token", "")?;
    write_setting(&app, "flickr.request_secret", "")?;
    Ok(())
}

#[cfg(feature = "flickr")]
#[tauri::command]
pub fn flickr_connected(app: AppHandle) -> Result<bool, String> {
    Ok(!read_setting(&app, "flickr.access_token")?.is_empty())
}

/// Render the selected version and upload it to Flickr. Returns the photo's page URL
/// (canonical `/photos/<nsid>/<id>/` when the NSID is known, else the `photo.gne?id=`
/// redirect form — both carry the photo id, which the importer's signal P extracts).
/// The frontend records the publication with this URL (stamping the module's marker).
#[cfg(feature = "flickr")]
#[tauri::command]
pub async fn post_to_flickr(
    app: AppHandle,
    photo_id: i64,
    version_id: Option<i64>,
    title: String,
    description: String,
    tags: String,
) -> Result<String, String> {
    let (key, secret) = read_app_keys(&app, "flickr")?;
    let (token, token_secret) = read_access(&app, "flickr")?;
    let max_long_edge = read_max_long_edge(&app, "flickr");
    // `img` owns the job temp directory: it must outlive the upload, and dropping it after
    // this function returns removes the render on both the success and the error path.
    let img = render_export_jpeg(&app, photo_id, version_id, "flickr", max_long_edge).await?;
    let flickr_photo_id = crate::flickr::upload(
        &key,
        &secret,
        &token,
        &token_secret,
        img.path(),
        &title,
        &description,
        &tags,
    )
    .await?;
    let nsid = read_setting(&app, "flickr.user_nsid").unwrap_or_default();
    Ok(if nsid.is_empty() {
        format!("https://www.flickr.com/photo.gne?id={flickr_photo_id}")
    } else {
        format!("https://www.flickr.com/photos/{nsid}/{flickr_photo_id}/")
    })
}

/// Suggested Flickr tags for a photo: its export keywords (same pipeline as XMP export
/// and the Instagram caption) in Flickr's `tags` format — space-separated, multi-word
/// tags quoted. Prefills the publish panel's Tags field; the user edits before upload.
#[cfg(feature = "flickr")]
#[tauri::command]
pub fn flickr_suggest_tags(state: State<'_, AppState>, photo_id: i64) -> Result<String, String> {
    with_catalog(&state, |c| {
        let keywords = c
            .assemble_export_keywords(photo_id, &[])
            .map(|k| k.flat)
            .unwrap_or_default();
        Ok(crate::flickr::format_tags(&keywords))
    })
}

/// One matched photo returned in the dry-run preview response.
#[cfg(feature = "flickr")]
#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FlickrImportMatch {
    pub catalog_id: i64,
    pub catalog_path: String,
    pub flickr_id: String,
    pub flickr_url: String,
    /// Unix timestamp of the Flickr upload (the real historical date).
    pub published_at: i64,
    /// Small (240 px) Flickr thumbnail URL (`url_s` extras field); absent on some photos.
    pub thumb_url: Option<String>,
}

/// One catalog candidate surfaced in an ambiguous match (for manual resolution UI).
#[cfg(feature = "flickr")]
#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FlickrImportCandidate {
    pub catalog_id: i64,
    pub catalog_path: String,
    /// ISO-like capture time from the catalog ("YYYY-MM-DDTHH:MM:SS"), or null.
    pub capture_time: Option<String>,
}

/// Summary of ambiguous Flickr photos (too many or conflicting catalog candidates).
#[cfg(feature = "flickr")]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlickrImportAmbiguous {
    pub flickr_id: String,
    pub flickr_url: String,
    /// Unix timestamp of the Flickr upload — needed to build a plan entry on resolution.
    pub published_at: i64,
    pub title: String,
    pub reason: String,
    /// Small (240 px) Flickr thumbnail URL (`url_s` extras field); absent on some photos.
    pub thumb_url: Option<String>,
    /// Post-collapse candidate set (up to 10) for manual resolution.
    pub candidates: Vec<FlickrImportCandidate>,
}

/// Return type for `flickr_import_published` (dry-run preview mode).
///
/// `plan` carries the **full** match set so the frontend can send it back verbatim to
/// `flickr_import_apply` without re-fetching the photostream.
/// `matches` is capped at 50 for display; `plan` is the uncapped list.
/// `ambiguous` is capped at 50.
#[cfg(feature = "flickr")]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlickrImportResult {
    pub matched_count: usize,
    pub ambiguous_count: usize,
    pub unmatched_count: usize,
    /// Up to 50 matches for display in the UI.
    pub matches: Vec<FlickrImportMatch>,
    /// The full (uncapped) match set — send this back to `flickr_import_apply`.
    pub plan: Vec<FlickrImportMatch>,
    /// Up to 50 ambiguous items that need manual attention.
    pub ambiguous: Vec<FlickrImportAmbiguous>,
}

/// Fetch the authenticated user's Flickr photostream, match against the local catalog by
/// capture datetime and/or title, and return a preview plan.
///
/// The command is strictly read-only toward Flickr (no writes to the remote API) and does
/// NOT upsert any publications — call `flickr_import_apply` with the returned `plan` to apply.
#[cfg(feature = "flickr")]
#[tauri::command]
pub async fn flickr_import_published(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<FlickrImportResult, String> {
    let (key, secret) = read_app_keys(&app, "flickr")?;
    let (token, token_secret) = read_access(&app, "flickr")?;

    // 1. Fetch the full photostream (may take a few seconds for large accounts).
    let flickr_photos = crate::flickr::fetch_photostream(&key, &secret, &token, &token_secret)
        .await?;

    // 2. Load all catalog photos (id, path, capture_time) for matching, plus the existing
    //    flickr publication rows — photos chairphoto itself uploaded (or already imported)
    //    match by the photo id in the recorded URL (signal P) or by publish-time proximity,
    //    so they never land in the ambiguous pile.
    let catalog_rows: Vec<crate::flickr::CatalogPhotoRow> =
        with_catalog_blocking(&state, |c| c.photos_for_flickr_match()).await?;
    let existing: Vec<crate::flickr::ExistingPublication> =
        with_catalog_blocking(&state, |c| c.publications_for_platform("flickr"))
            .await?
            .into_iter()
            .map(|(photo_id, url, published_at)| crate::flickr::ExistingPublication {
                catalog_id: photo_id,
                flickr_id: url.as_deref().and_then(crate::flickr::flickr_id_from_url),
                published_at,
            })
            .collect();

    // 3. Match (pure logic, no I/O).
    let outcomes = crate::flickr::match_photos(&flickr_photos, &catalog_rows, &existing);

    // 4. Partition outcomes.
    let mut plan: Vec<FlickrImportMatch> = Vec::new();
    let mut ambiguous: Vec<FlickrImportAmbiguous> = Vec::new();
    let mut unmatched_count: usize = 0;

    for outcome in &outcomes {
        match outcome {
            crate::flickr::MatchOutcome::Matched {
                flickr,
                catalog_id,
                catalog_path,
            } => {
                plan.push(FlickrImportMatch {
                    catalog_id: *catalog_id,
                    catalog_path: catalog_path.clone(),
                    flickr_id: flickr.id.clone(),
                    flickr_url: flickr.page_url(),
                    published_at: flickr.date_upload_unix,
                    thumb_url: flickr.thumb_url.clone(),
                });
            }
            crate::flickr::MatchOutcome::Ambiguous { flickr, reason, candidates } => {
                let import_candidates: Vec<FlickrImportCandidate> = candidates
                    .iter()
                    .map(|c| FlickrImportCandidate {
                        catalog_id: c.catalog_id,
                        catalog_path: c.catalog_path.clone(),
                        capture_time: c.capture_time.clone(),
                    })
                    .collect();
                ambiguous.push(FlickrImportAmbiguous {
                    flickr_id: flickr.id.clone(),
                    flickr_url: flickr.page_url(),
                    published_at: flickr.date_upload_unix,
                    title: flickr.title.clone(),
                    reason: reason.clone(),
                    thumb_url: flickr.thumb_url.clone(),
                    candidates: import_candidates,
                });
            }
            crate::flickr::MatchOutcome::Unmatched { .. } => {
                unmatched_count += 1;
            }
        }
    }

    let matched_count = plan.len();
    let ambiguous_count = ambiguous.len();

    // Cap the display lists to 50 items to keep the IPC response reasonable.
    // `plan` is intentionally NOT capped — the frontend sends it back verbatim to apply.
    const CAP: usize = 50;
    let matches: Vec<FlickrImportMatch> = plan.iter().take(CAP).cloned().collect();
    ambiguous.truncate(CAP);

    Ok(FlickrImportResult {
        matched_count,
        ambiguous_count,
        unmatched_count,
        matches,
        plan,
        ambiguous,
    })
}

/// Apply a previously-previewed import plan: upsert a `publications` row for each entry with
/// the real historical upload timestamp.  Takes the `plan` array from `flickr_import_published`
/// verbatim — does not re-fetch the photostream.
#[cfg(feature = "flickr")]
#[tauri::command]
pub async fn flickr_import_apply(
    state: State<'_, AppState>,
    plan: Vec<FlickrImportMatch>,
) -> Result<usize, String> {
    let applied = plan.len();
    let to_record: Vec<(i64, String, i64)> = plan
        .into_iter()
        .map(|m| (m.catalog_id, m.flickr_url, m.published_at))
        .collect();
    with_catalog_blocking(&state, move |c| {
        for (photo_id, url, published_at) in &to_record {
            c.record_publication_historical(*photo_id, "flickr", url, *published_at)?;
        }
        Ok(())
    })
    .await?;
    Ok(applied)
}

// --- SmugMug ---

