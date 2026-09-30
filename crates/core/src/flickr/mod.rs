//! Publish a photo to Flickr via its official OAuth 1.0a API (the `flickr` Cargo feature).
//! Pure transport: the auth dance (request token → user authorizes in the browser → access
//! token) and the upload, all signed with [`crate::oauth1`]. Commands in `commands.rs` wire
//! these to the catalog (credentials in settings) and the render path; the frontend module
//! records the publication. Endpoints per the Flickr API docs (flickr.com/services/api).

use crate::oauth1;
use serde::Deserialize;
use std::path::Path;

const REQUEST_TOKEN_URL: &str = "https://www.flickr.com/services/oauth/request_token";
const AUTHORIZE_URL: &str = "https://www.flickr.com/services/oauth/authorize";
const ACCESS_TOKEN_URL: &str = "https://www.flickr.com/services/oauth/access_token";
const UPLOAD_URL: &str = "https://up.flickr.com/services/upload/";
const REST_URL: &str = "https://api.flickr.com/services/rest/";

// ── Photostream fetch ──────────────────────────────────────────────────────────

/// One photo returned by `flickr.people.getPhotos`.
#[derive(Debug, Clone)]
pub struct FlickrPhoto {
    /// Flickr numeric photo id.
    pub id: String,
    /// Flickr nsid of the owner (used to build page URL).
    pub owner: String,
    /// Photo title as shown on Flickr (Flickr defaults this to the filename stem on upload).
    pub title: String,
    /// Capture datetime reported by Flickr (`datetaken` field): "YYYY-MM-DD HH:MM:SS"
    /// (Flickr's local naive time, matches what was in the EXIF at upload time).
    pub date_taken: String,
    /// Unix timestamp of when the photo was uploaded to Flickr (`dateupload` field).
    pub date_upload_unix: i64,
    /// Small (240 px) thumbnail URL from the `url_s` extras field.
    /// `None` when Flickr omits it (e.g. restricted/private photos, very old uploads).
    pub thumb_url: Option<String>,
}

impl FlickrPhoto {
    /// Canonical Flickr page URL for this photo.
    pub fn page_url(&self) -> String {
        format!(
            "https://www.flickr.com/photos/{}/{}/",
            self.owner, self.id
        )
    }
}

// JSON shapes for flickr.people.getPhotos (format=json, nojsoncallback=1).
//
// `photos` is `Option` so that error responses (which have no `photos` key, only
// `stat`/`code`/`message`) still deserialise successfully.  The caller checks `stat`
// first and converts a non-"ok" response into a human-readable error before trying to
// unwrap `photos`.
#[derive(Deserialize)]
struct GetPhotosResponse {
    stat: String,
    #[serde(default)]
    code: Option<u32>,
    #[serde(default)]
    message: Option<String>,
    photos: Option<PhotosPage>,
}

#[derive(Deserialize)]
struct PhotosPage {
    // The response also echoes `page`; the fetch loop tracks its own counter, so only
    // the total is read (serde ignores unknown fields).
    pages: u32,
    photo: Vec<PhotoItem>,
}

#[derive(Deserialize)]
struct PhotoItem {
    id: String,
    owner: String,
    title: String,
    #[serde(default)]
    datetaken: String,
    #[serde(default)]
    dateupload: String,
    /// Small (240 px) image URL; absent on some photos — Flickr omits the field entirely.
    #[serde(default)]
    url_s: Option<String>,
}

/// Fetch **all** photos from the authenticated user's photostream.  Signs each
/// `flickr.people.getPhotos` request with the existing oauth1 helper (same pattern as
/// `begin_auth` / `complete_auth`).  Pages through until `page == pages`.
pub async fn fetch_photostream(
    key: &str,
    secret: &str,
    token: &str,
    token_secret: &str,
) -> Result<Vec<FlickrPhoto>, String> {
    let mut all: Vec<FlickrPhoto> = Vec::new();
    let mut page: u32 = 1;
    loop {
        let page_str = page.to_string();
        let extra_params = [
            ("method", "flickr.people.getPhotos"),
            ("user_id", "me"),
            ("extras", "date_taken,date_upload,url_s"),
            ("per_page", "500"),
            ("page", &page_str),
            ("format", "json"),
            ("nojsoncallback", "1"),
        ];
        let params = oauth1::signed_params(
            "GET",
            REST_URL,
            key,
            secret,
            Some(token),
            token_secret,
            &extra_params,
        );
        let url = format!("{REST_URL}?{}", oauth1::query_string(&params));
        let body = http_get_text(&url).await?;
        let resp: GetPhotosResponse = serde_json::from_str(&body)
            .map_err(|e| format!("Flickr photostream JSON parse failed: {e}\nbody: {body}"))?;
        if resp.stat != "ok" {
            return Err(format!(
                "Flickr API error {}: {}",
                resp.code.unwrap_or(0),
                resp.message.unwrap_or_else(|| resp.stat.clone())
            ));
        }
        let photos_page = resp
            .photos
            .ok_or_else(|| format!("Flickr response missing photos field\nbody: {body}"))?;
        let total_pages = photos_page.pages;
        for item in photos_page.photo {
            let date_upload_unix: i64 = item.dateupload.trim().parse().unwrap_or(0);
            all.push(FlickrPhoto {
                id: item.id,
                owner: item.owner,
                title: item.title,
                date_taken: item.datetaken,
                date_upload_unix,
                thumb_url: item.url_s,
            });
        }
        if page >= total_pages || total_pages == 0 {
            break;
        }
        page += 1;
    }
    Ok(all)
}

// ── Catalog photo record for matching ─────────────────────────────────────────

/// Minimal catalog photo info needed for matching (no image bytes).
#[derive(Debug, Clone)]
pub struct CatalogPhotoRow {
    pub id: i64,
    /// Catalog-root-relative path, e.g. "2023/DSC01234.ARW".
    pub path: String,
    /// `capture_time` from the catalog, "YYYY-MM-DDTHH:MM:SS" or None.
    pub capture_time: Option<String>,
}

// ── Matching ──────────────────────────────────────────────────────────────────

/// An existing `publications` row for platform "flickr", used as the strongest matching
/// signal: a Flickr photo whose id already appears in a recorded publication URL was
/// uploaded (or previously imported) by chairphoto itself — no heuristics needed.
#[derive(Debug, Clone)]
pub struct ExistingPublication {
    pub catalog_id: i64,
    /// Flickr photo id extracted from the stored publication URL; `None` for legacy rows
    /// recorded before the URL was captured at publish time.
    pub flickr_id: Option<String>,
    /// `published_at` from the row (unix seconds). For chairphoto-side uploads this is
    /// within seconds of Flickr's `dateupload`, which lets URL-less rows break ties.
    pub published_at: i64,
}

