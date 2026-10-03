//! The Darkroom's backend bodies, shared by the Tauri commands (`commands::develop`,
//! `commands::editing`) and the GPUI Darkroom (`crates/app/src/darkroom`): opening a photo
//! for development, the tone strip's zone masses, the proof sheet's auto-tone fragment, the
//! `.cube` LUT folder, and writing a version's settings with the monochrome refresh every such
//! write owes.
//!
//! **Blocking.** Every function here takes the catalog lock, stats files or renders; call
//! it on a worker (`spawn_blocking`, the app's storage runner), never on a UI thread.
//!
//! **Catalog identity.** Each id-keyed function takes `from: Option<CatalogIdentity>`. The
//! GPUI app passes the identity it read the photo or version under, so a catalog switch in
//! between fails the call closed with [`CATALOG_CHANGED`](super::CATALOG_CHANGED) instead of
//! reading or writing the new catalog's row with the same id (map #92, "Catalog identity").
//! The Tauri commands pass `None`, which is their behaviour before this module existed.

use super::{with_catalog, with_catalog_as, AppState, CatalogIdentity};
use crate::catalog::Catalog;
use crate::develop_source::{probe_source, DevelopSource};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// [`with_catalog`], or [`with_catalog_as`] when bound to an identity.
pub fn with_catalog_from<T>(
    state: &AppState,
    from: Option<CatalogIdentity>,
    f: impl FnOnce(&Catalog) -> crate::catalog::Result<T>,
) -> Result<T, String> {
    match from {
        Some(id) => with_catalog_as(state, id, f),
        None => with_catalog(state, f),
    }
}

/// A photo's reachable original: candidates under a brief catalog lock, then a stat off it
/// (`OriginalRequired`: an edit render, a decode or a probe needs the real file).
pub fn original_path(state: &AppState, from: Option<CatalogIdentity>, photo_id: i64) -> Result<PathBuf, String> {
    let candidates = with_catalog_from(state, from, |c| c.photo_path_candidates(photo_id))?;
    crate::volume_health::pick_existing(&candidates, &state.volume_health, crate::catalog::ResolveMode::OriginalRequired)
        .ok_or_else(|| format!("no reachable copy of photo {photo_id}"))
}

/// What a develop open answers when a newer open or close was made before it took effect
/// (#225): it claimed nothing and released nothing. A front end drops it, as it drops any
/// answer for a photo it has left.
pub const DEVELOP_SUPERSEDED: &str = "A newer Develop call came first";

/// A develop session call's place among the calls a front end made: minted when the call is
/// made ([`develop_ticket`]), not when a worker gets to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DevelopTicket(u64);

/// The order of the develop session's opens and closes (#203, #225). Both run on a blocking
/// pool, which may run a newer call first, and one of them may stall for long (an open reads
/// a RAW header from a share that stopped answering). So the calls are not queued; each
/// carries the [`DevelopTicket`] minted when it was made, and its ownership transition — an
/// open's claim, a close's trip — takes effect only if no newer call's has. A close that
/// runs after a newer open never trips that open's claim, and an open that answers after a
/// newer open or close claims nothing.
///
/// `applied` is held across the transition, so two transitions never interleave; it comes
/// before the catalog in the lock order (`app::jobs`), and nothing takes it while holding
/// another lock.
#[derive(Debug, Default)]
pub struct DevelopOrder {
    minted: AtomicU64,
    /// The newest ticket whose transition took effect (or was attempted: a failed one still
    /// supersedes every older call).
    applied: Mutex<u64>,
}

