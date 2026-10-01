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
//! `XDG_RUNTIME_DIR` is unset; either way created `0700`, refused unless it is a real
//! directory (not a symlink) owned by this user, and tightened to `0700` if it already exists
//! with any group/other bits ([`ensure_private_dir`]).
//!
//! # Who may connect
//!
//! Only this user. Three layers, each enough on its own against another local user:
//!
//! 1. The directory is `0700`, so nobody else can reach the socket file at all (connecting
//!    to a Unix socket needs search permission on every directory of its path).
//! 2. The socket is `chmod 0600` right after `bind`. Between the two it has the umask's mode,
//!    but it is already inside the `0700` directory, so that moment exposes nothing; this
//!    avoids changing the process-global umask, which would race every other thread that
//!    creates files.
//! 3. The primary reads each peer's uid (`SO_PEERCRED`, [`peer_uid`]) and drops a connection
//!    from any other uid unanswered.
//! 4. A second launch reads the *server's* uid the same way and forwards only to this user
//!    ([`claim`]): a lock and socket planted by someone else are never told our links.
//!
//! An existing directory of ours that others could only read or search is tightened to
//! `0700`; one they could **write** is refused, because they may already have planted a lock
//! file (and hold it) and a socket there, and tightening the mode later revokes neither
//! (Codex re-check of b6a788c).
//!
//! A socket path must fit `sun_path`
//! ([`MAX_SOCKET_PATH`] bytes): when `$TMPDIR` is too deep for that, the fallback is
//! `/tmp/chairphoto-<uid>`, and a path that still does not fit is refused by [`claim`] with an
//! error that says so (the app then runs without single-instance).
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
//! The primary hands the [`Request`] to the app (which focuses the main window and opens each
//! URL; a request with no URL only focuses, a second launch from the app launcher) and only
//! then answers: `ok` if the app took it, `closing` if the app is quitting ([`Closer`]) or
//! its router is gone, `busy` if too many requests already wait for the main thread
//! ([`Refused`]). On `closing` or `busy` the second launch keeps trying for its patience:
//! after `closing` the lock comes free and it becomes the primary itself, link in hand; a
//! primary that stays busy that long is stuck, and the second launch exits 1 saying so. A
//! malformed request gets `error <reason>`.
//!
//! What a quitting primary still loses: a request the router already took (`ok` sent) but
//! did not apply before the event loop ended. The endpoint closes when the quit is asked for,
//! not when the loop ends, which leaves only requests accepted in the same instant.
//!
//! The primary serves one connection at a time, gives each peer one deadline for its whole
//! request (not a timeout per read, which a trickling peer could renew forever), and caps
//! line length and URL count, so a stuck or hostile peer (only this user can connect, see
//! "Who may connect") costs at most that deadline.

use std::fs::File;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const HELLO: &str = "chairphoto-instance 1";
/// Longest accepted line, in bytes (a URL line with its `url ` prefix).
const MAX_LINE: usize = 8 * 1024;
/// Most URLs in one request.
const MAX_URLS: usize = 64;
/// How long a peer waits on the primary's answer, and either side on a write.
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the primary gives one peer to send its whole request, however it trickles in.
const REQUEST_DEADLINE: Duration = Duration::from_secs(2);
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

/// The longest Unix socket path Linux accepts: `sun_path` is 108 bytes with the NUL.
pub const MAX_SOCKET_PATH: usize = 107;
/// `<16 hex digits>.sock`, the socket's file name.
const SOCKET_NAME_LEN: usize = 16 + ".sock".len();

/// `$XDG_RUNTIME_DIR/chairphoto`, else [`fallback_dir`], created `0700` and checked to be
/// this user's and private.
pub fn runtime_dir() -> io::Result<PathBuf> {
    let uid = current_uid()?;
    let dir = match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(base) => PathBuf::from(base).join("chairphoto"),
        None => fallback_dir(&std::env::temp_dir(), uid),
    };
    ensure_private_dir(&dir, uid)?;
    Ok(dir)
}

/// Without `XDG_RUNTIME_DIR`: `<tmp>/chairphoto-<uid>`, unless a socket in it would be too
/// long for `sun_path` (a deep `$TMPDIR`); then `/tmp/chairphoto-<uid>`.
fn fallback_dir(tmp: &Path, uid: u32) -> PathBuf {
    let name = format!("chairphoto-{uid}");
    let dir = tmp.join(&name);
    if dir.as_os_str().len() + 1 + SOCKET_NAME_LEN <= MAX_SOCKET_PATH {
        dir
    } else {
        Path::new("/tmp").join(name)
    }
}