/// Extract the Flickr photo id from a publication URL. Understands both the canonical
/// page form `…/photos/<owner>/<id>/` and the owner-less redirect form
/// `…/photo.gne?id=<id>` (recorded when the user's NSID isn't known).
pub fn flickr_id_from_url(url: &str) -> Option<String> {
    if let Some(idx) = url.find("photo.gne?id=") {
        let digits: String = url[idx + "photo.gne?id=".len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        return (!digits.is_empty()).then_some(digits);
    }
    if !url.contains("/photos/") {
        return None;
    }
    let last = url.trim_end_matches('/').rsplit('/').next()?;
    (!last.is_empty() && last.chars().all(|c| c.is_ascii_digit())).then(|| last.to_string())
}

/// A single catalog candidate surfaced in an `Ambiguous` outcome.
/// Carries enough data to build a `FlickrImportMatch` once the user resolves it.
#[derive(Debug, Clone)]
pub struct AmbiguousCandidate {
    pub catalog_id: i64,
    pub catalog_path: String,
    pub capture_time: Option<String>,
}

/// Result of matching one Flickr photo against the local catalog.
#[derive(Debug, Clone)]
pub enum MatchOutcome {
    /// Exactly one catalog photo identified.
    Matched {
        flickr: FlickrPhoto,
        catalog_id: i64,
        catalog_path: String,
    },
    /// More than one catalog candidate, or two signals point to different photos.
    Ambiguous {
        flickr: FlickrPhoto,
        reason: String,
        /// The post-collapse candidate set (up to 10) for manual resolution.
        candidates: Vec<AmbiguousCandidate>,
    },
    /// No catalog photo found by any signal.
    Unmatched { flickr: FlickrPhoto },
}

/// Pure matching logic — no I/O, no network.  Exposed as a free function so unit tests can
/// drive it with fixture data.
///
/// Signals:
/// - **P** (publication, strongest): the Flickr photo id appears in an existing `flickr`
///   publication URL — chairphoto uploaded or previously imported this exact photo, so it
///   matches that catalog photo directly and never lands in the ambiguous pile.
/// - **A** (datetime): `capture_time` in the catalog equals `datetaken` from Flickr, compared
///   at second precision after normalising "YYYY-MM-DDTHH:MM:SS" ↔ "YYYY-MM-DD HH:MM:SS".
/// - **B** (title): Flickr title stem (trailing image extension stripped, lowercase) equals
///   the catalog filename stem (case-insensitive), or the full Flickr title equals the catalog
///   filename (case-insensitive).
///
/// Decision pipeline:
/// 1. Signal P short-circuit (see above).
/// 2. Compute A∩B intersection — if non-empty, that becomes the candidate set.
/// 3. Otherwise fall through to single-signal arms (A alone, B alone) or conflict.
/// 4. Fix 3: when A is empty and decision rests on B alone, drop B-candidates whose
///    `capture_time` is known and more than 48 hours from Flickr's `datetaken`.
/// 5. Fix 1 (RAW+JPEG collapse): if all remaining candidates share the same filename stem
///    and capture second, prefer the non-JPEG/PNG/HEIC raw file; one survivor → Matched.
/// 6. Publication-time tiebreak: among still-ambiguous candidates, exactly one having an
///    existing `flickr` publication recorded within an hour of Flickr's `dateupload` wins —
///    this rescues chairphoto-side uploads recorded before the URL was captured.
/// 7. Exactly one candidate → Matched; more → Ambiguous (with paths); none → Unmatched.
pub fn match_photos(
    flickr_items: &[FlickrPhoto],
    catalog_rows: &[CatalogPhotoRow],
    existing: &[ExistingPublication],
) -> Vec<MatchOutcome> {
    flickr_items
        .iter()
        .map(|flic| match_one(flic, catalog_rows, existing))
        .collect()
}

fn match_one(
    flic: &FlickrPhoto,
    catalog_rows: &[CatalogPhotoRow],
    existing: &[ExistingPublication],
) -> MatchOutcome {
    // Signal P: the publication URL already names this Flickr photo id.
    if let Some(ep) = existing
        .iter()
        .find(|e| e.flickr_id.as_deref() == Some(flic.id.as_str()))
    {
        if let Some(row) = catalog_rows.iter().find(|r| r.id == ep.catalog_id) {
            return MatchOutcome::Matched {
                flickr: flic.clone(),
                catalog_id: row.id,
                catalog_path: row.path.clone(),
            };
        }
        // Recorded photo no longer in the catalog → fall through to the heuristics.
    }

    // Signal A: datetime match at second precision.
    let flickr_dt = normalise_dt(&flic.date_taken);
    let by_datetime: Vec<&CatalogPhotoRow> = catalog_rows
        .iter()
        .filter(|r| {
            if let Some(ct) = &r.capture_time {
                !flickr_dt.is_empty() && normalise_dt(ct) == flickr_dt
            } else {
                false
            }
        })
        .collect();

    // Signal B: title matches filename stem or full filename (case-insensitive).
    // The Flickr title may include a trailing image extension (e.g. "_81A8352-2.jpg"):
    // strip it before comparing so it aligns with the catalog stem.
    let title_lc = flic.title.trim().to_lowercase();
    let title_stem_lc = title_stem(&title_lc);
    let by_title: Vec<&CatalogPhotoRow> = catalog_rows
        .iter()
        .filter(|r| {
            if title_lc.is_empty() {
                return false;
            }
            let filename = filename_of(&r.path);
            let file_lc = filename.to_lowercase();
            let stem_lc = stem_of(&filename).to_lowercase();
            // Full filename equality (original title form) OR stem-to-stem equality.
            file_lc == title_lc || stem_lc == title_lc || stem_lc == title_stem_lc
        })
        .collect();

    decide(flic, by_datetime, by_title, existing)
}

/// Publication-time tiebreak: among ambiguous candidates, the one whose existing `flickr`
/// publication was recorded within an hour of this photo's Flickr upload time is the photo
/// chairphoto itself uploaded (legacy rows lack the URL, so signal P can't catch them).
/// Returns the winner only when it is unique.
fn pub_time_tiebreak<'a>(
    candidates: &[&'a CatalogPhotoRow],
    flic: &FlickrPhoto,
    existing: &[ExistingPublication],
) -> Option<&'a CatalogPhotoRow> {
    if flic.date_upload_unix == 0 {
        return None;
    }
    let hits: Vec<&&CatalogPhotoRow> = candidates
        .iter()
        .filter(|r| {
            existing.iter().any(|e| {
                e.catalog_id == r.id && (e.published_at - flic.date_upload_unix).abs() <= 3600
            })
        })
        .collect();
    match hits.as_slice() {
        [row] => Some(**row),
        _ => None,
    }
}

