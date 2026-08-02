//! Integration tests for the "Edit in RapidRAW" round-trip state machine.
//!
//! These drive `rapidraw::run_roundtrip` (the Tauri-free core of the round-trip) against a
//! **mock** RapidRAW binary — a shell script written to a temp dir — so no real editor GUI is
//! ever launched. Each test exercises one branch of the completion state machine:
//! happy path, error path, forwarded (exit-0-without-output) + cancel, and size-stability.

use chairphoto_lib::catalog::Catalog;
use chairphoto_lib::rapidraw::{run_roundtrip, Resolved};
use chairphoto_lib::scanner::scan_folder;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};

/// Serialises the tests in this file — take it as the first line of every test.
///
/// Each test writes an executable mock script and then execs it. The harness runs tests as
/// threads of one process, so another test's `fork` can land while this test's write
/// descriptor on its mock binary is still open; the child inherits that descriptor and its
/// `exec` then fails with `ETXTBSY` ("Text file busy"). The failure is a property of writing
/// and executing files from a multithreaded process, not a defect in the code under test —
/// which is why it only ever showed up intermittently, and never under `--test-threads=1`.
///
/// Holding this lock for the whole test keeps writing and spawning from overlapping. The
/// poison is deliberately ignored: one failing test should report itself, not cascade into
/// five spurious failures.
static EXEC_LOCK: Mutex<()> = Mutex::new(());

fn exec_guard() -> MutexGuard<'static, ()> {
    EXEC_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Build a catalog + root in a unique temp dir, index one source photo under the root, and