/// This process's effective uid: what the kernel checks file access against and what a peer
/// sees through `SO_PEERCRED`.
fn current_uid() -> io::Result<u32> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    Ok(unsafe { libc::geteuid() })
}

/// The runtime directory exists but is not safe to coordinate through (someone else's, a
/// symlink, a file, or writable by others). Unlike an endpoint that simply cannot be made,
/// this must not degrade to running without single-instance: every launch would then start
/// on its own against the same catalog (Codex re-check of 9c3c834). See [`is_unsafe_dir`].
#[derive(Debug)]
pub struct UnsafeRuntimeDir(String);

impl std::fmt::Display for UnsafeRuntimeDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; remove it (or fix its owner and mode) and start ChairPhoto again", self.0)
    }
}

impl std::error::Error for UnsafeRuntimeDir {}

/// Whether `e` is the refusal of an unsafe runtime directory ([`UnsafeRuntimeDir`]), which
/// the app treats as fatal rather than running without single-instance.
pub fn is_unsafe_dir(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<UnsafeRuntimeDir>())
}

/// Create `dir` (mode 0700) if missing. Refuse it unless it is a real directory (not a
/// symlink) owned by `uid` that no one else could write; if others could read or search it,
/// tighten it to `0700`.
/// A shared temp dir makes this matter: another user could otherwise pre-create the
/// directory and receive our URLs, or reach a socket in a `0755` directory of ours.
///
/// The checks and the `chmod` act on one open descriptor (`O_NOFOLLOW`, `fstat`, `fchmod`),
/// so the directory cannot be swapped for a symlink or another directory in between.
fn ensure_private_dir(dir: &Path, uid: u32) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let refuse = |why: &str| {
        io::Error::new(
            ErrorKind::PermissionDenied,
            UnsafeRuntimeDir(format!("{} is not a private directory owned by uid {uid}: {why}", dir.display())),
        )
    };
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let handle = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)
    {
        Ok(handle) => handle,
        // O_NOFOLLOW on a symlink: ELOOP; O_DIRECTORY on anything else: ENOTDIR.
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)) => {
            return Err(refuse("not a directory (or a symlink)"))
        }
        // It exists but we may not open it: most likely someone else's (a `0700` directory of
        // another user under the shared `/tmp` fallback). Its owner cannot be checked through
        // a descriptor we cannot get, and it is no place to coordinate (Codex re-check of
        // 252368e).
        Err(e) if matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) => {
            return Err(refuse(&format!("it cannot be opened ({e}); it may belong to another user")))
        }
        Err(e) => return Err(e),
    };
    let meta = handle.metadata()?;
    if !meta.is_dir() {
        return Err(refuse("not a directory"));
    }
    if meta.uid() != uid {
        return Err(refuse(&format!("owned by uid {}", meta.uid())));
    }
    let mode = meta.permissions().mode() & 0o7777;
    if mode & 0o022 != 0 {
        return Err(refuse(&format!("mode {mode:o} lets others write in it, so its contents may not be ours")));
    }
    if mode & 0o077 != 0 {
        eprintln!("single instance: {} had mode {mode:o}; tightening it to 700", dir.display());
        handle.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        if handle.metadata()?.permissions().mode() & 0o077 != 0 {
            return Err(refuse("could not remove its group/other permissions"));
        }
    }
    Ok(())
}

/// The uid of the process on the other end of `stream` when it connected (`SO_PEERCRED`;
/// std's `UnixStream::peer_cred` is unstable).
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd as _;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for writes and `len` is `cred`'s size; the fd is
    // open for as long as `stream` is borrowed.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::other("SO_PEERCRED returned a short ucred"));
    }
    Ok(cred.uid)
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
    closing: Closer,
    /// The only uid whose connections are served: this process's.
    peer_uid: u32,
    _lock: File,
}

/// Why the app did not take a request; the second launch hears it as its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The app is quitting, or its router is gone.
    Closing,
    /// Too many requests already wait for the main thread.
    Busy,
}

impl Refused {
    fn answer(self) -> &'static str {
        match self {
            Refused::Closing => "closing\n",
            Refused::Busy => "busy\n",
        }
    }
}

