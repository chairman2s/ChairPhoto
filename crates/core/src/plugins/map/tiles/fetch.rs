//! The tile fetcher: the disk cache first, the network only when the cache cannot answer,
//! always as the OSM tile policy asks.
//!
//! | Policy | Here |
//! |---|---|
//! | "a clear, unique User-Agent that names your app" | every request sends [`USER_AGENT`](super::super::USER_AGENT) ([`ReqwestHttp`]) |
//! | honour caching headers, or cache at least 7 days | a tile stays fresh for `max(max-age, 7 days)` ([`expiry`]) |
//! | "use conditional requests" | a stale tile is revalidated with `If-None-Match` / `If-Modified-Since`; a `304` keeps the bytes |
//! | never send `no-cache` by default | no cache-control request header is ever set |
//! | no bulk or background fetching | the fetcher fetches only what it is asked for; the map asks only for visible tiles (`math`) |
//! | modest concurrency | at most [`MAX_CONCURRENT`] requests in flight per fetcher; queued loads that are dropped never reach the network |
//!
//! A fresh cached tile makes no request at all. A stale one whose revalidation fails (offline,
//! a server error) is still shown — re-viewing what was already fetched, not an offline mode.
//!
//! **Consent is the caller's.** The fetcher does not know whether the user allowed the host
//! (decision #118, per host, asked on first open); the app's map never calls it for a host
//! that is not allowed, and passes the other allowed hosts so that a redirect reaches only
//! the source's own host or one of those ([`ReqwestHttp`] follows redirects itself). The
//! HTTP client is behind [`TileHttp`] so tests count requests without a network.

use super::cache::{CachedTile, TileCache, TileMeta, DEFAULT_CAP_BYTES};
use super::math::TileKey;
use super::source::{authority, TileSource};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// A tile stays fresh at least this long after it was fetched or revalidated.
pub const MIN_FRESH_SECS: i64 = 7 * 24 * 3600;
/// Requests in flight per fetcher.
pub const MAX_CONCURRENT: usize = 4;
/// How long one request may take.
pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Redirects one tile request may follow.
pub const MAX_REDIRECTS: usize = 5;

/// One GET, with the validators of a stale cached copy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpRequest {
    pub url: String,
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<String>,
    /// Hosts (as [`authority`] names them) a redirect may lead to besides the request's
    /// own: the other hosts the user allowed. A redirect anywhere else fails the tile
    /// without contacting that host.
    pub redirect_hosts: Vec<String>,
}

/// What came back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub cache_control: Option<String>,
}

pub type HttpFuture = Pin<Box<dyn Future<Output = Result<HttpResponse, String>> + Send>>;

/// The network seam: [`ReqwestHttp`] in the app, a counting fake in tests.
pub trait TileHttp: Send + Sync + 'static {
    fn get(&self, request: HttpRequest) -> HttpFuture;
}

/// The real client: one `reqwest::Client` (connection reuse) with ChairPhoto's User-Agent.
///
/// **Redirects are followed by hand**, never by reqwest: consent is per host (decision
/// #118), and reqwest's default policy would follow a tile server's redirect to any host.
/// Each `Location` is checked before it is requested — the request's own host or one of
/// [`HttpRequest::redirect_hosts`], `http`/`https` only, at most [`MAX_REDIRECTS`] —
/// and anything else fails the tile.
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl ReqwestHttp {
    pub fn new() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .user_agent(super::super::USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("tiles: could not build the HTTP client: {e}"))?;
        Ok(ReqwestHttp { client })
    }
}

/// Where a redirect from `from` to `location` may go, or why not.
fn redirect_target(from: &reqwest::Url, location: &str, origin: &str, allowed: &[String]) -> Result<reqwest::Url, String> {
    let next = from.join(location).map_err(|e| format!("the tile server redirected to an invalid URL: {e}"))?;
    if !matches!(next.scheme(), "http" | "https") {
        return Err(format!("the tile server redirected to a {} URL", next.scheme()));
    }
    let host = authority(&next);
    if host == origin || allowed.iter().any(|a| a.eq_ignore_ascii_case(&host)) {
        Ok(next)
    } else {
        Err(format!("the tile server redirected to {host}, which you have not allowed"))
    }
}