/// return `(catalog, root, source_path, source_photo_id)`. The source resolves via the default
/// volume, so `run_roundtrip`'s import + stack works against a real, resolvable original.
fn setup(tag: &str) -> (PathBuf, PathBuf, PathBuf, i64) {
    let dir = std::env::temp_dir().join(format!("chairphoto-rapidraw-it-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let root = dir.join("photos");
    std::fs::create_dir_all(&root).unwrap();
    let db_path = dir.join("test.chairphoto");

    // A minimal but real source image so the scanner indexes it as a photo.
    let source = root.join("DSC1.jpg");
    write_tiny_image(&source);

    let catalog = Catalog::open(&db_path, &root).unwrap();
    scan_folder(&catalog, &root, &chairphoto_lib::scanner::never_abort(), &|_| {}).unwrap();
    let photos = catalog
        .list_photos(None, None, None, &[], "all", None, "all", None, None, &[], None)
        .unwrap();
    let source_id = photos
        .iter()
        .find(|p| p.path.ends_with("DSC1.jpg"))
        .expect("source photo indexed")
        .id;
    // Drop the catalog handle: run_roundtrip opens its own secondary connection.
    drop(catalog);
    (db_path, root, source, source_id)
}

/// A 1x1 valid image; the format is inferred from `path`'s extension (jpg/tiff), so exiftool
/// and the scanner accept it and the best-effort EXIF copy onto a `.tiff` output succeeds.
fn write_tiny_image(path: &Path) {
    use image::{ImageBuffer, Rgb};
    let img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(1, 1, Rgb([120, 130, 140]));
    img.save(path).unwrap();
}

/// Write an executable mock RapidRAW to `dir/RapidRAW` running `body`, and return its path.
/// `body` is the shell after the `#!/bin/sh` line; it may use `$1..$N` (the CLI args) — the
/// output path is the argument following `--output`.
fn mock_binary(dir: &Path, name: &str, body: &str) -> PathBuf {
    let bin = dir.join(name);
    // Parse `--output <path>` out of the args into $OUT for convenience.
    let script = format!(
        "#!/bin/sh\nOUT=\"\"\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = \"--output\" ]; then OUT=\"$2\"; shift; fi\n  shift\ndone\n{body}\n"
    );
    std::fs::write(&bin, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
    }
    bin
}

fn resolved(db_path: &Path, root: &Path, source: &Path, id: i64, bin: &Path) -> Resolved {
    Resolved::for_test(
        db_path.to_path_buf(),
        root.to_path_buf(),
        source.to_path_buf(),
        id,
        bin.to_string_lossy().into_owned(),
        "tiff".into(),
    )
}

/// Collect the phases emitted, so tests can assert the state-machine transitions.
fn phases() -> (Arc<std::sync::Mutex<Vec<String>>>, impl Fn(&str, &str)) {
    let log = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let l2 = log.clone();
    (log, move |phase: &str, _msg: &str| l2.lock().unwrap().push(phase.to_string()))
}

/// Happy path: the mock writes a valid image to --output and exits 0 → the result is imported
/// and stacked under the original.
#[test]
fn happy_path_imports_and_stacks() {
    let _exec = exec_guard();
    let (db, root, source, id) = setup("happy");
    // Mock: emit a valid 1x1 JPEG (renamed to the .tiff output path) and exit 0.
    let out_img = root.join("seed.tiff");
    write_tiny_image(&out_img);
    let bin = mock_binary(
        &root,
        "RapidRAW",
        &format!("cp \"{}\" \"$OUT\"\nexit 0", out_img.display()),
    );

    let (log, progress) = phases();
    let cancel = AtomicBool::new(false);
    let r = resolved(&db, &root, &source, id, &bin);
    let child = run_roundtrip(&r, &cancel, &progress).unwrap();

    let child_id = child.expect("happy path returns the new child id");
    let catalog = Catalog::open(&db, &root).unwrap();
    let children = catalog.list_stack_children(id).unwrap();
    assert!(
        children.iter().any(|c| c.id == child_id),
        "the RapidRAW output must be stacked under the original"
    );
    // The output file exists next to the original, following the naming convention.
    assert!(root.join("DSC1-rapidraw.tiff").is_file());
    let seen = log.lock().unwrap().clone();
    assert!(seen.contains(&"editing".to_string()));
    assert!(seen.contains(&"importing".to_string()));
    assert!(seen.contains(&"done".to_string()));
}

/// Error path: the mock exits 1 → run_roundtrip errors, so the command layer clears the
/// editing state and surfaces the error. No child is stacked.
#[test]
fn error_path_surfaces_error_and_stacks_nothing() {
    let _exec = exec_guard();
    let (db, root, source, id) = setup("error");
    let bin = mock_binary(&root, "RapidRAW", "exit 1");

    let (log, progress) = phases();
    let cancel = AtomicBool::new(false);
    let r = resolved(&db, &root, &source, id, &bin);
    let res = run_roundtrip(&r, &cancel, &progress);

    assert!(res.is_err(), "a nonzero exit must be an error");
    let catalog = Catalog::open(&db, &root).unwrap();
    assert!(
        catalog.list_stack_children(id).unwrap().is_empty(),
        "no child should be stacked on the error path"
    );
    // The state machine never reached importing/done.
    let seen = log.lock().unwrap().clone();
    assert!(!seen.contains(&"done".to_string()));
}

/// Forwarded path: the mock exits 0 WITHOUT writing the output (as when the session is
/// forwarded to an already-open window). The watcher engages; when the test writes the output
/// file itself, the import completes.
#[test]
fn forwarded_path_waits_then_imports() {
    let _exec = exec_guard();
    let (db, root, source, id) = setup("forwarded");
    // Mock does nothing but exit 0 — no output file.
    let bin = mock_binary(&root, "RapidRAW", "exit 0");
    let out = root.join("DSC1-rapidraw.tiff");

    // A separate thread simulates the user clicking Done in the other window a moment later.
    let out_w = out.clone();
    let root_w = root.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        let seed = root_w.join("seed2.tiff");
        write_tiny_image(&seed);
        std::fs::copy(&seed, &out_w).unwrap();
    });

    let (log, progress) = phases();
    let cancel = AtomicBool::new(false);
    let r = resolved(&db, &root, &source, id, &bin);
    let child = run_roundtrip(&r, &cancel, &progress).unwrap();
    writer.join().unwrap();

    let child_id = child.expect("forwarded path imports once the file appears");
    let catalog = Catalog::open(&db, &root).unwrap();
    assert!(catalog.list_stack_children(id).unwrap().iter().any(|c| c.id == child_id));
    // We passed through the waiting state before importing.
    let seen = log.lock().unwrap().clone();
    assert!(seen.contains(&"waiting".to_string()), "must report waiting in the forwarded case");
}

/// Forwarded path + cancel: the mock exits 0 without output; the watcher waits. Tripping the
/// cancel flag abandons the wait (Ok(None), no import) — this doubles as the
/// closed-without-Done resolution.
#[test]
fn forwarded_wait_can_be_cancelled() {
    let _exec = exec_guard();
    let (db, root, source, id) = setup("cancel");
    let bin = mock_binary(&root, "RapidRAW", "exit 0"); // never writes output

    let (_log, progress) = phases();
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    // Cancel shortly after the wait begins.
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        c2.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    let r = resolved(&db, &root, &source, id, &bin);
    let child = run_roundtrip(&r, &cancel, &progress).unwrap();
    canceller.join().unwrap();

    assert!(child.is_none(), "cancelling the wait must abandon the round-trip");
    let catalog = Catalog::open(&db, &root).unwrap();
    assert!(catalog.list_stack_children(id).unwrap().is_empty());
}

