//! The crash marker wired into LibRaw, end to end (src/crash_marker.rs, src/raw/mod.rs):
//! a file with two recorded crashes is skipped by every LibRaw entry point before LibRaw
//! is called, and the reason says what happens instead; any other file still goes to
//! LibRaw; and no in-flight marker outlives a call. Its own test binary, so the process-wide
//! store is initialised here and nowhere else.
#![cfg(feature = "raw")]

use chairphoto_lib::crash_marker::{self, Markers, BLOCK_AFTER};
use chairphoto_lib::raw;
use std::sync::atomic::AtomicBool;

#[test]
fn a_twice_crashed_file_is_skipped_by_every_libraw_entry_point() {
    let root = std::env::temp_dir().join(format!("chairphoto-crash-raw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = root.join("crash-markers");
    std::fs::create_dir_all(&root).unwrap();
    let poison = root.join("poison.ARW");
    let other = root.join("other.ARW");
    std::fs::write(&poison, b"stands in for a file that segfaults LibRaw").unwrap();
    std::fs::write(&other, b"not a raw either, but never crashed anything").unwrap();

    // Two previous runs died inside a decode of `poison` (a leaked guard is exactly what a
    // dead process leaves behind), each recovered at the next "launch".
    let subject = raw::crash_subject(&poison);
    for _ in 0..BLOCK_AFTER {
        let previous_run = Markers::open(&store);
        std::mem::forget(previous_run.enter(raw::KIND_DECODE, &subject, "poison.ARW"));
        previous_run.recover();
    }

    // This launch.
    assert!(crash_marker::init(&store).is_empty(), "nothing new to recover");

    match raw::probe(&poison) {
        raw::RawSupport::Unsupported { reason, .. } => {
            assert!(reason.contains("crashed") && reason.contains("camera preview"), "{reason}")
        }
        other => panic!("a blocked file must not probe as supported: {other:?}"),
    }
    let e = raw::decode_linear(&poison, &AtomicBool::new(false)).unwrap_err();
    assert!(e.contains("crashed"), "{e}");
    let e = raw::decode_to_image(&poison).map(|_| ()).unwrap_err();
    assert!(e.contains("crashed"), "{e}");

    // Any other file still reaches LibRaw and gets LibRaw's own verdict.
    let e = raw::decode_linear(&other, &AtomicBool::new(false)).unwrap_err();
    assert!(e.contains("libraw") && !e.contains("crashed"), "{e}");

    // Guards cleaned up after themselves, success or error.
    let inflight = std::fs::read_dir(store.join("inflight")).unwrap().count();
    assert_eq!(inflight, 0, "no marker may outlive its call");

    // Replacing the file is a fresh chance: it goes to LibRaw again.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&poison, b"a replaced file, different size").unwrap();
    let e = raw::decode_linear(&poison, &AtomicBool::new(false)).unwrap_err();
    assert!(!e.contains("crashed"), "{e}");

    std::fs::remove_dir_all(&root).ok();
}
