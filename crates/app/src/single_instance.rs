//! One running ChairPhoto per catalog home, and `chairphoto://` URLs forwarded to it — what
//! `tauri-plugin-single-instance` (D-Bus) and `tauri-plugin-deep-link` did for the Tauri shell.
//!
//! # The endpoint
//!
//! Two files in a private runtime directory ([`runtime_dir`]):
//!
//! - `<key>.lock` — whoever holds an exclusive `flock` on it is the **primary**. The kernel
//!   drops the lock when that process dies, however it dies, so a crash never leaves a
//!   stale lock.
//! - `<key>.sock` — the primary's Unix socket. A crashed primary leaves the file behind; the
//!   next primary (which can only be one, it holds the lock) removes it before binding.
//!
//! The directory is `$XDG_RUNTIME_DIR/chairphoto`, or `<tmp>/chairphoto-<uid>` when
//! `XDG_RUNTIME_DIR` is unset; either way created `0700` and refused unless it is owned by
//! this user and not writable by anyone else.
//!
//! **The key** ([`instance_key`]) is a hash of the app data directory — the directory that
//! holds `default.chairphoto` (`app::app_data_dir`, `$XDG_DATA_HOME/chairphoto`). One app
//! data dir, one default catalog, one instance: an agent or test run with its own
//! `XDG_DATA_HOME` gets its own key and never forwards into (or blocks) the user's real
//! instance. The hash is FNV-1a over the path's bytes, fixed so that two builds agree.
//!
//! # The protocol (version 1)
//!
//! A second launch connects and sends newline-terminated lines, then waits for one line back:
//!
//! ```text
//! chairphoto-instance 1
//! url chairphoto://0a1b2c3d-…/loupe      (zero or more)
//! end
//! ```
//!
//! The primary answers `ok` and hands the [`Request`] to the app: it focuses the main window
//! and opens each URL. A request with no URL only focuses — a second launch from the app
//! launcher. Anything else gets `error <reason>` and is dropped. The primary serves one
//! connection at a time with a read timeout, and caps line length and URL count, so a stuck
//! or hostile peer (the socket is private to this user anyway) costs at most a timeout.

use std::fs::File;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const HELLO: &str = "chairphoto-instance 1";
/// Longest accepted line, in bytes (a URL line with its `url ` prefix).
const MAX_LINE: usize = 8 * 1024;
/// Most URLs in one request.
const MAX_URLS: usize = 64;
/// How long the primary waits on one peer, and a peer on the primary's answer.
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a second launch keeps trying to connect while the primary (which already holds
/// the lock) is still starting up and has not bound the socket yet.
pub const CONNECT_PATIENCE: Duration = Duration::from_secs(5);

/// What a second launch asks of the primary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Request {
    /// `chairphoto:` URLs from the second launch's command line, in order. Empty: just focus.
    pub urls: Vec<String>,
}

impl Request {
    /// The request for a launch with these command-line arguments (without `argv[0]`): every
    /// argument whose scheme is `chairphoto:` (any case). The desktop entry passes the URL as
    /// `%u`; anything else on the command line is not ours to forward.
    pub fn from_args<I: IntoIterator<Item = String>>(args: I) -> Self {
        let urls = args
            .into_iter()
            .filter(|a| a.get(..11).is_some_and(|s| s.eq_ignore_ascii_case("chairphoto:")))
            .filter(|a| !a.contains(['\n', '\r']))
            .collect();
        Request { urls }
    }
}

/// Where one instance key's lock and socket live.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub lock: PathBuf,
    pub socket: PathBuf,
}

impl Endpoint {
    /// The endpoint for `key` in `dir` (see [`runtime_dir`], [`instance_key`]).
    pub fn new(dir: &Path, key: &str) -> Self {
        Endpoint { lock: dir.join(format!("{key}.lock")), socket: dir.join(format!("{key}.sock")) }
    }

    /// The endpoint for this process's app data dir, in the runtime dir.
    pub fn for_app_data_dir(app_data_dir: &Path) -> io::Result<Self> {
        Ok(Self::new(&runtime_dir()?, &instance_key(app_data_dir)))
    }
}

