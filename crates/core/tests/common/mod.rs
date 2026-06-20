use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub enum Mode {
    /// Serve normally, honoring Range requests.
    Ok,
    /// Serve the full content with one byte flipped (poisoned).
    Corrupt,
    NotFound,
    /// On the first request, return only the first N bytes (simulating a
    /// dropped connection). Subsequent requests serve normally (with Range).
    TruncateFirst(usize),
}

#[derive(Clone)]
struct Spec {
    data: Vec<u8>,
    mode: Mode,
    hits: usize,
}

type Specs = Arc<Mutex<HashMap<String, Spec>>>;

pub struct TestServer {
    pub addr: std::net::SocketAddr,
    specs: Specs,
    _handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    pub async fn start() -> Self {
        let specs: Specs = Arc::new(Mutex::new(HashMap::new()));
        let app = Router::new().fallback(handler).with_state(specs.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        TestServer {
            addr,
            specs,
            _handle: handle,
        }
    }

    pub fn put(&self, path: &str, data: Vec<u8>, mode: Mode) {
        self.specs.lock().unwrap().insert(
            path.to_string(),
            Spec {
                data,
                mode,
                hits: 0,
            },
        );
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn hits(&self, path: &str) -> usize {
        self.specs
            .lock()
            .unwrap()
            .get(path)
            .map(|s| s.hits)
            .unwrap_or(0)
    }
}

fn parse_range(s: &str, total: u64) -> Option<(u64, u64)> {
    let s = s.strip_prefix("bytes=")?;
    let (a, b) = s.split_once('-')?;
    let start: u64 = a.trim().parse().ok()?;
    let end = if b.trim().is_empty() {
        total - 1
    } else {
        b.trim().parse().ok()?
    };
    if start > end || start >= total {
        return None;
    }
    Some((start, end.min(total - 1)))
}

async fn handler(State(specs): State<Specs>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let range_hdr = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let mut guard = specs.lock().unwrap();
    let Some(spec) = guard.get_mut(&path) else {
        return (StatusCode::NOT_FOUND, "no route").into_response();
    };
    spec.hits += 1;
    let hit = spec.hits;
    let data = spec.data.clone();
    let mode = spec.mode.clone();
    drop(guard);

    let total = data.len() as u64;
    match mode {
        Mode::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
        Mode::Corrupt => {
            let mut d = data;
            if !d.is_empty() {
                d[0] ^= 0xFF;
            }
            full_response(d)
        }
        Mode::TruncateFirst(n) if hit == 1 => {
            let cut = n.min(data.len());
            full_response(data[..cut].to_vec())
        }
        _ => {
            if let Some(rh) = range_hdr {
                if let Some((start, end)) = parse_range(&rh, total) {
                    let slice = data[start as usize..=end as usize].to_vec();
                    let len = slice.len() as u64;
                    return Response::builder()
                        .status(StatusCode::PARTIAL_CONTENT)
                        .header(header::ACCEPT_RANGES, "bytes")
                        .header(header::CONTENT_LENGTH, len)
                        .header(
                            header::CONTENT_RANGE,
                            format!("bytes {start}-{end}/{total}"),
                        )
                        .body(Body::from(slice))
                        .unwrap();
                }
            }
            full_response(data)
        }
    }
}

fn full_response(data: Vec<u8>) -> Response {
    let len = data.len() as u64;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len)
        .body(Body::from(data))
        .unwrap()
}