/// Tells a serving [`Primary`] that the app is quitting. From then on it answers every second
/// launch `closing` without handing the request on, and the second launch waits for the lock
/// to come free and starts fresh instead of sending its link into an app that is going away.
#[derive(Clone, Default)]
pub struct Closer(Arc<AtomicBool>);

impl Closer {
    pub fn close(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_closed(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
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
    ///
    /// `on_request` says whether the app took the request. The peer is answered only after
    /// that: `ok` if it did, the [`Refused`] reason if not (`closing` also once
    /// [`Primary::closer`] has closed the endpoint), so a second launch is never told `ok`
    /// for a link the app will not see.
    pub fn serve(
        mut self,
        on_request: impl Fn(Request) -> Result<(), Refused> + Send + 'static,
    ) -> io::Result<Self> {
        let listener = self.listener.take().expect("serve is called once");
        let closing = self.closing.clone();
        let allowed_uid = self.peer_uid;
        std::thread::Builder::new().name("chairphoto-instance".into()).spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        match peer_uid(&stream) {
                            Ok(uid) if uid == allowed_uid => {}
                            Ok(uid) => {
                                eprintln!("single instance: refused a connection from uid {uid}");
                                continue;
                            }
                            Err(e) => {
                                eprintln!("single instance: refused a connection, no peer credentials: {e}");
                                continue;
                            }
                        }
                        let accept = |request: Request| {
                            if closing.is_closed() {
                                Err(Refused::Closing)
                            } else {
                                on_request(request)
                            }
                        };
                        if let Err(e) = serve_one(stream, accept) {
                            eprintln!("single instance: dropped a request: {e}");
                        }
                    }
                    Err(e) => eprintln!("single instance: accept failed: {e}"),
                }
            }
        })?;
        Ok(self)
    }

    /// The handle that closes this endpoint at quit.
    pub fn closer(&self) -> Closer {
        self.closing.clone()
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
///   anything but `ok` or `closing`).
/// - The lock is held but nothing listens, or the primary answers `closing`: it is still
///   starting (it locks before it binds) or on its way out (it is quitting, or it unlinks
///   the socket before its lock goes, and a child process between `fork` and `exec` can hold
///   the lock a moment longer). Wait a little and look again: either the socket answers or
///   the lock comes free and this launch starts fresh with its link.
pub fn claim(endpoint: &Endpoint, request: &Request, patience: Duration) -> Result<Claim, ClaimError> {
    let uid = current_uid().map_err(ClaimError::Endpoint)?;
    claim_trusting(endpoint, request, patience, uid)
}

/// [`claim`], forwarding only to a primary running as `server_uid` (tests pass another uid to
/// play a stranger's server).
fn claim_trusting(
    endpoint: &Endpoint,
    request: &Request,
    patience: Duration,
    server_uid: u32,
) -> Result<Claim, ClaimError> {
    let socket_len = endpoint.socket.as_os_str().len();
    if socket_len > MAX_SOCKET_PATH {
        return Err(ClaimError::Endpoint(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "socket path {} is {socket_len} bytes; a Unix socket path holds at most {MAX_SOCKET_PATH}",
                endpoint.socket.display()
            ),
        )));
    }
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
        let not_now = match UnixStream::connect(&endpoint.socket) {
            Ok(stream) => {
                // Only to this user's primary: a stranger may hold a lock and serve a socket
                // planted before the directory was ours alone (see the module docs).
                match peer_uid(&stream).map_err(ClaimError::NoAnswer)? {
                    uid if uid == server_uid => {}
                    uid => {
                        return Err(ClaimError::NoAnswer(io::Error::new(
                            ErrorKind::PermissionDenied,
                            format!(
                                "the instance socket {} is served by uid {uid}, not this user",
                                endpoint.socket.display()
                            ),
                        )))
                    }
                }
                match forward(stream, request).map_err(ClaimError::NoAnswer)? {
                    Answer::Accepted => return Ok(Claim::Forwarded),
                    Answer::Refused(Refused::Closing) => io::Error::other("the running instance is quitting"),
                    Answer::Refused(Refused::Busy) => io::Error::other("the running instance is busy"),
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => e,
            Err(e) => return Err(ClaimError::NoAnswer(e)),
        };
        if Instant::now() >= deadline {
            return Err(ClaimError::NoAnswer(not_now));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn become_primary(endpoint: &Endpoint, lock: File) -> Result<Claim, ClaimError> {
    match std::fs::remove_file(&endpoint.socket) {
        Ok(()) => eprintln!("single instance: removed a stale socket (previous instance crashed?)"),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(ClaimError::Endpoint(e)),
    }
    let listener = UnixListener::bind(&endpoint.socket).map_err(ClaimError::Endpoint)?;
    // The socket was created with the umask's mode, inside the 0700 directory (see the module
    // docs, "Who may connect"); narrow it to this user before anyone is served.
    std::fs::set_permissions(&endpoint.socket, std::fs::Permissions::from_mode(0o600))
        .map_err(ClaimError::Endpoint)?;
    Ok(Claim::Primary(Primary {
        listener: Some(listener),
        socket: endpoint.socket.clone(),
        closing: Closer::default(),
        peer_uid: current_uid().map_err(ClaimError::Endpoint)?,
        _lock: lock,
    }))
}

/// What the primary answered.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// `ok`: the app took the request.
    Accepted,
    /// `closing` or `busy`: the app did not take it.
    Refused(Refused),
}

/// Send `request` over a connection to the primary and wait for its answer.
fn forward(mut stream: UnixStream, request: &Request) -> io::Result<Answer> {
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
        "ok" => Ok(Answer::Accepted),
        "closing" => Ok(Answer::Refused(Refused::Closing)),
        "busy" => Ok(Answer::Refused(Refused::Busy)),
        "" => Err(io::Error::new(ErrorKind::UnexpectedEof, "the running instance closed the connection")),
        other => Err(io::Error::other(format!("the running instance answered {other:?}"))),
    }
}

/// Read one request from a peer, offer it to `accept`, then answer: `ok` if it was taken,
/// the [`Refused`] reason if not, `error <why>` if it was malformed. The answer comes after the handoff so
/// that `ok` is never a promise the app cannot keep. Errors are the peer's fault; the caller
/// logs them and moves on.
fn serve_one(stream: UnixStream, accept: impl FnOnce(Request) -> Result<(), Refused>) -> io::Result<()> {
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let reader = Deadline { stream, until: Instant::now() + REQUEST_DEADLINE };
    let (answer, result) = match parse_request(&mut BufReader::new(reader)) {
        Ok(request) => {
            let answer = match accept(request) {
                Ok(()) => "ok\n",
                Err(refused) => refused.answer(),
            };
            (answer.to_string(), Ok(()))
        }
        Err(e) => (format!("error {}\n", e.to_string().replace('\n', " ")), Err(e)),
    };
    let _ = writer.write_all(answer.as_bytes());
    result
}

/// A stream whose reads share one deadline: each read may wait only for what is left of it.
/// A per-read timeout alone would let a peer that sends a byte now and then hold the
/// one-at-a-time server indefinitely.
struct Deadline {
    stream: UnixStream,
    until: Instant,
}

impl Read for Deadline {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(ErrorKind::TimedOut, "request took too long"));
        }
        self.stream.set_read_timeout(Some(left))?;
        self.stream.read(buf)
    }
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

    /// A private directory, removed on drop. Under `$XDG_RUNTIME_DIR` (else `/tmp`), not
    /// `std::env::temp_dir()`: a socket path must fit in `sun_path` (108 bytes), and a long
    /// `TMPDIR` (the suite runs with one on disk) overflows it.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let base = std::env::var_os("XDG_RUNTIME_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp"));
            let dir = base.join(format!("cp-si-{tag}-{}-{nanos}", std::process::id()));
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
            .serve(move |r| tx.send(r).map_err(|_| Refused::Closing))
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
        // What a crash leaves: a socket file whose listener is gone, and no lock holder.
        drop(UnixListener::bind(&endpoint.socket).unwrap());
        // Checked by file type, not by a connect that should fail: another test thread's
        // process spawn can hold a copy of the dropped listener's fd between its fork and
        // exec (CLOEXEC closes it only at exec), and for that moment a connect succeeds. With
        // a thread spawning /bin/true alongside, 11 and 19 of 2000 such connects succeeded.
        use std::os::unix::fs::FileTypeExt as _;
        assert!(std::fs::symlink_metadata(&endpoint.socket).unwrap().file_type().is_socket());

        let (tx, rx) = mpsc::channel();
        let _primary = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE))
            .serve(move |r| tx.send(r).map_err(|_| Refused::Closing))
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
        let first = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE)).serve(|_| Ok(())).unwrap();
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
            serve_one(stream, |r| tx.send(r).map_err(|_| Refused::Closing)).unwrap();
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

    /// A second launch that arrives while the primary is quitting is told `closing`, not
    /// `ok`: its link is not handed to an app that is going away. It waits for the lock and
    /// becomes the next primary with its link.
    #[test]
    fn a_second_launch_during_the_primarys_quit_starts_fresh() {
        let dir = TempDir::new("quitting");
        let endpoint = Endpoint::new(&dir.0, "k");
        let (tx, rx) = mpsc::channel();
        let quitting = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE))
            .serve(move |r| tx.send(r).map_err(|_| Refused::Closing))
            .unwrap();
        quitting.closer().close();
        let departing = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(quitting);
        });
        let _next = primary(claim(&endpoint, &Request { urls: vec![PHOTO.into()] }, CONNECT_PATIENCE));
        departing.join().unwrap();
        assert!(rx.try_recv().is_err(), "the quitting primary must not take the link");
    }

    /// A request the app does not take (its receiver is gone) is not acknowledged: the second
    /// launch never hears `ok`, and gives up after its patience instead of exiting 0.
    #[test]
    fn a_request_the_app_does_not_take_is_not_acknowledged() {
        let dir = TempDir::new("refused");
        let endpoint = Endpoint::new(&dir.0, "k");
        let _primary = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE)).serve(|_| Err(Refused::Closing)).unwrap();
        let result = claim(&endpoint, &Request { urls: vec![PHOTO.into()] }, Duration::from_millis(200));
        assert!(matches!(result, Err(ClaimError::NoAnswer(_))), "expected NoAnswer");
    }

    /// A peer that trickles its request a byte at a time (each byte well inside a per-read
    /// timeout) is cut off at the whole-request deadline, not served for as long as it keeps
    /// trickling.
    #[test]
    fn a_trickling_peer_is_cut_off_at_the_request_deadline() {
        let dir = TempDir::new("trickle");
        let endpoint = Endpoint::new(&dir.0, "k");
        let _primary = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE)).serve(|_| Ok(())).unwrap();
        let stream = UnixStream::connect(&endpoint.socket).unwrap();
        let mut reader = stream.try_clone().unwrap();
        let started = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        let trickle = {
            let (mut stream, stop) = (stream, stop.clone());
            std::thread::spawn(move || {
                // One byte of a line that never ends, every 200 ms, for up to 8 s.
                while !stop.load(Ordering::SeqCst) && started.elapsed() < Duration::from_secs(8) {
                    if stream.write_all(b"x").is_err() {
                        return; // the server hung up
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            })
        };
        let mut answer = String::new();
        let _ = reader.read_to_string(&mut answer);
        let cut_off_after = started.elapsed();
        stop.store(true, Ordering::SeqCst);
        trickle.join().unwrap();
        assert!(answer.starts_with("error "), "{answer:?}");
        assert!(
            cut_off_after < REQUEST_DEADLINE + Duration::from_secs(1),
            "the trickling peer held the server for {cut_off_after:?}"
        );
    }

    /// A primary whose app refuses as busy is not acknowledged either: the second launch
    /// keeps trying for its patience, then gives up saying the instance is busy.
    #[test]
    fn a_busy_primary_is_reported_as_busy() {
        let dir = TempDir::new("busy");
        let endpoint = Endpoint::new(&dir.0, "k");
        let _primary = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE))
            .serve(|_| Err(Refused::Busy))
            .unwrap();
        match claim(&endpoint, &Request { urls: vec![PHOTO.into()] }, Duration::from_millis(200)) {
            Err(ClaimError::NoAnswer(e)) => assert_eq!(e.to_string(), "the running instance is busy"),
            _ => panic!("expected NoAnswer"),
        }
    }

    /// A peer that speaks something else gets an error line and its request is dropped; the
    /// primary keeps serving.
    #[test]
    fn a_malformed_request_is_refused_and_the_primary_keeps_serving() {
        let dir = TempDir::new("malformed");
        let endpoint = Endpoint::new(&dir.0, "k");
        let (tx, rx) = mpsc::channel();
        let _primary = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE))
            .serve(move |r| tx.send(r).map_err(|_| Refused::Closing))
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

    /// A deep `$TMPDIR` does not push the fallback socket past `sun_path`.
    #[test]
    fn the_fallback_dir_stays_short_enough_for_a_socket() {
        assert_eq!(fallback_dir(Path::new("/tmp"), 1000), Path::new("/tmp/chairphoto-1000"));
        let deep = Path::new("/home/someone/.local/share/chairphoto-agent/tmp/imgfix-a9c59/and/deeper/still");
        let dir = fallback_dir(deep, 1000);
        assert_eq!(dir, Path::new("/tmp/chairphoto-1000"));
        let endpoint = Endpoint::new(&dir, &instance_key(Path::new("/x")));
        assert!(endpoint.socket.as_os_str().len() <= MAX_SOCKET_PATH);
    }

    /// A socket path too long for `sun_path` is refused up front, saying why, as an
    /// `Endpoint` error (the app runs without single-instance), not as a bind failure.
    #[test]
    fn a_socket_path_too_long_is_refused_clearly() {
        let dir = TempDir::new("long");
        let deep = dir.0.join("d".repeat(100));
        std::fs::create_dir_all(&deep).unwrap();
        match claim(&Endpoint::new(&deep, "k"), &Request::default(), CONNECT_PATIENCE) {
            Err(ClaimError::Endpoint(e)) => {
                assert!(e.to_string().contains("a Unix socket path holds at most 107"), "{e}")
            }
            _ => panic!("expected an Endpoint error"),
        }
    }

    /// The runtime dir is created 0700; an existing one of ours with group/other bits (0755,
    /// 0777, 0710) is tightened to 0700; someone else's, a symlink, or a file is refused and
    /// left as it was.
    #[test]
    fn the_runtime_dir_is_made_private_or_refused() {
        let dir = TempDir::new("perm");
        let uid = current_uid().unwrap();
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777;
        let private = dir.0.join("private");
        ensure_private_dir(&private, uid).unwrap();
        assert_eq!(mode(&private), 0o700);

        for loose in [0o755, 0o750, 0o710, 0o701] {
            let existing = dir.0.join(format!("loose-{loose:o}"));
            std::fs::create_dir(&existing).unwrap();
            std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(loose)).unwrap();
            ensure_private_dir(&existing, uid).unwrap();
            assert_eq!(mode(&existing), 0o700, "a {loose:o} dir of ours was not tightened");
        }
        // Writable by others: something may already be planted inside, so refuse, unchanged.
        for writable in [0o777, 0o775, 0o730, 0o703] {
            let existing = dir.0.join(format!("writable-{writable:o}"));
            std::fs::create_dir(&existing).unwrap();
            std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(writable)).unwrap();
            assert!(ensure_private_dir(&existing, uid).is_err(), "a {writable:o} dir was accepted");
            assert_eq!(mode(&existing), writable, "a refused {writable:o} dir was changed");
        }

        let link = dir.0.join("link");
        std::os::unix::fs::symlink(&private, &link).unwrap();
        assert!(ensure_private_dir(&link, uid).is_err(), "a symlink is not accepted");
        let loose_target = dir.0.join("loose-target");
        std::fs::create_dir(&loose_target).unwrap();
        std::fs::set_permissions(&loose_target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link_to_loose = dir.0.join("link-to-loose");
        std::os::unix::fs::symlink(&loose_target, &link_to_loose).unwrap();
        assert!(ensure_private_dir(&link_to_loose, uid).is_err());
        assert_eq!(mode(&loose_target), 0o755, "the chmod followed a symlink");

        let file = dir.0.join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(ensure_private_dir(&file, uid).is_err(), "a file is not a directory");

        let theirs = dir.0.join("theirs");
        std::fs::create_dir(&theirs).unwrap();
        std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(ensure_private_dir(&theirs, uid + 1).is_err(), "someone else's directory");
        assert_eq!(mode(&theirs), 0o755, "someone else's directory was changed");
    }

    /// Every refusal of an unsafe directory is recognisable as one (the app stops on it), and
    /// an ordinary I/O failure is not (the app may then run without single-instance).
    #[test]
    fn an_unsafe_runtime_dir_is_told_apart_from_an_unusable_one() {
        let dir = TempDir::new("unsafe");
        let uid = current_uid().unwrap();
        let writable = dir.0.join("writable");
        std::fs::create_dir(&writable).unwrap();
        std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o777)).unwrap();
        let file = dir.0.join("file");
        std::fs::write(&file, b"").unwrap();
        for (path, owner) in [(&writable, uid), (&file, uid), (&dir.0, uid + 1)] {
            let e = ensure_private_dir(path, owner).unwrap_err();
            assert!(is_unsafe_dir(&e), "{} not told apart: {e}", path.display());
            assert!(e.to_string().contains("remove it"), "{e}");
        }
        // Genuinely unreadable (as another user's 0700 dir is to us): open fails with EACCES
        // before any ownership check can run, and that must count as unsafe too.
        let unreadable = dir.0.join("unreadable");
        std::fs::create_dir(&unreadable).unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        // With a permission bypass (root, CAP_DAC_OVERRIDE) mode 000 does not stop the open,
        // so this case cannot be staged (Codex re-check of 272b36c).
        if std::fs::read_dir(&unreadable).is_ok() {
            println!("SKIPPED: an_unsafe_runtime_dir_is_told_apart_from_an_unusable_one (unreadable case) — this process bypasses file permissions, so mode 000 cannot make a directory unopenable");
        } else {
            let e = ensure_private_dir(&unreadable, uid).unwrap_err();
            assert!(is_unsafe_dir(&e), "an unreadable dir not told apart: {e}");
        }
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let missing_parent = dir.0.join("no/such/parent");
        let e = ensure_private_dir(&missing_parent, uid).unwrap_err();
        assert!(!is_unsafe_dir(&e), "an ordinary failure taken for an unsafe dir: {e}");
    }

    /// The primary's socket is 0600, whatever mode the umask gave it at bind.
    #[test]
    fn the_socket_is_private_to_this_user() {
        let dir = TempDir::new("sockmode");
        let endpoint = Endpoint::new(&dir.0, "k");
        let _primary = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE));
        let meta = std::fs::symlink_metadata(&endpoint.socket).unwrap();
        assert_eq!(meta.permissions().mode() & 0o7777, 0o600);
    }

    /// SO_PEERCRED on a connection of ours names this user.
    #[test]
    fn peer_credentials_name_this_user() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&a).unwrap(), current_uid().unwrap());
    }

    /// A connection from any other uid is dropped unanswered, and its request never reaches
    /// the app. Another user cannot be had without root, so this primary serves only a uid
    /// that is not ours, and our own connection plays the stranger.
    #[test]
    fn a_peer_with_another_uid_is_dropped_unanswered() {
        let dir = TempDir::new("peercred");
        let endpoint = Endpoint::new(&dir.0, "k");
        let (tx, rx) = mpsc::channel();
        let mut stranger_only = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE));
        stranger_only.peer_uid = current_uid().unwrap() + 1;
        let _primary = stranger_only.serve(move |r| tx.send(r).map_err(|_| Refused::Closing)).unwrap();

        let mut stream = UnixStream::connect(&endpoint.socket).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = stream.write_all(format!("{HELLO}\nurl {PHOTO}\nend\n").as_bytes());
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        assert_eq!(answer, "", "a stranger was answered");
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "a stranger's link reached the app");
    }

    /// A second launch forwards only to a primary running as this user: facing someone
    /// else's server (played by expecting uid+1 from ours), it sends nothing and fails
    /// instead of exiting as if the link had been taken (Codex re-check of b6a788c).
    #[test]
    fn a_second_launch_never_forwards_to_another_users_server() {
        let dir = TempDir::new("srvcred");
        let endpoint = Endpoint::new(&dir.0, "k");
        let (tx, rx) = mpsc::channel();
        let _primary = primary(claim(&endpoint, &Request::default(), CONNECT_PATIENCE))
            .serve(move |r| tx.send(r).map_err(|_| Refused::Closing))
            .unwrap();
        let request = Request { urls: vec![PHOTO.into()], ..Request::default() };
        let stranger = current_uid().unwrap() + 1;
        match claim_trusting(&endpoint, &request, CONNECT_PATIENCE, stranger) {
            Err(ClaimError::NoAnswer(e)) => assert_eq!(e.kind(), ErrorKind::PermissionDenied, "{e}"),
            Ok(Claim::Forwarded) => panic!("forwarded to another user's server"),
            other => panic!("unexpected: {:?}", other.map(|_| ())),
        }
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "the link reached that server");
    }
}
