//! The `develop` job family: one claim per opened photo. `open` claims, probes, and starts
//! the worker on its own thread (never the image pool — a two-second decode must not
//! block tile serving); the worker loads the working image — from the `.rawf` decode cache
//! when it can, else LibRaw, writing the cache — publishes it, emits `develop:source`, and
//! then prepares the neighbours (N+1, then N−1) into the same claim's memory budget without
//! announcing them. `close` trips the claim. Every step is preceded by an abort check, so a
//! switch mid-decode stops at the next one and clears only its own slot.
//!
//! The slot describes the open session, not the worker: it stays set (`resident: true`)
//! after the worker ends, until the next claim or a failure clears it. Readers check the
//! resident set too, so a slot that outlives its image (a `close` after the worker ended)
//! reads as "not resident", never as other pixels.

use super::{release_all, with_resident};
use crate::commands::jobs::{JobClaim, JobStatus};
use crate::commands::{AppState, DevelopSource};
use crate::plugins::edit::{SourceToken, WorkingImage};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, Runtime};

/// Settings key: `"1"` renders Develop from the RAW working image (engine 2). Off by
/// default until slice 8 — until then the Darkroom behaves exactly as before.
pub const RAW_ENGINE_KEY: &str = "develop.rawEngine";

/// Settings key: `"0"` turns neighbour preload off. On by default.
pub const PRELOAD_KEY: &str = "develop.preloadNeighbours";

/// What the worker reads from the catalog's settings, once, at the claim.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Prep {
    /// The decode cache's size limit.
    pub cache_budget_bytes: u64,
    pub preload: bool,
}

fn prep_settings(state: &AppState) -> Prep {
    let get = |k: &str| -> Option<String> {
        let guard = state.catalog.lock().ok()?;
        guard.as_ref()?.get_setting(k).ok().flatten()
    };
    let gb = get(super::cache::BUDGET_KEY)
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|g| g.is_finite() && *g >= 0.0)
        .unwrap_or(super::cache::DEFAULT_BUDGET_GB as f64);
    Prep {
        cache_budget_bytes: (gb * 1024.0 * 1024.0 * 1024.0) as u64,
        preload: get(PRELOAD_KEY).as_deref() != Some("0"),
    }
}

/// The develop family's status slot: which photo the claim is for and whether its
/// working image is resident. `generation` is the job id — the token's second half.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevelopStatus {
    pub job: u64,
    pub photo_id: i64,
    pub generation: u64,
    pub resident: bool,
}

impl JobStatus for DevelopStatus {
    fn job_id(&self) -> u64 {
        self.job
    }
}

/// Event `develop:source`: the source state for a photo changed.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DevelopSourceEvent {
    pub photo_id: i64,
    pub job: u64,
    #[serde(flatten)]
    pub source: DevelopSource,
}

/// Whether the RAW engine is switched on in this catalog's settings.
fn raw_engine_enabled(state: &AppState) -> bool {
    let Ok(guard) = state.catalog.lock() else { return false };
    guard
        .as_ref()
        .and_then(|c| c.get_setting(RAW_ENGINE_KEY).ok().flatten())
        .is_some_and(|v| v == "1")
}

/// What claiming the family for a photo decided: the answer is already known; or the photo
/// was already resident (a preloaded neighbour) and is adopted under the new claim, whose
/// worker only prepares the neighbours; or a load must run under this claim.
pub(crate) enum Claimed {
    Ready(DevelopSource),
    Adopted(JobClaim<DevelopStatus>, SourceToken),
    Decode(JobClaim<DevelopStatus>),
}