/// The instance key for an app data dir: 16 hex digits of FNV-1a (64-bit) over its path,
/// canonicalized when it exists so that a symlinked `XDG_DATA_HOME` names one instance.
pub fn instance_key(app_data_dir: &Path) -> String {
    let path = std::fs::canonicalize(app_data_dir).unwrap_or_else(|_| app_data_dir.to_path_buf());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// `$XDG_RUNTIME_DIR/chairphoto`, else `<tmp>/chairphoto-<uid>`, created `0700` and checked
/// to be this user's and private.
pub fn runtime_dir() -> io::Result<PathBuf> {
    let uid = current_uid()?;
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(base) => PathBuf::from(base).join("chairphoto"),
        None => std::env::temp_dir().join(format!("chairphoto-{uid}")),
    };
    ensure_private_dir(&dir, uid)?;
    Ok(dir)
}

/// This process's uid, read from `/proc/self` (no libc call needed on Linux).
fn current_uid() -> io::Result<u32> {
    std::fs::metadata("/proc/self").map(|m| m.uid())
}

/// Create `dir` (mode 0700) if missing; refuse it unless it is a real directory owned by
/// `uid` with no group/other write permission. A shared temp dir makes this matter: another
/// user could otherwise pre-create the directory and receive our URLs.
fn ensure_private_dir(dir: &Path, uid: u32) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.uid() != uid || meta.permissions().mode() & 0o022 != 0 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("{} is not a private directory owned by uid {uid}", dir.display()),
        ));
    }
    Ok(())
}

/// Why [`claim`] failed.
#[derive(Debug)]
pub enum ClaimError {
    /// The endpoint itself is unusable (runtime dir, lock file, bind). Single-instance
    /// cannot be enforced; the caller may run without it.
    Endpoint(io::Error),
    /// Another instance holds the lock but did not take the request. Starting anyway would
    /// put a second instance on the same catalog.
    NoAnswer(io::Error),
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimError::Endpoint(e) => write!(f, "single-instance endpoint unusable: {e}"),
            ClaimError::NoAnswer(e) => write!(f, "ChairPhoto is already running but did not answer: {e}"),
        }
    }
}

/// The outcome of [`claim`].
pub enum Claim {
    /// This process is the primary: serve the endpoint for the rest of its life.
    Primary(Primary),
    /// Another instance is the primary and accepted the request; this process should exit.
    Forwarded,
}

/// The primary's hold on the endpoint: the lock and the bound socket. Dropping it removes
/// the socket file, then releases the lock (fields drop after `Drop::drop`).
pub struct Primary {
    listener: Option<UnixListener>,
    socket: PathBuf,
    _lock: File,
}

impl Drop for Primary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Primary {
    /// Serve second launches on a thread of its own for as long as the returned `Primary`
    /// lives, handing each accepted [`Request`] to `on_request` (on that thread). Returns
    /// `self` back so the caller keeps the lock and the socket file alive.
    pub fn serve(mut self, on_request: impl Fn(Request) + Send + 'static) -> io::Result<Self> {
        let listener = self.listener.take().expect("serve is called once");
        std::thread::Builder::new().name("chairphoto-instance".into()).spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => match read_request(stream) {
                        Ok(request) => on_request(request),
                        Err(e) => eprintln!("single instance: dropped a request: {e}"),
                    },
                    Err(e) => eprintln!("single instance: accept failed: {e}"),
                }
            }
        })?;
        Ok(self)
    }
}

/// Become the primary for `endpoint`, or forward `request` to the one that is.
///
/// Until `patience` runs out, in a loop:
///
/// - The lock is free: take it, remove a stale socket (a crashed primary's), bind, and
///   return [`Claim::Primary`].
/// - The lock is held and the socket answers: send `request` and return
///   [`Claim::Forwarded`] once the primary says `ok` ([`ClaimError::NoAnswer`] if it says
///   anything else).
/// - The lock is held but nothing listens: the primary is still starting (it locks before
///   it binds) or on its way out (it unlinks the socket before its lock goes, and a child
///   process between `fork` and `exec` can hold the lock a moment longer). Wait a little and
///   look again: either the socket appears or the lock comes free.
pub fn claim(endpoint: &Endpoint, request: &Request, patience: Duration) -> Result<Claim, ClaimError> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&endpoint.lock)
        .map_err(ClaimError::Endpoint)?;
    let deadline = Instant::now() + patience;
    loop {
        match lock.try_lock() {
            Ok(()) => return become_primary(endpoint, lock),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => return Err(ClaimError::Endpoint(e)),
        }
        match UnixStream::connect(&endpoint.socket) {
            Ok(stream) => {
                forward(stream, request).map_err(ClaimError::NoAnswer)?;
                return Ok(Claim::Forwarded);
            }
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
                if Instant::now() >= deadline {
                    return Err(ClaimError::NoAnswer(e));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(ClaimError::NoAnswer(e)),
        }
    }
}

