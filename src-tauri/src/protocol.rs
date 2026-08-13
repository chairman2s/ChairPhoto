//! Custom URI scheme protocols `thumb://` and `preview://` for serving images
//! directly to the webview as native HTTP responses.
//!
//! This is far faster than returning images as base64 over the IPC channel: there
//! is no JSON serialization, the webview decodes a normal JPEG response, and it
//! caches results by URL so an already-seen image is redisplayed instantly. The
//! frontend builds URLs with `convertFileSrc(String(photoId), "thumb" | "preview")`.
//!
//! Generation is the same cached pipeline as the commands; the handler just maps a
//! photo id to its file and renders. Work happens on a worker thread so the webview
//! networking thread is never blocked.

use crate::catalog::ResolveMode;
use crate::commands::AppState;
use crate::image_pool::{ImagePool, JobKey};
use crate::thumbnails::{preview_bytes, thumbnail_bytes, zoom_bytes};
use tauri::http::{Request, Response};
use tauri::{Manager, Runtime, UriSchemeContext, UriSchemeResponder};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ImageKind {
    Thumb,
    Preview,
    Zoom,
}

/// Handle one `thumb://`/`preview://` request.
///
/// Parses the photo id, wraps the [`UriSchemeResponder`] into a [`crate::image_pool::Respond`]
/// closure, and delegates to the bounded LIFO pool.  If the pool is not yet
/// managed (shouldn't happen post-setup), falls back to a one-off thread.
pub fn handle_image_request<R: Runtime>(
    kind: ImageKind,
    ctx: UriSchemeContext<'_, R>,
    request: Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let app = ctx.app_handle().clone();

    let id = match photo_id_from_uri(&request) {
        Ok(id) => id,
        Err(_) => {
            responder.respond(
                Response::builder()
                    .status(404)
                    .body(Vec::<u8>::new())
                    .unwrap(),
            );
            return;
        }
    };

    let key: JobKey = (id, kind);

    // Build the one-shot HTTP-response callback.
    let respond: crate::image_pool::Respond = Box::new(move |result| {
        let response = match result {
            Ok(bytes) => Response::builder()
                .status(200)
                .header("Content-Type", "image/jpeg")
                // Previews are immutable for a given file version; let the webview
                // cache them so re-viewing is instant.
                .header("Cache-Control", "max-age=86400")
                .body(bytes)
                .unwrap(),
            Err(msg) => Response::builder()
                .status(404)
                .header("Content-Type", "text/plain")
                .body(msg.into_bytes())
                .unwrap(),
        };
        responder.respond(response);
    });

    // Use the managed pool when available (normal path), else fall back to a
    // one-off thread (belt-and-braces: handles the narrow window during setup).
    if let Some(pool) = app.try_state::<std::sync::Arc<ImagePool>>() {
        pool.submit(key, respond);
    } else {
        // The responder must ALWAYS be answered — a dropped UriSchemeResponder hangs the
        // webview request forever. If the OS refuses the thread, answer with the error
        // instead of dropping the closure (take-once so only one side responds).
        let respond = std::sync::Arc::new(std::sync::Mutex::new(Some(respond)));
        let respond2 = std::sync::Arc::clone(&respond);
        let spawned = std::thread::Builder::new()
            .name("image-fallback".into())
            .spawn(move || {
                let result = render_bytes(&app, key);
                if let Some(r) = respond2.lock().ok().and_then(|mut g| g.take()) {
                    r(result);
                }
            });
        if spawned.is_err() {
            if let Some(r) = respond.lock().ok().and_then(|mut g| g.take()) {
                r(Err("failed to spawn image render thread".into()));
            }
        }
    }
}