/// The ownership transition of an open, with no thread and no decode: trip whatever the
/// family was doing and claim it for `photo_id`, keeping only the images of `photo_id` and
/// its `neighbours` (re-keyed to the new claim) and releasing the rest — or answer from the
/// current claim when it is already for this photo and resident. Every path that returns
/// without a claim has still released the previous photo's image.
pub(crate) fn claim(
    state: &AppState,
    photo_id: i64,
    probe: DevelopSource,
    neighbours: &[i64],
) -> Result<Claimed, String> {
    if !raw_engine_enabled(state) {
        // Switched off: nothing to prepare, but a photo opened while it was on may still be
        // resident, and this open is the ownership change that releases it.
        let _ = state.jobs.develop.cancel();
        release_all();
        return Ok(Claimed::Ready(DevelopSource::Preview { preparing: false }));
    }
    let DevelopSource::Raw { .. } = &probe else {
        // Not a supported RAW: nothing to prepare. The claim is not taken, so a previous
        // photo's image is released by the trip here all the same.
        let _ = state.jobs.develop.cancel();
        release_all();
        return Ok(Claimed::Ready(probe));
    };
    // Same photo, still resident? Then the current claim already answers.
    if let Ok(Some(status)) = state.jobs.develop.status() {
        if status.photo_id == photo_id && status.resident {
            let token = SourceToken::Working { photo_id, generation: status.generation };
            if super::resident(&token).is_some() {
                return Ok(Claimed::Ready(with_token(probe, &token)));
            }
        }
    }
    let claim = state.jobs.develop.begin(&state.catalog, |job| DevelopStatus {
        job,
        photo_id,
        generation: job,
        resident: false,
    })?;
    // Everything not about this photo or its neighbours is unreachable from now on: drop it
    // before any load starts, so memory never holds a stale photo alongside a new decode.
    // What stays is re-keyed to this claim — the previous claim's tokens name nothing. Its
    // worker, if still running, was tripped by `begin`; a late insert it makes is removed by
    // its own owner check.
    let mut keep = vec![photo_id];
    keep.extend(neighbours.iter().copied().filter(|&n| n != photo_id));
    let kept = with_resident(|r| r.retain_rekey(&keep, claim.job));
    if kept.contains(&photo_id) {
        let token = SourceToken::Working { photo_id, generation: claim.job };
        claim.slot.publish(|job| DevelopStatus { job, photo_id, generation: job, resident: true });
        return Ok(Claimed::Adopted(claim, token));
    }
    Ok(Claimed::Decode(claim))
}

/// Claim the family for `photo_id` and start preparing its working image and then its
/// `neighbours` (resolved paths, N+1 first). Returns the state right now:
/// `Preview{preparing:true}` while the load runs, `Raw{token}` when the image is already
/// resident (the same photo, or a preloaded neighbour), or the honest exceptions.
pub fn open<R: Runtime>(
    app: &AppHandle<R>,
    photo_id: i64,
    path: PathBuf,
    probe: DevelopSource,
    neighbours: Vec<(i64, PathBuf)>,
) -> Result<DevelopSource, String> {
    let state = app.state::<AppState>();
    let prep = prep_settings(&state);
    let ids: Vec<i64> = if prep.preload { neighbours.iter().map(|(id, _)| *id).collect() } else { Vec::new() };
    let (claim, answer, current_ready) = match claim(&state, photo_id, probe.clone(), &ids)? {
        Claimed::Ready(source) => return Ok(source),
        Claimed::Adopted(claim, token) => (claim, with_token(probe.clone(), &token), true),
        Claimed::Decode(claim) => (claim, DevelopSource::Preview { preparing: true }, false),
    };
    let neighbours = if prep.preload { neighbours } else { Vec::new() };
    let app2 = app.clone();
    std::thread::Builder::new()
        .name(format!("develop-decode-{photo_id}"))
        .spawn(move || prepare(claim, app2, photo_id, path, probe, current_ready, neighbours, prep))
        .map_err(|e| format!("could not start the decode thread: {e}"))?;
    Ok(answer)
}

/// Trip the claim and release every working image. Idempotent.
pub fn close(state: &AppState) -> Result<(), String> {
    state.jobs.develop.cancel()?;
    release_all();
    Ok(())
}

/// The source state right now for `photo_id`, from the slot and the resident set.
pub fn current(state: &AppState, photo_id: i64, probe: DevelopSource) -> DevelopSource {
    if !raw_engine_enabled(state) {
        return DevelopSource::Preview { preparing: false };
    }
    match state.jobs.develop.status() {
        Ok(Some(s)) if s.photo_id == photo_id => {
            let token = SourceToken::Working { photo_id, generation: s.generation };
            if !s.resident {
                DevelopSource::Preview { preparing: true }
            } else if super::resident(&token).is_some() {
                with_token(probe, &token)
            } else {
                // The session outlived its image (Develop was closed after the worker
                // ended): nothing is resident and nothing is being prepared.
                probe
            }
        }
        _ => probe,
    }
}