impl TileHttp for ReqwestHttp {
    fn get(&self, request: HttpRequest) -> HttpFuture {
        let client = self.client.clone();
        Box::pin(async move {
            use reqwest::header;
            let mut url = reqwest::Url::parse(&request.url).map_err(|e| format!("bad tile URL: {e}"))?;
            let origin = authority(&url);
            let mut redirects = 0;
            let response = loop {
                let mut builder = client.get(url.clone());
                if let Some(etag) = &request.if_none_match {
                    builder = builder.header(header::IF_NONE_MATCH, etag);
                }
                if let Some(date) = &request.if_modified_since {
                    builder = builder.header(header::IF_MODIFIED_SINCE, date);
                }
                let response = builder.send().await.map_err(|e| format!("tile request failed: {e}"))?;
                let location = response.headers().get(header::LOCATION).and_then(|v| v.to_str().ok());
                let redirect = matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308);
                match (redirect, location) {
                    (true, Some(location)) => {
                        redirects += 1;
                        if redirects > MAX_REDIRECTS {
                            return Err("the tile server redirected too many times".into());
                        }
                        url = redirect_target(&url, location, &origin, &request.redirect_hosts)?;
                    }
                    _ => break response,
                }
            };
            let text = |name: header::HeaderName| {
                response.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
            };
            let (etag, last_modified, cache_control) =
                (text(header::ETAG), text(header::LAST_MODIFIED), text(header::CACHE_CONTROL));
            let status = response.status().as_u16();
            let body = response.bytes().await.map_err(|e| format!("tile download failed: {e}"))?.to_vec();
            Ok(HttpResponse { status, body, etag, last_modified, cache_control })
        })
    }
}

/// When a tile fetched (or revalidated) at `now` goes stale: the server's `max-age`, but
/// never sooner than [`MIN_FRESH_SECS`].
pub fn expiry(now: i64, cache_control: Option<&str>) -> i64 {
    let max_age = cache_control
        .into_iter()
        .flat_map(|cc| cc.split(','))
        .filter_map(|d| d.trim().strip_prefix("max-age="))
        .find_map(|v| v.trim_matches('"').parse::<i64>().ok())
        .unwrap_or(0);
    now + max_age.max(MIN_FRESH_SECS)
}