/// Size-stability: the mock writes the output in two chunks with a delay. The import must only
/// happen after the size is stable across two checks — verified by the final file being the
/// full two-chunk content.
#[test]
fn imports_only_after_size_is_stable() {
    let _exec = exec_guard();
    let (db, root, source, id) = setup("stable");
    let out = root.join("DSC1-rapidraw.tiff");
    let seed = root.join("seed3.tiff");
    write_tiny_image(&seed);
    let seed_bytes = std::fs::read(&seed).unwrap();
    // Final size = the decodable seed image + three appended chunks.
    let full_len = seed_bytes.len() * 4;

    // Mock: write the valid image immediately (import needs a decodable file), then in the
    // background grow it in three quick chunks. The size therefore changes across the watcher's
    // first ~500ms poll and only settles once all appends land (~450ms), so a stable pair is
    // observed only afterwards — proving import waited for stability rather than firing the
    // instant the file appeared.
    let body = format!(
        "cp \"{seed}\" \"$OUT\"\n( sleep 0.15; cat \"{seed}\" >> \"$OUT\"; sleep 0.15; cat \"{seed}\" >> \"$OUT\"; sleep 0.15; cat \"{seed}\" >> \"$OUT\" ) &\nexit 0",
        seed = seed.display()
    );
    let bin = mock_binary(&root, "RapidRAW", &body);

    let (_log, progress) = phases();
    let cancel = AtomicBool::new(false);
    let r = resolved(&db, &root, &source, id, &bin);
    let child = run_roundtrip(&r, &cancel, &progress).unwrap();

    assert!(child.is_some(), "should import after the size settles");
    // By the time import ran, the background appends had completed → at least the full
    // four-chunk size. (>=, not ==: the post-import EXIF carry-over rewrites the file with
    // exiftool — forcing Orientation=1 — which can grow it by a few bytes.)
    assert!(
        std::fs::metadata(&out).unwrap().len() as usize >= full_len,
        "import must wait until the appended chunk has landed and the size is stable"
    );
}

/// Orientation regression: RapidRAW bakes the rotation into the exported pixels (and writes
/// no Orientation tag of its own), so the EXIF carry-over must never copy the source's
/// Orientation onto the output — an orientation-honoring viewer would rotate it a second
/// time. The carry-over forces Orientation=1; the rest of the tags still come across.
#[test]
fn exif_carryover_resets_orientation_to_upright() {
    let _exec = exec_guard();
    if std::process::Command::new("exiftool").arg("-ver").output().is_err() {
        eprintln!("exiftool not installed — skipping");
        return;
    }
    let (db, root, source, id) = setup("orient");
    // Give the source shooting EXIF incl. a rotate-90 Orientation, like a portrait ARW.
    let st = std::process::Command::new("exiftool")
        .args(["-Orientation#=6", "-Model=MockCam", "-overwrite_original"])
        .arg(&source)
        .status()
        .unwrap();
    assert!(st.success());

    // Mock: emit an EXIF-less image (like RapidRAW's export) and exit 0.
    let out_img = root.join("seed.tiff");
    write_tiny_image(&out_img);
    let bin = mock_binary(
        &root,
        "RapidRAW",
        &format!("cp \"{}\" \"$OUT\"\nexit 0", out_img.display()),
    );
    let (_log, progress) = phases();
    let cancel = AtomicBool::new(false);
    let r = resolved(&db, &root, &source, id, &bin);
    run_roundtrip(&r, &cancel, &progress)
        .unwrap()
        .expect("orientation test expects a successful import");

    let out = root.join("DSC1-rapidraw.tiff");
    let o = std::process::Command::new("exiftool")
        .args(["-Orientation", "-n", "-s3"])
        .arg(&out)
        .output()
        .unwrap();
    let val = String::from_utf8_lossy(&o.stdout).trim().to_string();
    assert!(
        val.is_empty() || val == "1",
        "output Orientation must be upright/absent after carry-over, got {val:?}"
    );
    let m = std::process::Command::new("exiftool")
        .args(["-Model", "-s3"])
        .arg(&out)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&m.stdout).trim(),
        "MockCam",
        "the rest of the source EXIF must still carry over"
    );
}
