//! The `develop` job family: one claim per opened photo. `open` claims, probes, and starts
//! the decode on its own thread (never the image pool — a two-second decode must not
//! block tile serving); the worker publishes the working image and emits
//! `develop:source`; `close` trips the claim. Every step after a `?` is preceded by an
//! abort check, so a switch mid-decode stops at the next one and clears only its own slot.

use super::{release_all, with_resident, DEFAULT_BUDGET_BYTES};
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

/// What claiming the family for a photo decided: the answer is already known, or a decode
/// must run under this claim.
pub(crate) enum Claimed {
    Ready(DevelopSource),
    Decode(JobClaim<DevelopStatus>),
}

/// The ownership transition of an open, with no thread and no decode: trip whatever the
/// family was doing, release its images, and claim it for `photo_id` — or answer from the
/// current claim when it is already for this photo and resident. Every path that returns
/// without a claim has still released the previous photo's image.
pub(crate) fn claim(state: &AppState, photo_id: i64, probe: DevelopSource) -> Result<Claimed, String> {
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
    // The previous photo's image is unreachable from now on: drop it before the decode
    // starts, so two 67 MP images are never resident at once for one open. Its worker, if
    // still running, was tripped by `begin` and removes only its own token on the way out.
    release_all();
    Ok(Claimed::Decode(claim))
}

/// Claim the family for `photo_id` and start preparing its working image. Returns the
/// state right now: `Preview{preparing:true}` while the decode runs, `Raw{token}` when the
/// image is already resident, or the honest exceptions.
pub fn open<R: Runtime>(
    app: &AppHandle<R>,
    photo_id: i64,
    path: PathBuf,
    probe: DevelopSource,
) -> Result<DevelopSource, String> {
    let state = app.state::<AppState>();
    let claim = match claim(&state, photo_id, probe.clone())? {
        Claimed::Ready(source) => return Ok(source),
        Claimed::Decode(claim) => claim,
    };
    let app2 = app.clone();
    std::thread::Builder::new()
        .name(format!("develop-decode-{photo_id}"))
        .spawn(move || prepare(claim, app2, photo_id, path, probe))
        .map_err(|e| format!("could not start the decode thread: {e}"))?;
    Ok(DevelopSource::Preview { preparing: true })
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
            if s.resident && super::resident(&token).is_some() {
                with_token(probe, &token)
            } else {
                DevelopSource::Preview { preparing: true }
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

/// The worker: decode, publish, announce. Runs on its own thread with the claim.
fn prepare<R: Runtime>(
    claim: JobClaim<DevelopStatus>,
    app: AppHandle<R>,
    photo_id: i64,
    path: PathBuf,
    probe: DevelopSource,
) {
    let generation = claim.job;
    let abort = claim.abort.clone();
    let emit = |source: DevelopSource| {
        let _ = app.emit("develop:source", DevelopSourceEvent { photo_id, job: generation, source });
    };
    let done = || {
        // Only this job may clear its slot (a newer claim already owns it otherwise).
        claim.slot.clear();
    };
    if abort.load(Ordering::Relaxed) {
        return done();
    }
    let decoded = match crate::raw::decode_linear(&path, &abort) {
        Ok(d) => d,
        Err(e) => {
            if !abort.load(Ordering::Relaxed) {
                eprintln!("develop: decode of photo {photo_id} failed: {e}");
                emit(DevelopSource::Unsupported { camera: None, reason: e });
            }
            return done();
        }
    };
    if abort.load(Ordering::Relaxed) {
        return done();
    }
    let image = Arc::new(working_image_from(decoded));
    if abort.load(Ordering::Relaxed) {
        return done();
    }
    let token = SourceToken::Working { photo_id, generation };
    match publish(&claim, photo_id, &token, image) {
        Published::Resident => emit(with_token(probe, &token)),
        Published::OverBudget => {
            emit(DevelopSource::Unsupported { camera: None, reason: "working image exceeds the memory budget".into() });
        }
        Published::Superseded => {}
    }
    done()
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
    let inserted = with_resident(|r| {
        // A default budget; the setting arrives with the decode cache slice.
        let _ = DEFAULT_BUDGET_BYTES;
        r.insert(token.clone(), image)
    });
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
    let n = d.rgb16.len();
    let mut f = Vec::with_capacity(n);
    f.extend(d.rgb16.iter().map(|&v| v as f32 / 65535.0));
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
        match claim(state, photo_id, raw_probe()).unwrap() {
            Claimed::Decode(c) => c,
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

        match claim(&state, 1, raw_probe()).unwrap() {
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

        assert!(matches!(claim(&state, 2, DevelopSource::Jpeg).unwrap(), Claimed::Ready(DevelopSource::Jpeg)));
        assert!(a.abort.load(Ordering::Relaxed));
        assert!(resident(&ta).is_none());

        let b = decode_claim(&state, 3);
        let tb = token_of(&b, 3);
        assert_eq!(publish(&b, 3, &tb, test_image(8, 8)), Published::Resident);
        state.catalog.lock().unwrap().as_ref().unwrap().set_setting(RAW_ENGINE_KEY, "0").unwrap();
        assert!(matches!(
            claim(&state, 3, raw_probe()).unwrap(),
            Claimed::Ready(DevelopSource::Preview { preparing: false })
        ));
        assert!(b.abort.load(Ordering::Relaxed), "switching the engine off stands the decode down");
        assert!(resident(&tb).is_none());
        assert!(matches!(current(&state, 3, raw_probe()), DevelopSource::Preview { preparing: false }));
    }
}