fn become_primary(endpoint: &Endpoint, lock: File) -> Result<Claim, ClaimError> {
    match std::fs::remove_file(&endpoint.socket) {
        Ok(()) => eprintln!("single instance: removed a stale socket (previous instance crashed?)"),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(ClaimError::Endpoint(e)),
    }
    let listener = UnixListener::bind(&endpoint.socket).map_err(ClaimError::Endpoint)?;
    Ok(Claim::Primary(Primary { listener: Some(listener), socket: endpoint.socket.clone(), _lock: lock }))
}

/// Send `request` over a connection to the primary and wait for its `ok`.
fn forward(mut stream: UnixStream, request: &Request) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut message = format!("{HELLO}\n");
    for url in &request.urls {
        message.push_str(&format!("url {url}\n"));
    }
    message.push_str("end\n");
    stream.write_all(message.as_bytes())?;
    let mut answer = String::new();
    BufReader::new(stream).take(MAX_LINE as u64).read_line(&mut answer)?;
    match answer.trim_end() {
        "ok" => Ok(()),
        "" => Err(io::Error::new(ErrorKind::UnexpectedEof, "the running instance closed the connection")),
        other => Err(io::Error::other(format!("the running instance answered {other:?}"))),
    }
}

/// Read one request from a peer and answer it. Errors are the peer's fault; the caller logs
/// them and moves on.
fn read_request(stream: UnixStream) -> io::Result<Request> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let result = parse_request(&mut BufReader::new(stream));
    let answer = match &result {
        Ok(_) => "ok\n".to_string(),
        Err(e) => format!("error {}\n", e.to_string().replace('\n', " ")),
    };
    let _ = writer.write_all(answer.as_bytes());
    result
}

fn parse_request(reader: &mut impl BufRead) -> io::Result<Request> {
    let invalid = |why: &str| io::Error::new(ErrorKind::InvalidData, why.to_string());
    if read_line(reader)?.as_deref() != Some(HELLO) {
        return Err(invalid("unsupported protocol"));
    }
    let mut request = Request::default();
    loop {
        match read_line(reader)?.as_deref() {
            Some("end") => return Ok(request),
            Some(line) => {
                let url = line.strip_prefix("url ").ok_or_else(|| invalid("unknown line"))?;
                if request.urls.len() == MAX_URLS {
                    return Err(invalid("too many URLs"));
                }
                request.urls.push(url.to_string());
            }
            None => return Err(invalid("request ended without `end`")),
        }
    }
}