fn with_token(probe: DevelopSource, token: &SourceToken) -> DevelopSource {
    match probe {
        DevelopSource::Raw { camera, megapixels, bits, decoder, .. } => DevelopSource::Raw {
            camera,
            megapixels,
            bits,
            decoder,
            token: Some(token.to_query()),
        },
        other => other,
    }
}

/// Load `path`'s linear decode: the `.rawf` cache when it holds this file for this decoder,
/// else LibRaw (under its crash marker), writing the cache and trimming it to the budget.
/// A cache that cannot be written costs the next open a decode, nothing more.
pub(crate) fn load_linear(
    path: &std::path::Path,
    abort: &std::sync::atomic::AtomicBool,
    cache_budget_bytes: u64,
) -> Result<(crate::raw::LinearDecode, &'static str), String> {
    load_linear_in(&super::cache::root(), path, abort, cache_budget_bytes)
}

fn load_linear_in(
    root: &std::path::Path,
    path: &std::path::Path,
    abort: &std::sync::atomic::AtomicBool,
    cache_budget_bytes: u64,
) -> Result<(crate::raw::LinearDecode, &'static str), String> {
    use super::cache;
    let key = cache::CacheKey::for_file(path);
    if let Some(d) = key.as_ref().and_then(|k| cache::read_in(root, k)) {
        return Ok((d, "cache"));
    }
    let d = crate::raw::decode_linear(path, abort)?;
    if let Some(key) = key {
        if !abort.load(Ordering::Relaxed) && cache_budget_bytes > 0 {
            if let Err(e) = cache::write_in(root, &key, &d) {
                eprintln!("develop: {e}");
            }
            cache::trim_in(root, crate::raw::decoder_version(), cache_budget_bytes);
        }
    }
    Ok((d, "decode"))
}

/// The worker: load, publish, announce; then preload the neighbours. Runs on its own thread
/// with the claim.
#[allow(clippy::too_many_arguments)]
fn prepare<R: Runtime>(
    claim: JobClaim<DevelopStatus>,
    app: AppHandle<R>,
    photo_id: i64,
    path: PathBuf,
    probe: DevelopSource,
    current_ready: bool,
    neighbours: Vec<(i64, PathBuf)>,
    prep: Prep,
) {
    let generation = claim.job;
    let abort = claim.abort.clone();
    let aborted = || abort.load(Ordering::Relaxed);
    let emit = |source: DevelopSource| {
        let _ = app.emit("develop:source", DevelopSourceEvent { photo_id, job: generation, source });
    };
    // Terminal for a failure: the slot goes (only if still ours), then the event.
    let fail = || claim.slot.clear();
    if !current_ready {
        if aborted() {
            return fail();
        }
        let t = std::time::Instant::now();
        let (decoded, from) = match load_linear(&path, &abort, prep.cache_budget_bytes) {
            Ok(d) => d,
            Err(e) => {
                if !aborted() {
                    eprintln!("develop: decode of photo {photo_id} failed: {e}");
                    fail();
                    emit(DevelopSource::Unsupported { camera: None, reason: e });
                }
                return fail();
            }
        };
        if aborted() {
            return fail();
        }
        let image = Arc::new(working_image_from(decoded));
        if aborted() {
            return fail();
        }
        eprintln!("develop: photo {photo_id} ready from {from} in {:.2?}", t.elapsed());
        let token = SourceToken::Working { photo_id, generation };
        match publish(&claim, photo_id, &token, image) {
            Published::Resident => emit(with_token(probe, &token)),
            Published::OverBudget => {
                fail();
                emit(DevelopSource::Unsupported { camera: None, reason: "working image exceeds the memory budget".into() });
                return;
            }
            Published::Superseded => return,
        }
    }
    preload_neighbours(&claim, &neighbours, prep.cache_budget_bytes);
}

