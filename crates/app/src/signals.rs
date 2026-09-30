//! `SIGTERM`, `SIGINT` and `SIGHUP` quit the app the way Ctrl+Q does.
//!
//! Left at their default action these signals kill the process on the spot: no quit
//! observers, no `crash_marker::clean_exit`, so a logout, a `kill` or a Ctrl+C in the
//! launching terminal during a LibRaw decode would count as a crash strike against that
//! file. (The Tauri shell has the same gap: neither it nor tao/wry/tauri installs a handler
//! for these signals, so they take the default action there too.)
//!
//! The handler does the one async-signal-safe thing it can: it writes the signal number to
//! a pipe (the self-pipe trick). A watcher thread reads the pipe and calls the app's
//! callback, which dispatches [`Quit`](crate::keymap::Quit) on the main thread, so a signal
//! and Ctrl+Q take the same path out.
//!
//! **A second signal forces the exit** (`_exit(128 + signal)`), for a quit that is stuck —
//! the second Ctrl+C a user presses when the first did nothing. That exit skips the clean
//! quit, which is the point.
//!
//! The dispositions are reset to the default across `exec`, so child processes (exiftool,
//! ffmpeg, external editors) are unaffected.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// The signals that quit the app.
pub const QUIT_SIGNALS: [i32; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// The pipe's write end, for the handler; -1 until [`install`].
static PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);
/// Set by the first quit signal; the second one forces the exit.
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(signal: libc::c_int) {
    if QUIT_REQUESTED.swap(true, Ordering::SeqCst) {
        // SAFETY: `_exit` is async-signal-safe.
        unsafe { libc::_exit(128 + signal) };
    }
    let fd = PIPE_WRITE.load(Ordering::SeqCst);
    if fd >= 0 {
        let byte = signal as u8;
        // SAFETY: `write` is async-signal-safe; the buffer is one live byte. A full pipe
        // (impossible: one byte per process lifetime reaches it) would just drop it.
        unsafe { libc::write(fd, (&byte as *const u8).cast(), 1) };
    }
}

/// Install the handlers and start the watcher thread, which calls `on_quit_signal` with the
/// signal number the first time one arrives. Once per process; a second call is an error.
pub fn install(on_quit_signal: impl Fn(i32) + Send + 'static) -> io::Result<()> {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return Err(io::Error::other("signal handlers are already installed"));
    }
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid two-element array for pipe2 to fill.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let [read_fd, write_fd] = fds;
    PIPE_WRITE.store(write_fd, Ordering::SeqCst);

    std::thread::Builder::new().name("chairphoto-signals".into()).spawn(move || {
        let mut byte = 0u8;
        loop {
            // SAFETY: reads one byte into a live local from the pipe this thread owns.
            let n = unsafe { libc::read(read_fd, (&mut byte as *mut u8).cast(), 1) };
            match n {
                1 => on_quit_signal(i32::from(byte)),
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
                _ => return, // write end closed or an unexpected error: nothing more to watch
            }
        }
    })?;

    for signal in QUIT_SIGNALS {
        // SAFETY: a zeroed sigaction is a valid starting point; the handler is an
        // `extern "C" fn(c_int)`, as `sa_sigaction` expects without SA_SIGINFO. SA_RESTART
        // keeps other threads' interrupted syscalls from failing with EINTR.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}