/// One `\n`-terminated line (without the `\n`), at most [`MAX_LINE`] bytes; `None` at EOF.
fn read_line(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = (&mut *reader).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.pop() != Some(b'\n') {
        return Err(io::Error::new(ErrorKind::InvalidData, "line too long or unterminated"));
    }
    String::from_utf8(buf).map(Some).map_err(|_| io::Error::new(ErrorKind::InvalidData, "not UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A private directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!("cp-si-{tag}-{}-{nanos}", std::process::id()));
            std::fs::DirBuilder::new().mode(0o700).recursive(true).create(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const PHOTO: &str = "chairphoto://0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d/loupe";

    fn primary(claim: Result<Claim, ClaimError>) -> Primary {
        match claim.expect("claim") {
            Claim::Primary(p) => p,
            Claim::Forwarded => panic!("expected to become the primary"),
        }
    }

    /// The first claim becomes the primary; a second claim on the same endpoint forwards its
    /// URLs to it and returns `Forwarded`; a URL-less second launch arrives as a bare focus.
    #[test]
    fn a_second_launch_forwards_its_urls_to_the_primary() {
        let dir = TempDir::new("forward");
        let endpoint = Endpoint::new(&dir.0, "k");
        let (tx, rx) = mpsc::channel();
        let _primary = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE))
            .serve(move |r| tx.send(r).unwrap())
            .unwrap();

        let second = Request { urls: vec![PHOTO.into(), "chairphoto://tag/x".into()] };
        assert!(matches!(claim(&endpoint, &second, CONNECT_PATIENCE).unwrap(), Claim::Forwarded));
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), second);

        assert!(matches!(claim(&endpoint, &Request::default(), CONNECT_PATIENCE).unwrap(), Claim::Forwarded));
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), Request::default());
    }

    /// Endpoints with different keys (two app data dirs) do not see each other.
    #[test]
    fn different_keys_are_different_instances() {
        let dir = TempDir::new("keys");
        let _a = primary(claim(&Endpoint::new(&dir.0, "a"), &Request::default(), CONNECT_PATIENCE));
        let _b = primary(claim(&Endpoint::new(&dir.0, "b"), &Request::default(), CONNECT_PATIENCE));
    }

    /// A crashed primary leaves its socket file but not its lock: the next launch becomes the
    /// primary, replaces the stale socket, and serves on it.
    #[test]
    fn a_stale_socket_from_a_crashed_primary_is_replaced() {
        let dir = TempDir::new("stale");
        let endpoint = Endpoint::new(&dir.0, "k");
        // What a crash leaves: a socket file nobody listens on, and no lock holder.
        drop(UnixListener::bind(&endpoint.socket).unwrap());
        assert!(endpoint.socket.exists());
        assert!(UnixStream::connect(&endpoint.socket).is_err(), "nobody listens on a stale socket");

        let (tx, rx) = mpsc::channel();
        let _primary = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE))
            .serve(move |r| tx.send(r).unwrap())
            .unwrap();
        let second = Request { urls: vec![PHOTO.into()] };
        assert!(matches!(claim(&endpoint, &second, CONNECT_PATIENCE).unwrap(), Claim::Forwarded));
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), second);
    }

    /// A primary that exits cleanly removes its socket and releases the lock, so the next
    /// launch is a primary again.
    #[test]
    fn a_primary_that_quits_hands_over_to_the_next_launch() {
        let dir = TempDir::new("handover");
        let endpoint = Endpoint::new(&dir.0, "k");
        let first = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE)).serve(|_| {}).unwrap();
        drop(first);
        assert!(!endpoint.socket.exists(), "a clean exit removes the socket");
        let _next = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE));
    }

    /// The lock is held but nobody ever binds (a primary hung during startup): the second
    /// launch gives up after its patience with an error rather than hanging or starting a
    /// second instance on the same catalog.
    #[test]
    fn a_primary_that_never_binds_is_an_error_after_the_patience() {
        let dir = TempDir::new("hung");
        let endpoint = Endpoint::new(&dir.0, "k");
        let held = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&endpoint.lock).unwrap();
        held.lock().unwrap();
        let started = Instant::now();
        let result = claim(&endpoint, &Request { urls: vec![PHOTO.into()] }, Duration::from_millis(200));
        assert!(matches!(result, Err(ClaimError::NoAnswer(_))), "expected NoAnswer");
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    /// A second launch that arrives while the primary holds the lock but has not bound yet
    /// keeps retrying and gets through once it binds.
    #[test]
    fn a_second_launch_waits_for_a_primary_that_is_still_starting() {
        let dir = TempDir::new("starting");
        let endpoint = Endpoint::new(&dir.0, "k");
        let held = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&endpoint.lock).unwrap();
        held.lock().unwrap();
        let socket = endpoint.socket.clone();
        let (tx, rx) = mpsc::channel();
        let late = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let listener = UnixListener::bind(&socket).unwrap();
            let (stream, _) = listener.accept().unwrap();
            tx.send(read_request(stream).unwrap()).unwrap();
        });
        let second = Request { urls: vec![PHOTO.into()] };
        assert!(matches!(claim(&endpoint, &second, CONNECT_PATIENCE).unwrap(), Claim::Forwarded));
        assert_eq!(rx.recv().unwrap(), second);
        late.join().unwrap();
    }

    /// A launch that arrives while the primary is on its way out — socket already unlinked,
    /// lock not yet released — becomes the primary once the lock comes free, instead of
    /// giving up for want of a socket.
    #[test]
    fn a_launch_during_the_primarys_exit_becomes_the_next_primary() {
        let dir = TempDir::new("exiting");
        let endpoint = Endpoint::new(&dir.0, "k");
        let held = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&endpoint.lock).unwrap();
        held.lock().unwrap();
        let departing = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(held);
        });
        let _next = primary(claim(&endpoint, &Request { urls: vec![PHOTO.into()] }, CONNECT_PATIENCE));
        assert!(endpoint.socket.exists(), "the new primary is bound");
        departing.join().unwrap();
    }

    /// A peer that speaks something else gets an error line and its request is dropped; the
    /// primary keeps serving.
    #[test]
    fn a_malformed_request_is_refused_and_the_primary_keeps_serving() {
        let dir = TempDir::new("malformed");
        let endpoint = Endpoint::new(&dir.0, "k");
        let (tx, rx) = mpsc::channel();
        let _primary = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE))
            .serve(move |r| tx.send(r).unwrap())
            .unwrap();

        for bad in ["hello\nend\n", "chairphoto-instance 1\nopen x\nend\n", "chairphoto-instance 1\nurl x\n"] {
            let mut stream = UnixStream::connect(&endpoint.socket).unwrap();
            stream.write_all(bad.as_bytes()).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).unwrap();
            assert!(answer.starts_with("error "), "{bad:?} → {answer:?}");
        }
        let second = Request { urls: vec![PHOTO.into()] };
        assert!(matches!(claim(&endpoint, &second, CONNECT_PATIENCE).unwrap(), Claim::Forwarded));
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), second, "only the good request arrives");
    }

    #[test]
    fn requests_are_bounded() {
        let long = format!("{HELLO}\nurl {}\nend\n", "a".repeat(MAX_LINE));
        assert!(parse_request(&mut long.as_bytes()).is_err());
        let mut many = format!("{HELLO}\n");
        for _ in 0..=MAX_URLS {
            many.push_str("url chairphoto://x\n");
        }
        many.push_str("end\n");
        assert!(parse_request(&mut many.as_bytes()).is_err());
        let ok = format!("{HELLO}\nurl a\nurl b\nend\n");
        assert_eq!(parse_request(&mut ok.as_bytes()).unwrap().urls, ["a", "b"]);
    }

    #[test]
    fn only_chairphoto_urls_are_forwarded() {
        let args = [
            "chairphoto://tag/x",
            "ChairPhoto://abc",
            "/home/me/a.nef",
            "--verbose",
            "https://example.com",
            "chairphoto://bad\nline",
        ]
        .map(String::from);
        assert_eq!(Request::from_args(args).urls, ["chairphoto://tag/x", "ChairPhoto://abc"]);
    }

    #[test]
    fn the_key_follows_the_app_data_dir() {
        let a = instance_key(Path::new("/nonexistent/one/chairphoto"));
        let b = instance_key(Path::new("/nonexistent/two/chairphoto"));
        assert_eq!(a.len(), 16);
        assert_ne!(a, b);
        assert_eq!(a, instance_key(Path::new("/nonexistent/one/chairphoto")), "stable");
        // FNV-1a 64 of the empty string is its offset basis: the hash is the fixed algorithm,
        // not std's randomly seeded one.
        assert_eq!(instance_key(Path::new("")), "cbf29ce484222325");
    }

    #[test]
    fn a_shared_runtime_dir_is_refused() {
        let dir = TempDir::new("perm");
        let uid = current_uid().unwrap();
        let private = dir.0.join("private");
        ensure_private_dir(&private, uid).unwrap();
        assert_eq!(std::fs::metadata(&private).unwrap().permissions().mode() & 0o777, 0o700);

        let shared = dir.0.join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(ensure_private_dir(&shared, uid).is_err());

        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&private, &link).unwrap();
        assert!(ensure_private_dir(&link, uid).is_err(), "a symlink is not accepted");

        assert!(ensure_private_dir(&private, uid + 1).is_err(), "someone else's directory");
    }
}
