// server.rs — the HTTP surface. Reached only by the Nordkraft controller
// (which relays for users), on the container's own address.
//
//   GET  /v1/health
//   POST /v1/exec                      buffered exec, JSON in / JSON out
//   GET  /v1/stream?tty=&cmd=&...      WebSocket, k8s v4 channel frames
//   PUT  /v1/files?path=&mode=         upload (raw body)
//   GET  /v1/files?path=               download
//
// Auth: `Authorization: Bearer <token>` or `?token=` (for WebSocket clients
// that cannot set headers). The token is what the deploy put in
// NORDKRAFT_SANDBOX_TOKEN; without one the daemon refuses to start unless
// NORDKRAFT_SANDBOX_INSECURE=1 (dev only).

use crate::exec::{self, Spec, StreamSpec};
use crate::init::Tracked;
use crate::protocol::Resize;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{debug, info};

#[derive(Clone)]
pub struct AppState {
    pub token: Option<Arc<String>>,
    pub tracked: Tracked,
    pub started: Instant,
    pub main_pid: Option<i32>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/exec", post(exec_buffered))
        .route("/v1/stream", get(stream))
        .route(
            "/v1/files",
            axum::routing::put(crate::files::put_file).get(crate::files::get_file),
        )
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state)
}

async fn auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(token) = &state.token else {
        return next.run(req).await;
    };
    let from_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);
    let from_query = req.uri().query().and_then(|q| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix("token=").map(|t| t.to_owned()))
    });
    let presented = from_header.or(from_query);
    let ok = presented
        .as_deref()
        .map(|p| constant_time_eq(p.as_bytes(), token.as_bytes()))
        .unwrap_or(false);
    if !ok {
        return (StatusCode::UNAUTHORIZED, "sandbox token required").into_response();
    }
    next.run(req).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "main_pid": state.main_pid,
        "uptime_s": state.started.elapsed().as_secs(),
    }))
}

async fn exec_buffered(
    State(state): State<AppState>,
    Json(req): Json<exec::ExecRequest>,
) -> Json<exec::ExecResult> {
    Json(exec::run_buffered(req, state.tracked.clone()).await)
}

/// Same query vocabulary as the controller's stream route, so a relayed
/// request needs no translation.
#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    #[serde(default)]
    pub tty: Option<String>,
    #[serde(default)]
    pub stdin: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

fn truthy(v: Option<&str>) -> Option<bool> {
    match v.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("1") | Some("true") | Some("yes") | Some("on") => Some(true),
        Some("0") | Some("false") | Some("no") | Some("off") => Some(false),
        _ => None,
    }
}

/// Repeated keys (`cmd`, `env`) are not expressible with axum's Query, so read them raw.
fn repeated(query: &str, key: &str) -> Vec<String> {
    query
        .split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            if k == key {
                Some(percent_decode(v))
            } else {
                None
            }
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(h) = u8::from_str_radix(
                    std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"),
                    16,
                ) {
                    out.push(h);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn stream(
    State(state): State<AppState>,
    Query(q): Query<StreamQuery>,
    req_uri: axum::http::Uri,
    ws: WebSocketUpgrade,
) -> Response {
    let raw = req_uri.query().unwrap_or("");
    let mut cmd = repeated(raw, "cmd");
    if cmd.is_empty() {
        cmd = vec!["/bin/sh".to_string()];
    }
    let env: HashMap<String, String> = repeated(raw, "env")
        .into_iter()
        .filter_map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    let tty = truthy(q.tty.as_deref()).unwrap_or(false);
    let spec = StreamSpec {
        spec: Spec {
            cmd,
            cwd: q.cwd,
            env,
            user: q.user,
        },
        tty,
        stdin: truthy(q.stdin.as_deref()).unwrap_or(true),
        size: match (q.cols, q.rows) {
            (Some(width), Some(height)) => Some(Resize { width, height }),
            _ => None,
        },
    };
    let tracked = state.tracked.clone();
    ws.on_upgrade(move |socket| session(socket, spec, tracked))
}

async fn session(socket: WebSocket, spec: StreamSpec, tracked: Tracked) {
    info!("stream: {:?} tty={}", spec.spec.cmd, spec.tty);
    let (to_proc_tx, to_proc_rx) = mpsc::channel::<Vec<u8>>(64);
    let (from_proc_tx, mut from_proc_rx) = mpsc::channel::<Vec<u8>>(64);
    let runner = tokio::spawn(exec::run_stream(spec, to_proc_rx, from_proc_tx, tracked));
    let (mut sink, mut source) = socket.split();
    loop {
        tokio::select! {
            msg = source.next() => match msg {
                Some(Ok(Message::Binary(b))) => { if to_proc_tx.send(b).await.is_err() { break; } }
                Some(Ok(Message::Text(t))) => { if to_proc_tx.send(t.into_bytes()).await.is_err() { break; } }
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | None => break,
                Some(Err(e)) => { debug!("ws read: {e}"); break; }
            },
            out = from_proc_rx.recv() => match out {
                Some(b) => { if sink.send(Message::Binary(b)).await.is_err() { break; } }
                None => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            },
        }
    }
    drop(to_proc_tx); // client gone (or done): the runner kills what is left
    let _ = runner.await;
}