/// Collapse two candidate lists into a `MatchOutcome`.
fn decide<'a>(
    flic: &FlickrPhoto,
    by_datetime: Vec<&'a CatalogPhotoRow>,
    by_title: Vec<&'a CatalogPhotoRow>,
    existing: &[ExistingPublication],
) -> MatchOutcome {
    let a_empty = by_datetime.is_empty();
    let b_empty = by_title.is_empty();

    // ── Step 1: try A∩B intersection ─────────────────────────────────────────
    let candidates: Vec<&'a CatalogPhotoRow> = if !a_empty && !b_empty {
        let a_ids: std::collections::HashSet<i64> = by_datetime.iter().map(|r| r.id).collect();
        let intersection: Vec<&'a CatalogPhotoRow> =
            by_title.iter().copied().filter(|r| a_ids.contains(&r.id)).collect();
        if !intersection.is_empty() {
            // Both signals agree: use the intersection.
            intersection
        } else {
            // Signals exist but point to disjoint sets → conflict.
            let a_paths: Vec<&str> = by_datetime.iter().map(|r| r.path.as_str()).collect();
            let b_paths: Vec<&str> = by_title.iter().map(|r| r.path.as_str()).collect();
            // Candidates for manual resolution: A ∪ B, sibling-collapsed across BOTH
            // signals (a JPG hit by one signal must not survive its RAW hit by the
            // other), capped at 10.
            let union = collapse_raw_jpeg_siblings(
                by_datetime.iter().chain(by_title.iter()).copied().collect(),
            );
            if let Some(row) = pub_time_tiebreak(&union, flic, existing) {
                return MatchOutcome::Matched {
                    flickr: flic.clone(),
                    catalog_id: row.id,
                    catalog_path: row.path.clone(),
                };
            }
            let conflict_candidates: Vec<AmbiguousCandidate> = union
                .iter()
                .take(10)
                .map(|r| AmbiguousCandidate {
                    catalog_id: r.id,
                    catalog_path: r.path.clone(),
                    capture_time: r.capture_time.clone(),
                })
                .collect();
            return MatchOutcome::Ambiguous {
                flickr: flic.clone(),
                reason: format!(
                    "datetime candidates [{}] conflict with title candidates [{}]",
                    format_paths(&a_paths),
                    format_paths(&b_paths)
                ),
                candidates: conflict_candidates,
            };
        }
    } else if !a_empty {
        // Only Signal A.
        by_datetime.clone()
    } else if !b_empty {
        // Only Signal B — apply Fix 3: plausibility window.
        let flickr_dt_norm = normalise_dt(&flic.date_taken);
        let filtered: Vec<&'a CatalogPhotoRow> = by_title
            .iter()
            .copied()
            .filter(|r| {
                // Keep candidates with NULL capture_time (can't disprove).
                // Drop candidates whose capture_time is known but > 48 h from datetaken.
                match &r.capture_time {
                    None => true,
                    Some(ct) => {
                        if flickr_dt_norm.is_empty() {
                            // No Flickr datetime to compare against — keep.
                            true
                        } else {
                            let cat_norm = normalise_dt(ct);
                            dt_within_48h(&flickr_dt_norm, &cat_norm)
                        }
                    }
                }
            })
            .collect();
        if filtered.is_empty() {
            return MatchOutcome::Unmatched { flickr: flic.clone() };
        }
        filtered
    } else {
        // Both empty.
        return MatchOutcome::Unmatched { flickr: flic.clone() };
    };

    // ── Step 2: Fix 1 — collapse RAW+JPEG siblings ───────────────────────────
    let candidates = collapse_raw_jpeg_siblings(candidates);

    // ── Step 3: final decision ────────────────────────────────────────────────
    match candidates.as_slice() {
        [row] => MatchOutcome::Matched {
            flickr: flic.clone(),
            catalog_id: row.id,
            catalog_path: row.path.clone(),
        },
        [] => MatchOutcome::Unmatched { flickr: flic.clone() },
        many => {
            if let Some(row) = pub_time_tiebreak(many, flic, existing) {
                return MatchOutcome::Matched {
                    flickr: flic.clone(),
                    catalog_id: row.id,
                    catalog_path: row.path.clone(),
                };
            }
            let paths: Vec<&str> = many.iter().map(|r| r.path.as_str()).collect();
            let ambig_candidates: Vec<AmbiguousCandidate> = many
                .iter()
                .take(10)
                .map(|r| AmbiguousCandidate {
                    catalog_id: r.id,
                    catalog_path: r.path.clone(),
                    capture_time: r.capture_time.clone(),
                })
                .collect();
            MatchOutcome::Ambiguous {
                flickr: flic.clone(),
                reason: format!("ambiguous candidates: {}", format_paths(&paths)),
                candidates: ambig_candidates,
            }
        }
    }
}

/// Extension priority rank for collapse logic.
/// - Rank 0: camera RAW formats (most authoritative original).
/// - Rank 1: TIFF/PSD (processed but lossless; above JPEG-tier).
/// - Rank 2: everything else (JPG, PNG, HEIC, WebP, GIF, …).
fn ext_rank(filename: &str) -> u8 {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "arw" | "cr2" | "cr3" | "nef" | "nrw" | "dng" | "raf" | "orf" | "rw2" | "pef"
        | "srw" | "srf" | "sr2" | "x3f" | "3fr" | "iiq" => 0,
        "tif" | "tiff" | "psd" => 1,
        _ => 2,
    }
}

/// Fix 1: candidates sharing a filename stem (case-insensitive) are derivatives of the same
/// photograph — collapse each stem group to its best member using an extension rank ladder
/// (rank 0 = camera RAW, 1 = TIF/PSD, 2 = other).
///
/// Per stem group:
/// 1. Keep only lowest-rank members. A RAW always beats its JPG/TIF siblings, **even when
///    their capture seconds differ or are missing** — exports routinely lose or shift the
///    EXIF time, and the owner's rule is "if there is a RAW, link the RAW" (2026-07-24).
/// 2. Same-rank survivors with the same capture second are duplicate copies → keep the
///    lowest catalog id (stable across runs).
/// 3. Same-rank survivors with *different* seconds are genuinely different photos that
///    happen to share a filename (counter rollover) → keep them all (stays ambiguous).
///
/// Groups with different stems are never merged (a burst stays ambiguous).
fn collapse_raw_jpeg_siblings<'a>(
    candidates: Vec<&'a CatalogPhotoRow>,
) -> Vec<&'a CatalogPhotoRow> {
    if candidates.len() <= 1 {
        return candidates;
    }

    // Group by stem, preserving first-seen stem order.
    let mut stems: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<&'a CatalogPhotoRow>> =
        std::collections::HashMap::new();
    for r in candidates {
        let stem = stem_of(&filename_of(&r.path)).to_lowercase();
        if !groups.contains_key(&stem) {
            stems.push(stem.clone());
        }
        groups.entry(stem).or_default().push(r);
    }

    let mut out: Vec<&'a CatalogPhotoRow> = Vec::new();
    for stem in stems {
        let group = groups.remove(&stem).unwrap_or_default();
        let best_rank = group
            .iter()
            .map(|r| ext_rank(&filename_of(&r.path)))
            .min()
            .unwrap_or(2);
        let mut best: Vec<&'a CatalogPhotoRow> = group
            .into_iter()
            .filter(|r| ext_rank(&filename_of(&r.path)) == best_rank)
            .collect();
        // Duplicate copies (same rank + same capture second) collapse to the lowest id;
        // different seconds at the same rank stay separate.
        if best.len() > 1 {
            let first_dt = best[0].capture_time.as_deref().map(normalise_dt);
            let all_same_second = best
                .iter()
                .all(|r| r.capture_time.as_deref().map(normalise_dt) == first_dt);
            if all_same_second {
                best.sort_by_key(|r| r.id);
                best.truncate(1);
            }
        }
        out.extend(best);
    }
    out
}

/// Fix 2: derive a "title stem" — lowercase, strip ONE trailing recognised image extension if
/// present.  A suffix like ".30" (as in "Sunset at 19.30") is NOT a recognised extension and
/// must be left intact.
fn title_stem(title_lc: &str) -> &str {
    const IMG_EXTS: &[&str] = &[
        ".jpg", ".jpeg", ".png", ".gif", ".tif", ".tiff", ".heic", ".webp",
    ];
    for ext in IMG_EXTS {
        if title_lc.ends_with(ext) {
            return &title_lc[..title_lc.len() - ext.len()];
        }
    }
    title_lc
}

/// Fix 3: returns true if two normalised datetime strings ("YYYYMMDDHHmmss") are within 48 hours.
/// Returns true if either string is malformed (can't disprove).
fn dt_within_48h(a: &str, b: &str) -> bool {
    /// Julian Day Number formula (Fliegel & Van Flandern / Richards 2013).
    /// Works for any proleptic Gregorian date; no external dependencies.
    fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
        let m = (month - 14) / 12;
        let y = year + 4800 + m;
        day + (153 * (month + 12 * (-m) - 3) + 2) / 5
            + 365 * y
            + y / 4
            - y / 100
            + y / 400
            - 32045
    }

    fn parse_norm(s: &str) -> Option<i64> {
        if s.len() < 14 {
            return None;
        }
        let year: i64 = s[0..4].parse().ok()?;
        let month: i64 = s[4..6].parse().ok()?;
        let day: i64 = s[6..8].parse().ok()?;
        let h: i64 = s[8..10].parse().ok()?;
        let m: i64 = s[10..12].parse().ok()?;
        let sec: i64 = s[12..14].parse().ok()?;
        Some(days_from_civil(year, month, day) * 86400 + h * 3600 + m * 60 + sec)
    }
    match (parse_norm(a), parse_norm(b)) {
        (Some(ta), Some(tb)) => (ta - tb).abs() <= 48 * 3600,
        _ => true, // can't parse → keep candidate
    }
}

