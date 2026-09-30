//! Real signals against `chairphoto_app::signals`, in processes of their own: the handlers are
//! process-wide, and the second-signal test ends its process on purpose.
//!
//! Each test re-runs this test binary as a child with `CHAIRPHOTO_SIGNAL_CHILD` naming the
//! scenario; the parent checks the child's exit status and output. That keeps the parent
//! (the test harness) free of installed handlers.

use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

const CHILD: &str = "CHAIRPHOTO_SIGNAL_CHILD";

fn run_child(scenario: &str) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CHILD, scenario)
        .output()
        .unwrap()
}

/// The child side. Without the variable it is an ordinary (empty) test.
#[test]
fn child() {
    let Ok(scenario) = std::env::var(CHILD) else { return };
    let (tx, rx) = mpsc::channel();
    chairphoto_app::signals::install(move |signal| tx.send(signal).unwrap()).unwrap();
    let pid = std::process::id() as libc::pid_t;
    match scenario.as_str() {
        "term" | "int" | "hup" => {
            let signal = match scenario.as_str() {
                "term" => libc::SIGTERM,
                "int" => libc::SIGINT,
                _ => libc::SIGHUP,
            };
            // SAFETY: signalling our own process, whose handler is installed above.
            assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
            let got = rx.recv_timeout(Duration::from_secs(5)).expect("the callback ran");
            assert_eq!(got, signal);
            println!("CHILD-OK callback got {got}");
        }
        "twice" => {
            // SAFETY: as above.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
            rx.recv_timeout(Duration::from_secs(5)).expect("the first signal reached the callback");
            println!("CHILD-FIRST");
            // The quit is "stuck" (nothing here quits); the second signal must end the process.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGINT) }, 0);
            std::thread::sleep(Duration::from_secs(5));
            println!("CHILD-SURVIVED");
        }
        other => panic!("unknown scenario {other}"),
    }
}

/// SIGTERM, SIGINT and SIGHUP each reach the callback (and do not kill the process).
#[test]
fn a_quit_signal_reaches_the_callback_instead_of_killing_the_process() {
    for scenario in ["term", "int", "hup"] {
        let out = run_child(scenario);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{scenario}: {:?}\n{stdout}\n{}", out.status, String::from_utf8_lossy(&out.stderr));
        assert!(stdout.contains("CHILD-OK"), "{scenario}: {stdout}");
    }
}

/// A second quit signal while the first is still being handled forces the exit, with the
/// conventional status 128 + signal.
#[test]
fn a_second_signal_forces_the_exit() {
    let out = run_child("twice");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("CHILD-FIRST"), "{stdout}");
    assert!(!stdout.contains("CHILD-SURVIVED"), "{stdout}");
    assert_eq!(out.status.code(), Some(128 + libc::SIGINT), "{:?}", out.status);
}