/// Prepare each neighbour into the claim's budget, silently: nothing is emitted, the slot is
/// not touched, and a neighbour that fails is simply not preloaded. Stops at the first
/// neighbour the budget refuses — the current photo is never evicted for one — and at any
/// trip. The same insert-then-check as [`publish`]: an insert that lands after a newer claim
/// has released everything takes itself back out.
pub(crate) fn preload_neighbours(
    claim: &JobClaim<DevelopStatus>,
    neighbours: &[(i64, PathBuf)],
    cache_budget_bytes: u64,
) {
    let generation = claim.job;
    let abort = claim.abort.clone();
    for (nid, npath) in neighbours {
        if abort.load(Ordering::Relaxed) {
            return;
        }
        let token = SourceToken::Working { photo_id: *nid, generation };
        if super::resident(&token).is_some() {
            continue; // kept from the previous claim
        }
        if !matches!(crate::raw::probe(npath), crate::raw::RawSupport::Supported(_)) {
            continue;
        }
        let t = std::time::Instant::now();
        let Ok((decoded, from)) = load_linear(npath, &abort, cache_budget_bytes) else { continue };
        if abort.load(Ordering::Relaxed) {
            return;
        }
        let image = Arc::new(working_image_from(decoded));
        if !preload_insert(claim, &token, image) {
            return;
        }
        eprintln!("develop: neighbour {nid} preloaded from {from} in {:.2?}", t.elapsed());
    }
}

/// Insert a neighbour's image under this claim; `false` when refused (budget) or taken back
/// out because the claim was superseded meanwhile — either way, stop preloading.
pub(crate) fn preload_insert(claim: &JobClaim<DevelopStatus>, token: &SourceToken, image: Arc<WorkingImage>) -> bool {
    if !with_resident(|r| r.insert(token.clone(), image)) {
        return false;
    }
    if claim.abort.load(Ordering::Relaxed) || !claim.slot.owns() {
        with_resident(|r| r.remove(token));
        return false;
    }
    true
}

/// The outcome of a worker's publish step.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Published {
    /// The image is resident and this claim's slot says so.
    Resident,
    /// The set refused the image; the slot stays "preparing" until `done` clears it.
    OverBudget,
    /// A newer claim took the family between the decode and here: this worker's image is
    /// gone again and the newer claim's slot and images are untouched.
    Superseded,
}

/// Make a decoded image resident under this claim's token and say so in the slot — or, if
/// the claim was superseded meanwhile, take the image back out. Insert-then-check rather
/// than check-then-insert, because the slot lock and the resident lock are never held
/// together (the set is a leaf): an insert after a newer `claim` has already released
/// everything would otherwise leave a stale image nothing owns.
pub(crate) fn publish(
    claim: &JobClaim<DevelopStatus>,
    photo_id: i64,
    token: &SourceToken,
    image: Arc<WorkingImage>,
) -> Published {
    let generation = claim.job;
    let inserted = with_resident(|r| r.insert(token.clone(), image));
    if !inserted {
        return Published::OverBudget;
    }
    claim.slot.publish(|job| DevelopStatus { job, photo_id, generation, resident: true });
    if claim.slot.owns() {
        Published::Resident
    } else {
        with_resident(|r| r.remove(token));
        Published::Superseded
    }
}