/// Normalise either "YYYY-MM-DDTHH:MM:SS" (catalog) or "YYYY-MM-DD HH:MM:SS" (Flickr) to
/// "YYYYMMDDHHMMSS" for comparison.  Returns an empty string if the input is unrecognised.
fn normalise_dt(s: &str) -> String {
    // Accept both separators (T or space).
    let s = s.trim().replace('T', " ");
    // Expect "YYYY-MM-DD HH:MM:SS" — 19 chars minimum.
    if s.len() < 19 {
        return String::new();
    }
    s.chars()
        .filter(|c| c.is_ascii_digit())
        .take(14) // YYYYMMDDHHmmss
        .collect()
}

fn filename_of(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

fn stem_of(filename: &str) -> String {
    std::path::Path::new(filename)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or(filename)
        .to_string()
}

/// A request token plus the URL the user must open to authorize (OOB flow → a verifier code).
pub struct RequestToken {
    pub token: String,
    pub secret: String,
    pub authorize_url: String,
}

/// The long-lived access token granted after the user pastes back the verifier.
pub struct AccessToken {
    pub token: String,
    pub secret: String,
    /// The user's NSID (`user_nsid` in the token response) — used to build canonical
    /// photo page URLs at publish time. `None` if Flickr omits it.
    pub user_nsid: Option<String>,
}

/// Step 1: get a request token (callback "oob") and the authorize URL (write permission).
pub async fn begin_auth(key: &str, secret: &str) -> Result<RequestToken, String> {
    let params = oauth1::signed_params(
        "GET",
        REQUEST_TOKEN_URL,
        key,
        secret,
        None,
        "",
        &[("oauth_callback", "oob")],
    );
    let url = format!("{REQUEST_TOKEN_URL}?{}", oauth1::query_string(&params));
    let body = http_get_text(&url).await?;
    let kv = oauth1::parse_kv(&body);
    let token = kv
        .get("oauth_token")
        .cloned()
        .ok_or_else(|| format!("Flickr returned no request token: {body}"))?;
    let secret = kv.get("oauth_token_secret").cloned().unwrap_or_default();
    let authorize_url = format!(
        "{AUTHORIZE_URL}?oauth_token={}&perms=write",
        oauth1::percent_encode(&token)
    );
    Ok(RequestToken {
        token,
        secret,
        authorize_url,
    })
}

/// Step 2: exchange the authorized request token + verifier for an access token.
pub async fn complete_auth(
    key: &str,
    secret: &str,
    request_token: &str,
    request_secret: &str,
    verifier: &str,
) -> Result<AccessToken, String> {
    let params = oauth1::signed_params(
        "GET",
        ACCESS_TOKEN_URL,
        key,
        secret,
        Some(request_token),
        request_secret,
        &[("oauth_verifier", verifier)],
    );
    let url = format!("{ACCESS_TOKEN_URL}?{}", oauth1::query_string(&params));
    let body = http_get_text(&url).await?;
    let kv = oauth1::parse_kv(&body);
    let token = kv
        .get("oauth_token")
        .cloned()
        .ok_or_else(|| format!("Flickr authorization failed: {body}"))?;
    let secret = kv.get("oauth_token_secret").cloned().unwrap_or_default();
    let user_nsid = kv.get("user_nsid").cloned().filter(|s| !s.is_empty());
    Ok(AccessToken { token, secret, user_nsid })
}

/// Format catalog keywords as a Flickr `tags` upload parameter: space-separated, with
/// multi-word tags double-quoted (per the Flickr upload API). Embedded double quotes are
/// dropped (Flickr has no escape for them), and blank keywords are skipped.
pub fn format_tags(keywords: &[String]) -> String {
    keywords
        .iter()
        .filter_map(|k| {
            let k = k.replace('"', "");
            let k = k.trim();
            if k.is_empty() {
                return None;
            }
            Some(if k.contains(char::is_whitespace) {
                format!("\"{k}\"")
            } else {
                k.to_string()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Upload `image` to the authenticated user's photostream. Returns the new Flickr photo id.
/// `tags` is the pre-formatted Flickr tags string (see [`format_tags`]); pass "" for none.
pub async fn upload(
    key: &str,
    secret: &str,
    token: &str,
    token_secret: &str,
    image: &Path,
    title: &str,
    description: &str,
    tags: &str,
) -> Result<String, String> {
    let bytes = std::fs::read(image).map_err(|e| format!("couldn't read render: {e}"))?;
    let filename = image
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("photo.jpg")
        .to_string();

    // The signature base covers the oauth_* params AND the text fields (title/description/
    // tags), but NOT the file part. oauth_* go in the Authorization header; the text fields
    // and the file go in the multipart body.
    let text_fields = [
        ("title", title),
        ("description", description),
        ("tags", tags),
    ];
    let params = oauth1::signed_params(
        "POST",
        UPLOAD_URL,
        key,
        secret,
        Some(token),
        token_secret,
        &text_fields,
    );
    let boundary = format!("chairphoto{}", uuid::Uuid::new_v4().simple());
    let body = multipart_body(&boundary, &text_fields, "photo", &filename, &bytes);

    let client = reqwest::Client::new();
    let resp = client
        .post(UPLOAD_URL)
        .header("Authorization", oauth1::auth_header(&params))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .map_err(|e| format!("Flickr upload request failed: {e}"))?;
    let text = resp.text().await.map_err(|e| e.to_string())?;
    parse_upload_response(&text)
}

async fn http_get_text(url: &str) -> Result<String, String> {
    reqwest::get(url)
        .await
        .map_err(|e| format!("Flickr request failed: {e}"))?
        .text()
        .await
        .map_err(|e| e.to_string())
}

/// Build a `multipart/form-data` body by hand (avoids reqwest's `multipart` feature): the
/// text `fields` first, then one file part (`image/jpeg`).
fn multipart_body(
    boundary: &str,
    fields: &[(&str, &str)],
    file_field: &str,
    filename: &str,
    file_bytes: &[u8],
) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"{file_field}\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: image/jpeg\r\n\r\n");
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// Flickr replies with XML: `<rsp stat="ok"><photoid>123</photoid></rsp>` on success, or
/// `<rsp stat="fail"><err code="…" msg="…"/></rsp>` on error.
fn parse_upload_response(xml: &str) -> Result<String, String> {
    if let Some(id) = between(xml, "<photoid>", "</photoid>") {
        return Ok(id);
    }
    if let Some(msg) = between(xml, "msg=\"", "\"") {
        return Err(format!("Flickr upload failed: {msg}"));
    }
    Err(format!("Flickr upload: unexpected response: {xml}"))
}

/// Format a list of paths for inclusion in an Ambiguous reason string.
/// Joins at most 10 entries; if there are more, appends "… and N more".
fn format_paths(paths: &[&str]) -> String {
    const MAX: usize = 10;
    if paths.len() <= MAX {
        paths.join(", ")
    } else {
        let head = paths[..MAX].join(", ");
        format!("{head}, … and {} more", paths.len() - MAX)
    }
}

fn between(s: &str, start: &str, end: &str) -> Option<String> {
    let i = s.find(start)? + start.len();
    let rest = &s[i..];
    let j = rest.find(end)?;
    Some(rest[..j].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_photoid_and_error() {
        let ok = r#"<?xml version="1.0"?><rsp stat="ok"><photoid>54321</photoid></rsp>"#;
        assert_eq!(parse_upload_response(ok).unwrap(), "54321");
        let fail = r#"<rsp stat="fail"><err code="98" msg="Invalid auth token"/></rsp>"#;
        assert!(parse_upload_response(fail).unwrap_err().contains("Invalid auth token"));
    }

    #[test]
    fn multipart_has_fields_and_file() {
        let body = multipart_body("B", &[("title", "Hi")], "photo", "a.jpg", b"\xff\xd8");
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("name=\"title\""));
        assert!(s.contains("Hi"));
        assert!(s.contains("filename=\"a.jpg\""));
        assert!(s.contains("--B--"));
    }

    // ── Fix 1: Flickr error responses deserialise without a `photos` key ──────

    #[test]
    fn api_fail_response_surfaces_flickr_message() {
        // A real Flickr error body: no `photos` key, stat="fail", code + message present.
        let body = r#"{"stat":"fail","code":100,"message":"Invalid API Key (Key has invalid format)"}"#;
        // Deserialisation must succeed (photos is Option).
        let resp: GetPhotosResponse = serde_json::from_str(body)
            .expect("fail body should deserialise");
        assert_ne!(resp.stat, "ok");
        // Caller converts non-ok stat to an error string containing the Flickr message.
        let err = format!(
            "Flickr API error {}: {}",
            resp.code.unwrap_or(0),
            resp.message.unwrap_or_else(|| resp.stat.clone())
        );
        assert!(
            err.contains("Invalid API Key"),
            "error message should contain the Flickr message, got: {err}"
        );
    }

    // ── Matching unit tests ────────────────────────────────────────────────────

    fn fp(id: &str, title: &str, date_taken: &str) -> FlickrPhoto {
        FlickrPhoto {
            id: id.to_string(),
            owner: "user1".to_string(),
            title: title.to_string(),
            date_taken: date_taken.to_string(),
            date_upload_unix: 1_700_000_000,
            thumb_url: None,
        }
    }

    fn cp(id: i64, path: &str, capture_time: Option<&str>) -> CatalogPhotoRow {
        CatalogPhotoRow {
            id,
            path: path.to_string(),
            capture_time: capture_time.map(str::to_string),
        }
    }

    fn matched(outcomes: &[MatchOutcome]) -> Vec<(i64, &str)> {
        outcomes
            .iter()
            .filter_map(|o| {
                if let MatchOutcome::Matched { catalog_id, flickr, .. } = o {
                    Some((*catalog_id, flickr.id.as_str()))
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn datetime_and_title_agree() {
        let flickr = vec![fp("F1", "DSC01234", "2023-05-10 12:30:00")];
        let catalog = vec![cp(10, "2023/DSC01234.ARW", Some("2023-05-10T12:30:00"))];
        let out = match_photos(&flickr, &catalog, &[]);
        let m = matched(&out);
        assert_eq!(m, vec![(10, "F1")]);
    }

    #[test]
    fn datetime_only_match() {
        // title differs (user renamed on Flickr), but datetime is unique
        let flickr = vec![fp("F2", "My Sunset", "2023-06-01 08:00:00")];
        let catalog = vec![cp(20, "summer/DSC04567.ARW", Some("2023-06-01T08:00:00"))];
        let out = match_photos(&flickr, &catalog, &[]);
        let m = matched(&out);
        assert_eq!(m, vec![(20, "F2")]);
    }

    #[test]
    fn title_only_match() {
        // catalog capture_time is NULL, but title (stem) matches
        let flickr = vec![fp("F3", "IMG_4200", "")];
        let catalog = vec![cp(30, "iphone/IMG_4200.heic", None)];
        let out = match_photos(&flickr, &catalog, &[]);
        let m = matched(&out);
        assert_eq!(m, vec![(30, "F3")]);
    }

    #[test]
    fn title_full_filename_match() {
        // Flickr title is the full filename including extension
        let flickr = vec![fp("F4", "DSC01234.ARW", "2023-05-10 12:30:00")];
        let catalog = vec![cp(40, "raw/DSC01234.ARW", Some("2023-05-10T12:30:00"))];
        let out = match_photos(&flickr, &catalog, &[]);
        let m = matched(&out);
        assert_eq!(m, vec![(40, "F4")]);
    }

    #[test]
    fn burst_same_second_is_ambiguous() {
        // Two photos with the same capture_time (burst) and no useful title → ambiguous.
        let flickr = vec![fp("F5", "burst", "2023-07-04 15:00:00")];
        let catalog = vec![
            cp(50, "burst/DSC00001.ARW", Some("2023-07-04T15:00:00")),
            cp(51, "burst/DSC00002.ARW", Some("2023-07-04T15:00:00")),
        ];
        let out = match_photos(&flickr, &catalog, &[]);
        assert!(matches!(out[0], MatchOutcome::Ambiguous { .. }));
    }

    // Fix 2 test (a): burst of two, title disambiguates to one → Matched.
    #[test]
    fn burst_disambiguated_by_title_matches() {
        // datetime hits {50, 51}; title uniquely hits {50} (it's in the datetime set).
        let flickr = vec![fp("F5a", "DSC00001", "2023-07-04 15:00:00")];
        let catalog = vec![
            cp(50, "burst/DSC00001.ARW", Some("2023-07-04T15:00:00")),
            cp(51, "burst/DSC00002.ARW", Some("2023-07-04T15:00:00")),
        ];
        let out = match_photos(&flickr, &catalog, &[]);
        let m = matched(&out);
        assert_eq!(m, vec![(50, "F5a")], "title should disambiguate burst");
    }

    // Fix 2 test (b): title matches a photo NOT in the datetime candidate set → Ambiguous.
    #[test]
    fn title_outside_datetime_set_is_ambiguous() {
        // datetime hits {50, 51}; title hits {99} which is not in {50, 51} → conflict.
        let flickr = vec![fp("F5b", "DSC00099", "2023-07-04 15:00:00")];
        let catalog = vec![
            cp(50, "burst/DSC00001.ARW", Some("2023-07-04T15:00:00")),
            cp(51, "burst/DSC00002.ARW", Some("2023-07-04T15:00:00")),
            cp(99, "other/DSC00099.ARW", Some("2023-07-04T16:00:00")),
        ];
        let out = match_photos(&flickr, &catalog, &[]);
        assert!(
            matches!(out[0], MatchOutcome::Ambiguous { .. }),
            "title outside datetime set should be Ambiguous"
        );
    }

    #[test]
    fn signals_conflict_is_ambiguous() {
        // datetime → photo 60, title → photo 61; signals disagree.
        let flickr = vec![fp("F6", "DSC99999", "2023-08-01 10:00:00")];
        let catalog = vec![
            cp(60, "a/DSC00001.ARW", Some("2023-08-01T10:00:00")),
            cp(61, "b/DSC99999.ARW", Some("2023-08-02T10:00:00")),
        ];
        let out = match_photos(&flickr, &catalog, &[]);
        assert!(matches!(out[0], MatchOutcome::Ambiguous { .. }));
    }

    #[test]
    fn no_match_is_unmatched() {
        let flickr = vec![fp("F7", "UnknownPhoto", "2020-01-01 00:00:00")];
        let catalog = vec![cp(70, "x/DSC00001.ARW", Some("2021-01-01T00:00:00"))];
        let out = match_photos(&flickr, &catalog, &[]);
        assert!(matches!(out[0], MatchOutcome::Unmatched { .. }));
    }

    #[test]
    fn normalise_dt_handles_both_separators() {
        assert_eq!(normalise_dt("2023-05-10T12:30:00"), "20230510123000");
        assert_eq!(normalise_dt("2023-05-10 12:30:00"), "20230510123000");
        assert_eq!(normalise_dt(""), "");
    }

    #[test]
    fn page_url_format() {
        let f = fp("123", "t", "");
        assert_eq!(f.page_url(), "https://www.flickr.com/photos/user1/123/");
    }

    // ── Fix 1: RAW+JPEG sibling collapse ──────────────────────────────────────

    /// (a) RAW+JPG pair: both datetime and title hit both files → Matched on the RAW.
    #[test]
    fn raw_jpeg_pair_matched_on_raw() {
        let flickr = vec![fp("F10", "IMG_3531", "2011-04-09 14:22:05")];
        let catalog = vec![
            cp(100, "2011/04/09/IMG_3531.CR2", Some("2011-04-09T14:22:05")),
            cp(101, "2011/04/09/IMG_3531.JPG", Some("2011-04-09T14:22:05")),
        ];
        let out = match_photos(&flickr, &catalog, &[]);
        assert_eq!(
            matched(&out),
            vec![(100, "F10")],
            "should pick the RAW over the JPG"
        );
    }

    /// (e) JPG-only sibling set (no RAW) with one stem → Matched on the JPG.
    #[test]
    fn jpeg_only_pair_collapses_to_one() {
        let flickr = vec![fp("F11", "PANO_0042", "2015-08-20 10:00:00")];
        let catalog = vec![
            cp(110, "2015/PANO_0042.JPG", Some("2015-08-20T10:00:00")),
            cp(111, "2015/PANO_0042.jpg", Some("2015-08-20T10:00:00")), // duplicate extension case
        ];
        let out = match_photos(&flickr, &catalog, &[]);
        // Both share the same stem and second; no RAW exists → collapse picks first.
        assert!(
            matches!(out[0], MatchOutcome::Matched { .. }),
            "JPG-only pair should collapse to Matched, got: {:?}", out[0]
        );
    }

    // ── Fix 2: Flickr title with trailing image extension ─────────────────────

    /// (b) Title `_81A8352-2.jpg` vs catalog stems `_81a8352-2` (CR2), `_81a8352` (CR2),
    ///     `_81a8353` (CR2), all at the same capture second → Matched on `_81a8352-2.CR2`.
    #[test]
    fn title_with_jpg_extension_matches_raw_stem() {
        let flickr = vec![fp("F12", "_81A8352-2.jpg", "2013-09-15 09:30:00")];
        let catalog = vec![
            cp(120, "2013/_81A8352-2.CR2", Some("2013-09-15T09:30:00")),
            cp(121, "2013/_81A8352.CR2",   Some("2013-09-15T09:30:00")),
            cp(122, "2013/_81A8353.CR2",   Some("2013-09-15T09:30:00")),
        ];
        let out = match_photos(&flickr, &catalog, &[]);
        // Signal A (datetime): all three at 09:30:00.
        // Signal B (title "_81a8352-2" after stripping ".jpg"): only id=120.
        // Intersection = {120} → Matched.
        assert_eq!(
            matched(&out),
            vec![(120, "F12")],
            "title extension strip should isolate the -2 file"
        );
    }

    /// (d) Title with dots that are not an image extension must not be truncated.
    #[test]
    fn title_with_non_extension_dot_not_stripped() {
        // "Sunset at 19.30" — ".30" is not a recognised image extension, must stay intact.
        assert_eq!(title_stem("sunset at 19.30"), "sunset at 19.30");
        // ".jpg" IS an extension and must be stripped.
        assert_eq!(title_stem("photo.jpg"), "photo");
        // ".tiff" stripped.
        assert_eq!(title_stem("scan.tiff"), "scan");
        // ".mp4" is NOT in the list, must not be stripped.
        assert_eq!(title_stem("video.mp4"), "video.mp4");
    }

    // ── Fix 3: plausibility window for B-only matches ─────────────────────────

    /// (c) Title `img_3531` with datetaken 2009 vs catalog IMG_3531.CR2+JPG captured 2011
    ///     → Unmatched (>48 h apart; camera recycled filename).
    #[test]
    fn filename_reuse_across_years_is_unmatched() {
        let flickr = vec![fp("F13", "img_3531", "2009-03-12 11:00:00")];
        let catalog = vec![
            cp(130, "2011/04/09/IMG_3531.CR2", Some("2011-04-09T14:22:05")),
            cp(131, "2011/04/09/IMG_3531.JPG", Some("2011-04-09T14:22:05")),
        ];
        let out = match_photos(&flickr, &catalog, &[]);
        assert!(
            matches!(out[0], MatchOutcome::Unmatched { .. }),
            "filename match across years without datetime corroboration should be Unmatched, got: {:?}", out[0]
        );
    }

    /// Fix 3: B-only candidate with NULL capture_time survives the 48 h filter.
    #[test]
    fn b_only_null_capture_time_survives_plausibility_filter() {
        // Flickr has a datetaken, catalog row has NULL capture_time → can't disprove → keep.
        let flickr = vec![fp("F14", "IMG_0001", "2010-06-15 08:00:00")];
        let catalog = vec![cp(140, "misc/IMG_0001.JPG", None)];
        let out = match_photos(&flickr, &catalog, &[]);
        assert!(
            matches!(out[0], MatchOutcome::Matched { .. }),
            "NULL capture_time should survive the 48 h plausibility filter"
        );
    }

    /// Fix 3: when A is non-empty (datetime corroborates), no plausibility window is applied.
    #[test]
    fn a_non_empty_bypasses_plausibility_window() {
        // Signal A hits id=150 (exact second), Signal B also hits id=150 via stem.
        // Even though both signals agree, the key check: when A is available, the 48 h filter
        // on B is NOT applied (it's only a fix for B-alone cases).
        let flickr = vec![fp("F15", "DSC08888", "2022-12-31 23:59:00")];
        let catalog = vec![cp(150, "2022/DSC08888.ARW", Some("2022-12-31T23:59:00"))];
        let out = match_photos(&flickr, &catalog, &[]);
        assert_eq!(matched(&out), vec![(150, "F15")]);
    }

    // ── Intersection logic ────────────────────────────────────────────────────

    /// A∩B intersection: datetime hits 3 photos, title hits 2 of those 3 → match both
    /// intersection members (still ambiguous if > 1 after collapse).
    #[test]
    fn intersection_multi_remains_ambiguous() {
        let flickr = vec![fp("F16", "burst", "2024-01-01 10:00:00")];
        let catalog = vec![
            cp(160, "2024/DSC00010.ARW", Some("2024-01-01T10:00:00")),
            cp(161, "2024/DSC00011.ARW", Some("2024-01-01T10:00:00")),
            cp(162, "2024/DSC00012.ARW", Some("2024-01-01T10:00:00")),
        ];
        // "burst" doesn't match any stem → B is empty → only A (3 candidates) → ambiguous.
        let out = match_photos(&flickr, &catalog, &[]);
        assert!(matches!(out[0], MatchOutcome::Ambiguous { .. }));
    }

    // ── Fix 1 (FIX 1 blocker): dt_within_48h calendar-boundary tests ─────────

    /// (a) Dec 31 23:59 vs Jan 1 00:01 two minutes apart — old code computed ~5 days, new code
    ///     must return within-48h.
    #[test]
    fn dt_within_48h_year_boundary() {
        let a = normalise_dt("2023-12-31 23:59:00");
        let b = normalise_dt("2024-01-01 00:01:00");
        assert!(
            dt_within_48h(&a, &b),
            "Dec 31 23:59 → Jan 1 00:01 is 2 minutes, must be within 48h"
        );
    }

    /// (b) Feb 28 vs Mar 1 in a leap year (2024) — one day apart, must be within 48h.
    #[test]
    fn dt_within_48h_leap_year_feb28_to_mar1() {
        let a = normalise_dt("2024-02-28 12:00:00");
        let b = normalise_dt("2024-03-01 12:00:00");
        assert!(
            dt_within_48h(&a, &b),
            "2024-02-28 → 2024-03-01 is 1 day in a leap year, must be within 48h"
        );
    }

    /// (c) 2009 vs 2011, same month and day — ~730 days apart, must NOT be within 48h.
    #[test]
    fn dt_within_48h_two_years_apart_not_within() {
        let a = normalise_dt("2009-06-15 10:00:00");
        let b = normalise_dt("2011-06-15 10:00:00");
        assert!(
            !dt_within_48h(&a, &b),
            "2009 vs 2011 same month/day is ~730 days, must NOT be within 48h"
        );
    }

    // ── Fix 2: collapse_raw_jpeg_siblings — per-stem-group behaviour ─────────

    /// Same stem, RAW + JPG with different capture seconds → the RAW wins anyway.
    /// Owner rule (2026-07-24): if there is a RAW, always link the RAW — exports often
    /// lose or shift the EXIF time, so second-mismatch must not resurrect the JPG.
    #[test]
    fn collapse_same_stem_raw_beats_jpg_despite_different_seconds() {
        let row_a = cp(200, "2023/DSC00100.ARW", Some("2023-05-01T10:00:00"));
        let row_b = cp(201, "2023/DSC00100.JPG", Some("2023-05-01T10:00:01")); // 1 second later
        let result = collapse_raw_jpeg_siblings(vec![&row_a, &row_b]);
        assert_eq!(result.len(), 1, "JPG sibling must be dropped in favour of the RAW");
        assert_eq!(result[0].id, 200);
    }

    /// Same stem, JPG sibling with NO capture time → the RAW still wins.
    #[test]
    fn collapse_same_stem_raw_beats_jpg_without_capture_time() {
        let row_a = cp(210, "2023/DSC00100.ARW", Some("2023-05-01T10:00:00"));
        let row_b = cp(211, "export/DSC00100.jpg", None);
        let result = collapse_raw_jpeg_siblings(vec![&row_a, &row_b]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 210);
    }

    /// Same stem, same rank (two ARWs), different seconds → genuinely different photos
    /// (counter rollover), both kept.
    #[test]
    fn collapse_same_stem_same_rank_different_seconds_kept() {
        let row_a = cp(220, "2015/DSC00100.ARW", Some("2015-05-01T10:00:00"));
        let row_b = cp(221, "2019/DSC00100.ARW", Some("2019-08-01T12:00:00"));
        let result = collapse_raw_jpeg_siblings(vec![&row_a, &row_b]);
        assert_eq!(result.len(), 2, "same-rank rollover twins must both survive");
    }

    /// Mixed burst: two RAW frames + a JPG export of one of them → the JPG collapses into
    /// its RAW group, leaving the two RAW frames as the only (still ambiguous) candidates.
    #[test]
    fn mixed_burst_surfaces_only_raw_candidates() {
        let flickr = vec![fp("F40", "Walking on the bridge", "2026-07-10 21:57:33")];
        let catalog = vec![
            cp(700, "bridge/_DSC8043.ARW", Some("2026-07-10T21:57:33")),
            cp(701, "bridge/_DSC8044.ARW", Some("2026-07-10T21:57:33")),
            cp(702, "bridge/export/_DSC8044.jpg", Some("2026-07-10T21:57:33")),
        ];
        let out = match_photos(&flickr, &catalog, &[]);
        match &out[0] {
            MatchOutcome::Ambiguous { candidates, .. } => {
                let ids: Vec<i64> = candidates.iter().map(|c| c.catalog_id).collect();
                assert_eq!(ids, vec![700, 701], "JPG sibling must not appear as a candidate");
            }
            other => panic!("expected Ambiguous over the two RAW frames, got {other:?}"),
        }
    }

    // ── Part B: ambiguous outcomes carry candidates ───────────────────────────

    /// Burst of two RAWs (same stem, same second) → Ambiguous with both as candidates.
    #[test]
    fn ambiguous_outcome_carries_candidates() {
        let flickr = vec![fp("F30", "burst", "2023-07-04 15:00:00")];
        let catalog = vec![
            cp(500, "burst/DSC00001.ARW", Some("2023-07-04T15:00:00")),
            cp(501, "burst/DSC00002.ARW", Some("2023-07-04T15:00:00")),
        ];
        let out = match_photos(&flickr, &catalog, &[]);
        match &out[0] {
            MatchOutcome::Ambiguous { candidates, .. } => {
                assert_eq!(candidates.len(), 2, "both burst candidates should be surfaced");
                let ids: Vec<i64> = candidates.iter().map(|c| c.catalog_id).collect();
                assert!(ids.contains(&500) && ids.contains(&501));
            }
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    /// Conflict (signals point to different photos) → Ambiguous with union of A and B candidates.
    #[test]
    fn conflict_ambiguous_carries_union_candidates() {
        let flickr = vec![fp("F31", "DSC99999", "2023-08-01 10:00:00")];
        let catalog = vec![
            cp(510, "a/DSC00001.ARW", Some("2023-08-01T10:00:00")), // A hit
            cp(511, "b/DSC99999.ARW", Some("2023-08-02T10:00:00")), // B hit
        ];
        let out = match_photos(&flickr, &catalog, &[]);
        match &out[0] {
            MatchOutcome::Ambiguous { candidates, .. } => {
                assert!(!candidates.is_empty(), "conflict should surface candidates");
                let ids: Vec<i64> = candidates.iter().map(|c| c.catalog_id).collect();
                assert!(ids.contains(&510) || ids.contains(&511));
            }
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    /// Candidates are capped at 10 even when more than 10 burst frames match.
    #[test]
    fn ambiguous_candidates_capped_at_10() {
        let flickr = vec![fp("F32", "burst", "2023-07-04 15:00:00")];
        // 15 different stems but same capture second, title "burst" matches none of them
        // via B signal → only A fires → 15 candidates → Ambiguous, capped at 10.
        let catalog: Vec<CatalogPhotoRow> = (0..15_i64)
            .map(|i| cp(600 + i, &format!("burst/DSC{:05}.ARW", i), Some("2023-07-04T15:00:00")))
            .collect();
        let out = match_photos(&flickr, &catalog, &[]);
        match &out[0] {
            MatchOutcome::Ambiguous { candidates, .. } => {
                assert!(candidates.len() <= 10, "candidates must be capped at 10, got {}", candidates.len());
            }
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    // ── Part A: ext_rank ladder ───────────────────────────────────────────────

    /// CR2 + PSD + JPG of one shot → collapse to CR2 (rank 0 wins over rank 1 and rank 2).
    #[test]
    fn collapse_cr2_psd_jpg_picks_cr2() {
        let cr2 = cp(300, "2013/2013-06-20/_81A7982.CR2", Some("2013-06-20T10:00:00"));
        let psd = cp(301, "2013/2013-06-20/_81A7982.PSD", Some("2013-06-20T10:00:00"));
        let jpg = cp(302, "2013/2013-06-20/_81A7982.JPG", Some("2013-06-20T10:00:00"));
        let result = collapse_raw_jpeg_siblings(vec![&cr2, &psd, &jpg]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 300, "CR2 (rank 0) should beat PSD (rank 1) and JPG (rank 2)");
    }

    /// Two identical CR2s (same stem, same second, same rank) → pick lowest catalog id.
    #[test]
    fn collapse_duplicate_cr2_paths_picks_lowest_id() {
        // Same photo indexed under two different folder layouts.
        let cr2_a = cp(310, "2013/2013-06-20/_81A7982.CR2", Some("2013-06-20T10:00:00"));
        let cr2_b = cp(311, "2013/06/20/_81A7982.CR2",      Some("2013-06-20T10:00:00"));
        let result = collapse_raw_jpeg_siblings(vec![&cr2_a, &cr2_b]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 310, "lowest catalog id should be chosen for duplicate copies");
    }

    /// Duplicate CR2 paths with higher id first → still picks lower id (stable, order-independent).
    #[test]
    fn collapse_duplicate_cr2_order_independent() {
        let cr2_a = cp(320, "2013/2013-06-20/_81A7982.CR2", Some("2013-06-20T11:00:00"));
        let cr2_b = cp(319, "2013/06/20/_81A7982.CR2",      Some("2013-06-20T11:00:00"));
        // cr2_b has lower id (319) even though it's second in the slice.
        let result = collapse_raw_jpeg_siblings(vec![&cr2_a, &cr2_b]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 319, "lowest catalog id wins regardless of input order");
    }

    /// PSD + JPG (no RAW) → PSD wins (rank 1 < rank 2).
    #[test]
    fn collapse_psd_jpg_picks_psd() {
        let psd = cp(330, "2013/_81A7982.PSD", Some("2013-06-20T10:00:00"));
        let jpg = cp(331, "2013/_81A7982.JPG", Some("2013-06-20T10:00:00"));
        let result = collapse_raw_jpeg_siblings(vec![&psd, &jpg]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 330, "PSD (rank 1) should beat JPG (rank 2)");
    }

    /// Uniform set CR2+PSD+JPG via match_photos: the ambiguous outcome disappears.
    #[test]
    fn rank_ladder_via_match_photos_cr2_psd_jpg() {
        let flickr = vec![fp("F20", "_81A7982", "2013-06-20 10:00:00")];
        let catalog = vec![
            cp(340, "2013/_81A7982.CR2", Some("2013-06-20T10:00:00")),
            cp(341, "2013/_81A7982.PSD", Some("2013-06-20T10:00:00")),
            cp(342, "2013/_81A7982.JPG", Some("2013-06-20T10:00:00")),
        ];
        let out = match_photos(&flickr, &catalog, &[]);
        assert_eq!(
            matched(&out),
            vec![(340, "F20")],
            "CR2+PSD+JPG should collapse to Matched on CR2, not Ambiguous"
        );
    }

    /// Duplicate CR2 paths via match_photos: the ambiguous outcome disappears.
    #[test]
    fn rank_ladder_via_match_photos_duplicate_cr2() {
        let flickr = vec![fp("F21", "_81A7982", "2013-06-20 10:00:00")];
        let catalog = vec![
            cp(350, "2013/2013-06-20/_81A7982.CR2", Some("2013-06-20T10:00:00")),
            cp(351, "2013/06/20/_81A7982.CR2",      Some("2013-06-20T10:00:00")),
        ];
        let out = match_photos(&flickr, &catalog, &[]);
        assert_eq!(
            matched(&out),
            vec![(350, "F21")],
            "duplicate CR2 paths should collapse to Matched on lowest id, not Ambiguous"
        );
    }

    // ── Fix 3: format_paths helper caps at 10 entries ────────────────────────

    #[test]
    fn format_paths_under_limit_no_ellipsis() {
        let paths: Vec<&str> = (0..5).map(|i| ["a", "b", "c", "d", "e"][i]).collect();
        let s = format_paths(&paths);
        assert!(!s.contains("more"), "under limit should have no ellipsis: {s}");
        assert!(s.contains("a") && s.contains("e"));
    }

    #[test]
    fn format_paths_over_limit_appends_count() {
        let paths: Vec<&str> = vec![
            "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9", "p10", "p11", "p12",
        ];
        let s = format_paths(&paths);
        assert!(
            s.contains("and 2 more"),
            "12 paths with cap 10 should say 'and 2 more': {s}"
        );
        assert!(s.contains("p10"), "first 10 paths should be present: {s}");
        assert!(!s.contains("p11"), "11th path should not appear verbatim: {s}");
    }

    // ── format_tags — Flickr upload `tags` parameter ─────────────────────────

    #[test]
    fn format_tags_quotes_multiword_and_skips_blanks() {
        let kws = vec![
            "aurora".to_string(),
            "northern lights".to_string(),
            "  ".to_string(),
            "Norway".to_string(),
        ];
        assert_eq!(format_tags(&kws), "aurora \"northern lights\" Norway");
    }

    #[test]
    fn format_tags_drops_embedded_double_quotes() {
        let kws = vec!["he said \"hi\"".to_string()];
        assert_eq!(format_tags(&kws), "\"he said hi\"");
    }

    #[test]
    fn format_tags_empty_input_is_empty() {
        assert_eq!(format_tags(&[]), "");
    }

    // ── Signal P (recorded publication) + publication-time tiebreak ──────────

    #[test]
    fn flickr_id_from_url_forms() {
        assert_eq!(
            flickr_id_from_url("https://www.flickr.com/photos/12037949754@N08/54321/"),
            Some("54321".to_string())
        );
        assert_eq!(
            flickr_id_from_url("https://www.flickr.com/photos/someuser/54321"),
            Some("54321".to_string())
        );
        assert_eq!(
            flickr_id_from_url("https://www.flickr.com/photo.gne?id=98765"),
            Some("98765".to_string())
        );
        assert_eq!(flickr_id_from_url("https://www.instagram.com/p/abc123/"), None);
        assert_eq!(flickr_id_from_url("https://www.flickr.com/photos/someuser/"), None);
    }

    #[test]
    fn publication_url_match_wins_over_ambiguity() {
        // Same-second burst with a non-filename title — normally ambiguous — but the
        // flickr id is already recorded in a publication URL → matched, no heuristics.
        let flickr = vec![fp("777", "Walking on the bridge", "2026-07-10 21:57:33")];
        let catalog = vec![
            cp(1, "bridge/_DSC8043.ARW", Some("2026-07-10T21:57:33")),
            cp(2, "bridge/_DSC8044.ARW", Some("2026-07-10T21:57:33")),
        ];
        let existing = vec![ExistingPublication {
            catalog_id: 2,
            flickr_id: Some("777".to_string()),
            published_at: 0,
        }];
        let out = match_photos(&flickr, &catalog, &existing);
        assert_eq!(matched(&out), vec![(2, "777")]);
    }

    #[test]
    fn publication_time_tiebreak_resolves_burst() {
        // Legacy URL-less publication row recorded minutes after the Flickr upload —
        // among the two burst candidates only one carries it → matched.
        let flickr = vec![fp("778", "Walking on the bridge", "2026-07-10 21:57:33")];
        let catalog = vec![
            cp(1, "bridge/_DSC8043.ARW", Some("2026-07-10T21:57:33")),
            cp(2, "bridge/_DSC8044.ARW", Some("2026-07-10T21:57:33")),
        ];
        // fp() uploads at 1_700_000_000; the record was stamped two minutes later.
        let existing = vec![ExistingPublication {
            catalog_id: 1,
            flickr_id: None,
            published_at: 1_700_000_000 + 120,
        }];
        let out = match_photos(&flickr, &catalog, &existing);
        assert_eq!(matched(&out), vec![(1, "778")]);
    }

    #[test]
    fn publication_time_tiebreak_requires_unique_hit() {
        // Both burst candidates have near-time flickr publications → stays ambiguous.
        let flickr = vec![fp("779", "Some title", "2026-07-10 21:57:33")];
        let catalog = vec![
            cp(1, "bridge/_DSC8043.ARW", Some("2026-07-10T21:57:33")),
            cp(2, "bridge/_DSC8044.ARW", Some("2026-07-10T21:57:33")),
        ];
        let existing = vec![
            ExistingPublication { catalog_id: 1, flickr_id: None, published_at: 1_700_000_000 },
            ExistingPublication { catalog_id: 2, flickr_id: None, published_at: 1_700_000_010 },
        ];
        let out = match_photos(&flickr, &catalog, &existing);
        assert!(
            matches!(out[0], MatchOutcome::Ambiguous { .. }),
            "two near-time publication hits must not auto-resolve"
        );
    }

    #[test]
    fn stale_publication_url_falls_back_to_heuristics() {
        // The recorded catalog photo is gone from the catalog → normal matching applies.
        let flickr = vec![fp("780", "DSC09999", "2023-05-10 12:30:00")];
        let catalog = vec![cp(5, "x/DSC09999.ARW", Some("2023-05-10T12:30:00"))];
        let existing = vec![ExistingPublication {
            catalog_id: 999,
            flickr_id: Some("780".to_string()),
            published_at: 0,
        }];
        let out = match_photos(&flickr, &catalog, &existing);
        assert_eq!(matched(&out), vec![(5, "780")]);
    }
}
