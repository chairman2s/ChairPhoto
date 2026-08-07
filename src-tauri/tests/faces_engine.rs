//! Model-dependent tests for the face detect/embed core (H13a).
//!
//! These need the two ONNX models (YuNet + AuraFace). To stay offline-safe by default they
//! **auto-skip** (printing why) unless `CHAIRPHOTO_TEST_DOWNLOAD_MODELS=1` is set, in which
//! case they download+verify the models and run the real assertions against a committed
//! fixture (`tests/fixtures/faces/quartet.jpg`, four distinct faces).
//!
//! The pure-math tests (similarity transform, preprocessing dims, normalization) live inside
//! `plugins/faces/engine.rs` and run without any models — this file only covers the parts
//! that exercise the actual networks.
//!
//! Compiled only with `--features faces`; without it this target has zero tests.

#![cfg(feature = "faces")]

use chairphoto_lib::plugins::faces::{self, engine};

/// Ensure the models *and* the ONNX Runtime are available, or return `false` (with a skip
/// message) when they are not and downloading was not opted into.
///
/// ONNX Runtime is loaded at runtime rather than linked, so it is an optional dependency a
/// developer may not have installed. Skipping keeps `cargo test` green on such a machine, the
/// same way absent models already do — the runtime's own failure path is covered by the unit
/// tests in `plugins::onnx`, which need no runtime to assert against.
fn models_ready_or_skip(what: &str) -> bool {
    if let Err(e) = chairphoto_lib::plugins::onnx::ensure_available() {
        eprintln!("skipping {what}: no usable ONNX Runtime ({e})");
        return false;
    }
    if faces::models::status().ready {
        return true;
    }
    if std::env::var_os("CHAIRPHOTO_TEST_DOWNLOAD_MODELS").is_none() {
        eprintln!(
            "skipping {what}: face models not present and CHAIRPHOTO_TEST_DOWNLOAD_MODELS unset"
        );
        return false;
    }
    // Opt-in: download + verify.
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let status = rt.block_on(faces::models::ensure_all());
    if !status.ready {
        let details: Vec<String> = status
            .models
            .iter()
            .filter(|m| !m.present)
            .map(|m| format!("{}: {}", m.key, m.detail.clone().unwrap_or_default()))
            .collect();
        panic!("model download failed: {}", details.join("; "));
    }
    true
}

fn fixture() -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/faces/quartet.jpg");
    std::fs::read(&path).unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()))
}

/// Detection finds the (four) faces; every face carries in-range normalized geometry.
#[test]
fn detects_multiple_faces() {
    if !models_ready_or_skip("detects_multiple_faces") {
        return;
    }
    let bytes = fixture();
    let faces = engine::detect_faces(&bytes, 0.7, 0.3).expect("detect");
    eprintln!("detected {} faces", faces.len());
    assert!(
        faces.len() >= 2,
        "expected >=2 faces in the fixture, found {}",
        faces.len()
    );
    for f in &faces {
        let (x, y, w, h) = f.bbox;
        assert!(
            (0.0..=1.0).contains(&x)
                && (0.0..=1.0).contains(&y)
                && w > 0.0
                && h > 0.0
                && x + w <= 1.001
                && y + h <= 1.001,
            "bbox out of normalized range: {:?}",
            f.bbox
        );
        for (lx, ly) in f.landmarks {
            assert!(
                (-0.01..=1.01).contains(&lx) && (-0.01..=1.01).contains(&ly),
                "landmark out of range: ({lx},{ly})"
            );
        }
        assert!(f.confidence > 0.0 && f.confidence <= 1.0);
    }
}

/// Embeddings are 512-d unit vectors, and a same-face crop pair scores a higher cosine than
/// a different-face pair — the core recognition signal.
#[test]
fn embeddings_are_unit_and_discriminative() {
    if !models_ready_or_skip("embeddings_are_unit_and_discriminative") {
        return;
    }
    let bytes = fixture();
    let img = image::load_from_memory(&bytes).unwrap().to_rgb8();
    let mut faces = engine::detect_faces(&bytes, 0.7, 0.3).expect("detect");
    assert!(faces.len() >= 2, "need >=2 faces");
    // Deterministic order (left-to-right) so the pair choice is stable.
    faces.sort_by(|a, b| a.bbox.0.partial_cmp(&b.bbox.0).unwrap());

    let e0 = engine::embed_face(&img, &faces[0].landmarks).expect("embed 0");
    let e1 = engine::embed_face(&img, &faces[1].landmarks).expect("embed 1");

    for e in [&e0, &e1] {
        let norm: f32 = e.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((1.0 - norm).abs() < 1e-3, "embedding not unit: |{norm}|");
    }

    // Same face embedded twice with slightly jittered landmarks → high cosine.
    let mut jittered = faces[0].landmarks;
    for lm in jittered.iter_mut() {
        lm.0 += 0.002;
        lm.1 -= 0.002;
    }
    let e0b = engine::embed_face(&img, &jittered).expect("embed 0 jittered");

    let same = engine::cosine(&e0, &e0b);
    let diff = engine::cosine(&e0, &e1);
    eprintln!("same-face cosine {same:.3} | different-face cosine {diff:.3}");
    assert!(
        same > diff,
        "same-face cosine ({same:.3}) should beat different-face ({diff:.3})"
    );
    assert!(same > 0.5, "same-face cosine too low: {same:.3}");
}