/// Render one image and return its JPEG bytes, or an error string.
///
/// This is the runner function injected into [`ImagePool`].  It is `pub` so
/// that `lib.rs` can reference it when building the pool runner closure.
pub fn render_bytes<R: Runtime>(
    app: &tauri::AppHandle<R>,
    key: JobKey,
) -> Result<Vec<u8>, String> {
    let (id, kind) = key;
    let state = app.state::<AppState>();
    // Gather the path CANDIDATES (pure SQL) and the rotation under a brief lock, then
    // stat them OFF the lock via `pick_existing` so a slow/offline NAS can't serialize
    // the whole app. `pick_existing` still returns the best available copy (local cache
    // > primary > backup); the reachability cache only reorders the stats.
    let (candidates, rotation) = {
        let guard = state.catalog.lock().map_err(|e| e.to_string())?;
        let catalog = guard.as_ref().ok_or("no catalog open")?;
        let candidates = catalog.photo_path_candidates(id).map_err(|e| e.to_string())?;
        let rotation = catalog.photo_rotation(id).unwrap_or(0);
        (candidates, rotation)
    };
    // A thumbnail has a persistent fallback below, so it resolves in FastDisplay: a
    // cached-unreachable volume is never statted and the grid falls back at once. Preview
    // and zoom have no fallback — they need the original, so they keep strict checking.
    let mode = match kind {
        ImageKind::Thumb => ResolveMode::FastDisplay,
        ImageKind::Preview | ImageKind::Zoom => ResolveMode::OriginalRequired,
    };
    let resolved = crate::volume_health::pick_existing(&candidates, &state.volume_health, mode);
    match resolved {
        Some(absolute) => match kind {
            ImageKind::Thumb => {
                // Apply the user rotation on top of the file's baked EXIF orientation, then
                // keep the rotated id-keyed copy so the photo stays browsable (correctly
                // oriented) after it's offloaded and the NAS goes offline.
                let bytes = crate::thumbnails::rotate_jpeg(thumbnail_bytes(&absolute)?, rotation)?;
                crate::thumbnails::save_persistent_thumb(id, &bytes);
                Ok(bytes)
            }
            ImageKind::Preview => crate::thumbnails::rotate_jpeg(preview_bytes(&absolute)?, rotation),
            ImageKind::Zoom => crate::thumbnails::rotate_jpeg(zoom_bytes(&absolute)?, rotation),
        },
        // Original unreachable (e.g. offloaded + NAS unmounted): fall back to the kept
        // thumbnail so the grid still shows the photo. Preview/zoom need the original.
        None => {
            let e = format!("no reachable copy of photo {id}");
            match kind {
                ImageKind::Thumb => crate::thumbnails::read_persistent_thumb(id).ok_or(e),
                _ => Err(e),
            }
        }
    }
}

// --- video: a tiny localhost HTTP server -----------------------------------
// WebKitGTK plays <video> via GStreamer, which does NOT go through WebKit's custom URI
// scheme handlers (unlike <img>), so thumb://-style protocols can't feed it. GStreamer's
// http source CAN fetch a real localhost URL, so we serve catalog videos over
// http://127.0.0.1:<port>/<photo_id> with byte-range support (streamed, constant memory).

use std::net::{TcpListener, TcpStream};
use std::sync::OnceLock;

static VIDEO_PORT: OnceLock<u16> = OnceLock::new();

/// The localhost port the video server is listening on (0 until started).
pub fn video_server_port() -> u16 {
    VIDEO_PORT.get().copied().unwrap_or(0)
}

/// Start the loopback video server and return its port. Binds an ephemeral port on
/// 127.0.0.1 and serves each connection on its own thread.
pub fn start_video_server<R: Runtime>(app: tauri::AppHandle<R>) -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let _ = VIDEO_PORT.set(port);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let app = app.clone();
            std::thread::spawn(move || {
                let _ = serve_video(stream, app);
            });
        }
    });
    Ok(port)
}

fn write_simple(stream: &mut TcpStream, status: &str, extra: &str) -> std::io::Result<()> {
    use std::io::Write;
    write!(
        stream,
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: 0\r\n{extra}\r\n"
    )
}

