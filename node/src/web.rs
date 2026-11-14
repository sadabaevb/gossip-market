//! The simulation with many agents, drawn as it runs: a page on localhost that shows each agent
//! as a dot and each settled ring as a line between its makers.
//!
//! The page learns everything from one list of events that only grows. Served live, it asks for
//! the events past the ones it has. The same list written to a file is a recording, and the same
//! page plays it back with no server at all: `docs/watch/index.html` opens from disk.
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::{Path as Segment, State};
use axum::http::header::CONTENT_TYPE;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::time::sleep;

/// The page, the one copy of it: served here, and opened from disk beside a recording.
const PAGE: &str = include_str!("../../docs/watch/index.html");

/// Where the page is served unless told otherwise.
pub const PORT: u16 = 7878;

/// How long after its last question a viewer is taken to be still watching. The page asks a few
/// times a second.
const WATCHING: Duration = Duration::from_secs(3);

/// What the page is told: who is in the market, and everything that has happened in it.
#[derive(Clone, Default)]
pub struct Board {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    market: Value,
    events: Vec<Value>,
    /// When the page last asked for events, if it ever has.
    asked: Option<Instant>,
}

impl Board {
    /// Starts serving the page on localhost and returns its address.
    pub async fn open(port: u16) -> Result<(Self, String)> {
        let board = Self::default();
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
            .await
            .with_context(|| format!("serving the page on port {port}; `--port` picks another"))?;
        let url = format!("http://{}", listener.local_addr()?);
        let app = Router::new()
            .route("/", get(|| async { Html(PAGE) }))
            // Beside a recording this file holds the run. Served live there is none, and saying
            // so here is what tells the page to ask for events instead.
            .route(
                "/run.js",
                get(|| async { ([(CONTENT_TYPE, "text/javascript")], "window.RUN = null;") }),
            )
            .route("/market", get(market))
            .route("/events/{after}", get(events))
            .with_state(board.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Ok((board, url))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("the board is never left half-written")
    }

    /// Who is in the market and what each came to trade. Said once, before anything happens.
    pub fn set_market(&self, market: Value) {
        self.lock().market = market;
    }

    /// One more thing that happened, `at` this long after the agents started.
    pub fn push(&self, at: Duration, kind: &str, mut event: Value) {
        event["t"] = json!((at.as_secs_f64() * 10.0).round() / 10.0);
        event["k"] = json!(kind);
        self.lock().events.push(event);
    }

    /// Whether a page is asking for events now.
    pub fn watched(&self) -> bool {
        self.lock().asked.is_some_and(|at| at.elapsed() < WATCHING)
    }

    /// Waits for a page to open, for at most `limit`. A market that started while the browser was
    /// still opening would have half its rings settled before anyone saw one.
    pub async fn wait_for_viewer(&self, limit: Duration) -> bool {
        let until = Instant::now() + limit;
        while !self.watched() && Instant::now() < until {
            sleep(Duration::from_millis(100)).await;
        }
        self.watched()
    }

    /// Keeps serving for as long as a page is open, and at most `limit`: the end of a run is the
    /// part worth looking at. With nobody watching it returns at once, so that a run started by a
    /// script ends when the market does.
    pub async fn linger(&self, limit: Duration) {
        let until = Instant::now() + limit;
        while self.watched() && Instant::now() < until {
            sleep(Duration::from_millis(200)).await;
        }
    }

    /// The whole run so far, as the page reads a recording.
    pub fn recording(&self) -> Value {
        let inner = self.lock();
        json!({ "market": inner.market, "events": inner.events })
    }

    /// Writes the run where the page looks for a recording: a script that sets `window.RUN`, so
    /// that the page can be opened from disk, where a browser lets it fetch nothing.
    pub fn record(&self, path: &Path) -> Result<()> {
        let script = format!("window.RUN = {};\n", self.recording());
        std::fs::write(path, script).with_context(|| format!("writing the recording to {}", path.display()))
    }
}

/// Asking who is in the market counts as watching too: it is what a page does first, and keeps
/// doing until the simulation has said.
async fn market(State(board): State<Board>) -> impl IntoResponse {
    let mut inner = board.lock();
    inner.asked = Some(Instant::now());
    Json(inner.market.clone())
}

/// The events past the first `after`.
async fn events(State(board): State<Board>, Segment(after): Segment<usize>) -> impl IntoResponse {
    let mut inner = board.lock();
    inner.asked = Some(Instant::now());
    let from = after.min(inner.events.len());
    Json(Value::Array(inner.events[from..].to_vec()))
}

/// One request over a plain socket, for the tests of the pages: the node has no HTTP client of its
/// own to borrow.
#[cfg(test)]
pub(crate) async fn ask(method: &str, url: &str, body: Option<&Value>) -> Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_once('/').unwrap();
    let body = body.map(Value::to_string).unwrap_or_default();
    let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
    let request = format!(
        "{method} /{path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await.unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The page keeps what it has drawn and asks only for what is new. Sent everything each time,
    // it would draw every ring again; sent nothing past a wrong count, it would stop for good.
    #[tokio::test]
    async fn a_page_is_sent_only_the_events_it_does_not_have() {
        let (board, url) = Board::open(0).await.unwrap();
        board.set_market(json!({ "agents": [{ "name": "alice" }] }));
        assert!(!board.watched());
        board.push(Duration::from_millis(1234), "ring", json!({ "makers": [0, 1] }));
        board.push(Duration::from_secs(2), "quit", json!({ "a": 1 }));

        let get = |path: &str| {
            let url = format!("{url}{path}");
            async move { ask("GET", &url, None).await }
        };
        assert_eq!(get("/market").await["agents"][0]["name"], "alice");
        let all = get("/events/0").await;
        assert_eq!(all.as_array().unwrap().len(), 2);
        assert_eq!(all[0], json!({ "makers": [0, 1], "t": 1.2, "k": "ring" }));
        assert_eq!(get("/events/1").await, json!([{ "a": 1, "t": 2.0, "k": "quit" }]));
        assert_eq!(get("/events/2").await, json!([]));
        assert_eq!(get("/events/99").await, json!([]));
        // Asking is what tells the board somebody is there.
        assert!(board.watched());
        assert_eq!(board.recording()["events"].as_array().unwrap().len(), 2);
    }
}
