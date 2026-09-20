//! Hand-rolled mock OpenAI-compatible server (ADR §9): tokio TCP only — no
//! heavyweight HTTP servers or mock frameworks in the dependency tree.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What the server does for a single request.
pub enum Step {
    /// 200 `text/event-stream` of JSON payloads; `data: [DONE]` appended.
    Sse(Vec<serde_json::Value>),
    /// 200 `text/event-stream` writing raw byte fragments in order with short
    /// pauses — forces separate TCP reads (chunk-boundary robustness tests).
    Fragments(Vec<Vec<u8>>),
    /// A plain HTTP status with a body.
    Status(u16, String),
    /// Accepts the connection and then sends nothing at all, until the
    /// client gives up. Models a server that is reachable but busy — on a
    /// single-model backend, a request queued behind the current generation
    /// (ADR §6.8).
    Stall,
    /// 200 JSON body (non-streaming completions).
    Json(serde_json::Value),
}

/// A scripted HTTP server on an OS-chosen loopback port.
pub struct MockServer {
    /// The loopback port the server listens on.
    pub port: u16,
    hits: Arc<AtomicU32>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockServer {
    /// Starts the server. `behavior` maps the 1-based request number to its [`Step`].
    pub fn start<F>(behavior: F) -> Self
    where
        F: Fn(u32) -> Step + Send + Sync + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("mock bind");
        let port = listener.local_addr().expect("mock addr").port();
        listener.set_nonblocking(true).expect("mock nonblocking");
        let listener = TcpListener::from_std(listener).expect("async mock listener");
        let hits = Arc::new(AtomicU32::new(0));
        let behavior: Arc<dyn Fn(u32) -> Step + Send + Sync> = Arc::new(behavior);
        let handle = tokio::spawn(accept_loop(
            listener,
            Arc::clone(&behavior),
            Arc::clone(&hits),
        ));
        Self { port, hits, handle }
    }

    /// Requests served so far.
    pub fn hits(&self) -> u32 {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn accept_loop(
    listener: TcpListener,
    behavior: Arc<dyn Fn(u32) -> Step + Send + Sync>,
    hits: Arc<AtomicU32>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let behavior = Arc::clone(&behavior);
        let hits = Arc::clone(&hits);
        tokio::spawn(async move {
            let request_number = hits.fetch_add(1, Ordering::SeqCst) + 1;
            serve(stream, behavior(request_number)).await;
        });
    }
}

async fn serve(mut stream: TcpStream, step: Step) {
    read_request(&mut stream).await;
    match step {
        Step::Sse(payloads) => {
            write_head(&mut stream, 200, "text/event-stream", None).await;
            for payload in payloads {
                write_all(&mut stream, format!("data: {payload}\n\n").as_bytes()).await;
            }
            write_all(&mut stream, b"data: [DONE]\n\n").await;
        }
        Step::Fragments(fragments) => {
            write_head(&mut stream, 200, "text/event-stream", None).await;
            for fragment in &fragments {
                write_all(&mut stream, fragment).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        Step::Stall => {
            // Hold the socket open, answering nothing. The client's response
            // budget is what ends this.
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        Step::Status(code, body) => {
            write_head(&mut stream, code, "text/plain", Some(body.as_bytes())).await;
        }
        Step::Json(value) => {
            let body = value.to_string();
            write_head(&mut stream, 200, "application/json", Some(body.as_bytes())).await;
        }
    }
    let _ = stream.shutdown().await;
}

/// Writes a response head. `body` gets a Content-Length; without one the body
/// streams until connection close (SSE).
async fn write_head(stream: &mut TcpStream, code: u16, content_type: &str, body: Option<&[u8]>) {
    let reason = reason_phrase(code);
    let head = match body {
        Some(body) => format!(
            "HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ),
        None => format!(
            "HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n"
        ),
    };
    stream.write_all(head.as_bytes()).await.expect("write head");
    if let Some(body) = body {
        stream.write_all(body).await.expect("write body");
    }
    stream.flush().await.expect("flush");
}

fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

async fn write_all(stream: &mut TcpStream, bytes: &[u8]) {
    stream.write_all(bytes).await.expect("write");
    stream.flush().await.expect("flush");
}

/// Reads one full HTTP request (headers plus Content-Length body).
async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await.expect("read request");
        if read == 0 {
            break; // client closed early
        }
        request.extend_from_slice(&chunk[..read]);
        let Some(header_end) = find(&request, b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if request.len() >= header_end + 4 + content_length {
            break;
        }
    }
    request
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