impl DevelopOrder {
    /// A ticket for a call made now: newer than every ticket minted before.
    pub fn ticket(&self) -> DevelopTicket {
        DevelopTicket(self.minted.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Whether a newer call has already taken effect — an open can then skip its slow work.
    pub fn superseded(&self, ticket: DevelopTicket) -> Result<bool, String> {
        Ok(*self.applied.lock().map_err(|e| e.to_string())? >= ticket.0)
    }

    /// Run `transition` if `ticket` is newer than every call that took effect so far, and
    /// record it; `Ok(None)` when it is not (the call is stale, nothing ran).
    pub fn apply<T>(&self, ticket: DevelopTicket, transition: impl FnOnce() -> Result<T, String>) -> Result<Option<T>, String> {
        let mut applied = self.applied.lock().map_err(|e| e.to_string())?;
        if *applied >= ticket.0 {
            return Ok(None);
        }
        *applied = ticket.0;
        transition().map(Some)
    }

    /// Record `ticket` as made with no transition (an open that claims nothing: not a RAW,
    /// no reachable copy), so an older open that answers later claims nothing either.
    pub fn settle(&self, ticket: DevelopTicket) -> Result<(), String> {
        self.apply(ticket, || Ok(())).map(|_| ())
    }
}

/// A ticket for a develop open or close made now; mint it where the call is made (the UI
/// thread, a command's entry), not on the worker that runs it.
pub fn develop_ticket(state: &AppState) -> DevelopTicket {
    state.jobs.develop_order.ticket()
}

/// The Darkroom opened `photo_id`: claim the develop session and start preparing its
/// working image, then — at lower priority, silently — its `neighbours` (N+1 first).
/// Returns the state right now; changes arrive as `develop:source`. A neighbour that is not
/// a RAW or whose original is unreachable is simply not preloaded. A JPEG claims nothing; a
/// previous photo's image is released by the next open or by [`develop_close`].
///
/// `ticket` orders it among the session's calls ([`DevelopOrder`]): if a newer open or close
/// took effect first, this one claims nothing and answers [`DEVELOP_SUPERSEDED`].
pub fn develop_open(
    state: &AppState,
    from: Option<CatalogIdentity>,
    photo_id: i64,
    neighbours: &[i64],
    ticket: DevelopTicket,
) -> Result<DevelopSource, String> {
    let order = &state.jobs.develop_order;
    if order.superseded(ticket)? {
        return Err(DEVELOP_SUPERSEDED.into());
    }
    let path = match original_path(state, from, photo_id) {
        Ok(path) => path,
        Err(e) => {
            order.settle(ticket)?;
            return Err(e);
        }
    };
    if !crate::scanner::is_raw(&path) {
        order.settle(ticket)?;
        return Ok(DevelopSource::Jpeg);
    }
    let probe = probe_source(&path);
    #[cfg(all(feature = "raw", feature = "edit"))]
    {
        let neighbours: Vec<(i64, PathBuf)> = neighbours
            .iter()
            .copied()
            .filter(|&n| n != photo_id)
            .filter_map(|n| original_path(state, from, n).ok().map(|p| (n, p)))
            .filter(|(_, p)| crate::scanner::is_raw(p))
            .collect();
        crate::develop::session::open(state, ticket, photo_id, path, probe, neighbours)
    }
    #[cfg(not(all(feature = "raw", feature = "edit")))]
    {
        let _ = neighbours;
        order.settle(ticket)?;
        Ok(probe)
    }
}

/// The develop source state right now for `photo_id` (a view re-attaching).
pub fn develop_current(state: &AppState, from: Option<CatalogIdentity>, photo_id: i64) -> Result<DevelopSource, String> {
    let path = original_path(state, from, photo_id)?;
    if !crate::scanner::is_raw(&path) {
        return Ok(DevelopSource::Jpeg);
    }
    let probe = probe_source(&path);
    #[cfg(all(feature = "raw", feature = "edit"))]
    {
        Ok(crate::develop::session::current(state, photo_id, probe))
    }
    #[cfg(not(all(feature = "raw", feature = "edit")))]
    {
        Ok(probe)
    }
}

/// Develop closed: release the working images. Idempotent; nothing to do without `raw` +
/// `edit`. A close made before an open that already took effect does nothing ([`DevelopOrder`]).
pub fn develop_close(state: &AppState, ticket: DevelopTicket) -> Result<(), String> {
    #[cfg(all(feature = "raw", feature = "edit"))]
    {
        crate::develop::session::close(state, ticket)
    }
    #[cfg(not(all(feature = "raw", feature = "edit")))]
    {
        state.jobs.develop_order.settle(ticket)
    }
}

/// A render's `source` argument as a working-image token, or `None` for the preview path.
#[cfg(feature = "edit")]
pub fn parse_working_token(source: Option<&str>) -> Result<Option<crate::plugins::edit::SourceToken>, String> {
    use crate::plugins::edit::SourceToken;
    match source {
        None | Some("") | Some("p") => Ok(None),
        Some(s) => match SourceToken::parse(s) {
            Some(SourceToken::Preview) => Ok(None),
            Some(t) => Ok(Some(t)),
            None => Err(format!("bad source token {s:?}")),
        },
    }
}

/// Share of pixels per Darkroom tone-strip zone — 8 equal gamma-luma bands of the photo's
/// **rendered working state**, so the strip always describes the print it sits under. 1024 px
/// is plenty for an 8-bin histogram and goes through the framed-base cache: the settle that
/// asks for masses has the geometry of the drag before it.
#[cfg(feature = "edit")]
pub fn zone_masses(
    state: &AppState,
    from: Option<CatalogIdentity>,
    photo_id: i64,
    edit_json: &str,
    source: Option<&str>,
) -> Result<[f32; 8], String> {
    use crate::plugins::edit::{render_proxy, RenderOpts, RenderSource};
    let path = original_path(state, from, photo_id)?;
    let out = if let Some(token) = parse_working_token(source)? {
        let image = crate::media::working_image(&token)?;
        render_proxy(RenderSource::Working { token, image }, edit_json, 1024, RenderOpts::default())?
    } else {
        let jpeg = crate::thumbnails::preview_bytes(&path)?;
        render_proxy(RenderSource::PreviewJpeg(&jpeg), edit_json, 1024, RenderOpts::default())?
    };
    Ok(crate::plugins::edit::zone_masses(&out.to_rgb8()))
}

/// Classical auto-tone suggestion for the Darkroom's proof sheet: a percentile analysis of
/// the proxy's luma histogram → an edit-json *fragment* with only
/// `tone.ev/contrast/highlights/shadows` (docs/plans/darkroom).
///
/// On the RAW engine (`source` names a resident working image) the analysis reads what the
/// stage shows as shot: the working image through `base_json` (engine, display transform and
/// camera match, no adjustments), so the fragment's EV means linear stops on the picture
/// being developed; otherwise the camera preview.
#[cfg(feature = "edit")]
pub fn suggest_auto_tone(
    state: &AppState,
    from: Option<CatalogIdentity>,
    photo_id: i64,
    source: Option<&str>,
    base_json: Option<&str>,
) -> Result<String, String> {
    let path = original_path(state, from, photo_id)?;
    let rgb = if let Some(token) = parse_working_token(source)? {
        let image = crate::media::working_image(&token)?;
        crate::plugins::edit::render_proxy(
            crate::plugins::edit::RenderSource::Working { token, image },
            base_json.unwrap_or(r#"{"engine":2}"#),
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
}

/// The `.cube` LUT filenames in `dir` (the app's [`luts_dir`](super::luts_dir)), sorted.
pub fn list_luts_in(dir: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let name = entry.map_err(|e| e.to_string())?.file_name().to_string_lossy().to_string();
        if name.to_lowercase().ends_with(".cube") {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// Validate a `.cube` file (by fully parsing it) and copy it into `dir`. Returns the bare
/// filename an edit record references. The source file is only read.
#[cfg(feature = "edit")]
pub fn import_lut_into(dir: &Path, path: &Path) -> Result<String, String> {
    let name = path.file_name().ok_or("not a file path")?.to_string_lossy().to_string();
    if !name.to_lowercase().ends_with(".cube") {
        return Err("only .cube LUTs are supported".to_string());
    }
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    crate::plugins::edit::cube::CubeLut::parse(&text).map_err(|e| format!("invalid LUT: {e}"))?;
    std::fs::write(dir.join(&name), text).map_err(|e| e.to_string())?;
    Ok(name)
}

/// Any write that changes a version's settings — a plain save, a history commit, a step
/// back or forward, a new version's first record — followed by the monochrome refresh
/// every such write owes (H6): a B&W develop on *any* version marks the photo monochrome;
/// when the last B&W version is edited away, the flag falls back to the pixel-derived
/// signal (recomputed from the cached thumbnail, off the lock). Returns what `write`
/// returned. With `from`, both catalog holds are bound to that catalog.
#[cfg(feature = "edit")]
pub fn write_version_then_refresh_monochrome<T>(
    state: &AppState,
    from: Option<CatalogIdentity>,
    version_id: i64,
    write: impl FnOnce(&Catalog) -> crate::catalog::Result<T>,
) -> Result<T, String> {
    // Save + gather everything the monochrome refresh needs under one brief lock.
    let (written, photo_id, any_bw, stored_gray, candidates) = with_catalog_from(state, from, |catalog| {
        let written = write(catalog)?;
        let photo_id = catalog.version_photo_id(version_id)?;
        let any_bw = catalog.list_versions(photo_id)?.iter().any(|v| crate::plugins::edit::is_bw(&v.edit_json));
        let stored = catalog.is_grayscale(photo_id)?;
        let cands = catalog.photo_path_candidates(photo_id)?;
        Ok((written, photo_id, any_bw, stored, cands))
    })?;
    let gray = if any_bw {
        true
    } else if !stored_gray {
        // Not B&W by edit and already not flagged — nothing can change.
        return Ok(written);
    } else {
        // The flag was set but no version is B&W anymore: fall back to the pixel-derived
        // signal (the photo itself may still be monochrome). OriginalRequired: the outcome is
        // PERSISTED, so it must not be decided by a cached reachability flag — or by a
        // decode failure. `None` covers both ways "could not tell" happens (no reachable
        // copy; a copy that fails to decode), and both leave the stored flag alone
        // (AGENTS.md: missing storage is normal, never evidence the row is wrong).
        let outcome = crate::volume_health::pick_existing(
            &candidates,
            &state.volume_health,
            crate::catalog::ResolveMode::OriginalRequired,
        )
        .and_then(|p| crate::thumbnails::thumbnail_bytes(&p).ok())
        .map(|t| crate::thumbnails::is_grayscale_jpeg(&t));
        match outcome {
            Some(g) => g,
            None => return Ok(written),
        }
    };
    if gray != stored_gray {
        with_catalog_from(state, from, |catalog| {
            catalog.set_grayscale(photo_id, gray)?;
            catalog.apply_auto_tags()
        })?;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{catalog_identity, CATALOG_CHANGED};
    use crate::test_support::TestTmpDir;

    fn catalog(dir: &Path, name: &str) -> (Catalog, i64, i64) {
        let root = dir.join(name);
        let c = Catalog::open(&root.join("c.chairphoto"), &root).unwrap();
        let photo = c.upsert_photo(&root.join("p.jpg"), None, 0, 1).unwrap().id;
        let version = c.create_version(photo, "V").unwrap();
        (c, photo, version)
    }

    // --- the develop session's order (#225) ------------------------------------------------

    /// A develop open older than a call that already took effect answers
    /// [`DEVELOP_SUPERSEDED`] before reading anything; an open that claims nothing (a JPEG,
    /// no reachable copy) still counts as taking effect, so an older open that answers after
    /// it claims nothing either. A ticket takes effect once.
    #[test]
    fn a_develop_open_older_than_a_call_that_took_effect_is_superseded() {
        let dir = TestTmpDir::new("app-editing-develop-order");
        let state = AppState::default();
        let (a, photo, _) = catalog(&dir, "a");
        std::fs::write(dir.join("a").join("p.jpg"), b"jpeg").unwrap();
        *state.catalog.lock().unwrap() = Some(a);
        let (older, newer) = (develop_ticket(&state), develop_ticket(&state));
        assert!(older < newer);
        assert_eq!(develop_open(&state, None, photo, &[], newer).unwrap(), DevelopSource::Jpeg);
        assert_eq!(develop_open(&state, None, photo, &[], older).unwrap_err(), DEVELOP_SUPERSEDED);
        assert_eq!(develop_open(&state, None, photo, &[], newer).unwrap_err(), DEVELOP_SUPERSEDED, "once");

        let next = develop_ticket(&state);
        std::fs::remove_file(dir.join("a").join("p.jpg")).unwrap();
        assert!(develop_open(&state, None, photo, &[], next).unwrap_err().starts_with("no reachable copy"));
        assert!(state.jobs.develop_order.superseded(next).unwrap(), "an unreachable open still took its turn");

        let order = DevelopOrder::default();
        let (t1, t2) = (order.ticket(), order.ticket());
        assert_eq!(order.apply(t2, || Ok(2)).unwrap(), Some(2));
        assert_eq!(order.apply(t1, || -> Result<i32, String> { panic!("a stale call runs nothing") }).unwrap(), None);
        let t3 = order.ticket();
        assert!(order.apply(t3, || -> Result<(), String> { Err("failed".into()) }).is_err());
        assert!(order.superseded(t3).unwrap(), "a failed transition still supersedes the calls before it");
    }

    /// The identity-bound calls fail closed once another catalog is open — one whose photo
    /// and version carry the same ids — and touch nothing in it.
    #[test]
    fn calls_bound_to_a_switched_away_catalog_fail_closed() {
        let dir = TestTmpDir::new("app-editing-identity");
        let state = AppState::default();
        let (a, photo, version) = catalog(&dir, "a");
        *state.catalog.lock().unwrap() = Some(a);
        let from = catalog_identity(&state).unwrap();
        let (b, b_photo, b_version) = catalog(&dir, "b");
        assert_eq!((b_photo, b_version), (photo, version), "the ids collide");
        *state.catalog.lock().unwrap() = Some(b);

        assert_eq!(original_path(&state, Some(from), photo).unwrap_err(), CATALOG_CHANGED);
        assert_eq!(develop_open(&state, Some(from), photo, &[], develop_ticket(&state)).unwrap_err(), CATALOG_CHANGED);
        #[cfg(feature = "edit")]
        {
            assert_eq!(zone_masses(&state, Some(from), photo, "{}", None).unwrap_err(), CATALOG_CHANGED);
            assert_eq!(suggest_auto_tone(&state, Some(from), photo, None, None).unwrap_err(), CATALOG_CHANGED);
            let err = write_version_then_refresh_monochrome(&state, Some(from), version, |c| {
                c.commit_version_edit(version, r#"{"tone":{"ev":1}}"#, "Exposure +1.00", false)
            })
            .unwrap_err();
            assert_eq!(err, CATALOG_CHANGED);
            let b = state.catalog.lock().unwrap();
            let b = b.as_ref().unwrap();
            assert_eq!(b.list_versions(photo).unwrap()[0].edit_json, "{}", "the new catalog's version was written");
            assert!(b.version_history(version).unwrap().steps.is_empty());
        }
        // Unbound (the Tauri commands), the call reads the open catalog.
        assert!(original_path(&state, None, photo).unwrap_err().contains("no reachable copy"));
    }

    /// Bound to the open catalog, the commit lands as a history step.
    #[cfg(feature = "edit")]
    #[test]
    fn a_bound_commit_writes_the_open_catalog() {
        let dir = TestTmpDir::new("app-editing-commit");
        let state = AppState::default();
        let (a, photo, version) = catalog(&dir, "a");
        *state.catalog.lock().unwrap() = Some(a);
        let from = catalog_identity(&state).unwrap();
        let h = write_version_then_refresh_monochrome(&state, Some(from), version, |c| {
            c.commit_version_edit(version, r#"{"tone":{"ev":1}}"#, "Exposure +1.00", false)
        })
        .unwrap();
        assert_eq!(h.steps.last().unwrap().label, "Exposure +1.00");
        let c = state.catalog.lock().unwrap();
        assert_eq!(c.as_ref().unwrap().list_versions(photo).unwrap()[0].edit_json, r#"{"tone":{"ev":1}}"#);
    }

    #[cfg(feature = "edit")]
    #[test]
    fn luts_are_listed_sorted_and_imported_after_parsing() {
        let dir = TestTmpDir::new("app-editing-luts");
        let luts = dir.join("luts");
        std::fs::create_dir_all(&luts).unwrap();
        std::fs::write(luts.join("b.CUBE"), "").unwrap();
        std::fs::write(luts.join("a.cube"), "").unwrap();
        std::fs::write(luts.join("notes.txt"), "").unwrap();
        assert_eq!(list_luts_in(&luts).unwrap(), ["a.cube", "b.CUBE"]);
        let src = dir.join("film.cube");
        std::fs::write(&src, "LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n").unwrap();
        assert_eq!(import_lut_into(&luts, &src).unwrap(), "film.cube");
        assert!(luts.join("film.cube").exists());
        let bad = dir.join("bad.cube");
        std::fs::write(&bad, "garbage").unwrap();
        assert!(import_lut_into(&luts, &bad).unwrap_err().starts_with("invalid LUT"));
        assert!(import_lut_into(&luts, &dir.join("x.png")).is_err());
    }
}
