//! Test-only LocalSend receiver: a scripted HTTP/1.1 stub on an ephemeral **loopback** port
//! that speaks just enough of the v2 protocol (`prepare-upload`, `upload`, `cancel`) to drive
//! the send path end to end — never the LAN, never the well-known port. It records every
//! request it receives.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// How the stub behaves.
#[derive(Default, Clone)]
pub(crate) struct Script {
    /// The PIN the receiver demands (`401` on prepare-upload without it).
    pub pin: Option<String>,
    /// Never answer the upload of this file id or any later one (a long transfer that a
    /// Cancel must interrupt); `Some(0)` holds every upload.
    pub hold_from: Option<usize>,
    /// Answer `500` to the upload of this file id.
    pub reject: Option<usize>,
}

/// One request the stub received.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    /// Path and query, e.g. `/api/localsend/v2/upload?sessionId=…`.
    pub target: String,
    pub body: Vec<u8>,
}

pub(crate) struct Receiver {
    pub port: u16,
    log: Arc<Mutex<Vec<Request>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Receiver {
    /// Bind `127.0.0.1:0` and serve on the core runtime.
    pub fn start(script: Script) -> Receiver {
        let rt = crate::app::runtime();
        let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).expect("bind a loopback port");
        let port = listener.local_addr().unwrap().port();
        let log: Arc<Mutex<Vec<Request>>> = Arc::default();
        let task = {
            let log = log.clone();
            rt.spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(serve(stream, script.clone(), log.clone()));
                }
            })
        };
        Receiver { port, log, task }
    }

    pub fn log(&self) -> Vec<Request> {
        self.log.lock().unwrap().clone()
    }

    /// The `fileName`s of the last prepare-upload, in file-id order.
    pub fn file_names(&self) -> Vec<String> {
        let log = self.log();
        let Some(prepare) = log.iter().rev().find(|r| r.target.contains("/prepare-upload")) else { return Vec::new() };
        let v: serde_json::Value = serde_json::from_slice(&prepare.body).unwrap();
        let files = v["files"].as_object().unwrap();
        let by_id: BTreeMap<usize, String> = files
            .iter()
            .map(|(id, f)| (id.parse().unwrap(), f["fileName"].as_str().unwrap().to_string()))
            .collect();
        by_id.into_values().collect()
    }

    /// Wait (up to 10 s) until the request log satisfies `done`.
    pub fn wait_for(&self, done: impl Fn(&[Request]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(&self.log()) {
            assert!(Instant::now() < deadline, "the receiver never saw what the test waits for: {:?}", self.log());
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(mut stream: TcpStream, script: Script, log: Arc<Mutex<Vec<Request>>>) {
    loop {
        let Some(request) = read_request(&mut stream).await else { return };
        log.lock().unwrap().push(request.clone());
        let target = request.target.as_str();
        let (status, body) = if target.contains("/prepare-upload") {
            let pin_ok = match &script.pin {
                None => true,
                Some(pin) => target.ends_with(&format!("pin={pin}")),
            };
            if pin_ok {
                let v: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
                let tokens: serde_json::Map<String, serde_json::Value> = v["files"]
                    .as_object()
                    .map(|f| f.keys().map(|id| (id.clone(), format!("token-{id}").into())).collect())
                    .unwrap_or_default();
                (200, serde_json::json!({ "sessionId": "session-1", "files": tokens }).to_string())
            } else {
                (401, String::new())
            }
        } else if target.contains("/upload?") {
            let file_id = file_id(target);
            if script.hold_from.is_some_and(|from| file_id.is_some_and(|id| id >= from)) {
                tokio::time::sleep(Duration::from_secs(30)).await;
                return;
            }
            if script.reject.is_some() && script.reject == file_id {
                (500, "disk full".to_string())
            } else {
                (200, String::new())
            }
        } else if target.contains("/cancel?") {
            (200, String::new())
        } else {
            (404, String::new())
        };
        let response = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// The `fileId` query value of an upload target.
pub(crate) fn file_id(target: &str) -> Option<usize> {
    target.split(['?', '&']).find_map(|kv| kv.strip_prefix("fileId=")).and_then(|v| v.parse().ok())
}

/// Read one request (headers, then a `Content-Length` body). `None` at EOF.
async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let target = head.split_whitespace().nth(1)?.to_string();
    let len: usize = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Some(Request { target, body })
}