/// The decoder's 16-bit linear output as the engine's f32 working image: normalized to
/// sensor white, oriented for display.
pub fn working_image_from(d: crate::raw::LinearDecode) -> WorkingImage {
    use image::{DynamicImage, Rgb32FImage};
    use rayon::prelude::*;
    // 200 million values for a 67 MP decode: across cores, it is part of every cache hit.
    let f: Vec<f32> = d.rgb16.par_iter().map(|&v| v as f32 / 65535.0).collect();
    let img = Rgb32FImage::from_raw(d.width, d.height, f).expect("rgb16 length matches its dimensions");
    let mut dynimg = DynamicImage::ImageRgb32F(img);
    dynimg.apply_orientation(d.orientation);
    let linear = dynimg.into_rgb32f();
    WorkingImage {
        width: linear.width(),
        height: linear.height(),
        linear,
        cam_mul: d.cam_mul,
        rgb_cam: d.rgb_cam,
        decoder: crate::raw::decoder_version(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::develop::{resident, resident_bytes, serial, test_image};

    fn state(engine_on: bool) -> (AppState, crate::test_support::TestTmpDir) {
        // The resident set is process-global and now survives a claim for the photos it
        // keeps: every test starts from an empty one (callers hold `serial()`).
        release_all();
        let dir = crate::test_support::TestTmpDir::new("develop-session");
        let root = dir.join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = Catalog::open(&dir.join("t.chairphoto"), &root).unwrap();
        catalog.set_setting(RAW_ENGINE_KEY, if engine_on { "1" } else { "0" }).unwrap();
        let state = AppState::default();
        *state.catalog.lock().unwrap() = Some(catalog);
        (state, dir)
    }

    fn raw_probe() -> DevelopSource {
        DevelopSource::Raw { camera: "Test".into(), megapixels: 1.0, bits: 16, decoder: "test".into(), token: None }
    }

    fn decode_claim(state: &AppState, photo_id: i64) -> JobClaim<DevelopStatus> {
        decode_claim_with(state, photo_id, &[])
    }

    fn decode_claim_with(state: &AppState, photo_id: i64, neighbours: &[i64]) -> JobClaim<DevelopStatus> {
        match claim(state, photo_id, raw_probe(), neighbours).unwrap() {
            Claimed::Decode(c) => c,
            Claimed::Adopted(_, t) => panic!("expected a decode claim, got an adoption of {t:?}"),
            Claimed::Ready(s) => panic!("expected a decode claim, got {s:?}"),
        }
    }

    fn token_of(claim: &JobClaim<DevelopStatus>, photo_id: i64) -> SourceToken {
        SourceToken::Working { photo_id, generation: claim.job }
    }

    /// **Forced.** Opening another photo trips the first claim, releases its image, and
    /// the first token names nothing from then on.
    #[test]
    fn open_for_another_photo_trips_the_previous_claim() {
        let _serial = serial();
        let (state, _dir) = state(true);
        let a = decode_claim(&state, 1);
        let ta = token_of(&a, 1);
        assert_eq!(publish(&a, 1, &ta, test_image(8, 8)), Published::Resident);
        assert!(resident(&ta).is_some());
        assert!(matches!(current(&state, 1, raw_probe()), DevelopSource::Raw { token: Some(_), .. }));

        let b = decode_claim(&state, 2);

        assert!(a.abort.load(Ordering::Relaxed), "the first worker is told to stop");
        assert!(!b.abort.load(Ordering::Relaxed));
        assert!(resident(&ta).is_none(), "the first photo's image is released at the claim");
        assert_eq!(resident_bytes(), 0);
        assert!(!a.slot.owns());
        let status = state.jobs.develop.status().unwrap().unwrap();
        assert_eq!((status.photo_id, status.resident), (2, false));
        // The first photo now reads as "not this claim": the probe without a token.
        assert!(matches!(current(&state, 1, raw_probe()), DevelopSource::Raw { token: None, .. }));
        assert!(matches!(current(&state, 2, raw_probe()), DevelopSource::Preview { preparing: true }));
    }

    /// **Forced.** A decode that finishes after a newer open has claimed the family must not
    /// stay resident, and must not disturb what the newer claim published.
    #[test]
    fn a_superseded_decode_releases_only_its_own_image() {
        let _serial = serial();
        let (state, _dir) = state(true);
        let a = decode_claim(&state, 1);
        let b = decode_claim(&state, 2);
        let (ta, tb) = (token_of(&a, 1), token_of(&b, 2));
        // B is fast (a cached decode, later) and publishes first.
        assert_eq!(publish(&b, 2, &tb, test_image(8, 8)), Published::Resident);
        // A's straggler arrives with its 800 MB.
        assert_eq!(publish(&a, 1, &ta, test_image(8, 8)), Published::Superseded);

        assert!(resident(&ta).is_none(), "the superseded image is gone again");
        assert!(resident(&tb).is_some(), "and the owner's image was not touched");
        let status = state.jobs.develop.status().unwrap().unwrap();
        assert_eq!((status.job, status.photo_id, status.resident), (b.job, 2, true));
        // And A's terminal clear is a no-op on B's slot.
        a.slot.clear();
        assert!(state.jobs.develop.status().unwrap().is_some());
    }

    /// Leaving Develop trips the claim and releases the image; the token it minted is then
    /// refused (a 404 on `edit://`), never answered with other pixels.
    #[test]
    fn close_releases_the_image_and_the_stale_token_names_nothing() {
        let _serial = serial();
        let (state, _dir) = state(true);
        let a = decode_claim(&state, 1);
        let ta = token_of(&a, 1);
        assert_eq!(publish(&a, 1, &ta, test_image(8, 8)), Published::Resident);

        close(&state).unwrap();

        assert!(a.abort.load(Ordering::Relaxed));
        assert!(resident(&ta).is_none());
        assert_eq!(resident_bytes(), 0);
        close(&state).unwrap(); // idempotent
        // The slot is still the worker's to clear (close trips; it does not unpublish), and
        // the photo reads as preparing until it does — the frontend has left anyway.
        a.slot.clear();
        assert!(matches!(current(&state, 1, raw_probe()), DevelopSource::Raw { token: None, .. }));
    }

    /// The same photo opened again while resident answers from the current claim without
    /// decoding again or releasing anything.
    #[test]
    fn reopening_the_resident_photo_answers_without_a_new_claim() {
        let _serial = serial();
        let (state, _dir) = state(true);
        let a = decode_claim(&state, 1);
        let ta = token_of(&a, 1);
        assert_eq!(publish(&a, 1, &ta, test_image(8, 8)), Published::Resident);

        match claim(&state, 1, raw_probe(), &[]).unwrap() {
            Claimed::Ready(DevelopSource::Raw { token: Some(t), .. }) => assert_eq!(t, ta.to_query()),
            other => panic!("expected the resident token, got {:?}", matches!(other, Claimed::Decode(_))),
        }
        assert!(!a.abort.load(Ordering::Relaxed), "the running claim is not tripped");
        assert!(resident(&ta).is_some());
    }

    /// Opening a non-RAW, or opening with the engine switched off, takes no claim but still
    /// releases whatever the previous open left resident.
    #[test]
    fn an_open_that_prepares_nothing_still_releases_the_previous_image() {
        let _serial = serial();
        let (state, _dir) = state(true);
        let a = decode_claim(&state, 1);
        let ta = token_of(&a, 1);
        assert_eq!(publish(&a, 1, &ta, test_image(8, 8)), Published::Resident);

        assert!(matches!(claim(&state, 2, DevelopSource::Jpeg, &[]).unwrap(), Claimed::Ready(DevelopSource::Jpeg)));
        assert!(a.abort.load(Ordering::Relaxed));
        assert!(resident(&ta).is_none());

        let b = decode_claim(&state, 3);
        let tb = token_of(&b, 3);
        assert_eq!(publish(&b, 3, &tb, test_image(8, 8)), Published::Resident);
        state.catalog.lock().unwrap().as_ref().unwrap().set_setting(RAW_ENGINE_KEY, "0").unwrap();
        assert!(matches!(
            claim(&state, 3, raw_probe(), &[]).unwrap(),
            Claimed::Ready(DevelopSource::Preview { preparing: false })
        ));
        assert!(b.abort.load(Ordering::Relaxed), "switching the engine off stands the decode down");
        assert!(resident(&tb).is_none());
        assert!(matches!(current(&state, 3, raw_probe()), DevelopSource::Preview { preparing: false }));
    }

    /// **Forced.** Stepping to a photo the previous claim preloaded as a neighbour adopts
    /// its image at once — no load, no preview state — under the new claim's generation.
    /// The previous claim's tokens name nothing afterwards, and the previous current photo
    /// stays only because it is a neighbour of the new one.
    #[test]
    fn stepping_to_a_preloaded_neighbour_adopts_it_and_keeps_only_the_new_neighbours() {
        let _serial = serial();
        let (state, _dir) = state(true);
        let a = decode_claim_with(&state, 1, &[2, 3]);
        let (t1, t2, t3) = (token_of(&a, 1), token_of(&a, 2), token_of(&a, 3));
        assert_eq!(publish(&a, 1, &t1, test_image(8, 8)), Published::Resident);
        assert!(preload_insert(&a, &t2, test_image(8, 8)));
        assert!(preload_insert(&a, &t3, test_image(8, 8)));

        // The user steps to photo 2, whose neighbours are 1 and 4 (3 is no longer adjacent).
        let (b, token) = match claim(&state, 2, raw_probe(), &[1, 4]).unwrap() {
            Claimed::Adopted(b, token) => (b, token),
            _ => panic!("a preloaded neighbour must be adopted, not decoded again"),
        };
        assert_eq!(token, token_of(&b, 2));
        assert!(a.abort.load(Ordering::Relaxed), "the previous worker is told to stop");
        assert!(resident(&token).is_some(), "photo 2's pixels survived the step");
        assert!(resident(&token_of(&b, 1)).is_some(), "photo 1 stays: it is a neighbour now");
        for old in [&t1, &t2, &t3] {
            assert!(resident(old).is_none(), "the previous claim's tokens name nothing: {old:?}");
        }
        assert!(resident(&token_of(&b, 3)).is_none(), "photo 3 is no longer adjacent: released");
        let status = state.jobs.develop.status().unwrap().unwrap();
        assert_eq!((status.photo_id, status.resident), (2, true));
        assert!(matches!(current(&state, 2, raw_probe()), DevelopSource::Raw { token: Some(_), .. }));
    }

    /// **Forced.** A tripped worker that finishes a neighbour after a newer claim took the
    /// family takes its insert back out, and stops preloading.
    #[test]
    fn a_superseded_preload_takes_itself_back_out() {
        let _serial = serial();
        let (state, _dir) = state(true);
        let a = decode_claim_with(&state, 1, &[2]);
        let _b = decode_claim(&state, 5);
        let late = token_of(&a, 2);
        assert!(!preload_insert(&a, &late, test_image(8, 8)), "refused: the claim is gone");
        assert!(resident(&late).is_none());
        assert_eq!(resident_bytes(), 0);
    }

    /// The session status stays after the worker's success (it describes the open photo,
    /// not the worker), so a remount re-attaches to the resident image; after Develop is
    /// closed the same status reads as "not resident", never as preparing forever.
    #[test]
    fn the_session_outlives_its_worker_but_not_its_image() {
        let _serial = serial();
        let (state, _dir) = state(true);
        let a = decode_claim(&state, 1);
        let t = token_of(&a, 1);
        assert_eq!(publish(&a, 1, &t, test_image(8, 8)), Published::Resident);
        // The worker ends here on success — it does not clear the slot.
        assert!(matches!(current(&state, 1, raw_probe()), DevelopSource::Raw { token: Some(_), .. }));
        match claim(&state, 1, raw_probe(), &[]).unwrap() {
            Claimed::Ready(DevelopSource::Raw { token: Some(q), .. }) => assert_eq!(q, t.to_query()),
            _ => panic!("reopening the resident photo must answer at once"),
        }
        close(&state).unwrap();
        assert!(matches!(current(&state, 1, raw_probe()), DevelopSource::Raw { token: None, .. }));
    }

    /// A photo the decode cache holds loads from it without calling LibRaw: the "file" here
    /// is not a RAW at all, so a decode would fail — the cache hit is the only way through.
    #[test]
    fn a_cached_decode_loads_without_the_decoder() {
        let dir = crate::test_support::TestTmpDir::new("develop-cache-hit");
        let root = dir.join("cache");
        let file = dir.join("a.ARW");
        std::fs::write(&file, b"not a raw file").unwrap();
        let key = super::super::cache::CacheKey::for_file(&file).unwrap();
        let d = crate::raw::LinearDecode {
            width: 4,
            height: 2,
            rgb16: (0..24).collect(),
            orientation: image::metadata::Orientation::NoTransforms,
            cam_mul: [1.0; 4],
            rgb_cam: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        };
        super::super::cache::write_in(&root, &key, &d).unwrap();
        let abort = std::sync::atomic::AtomicBool::new(false);
        let (got, from) = load_linear_in(&root, &file, &abort, u64::MAX).unwrap();
        assert_eq!(from, "cache");
        assert_eq!(got.rgb16, d.rgb16);
        // Change the file: the key changes, it is a miss, and the decoder is asked (and
        // refuses a text file) — never the stale decode.
        std::fs::write(&file, b"not a raw file, now longer").unwrap();
        assert!(load_linear_in(&root, &file, &abort, u64::MAX).is_err());
    }
}
