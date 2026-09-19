//! The `develop` job family: one claim per opened photo. `open` claims, probes, and starts
//! the decode on its own thread (never the image pool — a two-second decode must not
//! block tile serving); the worker publishes the working image and emits
//! `develop:source`; `close` trips the claim. Every step after a `?` is preceded by an
//! abort check, so a switch mid-decode stops at the next one and clears only its own slot.

use super::{with_resident, DEFAULT_BUDGET_BYTES};
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
    if !raw_engine_enabled(&state) {
        return Ok(DevelopSource::Preview { preparing: false });
    }
    let DevelopSource::Raw { .. } = &probe else {
        // Not a supported RAW: nothing to prepare. The claim is not taken, so a previous
        // photo's image is released by the trip below all the same.
        let _ = state.jobs.develop.cancel();
        with_resident(|r| r.clear());
        return Ok(probe);
    };
    // Same photo, still resident? Then the current claim already answers.
    if let Ok(Some(status)) = state.jobs.develop.status() {
        if status.photo_id == photo_id && status.resident {
            let token = SourceToken::Working { photo_id, generation: status.generation };
            if super::resident(&token).is_some() {
                return Ok(with_token(probe, &token));
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
    // starts, so two 67 MP images are never resident at once for one open.
    with_resident(|r| r.clear());
    let app2 = app.clone();
    let probe2 = probe.clone();
    std::thread::Builder::new()
        .name(format!("develop-decode-{photo_id}"))
        .spawn(move || prepare(claim, app2, photo_id, path, probe2))
        .map_err(|e| format!("could not start the decode thread: {e}"))?;
    Ok(DevelopSource::Preview { preparing: true })
}

/// Trip the claim and release every working image. Idempotent.
pub fn close(state: &AppState) -> Result<(), String> {
    state.jobs.develop.cancel()?;
    with_resident(|r| r.clear());
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
    let inserted = with_resident(|r| {
        // A default budget; the setting arrives with the decode cache slice.
        let _ = DEFAULT_BUDGET_BYTES;
        r.insert(token.clone(), image)
    });
    if !inserted {
        emit(DevelopSource::Unsupported { camera: None, reason: "working image exceeds the memory budget".into() });
        return done();
    }
    claim.slot.publish(|job| DevelopStatus { job, photo_id, generation, resident: true });
    if claim.slot.owns() {
        emit(with_token(probe, &token));
    } else {
        // Superseded between insert and publish: the newer claim cleared us; drop the image.
        with_resident(|r| r.clear());
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