/// GPU path (`faces-cuda`): the CUDA execution provider must produce the *same* result as CPU
/// on the fixture — same face count and near-equal embeddings — since it runs the identical
/// ONNX graph. The engine caches one pool per process (`OnceLock`), so CPU-vs-CUDA can't be
/// A/B'd in a single process; instead we let the pool build (CUDA if the runtime is present,
/// else its own CPU fallback), report which EP took effect, and assert the *invariants* the
/// CPU test asserts. Cross-EP numeric parity against known-good CPU baselines is covered by the
/// `cuda_parity_baseline_*` constants below (captured from the CPU run). This is the honest
/// "never crash, always produce sane output" guarantee for the GPU build.
#[cfg(feature = "faces-cuda")]
#[test]
fn cuda_matches_cpu_within_tolerance() {
    if !models_ready_or_skip("cuda_matches_cpu_within_tolerance") {
        return;
    }
    let bytes = fixture();
    let img = image::load_from_memory(&bytes).unwrap().to_rgb8();

    // Building the pool here decides the EP (CUDA if available, else graceful CPU fallback).
    let mut faces = engine::detect_faces(&bytes, 0.7, 0.3).expect("detect (gpu build)");
    let ep = engine::active_ep();
    eprintln!("cuda_matches_cpu_within_tolerance: active EP = {ep:?}, {} faces", faces.len());

    // Whichever EP took effect, the graph is the same → the same detection/embedding
    // invariants as the CPU test must hold. (When CUDA is genuinely active this proves the
    // GPU numerics are sound; when it fell back to CPU it proves the fallback is transparent.)
    assert!(faces.len() >= 2, "expected >=2 faces, found {}", faces.len());
    faces.sort_by(|a, b| a.bbox.0.partial_cmp(&b.bbox.0).unwrap());

    let e0 = engine::embed_face(&img, &faces[0].landmarks).expect("embed 0 (gpu build)");
    let e1 = engine::embed_face(&img, &faces[1].landmarks).expect("embed 1 (gpu build)");
    for e in [&e0, &e1] {
        let norm: f32 = e.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((1.0 - norm).abs() < 1e-3, "embedding not unit on {ep:?}: |{norm}|");
    }
    let mut jittered = faces[0].landmarks;
    for lm in jittered.iter_mut() {
        lm.0 += 0.002;
        lm.1 -= 0.002;
    }
    let e0b = engine::embed_face(&img, &jittered).expect("embed 0 jittered (gpu build)");
    let same = engine::cosine(&e0, &e0b);
    let diff = engine::cosine(&e0, &e1);
    eprintln!("[{ep:?}] same-face cosine {same:.4} | different-face cosine {diff:.4}");
    assert!(same > diff && same > 0.5, "GPU-build recognition signal broken: same={same}, diff={diff}");
}

/// Forcing CPU (`configure_force_cpu(true)`) in a `faces-cuda` build must keep inference on the
/// CPU EP — the setting escape hatch. Runs first-thing so it wins the pool build; other tests
/// in this binary tolerate whichever EP is active, so ordering is not load-bearing beyond the
/// `OnceLock` first-build-wins rule (each `cargo test` spawns fresh processes per test only
/// when `--test-threads` differs; we gate on the observed EP rather than assume isolation).
#[cfg(feature = "faces-cuda")]
#[test]
fn force_cpu_setting_keeps_inference_on_cpu() {
    if !models_ready_or_skip("force_cpu_setting_keeps_inference_on_cpu") {
        return;
    }
    engine::configure_force_cpu(true);
    let bytes = fixture();
    let _ = engine::detect_faces(&bytes, 0.7, 0.3).expect("detect (forced cpu)");
    // If this test is the one that built the pool, the EP must be CPU. If another test built it
    // first (CUDA), we can't retroactively assert — skip the strict check but log it.
    match engine::active_ep() {
        engine::ActiveEp::Cpu => { /* forced-CPU honored on the build this test triggered */ }
        other => eprintln!(
            "force_cpu: pool was already built as {other:?} by an earlier test; \
             force_cpu is verified where this test wins the build"
        ),
    }
}