/// Where a loaded tile's bytes came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A fresh cache entry: no request.
    Cache,
    /// A stale cache entry the server confirmed (`304`).
    Revalidated,
    /// Downloaded now.
    Network,
    /// A stale cache entry shown because revalidation failed.
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedTile {
    pub bytes: Vec<u8>,
    pub origin: Origin,
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Cache + client + concurrency limit. Cheap to share (`Arc` inside); one per app.
#[derive(Clone)]
pub struct TileFetcher {
    http: Arc<dyn TileHttp>,
    cache: Arc<TileCache>,
    permits: Arc<tokio::sync::Semaphore>,
    clock: fn() -> i64,
}

impl TileFetcher {
    pub fn new(http: Arc<dyn TileHttp>, cache: Arc<TileCache>, max_concurrent: usize) -> Self {
        TileFetcher { http, cache, permits: Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1))), clock: unix_now }
    }

    /// The app's fetcher: [`ReqwestHttp`], the default cache root and cap.
    pub fn production() -> Result<Self, String> {
        let cache = TileCache::new(super::cache::default_root(), DEFAULT_CAP_BYTES);
        Ok(TileFetcher::new(Arc::new(ReqwestHttp::new()?), Arc::new(cache), MAX_CONCURRENT))
    }

    /// Replace the clock (unix seconds), for tests.
    pub fn with_clock(mut self, clock: fn() -> i64) -> Self {
        self.clock = clock;
        self
    }

    pub fn cache(&self) -> &Arc<TileCache> {
        &self.cache
    }

    /// One tile: a fresh cache entry, else the network (conditionally when a stale entry
    /// exists), else the stale entry. Dropping the future before it reaches the network
    /// (while it waits for a permit) sends nothing.
    pub async fn load(&self, source: &TileSource, key: TileKey) -> Result<LoadedTile, String> {
        self.load_allowing(source, key, &[]).await
    }

    /// [`load`](Self::load), where a redirect may also lead to `redirect_hosts` (the other
    /// hosts the user allowed); with none, only to the source's own host.
    pub async fn load_allowing(&self, source: &TileSource, key: TileKey, redirect_hosts: &[String]) -> Result<LoadedTile, String> {
        let cached = {
            let (cache, source) = (self.cache.clone(), source.clone());
            tokio::task::spawn_blocking(move || cache.get(&source, key)).await.map_err(|e| e.to_string())?
        };
        let now = (self.clock)();
        if let Some(c) = &cached {
            if c.is_fresh(now) {
                return Ok(LoadedTile { bytes: c.bytes.clone(), origin: Origin::Cache });
            }
        }
        let response = {
            let _permit = self.permits.acquire().await.map_err(|e| e.to_string())?;
            let request = HttpRequest {
                url: source.url(key),
                if_none_match: cached.as_ref().and_then(|c| c.meta.etag.clone()),
                if_modified_since: cached.as_ref().and_then(|c| c.meta.last_modified.clone()),
                redirect_hosts: redirect_hosts.to_vec(),
            };
            self.http.get(request).await
        };
        let now = (self.clock)();
        match (response, cached) {
            (Ok(r), Some(c)) if r.status == 304 => {
                let meta = TileMeta {
                    etag: r.etag.or(c.meta.etag),
                    last_modified: r.last_modified.or(c.meta.last_modified),
                    expires: expiry(now, r.cache_control.as_deref()),
                };
                self.store(source, key, None, meta).await;
                Ok(LoadedTile { bytes: c.bytes, origin: Origin::Revalidated })
            }
            (Ok(r), _) if (200..300).contains(&r.status) && !r.body.is_empty() => {
                let meta = TileMeta {
                    etag: r.etag,
                    last_modified: r.last_modified,
                    expires: expiry(now, r.cache_control.as_deref()),
                };
                self.store(source, key, Some(r.body.clone()), meta).await;
                Ok(LoadedTile { bytes: r.body, origin: Origin::Network })
            }
            (Ok(_) | Err(_), Some(CachedTile { bytes, .. })) => Ok(LoadedTile { bytes, origin: Origin::Stale }),
            (Ok(r), None) => Err(format!("the tile server answered HTTP {}", r.status)),
            (Err(e), None) => Err(e),
        }
    }

    /// Write to the cache off the async worker; a failed write costs a refetch, not the tile.
    async fn store(&self, source: &TileSource, key: TileKey, bytes: Option<Vec<u8>>, meta: TileMeta) {
        let (cache, source) = (self.cache.clone(), source.clone());
        let written = tokio::task::spawn_blocking(move || match bytes {
            Some(bytes) => cache.put(&source, key, &bytes, &meta),
            None => cache.put_meta(&source, key, &meta),
        })
        .await;
        if let Ok(Err(e)) = written {
            eprintln!("tiles: could not cache tile {key:?}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::cache::tests::TempDir;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    const NOW: i64 = 1_800_000_000;
    const K: TileKey = TileKey { z: 4, x: 8, y: 5 };

    /// Answers every request with the next canned response (or the last one again), and
    /// records the requests.
    #[derive(Default)]
    struct Fake {
        requests: Mutex<Vec<HttpRequest>>,
        responses: Mutex<Vec<Result<HttpResponse, String>>>,
    }

    impl Fake {
        fn answering(responses: Vec<Result<HttpResponse, String>>) -> Arc<Self> {
            Arc::new(Fake { requests: Mutex::default(), responses: Mutex::new(responses) })
        }
        fn requests(&self) -> Vec<HttpRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl TileHttp for Fake {
        fn get(&self, request: HttpRequest) -> HttpFuture {
            self.requests.lock().unwrap().push(request);
            let mut r = self.responses.lock().unwrap();
            let next = if r.len() > 1 { r.remove(0) } else { r[0].clone() };
            Box::pin(async move { next })
        }
    }

    fn ok(body: &[u8], etag: Option<&str>, cache_control: Option<&str>) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status: 200,
            body: body.to_vec(),
            etag: etag.map(str::to_string),
            last_modified: Some("Mon, 31 Aug 2026 00:00:00 GMT".into()),
            cache_control: cache_control.map(str::to_string),
        })
    }

    fn fetcher(http: Arc<dyn TileHttp>, dir: &TempDir) -> TileFetcher {
        TileFetcher::new(http, Arc::new(TileCache::new(&dir.0, DEFAULT_CAP_BYTES)), MAX_CONCURRENT).with_clock(|| NOW)
    }

    #[test]
    fn freshness_is_max_age_with_a_seven_day_floor() {
        assert_eq!(expiry(0, None), MIN_FRESH_SECS);
        assert_eq!(expiry(0, Some("public, max-age=3600")), MIN_FRESH_SECS);
        assert_eq!(expiry(10, Some("max-age=1209600, stale-while-revalidate=60")), 10 + 1_209_600);
        assert_eq!(expiry(0, Some("no-cache")), MIN_FRESH_SECS);
    }

    /// A miss downloads once and caches; the next load of the same tile makes no request.
    #[tokio::test]
    async fn a_fresh_cached_tile_makes_no_request() {
        let dir = TempDir::new("fresh");
        let http = Fake::answering(vec![ok(b"tile", Some("\"e1\""), None)]);
        let f = fetcher(http.clone(), &dir);
        let src = TileSource::default();
        let first = f.load(&src, K).await.unwrap();
        assert_eq!(first, LoadedTile { bytes: b"tile".to_vec(), origin: Origin::Network });
        assert_eq!(http.requests(), vec![HttpRequest { url: "https://tile.openstreetmap.org/4/8/5.png".into(), ..Default::default() }]);
        let again = f.load(&src, K).await.unwrap();
        assert_eq!(again.origin, Origin::Cache);
        assert_eq!(http.requests().len(), 1, "served from the disk cache");
        assert_eq!(f.cache().get(&src, K).unwrap().meta.expires, NOW + MIN_FRESH_SECS);
    }

    /// A stale tile is revalidated with its validators; `304` keeps the bytes and renews it.
    #[tokio::test]
    async fn a_stale_tile_is_revalidated_conditionally() {
        let dir = TempDir::new("stale");
        let http = Fake::answering(vec![Ok(HttpResponse { status: 304, ..Default::default() })]);
        let f = fetcher(http.clone(), &dir);
        let src = TileSource::default();
        let meta = TileMeta { etag: Some("\"e1\"".into()), last_modified: Some("Mon, 31 Aug 2026 00:00:00 GMT".into()), expires: NOW - 1 };
        f.cache().put(&src, K, b"old", &meta).unwrap();
        let got = f.load(&src, K).await.unwrap();
        assert_eq!(got, LoadedTile { bytes: b"old".to_vec(), origin: Origin::Revalidated });
        let req = &http.requests()[0];
        assert_eq!(req.if_none_match.as_deref(), Some("\"e1\""));
        assert_eq!(req.if_modified_since.as_deref(), Some("Mon, 31 Aug 2026 00:00:00 GMT"));
        let renewed = f.cache().get(&src, K).unwrap().meta;
        assert_eq!((renewed.expires, renewed.etag.as_deref()), (NOW + MIN_FRESH_SECS, Some("\"e1\"")));
    }

    #[tokio::test]
    async fn a_changed_tile_replaces_the_stale_one() {
        let dir = TempDir::new("changed");
        let http = Fake::answering(vec![ok(b"new", Some("\"e2\""), Some("max-age=999999999"))]);
        let f = fetcher(http, &dir);
        let src = TileSource::default();
        f.cache().put(&src, K, b"old", &TileMeta { etag: Some("\"e1\"".into()), expires: NOW - 1, ..Default::default() }).unwrap();
        assert_eq!(f.load(&src, K).await.unwrap(), LoadedTile { bytes: b"new".to_vec(), origin: Origin::Network });
        let c = f.cache().get(&src, K).unwrap();
        assert_eq!((c.bytes.as_slice(), c.meta.expires), (&b"new"[..], NOW + 999_999_999));
    }

    /// Offline or refused: a stale copy is still shown; with none, the error.
    #[tokio::test]
    async fn failures_fall_back_to_a_stale_copy_or_report() {
        let dir = TempDir::new("offline");
        let http = Fake::answering(vec![
            Err("offline".into()),
            Ok(HttpResponse { status: 503, ..Default::default() }),
            Err("offline".into()),
            Ok(HttpResponse { status: 403, ..Default::default() }),
        ]);
        let f = fetcher(http, &dir);
        let src = TileSource::default();
        f.cache().put(&src, K, b"old", &TileMeta { expires: NOW - 1, ..Default::default() }).unwrap();
        assert_eq!(f.load(&src, K).await.unwrap().origin, Origin::Stale);
        assert_eq!(f.load(&src, K).await.unwrap().origin, Origin::Stale);
        let other = TileKey { z: 4, x: 9, y: 5 };
        assert_eq!(f.load(&src, other).await.unwrap_err(), "offline");
        assert_eq!(f.load(&src, other).await.unwrap_err(), "the tile server answered HTTP 403");
    }

    /// Holds every request until released, tracking how many are in flight at once.
    struct GateState {
        open: tokio::sync::Semaphore,
        now: AtomicUsize,
        max: AtomicUsize,
        calls: AtomicUsize,
        urls: Mutex<Vec<String>>,
    }

    struct Gate(Arc<GateState>);

    impl TileHttp for Gate {
        fn get(&self, request: HttpRequest) -> HttpFuture {
            let s = self.0.clone();
            s.urls.lock().unwrap().push(request.url);
            Box::pin(async move {
                s.calls.fetch_add(1, Ordering::SeqCst);
                let n = s.now.fetch_add(1, Ordering::SeqCst) + 1;
                s.max.fetch_max(n, Ordering::SeqCst);
                let _open = s.open.acquire().await.unwrap();
                s.now.fetch_sub(1, Ordering::SeqCst);
                Ok(HttpResponse { status: 200, body: b"t".to_vec(), ..Default::default() })
            })
        }
    }

    /// At most `MAX_CONCURRENT` requests are in flight; queued loads that are cancelled
    /// (the map scrolled past them) never reach the network.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn requests_are_capped_and_cancelled_queued_loads_send_nothing() {
        let dir = TempDir::new("cap");
        let gate = Arc::new(GateState {
            open: tokio::sync::Semaphore::new(0),
            now: AtomicUsize::new(0),
            max: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            urls: Mutex::default(),
        });
        let f = fetcher(Arc::new(Gate(gate.clone())), &dir);
        let src = TileSource::default();
        let mut tasks = Vec::new();
        for x in 0..10 {
            let (f, src) = (f.clone(), src.clone());
            let key = TileKey { z: 5, x, y: 1 };
            tasks.push((src.url(key), tokio::spawn(async move { f.load(&src, key).await })));
        }
        for _ in 0..100 {
            if gate.calls.load(Ordering::SeqCst) >= MAX_CONCURRENT {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(gate.calls.load(Ordering::SeqCst), MAX_CONCURRENT, "the rest wait for a permit");
        // Cancel the loads still waiting for a permit (whichever four got one first, in
        // whatever order the cache reads finished).
        let in_flight = gate.urls.lock().unwrap().clone();
        let (running, waiting): (Vec<_>, Vec<_>) = tasks.into_iter().partition(|(url, _)| in_flight.contains(url));
        for (_, t) in &waiting {
            t.abort();
        }
        gate.open.add_permits(100);
        assert_eq!(running.len(), MAX_CONCURRENT);
        for (_, t) in running {
            t.await.unwrap().unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(gate.calls.load(Ordering::SeqCst), MAX_CONCURRENT, "aborted loads sent nothing");
        assert_eq!(gate.max.load(Ordering::SeqCst), MAX_CONCURRENT);
    }

    /// A loopback HTTP server answering each request with `answer(path)`; counts requests.
    async fn loopback(
        answer: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let hits = Arc::new(AtomicUsize::new(0));
        let (answer, count) = (Arc::new(answer), hits.clone());
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&request).to_string();
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_string();
                let _ = stream.write_all(answer(&path).as_bytes()).await;
            }
        });
        (host, hits, server)
    }

    fn redirect_to(location: String) -> String {
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    }

    /// Review #119: reqwest's default policy followed a tile server's redirect to any host,
    /// though consent is per host. A redirect to a host the user has not allowed now fails
    /// the tile **without contacting that host**; one to the same host, or to another host
    /// the user allowed, is followed.
    #[tokio::test]
    async fn redirects_reach_only_the_same_or_an_allowed_host() {
        let (elsewhere, elsewhere_hits, s1) = loopback(|_| {
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nelse".to_string()
        })
        .await;
        let target = elsewhere.clone();
        let (origin, _, s2) = loopback(move |path| match path {
            "/same" => redirect_to("/tile".into()),
            "/tile" => "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nsame".to_string(),
            _ => redirect_to(format!("http://{target}/tile")),
        })
        .await;
        let http = ReqwestHttp::new().unwrap();
        let get = |path: &str, allowed: Vec<String>| {
            http.get(HttpRequest { url: format!("http://{origin}{path}"), redirect_hosts: allowed, ..Default::default() })
        };

        let err = get("/away", Vec::new()).await.unwrap_err();
        assert!(err.contains(&format!("redirected to {elsewhere}, which you have not allowed")), "{err}");
        assert_eq!(elsewhere_hits.load(Ordering::SeqCst), 0, "the unconsented host was contacted");

        let same = get("/same", Vec::new()).await.unwrap();
        assert_eq!((same.status, same.body.as_slice()), (200, &b"same"[..]));

        let allowed = get("/away", vec![elsewhere.clone()]).await.unwrap();
        assert_eq!((allowed.status, allowed.body.as_slice()), (200, &b"else"[..]));
        assert_eq!(elsewhere_hits.load(Ordering::SeqCst), 1);

        let to_file = redirect_target(&reqwest::Url::parse("http://h.org/a").unwrap(), "file:///etc/passwd", "h.org", &[]);
        assert!(to_file.unwrap_err().contains("file URL"));
        s1.abort();
        s2.abort();
    }

    /// The real client against a loopback server: the ChairPhoto UA, the validators, and no
    /// `no-cache` request header.
    #[tokio::test]
    async fn the_real_client_identifies_itself_and_revalidates_conditionally() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
            }
            stream.write_all(b"HTTP/1.1 304 Not Modified\r\nETag: \"e9\"\r\nConnection: close\r\n\r\n").await.unwrap();
            String::from_utf8_lossy(&request).to_lowercase()
        });
        let http = ReqwestHttp::new().unwrap();
        let src = TileSource::parse(&format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png")).unwrap();
        let response = http
            .get(HttpRequest {
                url: src.url(K),
                if_none_match: Some("\"e1\"".into()),
                if_modified_since: Some("Mon, 31 Aug 2026 00:00:00 GMT".into()),
                redirect_hosts: Vec::new(),
            })
            .await
            .unwrap();
        assert_eq!((response.status, response.etag.as_deref()), (304, Some("\"e9\"")));
        let request = server.await.unwrap();
        assert!(request.starts_with("get /4/8/5.png http/1.1\r\n"), "{request}");
        assert!(request.contains(&format!("user-agent: {}", super::super::super::USER_AGENT.to_lowercase())), "{request}");
        assert!(request.contains("if-none-match: \"e1\""), "{request}");
        assert!(request.contains("if-modified-since: mon, 31 aug 2026 00:00:00 gmt"), "{request}");
        assert!(!request.contains("no-cache") && !request.contains("cache-control"), "{request}");
    }
}