fn serve_video<R: Runtime>(mut stream: TcpStream, app: tauri::AppHandle<R>) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?; // "GET /<id> HTTP/1.1"
    let id: i64 = match line
        .split_whitespace()
        .nth(1)
        .map(|p| p.trim_start_matches('/').split('?').next().unwrap_or(""))
        .and_then(|s| s.parse().ok())
    {
        Some(v) => v,
        None => return write_simple(&mut stream, "400 Bad Request", ""),
    };

    // Read headers; capture Range (case-insensitive).
    let mut range_hdr: Option<String> = None;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("range") {
                range_hdr = Some(v.trim().to_string());
            }
        }
    }

    // Resolve the photo's file: gather candidates under a brief lock (pure SQL), then
    // stat them OFF the lock so a slow/offline NAS can't serialize the app.
    let state = app.state::<AppState>();
    let candidates = {
        let guard = match state.catalog.lock() {
            Ok(g) => g,
            Err(_) => return write_simple(&mut stream, "500 Internal Server Error", ""),
        };
        match guard.as_ref().and_then(|c| c.photo_path_candidates(id).ok()) {
            Some(cands) => cands,
            None => return write_simple(&mut stream, "404 Not Found", ""),
        }
    };
    // Video playback streams the original file itself — there is no cached proxy to fall
    // back to, so this is OriginalRequired: a stale "unreachable" flag must not turn a
    // playable file into a 404.
    let path = crate::volume_health::pick_existing(
        &candidates,
        &state.volume_health,
        ResolveMode::OriginalRequired,
    );
    let Some(path) = path else {
        return write_simple(&mut stream, "404 Not Found", "");
    };
    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return write_simple(&mut stream, "404 Not Found", ""),
    };
    let size = file.metadata()?.len();
    let mime = video_mime(&path);

    let (start, end, status) = match range_hdr.as_deref().and_then(parse_range) {
        Some((s, _)) if s >= size => {
            return write_simple(&mut stream, "416 Range Not Satisfiable", &format!("Content-Range: bytes */{size}\r\n"));
        }
        Some((s, e)) => (s, e.unwrap_or(size.saturating_sub(1)).min(size.saturating_sub(1)), "206 Partial Content"),
        None => (0, size.saturating_sub(1), "200 OK"),
    };
    let len = end + 1 - start;

    let mut head = format!("HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nAccept-Ranges: bytes\r\nContent-Length: {len}\r\nCache-Control: no-store\r\nConnection: close\r\n");
    if status.starts_with("206") {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{size}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;

    // Stream the requested range in 64 KB chunks (constant memory, any file size).
    file.seek(SeekFrom::Start(start))?;
    let mut remaining = len;
    let mut buf = [0u8; 64 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        stream.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}

/// Parse a single-range `Range: bytes=START-[END]` header.
fn parse_range(h: &str) -> Option<(u64, Option<u64>)> {
    let spec = h.trim().strip_prefix("bytes=")?;
    let (s, e) = spec.split_once('-')?;
    let start = s.trim().parse::<u64>().ok()?;
    let end = if e.trim().is_empty() {
        None
    } else {
        Some(e.trim().parse::<u64>().ok()?)
    };
    Some((start, end))
}

fn video_mime(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("mp4") | Some("m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("avi") => "video/x-msvideo",
        Some("webm") => "video/webm",
        Some("mkv") => "video/x-matroska",
        _ => "application/octet-stream",
    }
}

/// The photo id is carried in the URL path (e.g. `thumb://localhost/123`), with a
/// fallback to the host segment for platforms that map it there.
fn photo_id_from_uri<T>(request: &Request<T>) -> Result<i64, String> {
    let uri = request.uri();
    let from_path = uri.path().trim_matches('/');
    if let Ok(id) = from_path.parse::<i64>() {
        return Ok(id);
    }
    if let Some(host) = uri.host() {
        if let Ok(id) = host.parse::<i64>() {
            return Ok(id);
        }
    }
    Err(format!("could not parse photo id from {uri}"))
}
