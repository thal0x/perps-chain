//! HL-compatible HTTP + WebSocket API server for hlbft.
//!
//! * `POST /info` — current state from [`StateQuery`] plus history from the
//!   [`Indexer`] (fills, orders, funding, ledger, trades, candles).
//! * `POST /exchange` — parse (hlwire) -> submit -> wait for the committed
//!   receipt (≤ 10 s) -> HL response JSON.
//! * `GET /ws` (also `/`) — subscriptions, `post`, ping.
//! * `GET /admin/status`, `POST /admin/chaos`.
//!
//! See README.md and COMPAT.md.

pub mod candle;
pub mod coins;
pub mod fmt;
pub mod indexer;
pub mod info;
pub mod render;
pub mod shared;
pub mod testkit;
pub mod wire;
pub mod ws;

pub use indexer::{Indexer, IndexerLimits};
pub use shared::{ApiConfig, Shared};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State, WebSocketUpgrade};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use interfaces::ApiBackend;
use serde_json::Value;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

/// Serve the API on `addr` with default configuration.
pub fn serve(backend: Arc<dyn ApiBackend>, addr: SocketAddr) -> impl Future<Output = std::io::Result<()>> + Send {
    serve_with(backend, addr, ApiConfig::default())
}

pub fn serve_with(
    backend: Arc<dyn ApiBackend>,
    addr: SocketAddr,
    cfg: ApiConfig,
) -> impl Future<Output = std::io::Result<()>> + Send {
    async move {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let shared = Shared::start(backend, cfg);
        serve_listener(shared, listener).await
    }
}

/// Serve on an already-bound listener (tests use port 0).
pub async fn serve_listener(shared: Arc<Shared>, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    let app = router(shared);
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await
}

pub fn router(shared: Arc<Shared>) -> Router {
    let max_body = shared.cfg.max_body;
    Router::new()
        .route("/info", post(info_handler))
        .route("/exchange", post(exchange_handler))
        .route("/ws", get(ws_handler))
        .route("/", get(ws_handler))
        .route("/admin/status", get(status_handler))
        .route("/admin/chaos", post(chaos_handler))
        .layer(DefaultBodyLimit::max(max_body))
        .layer(CorsLayer::permissive())
        .with_state(shared)
}

fn json_response(status: StatusCode, v: &Value) -> Response {
    (status, [(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

fn text_response(status: StatusCode, t: String) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], t).into_response()
}

async fn info_handler(State(s): State<Arc<Shared>>, body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return text_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Failed to deserialize the JSON body into the target type: {e}"),
            )
        }
    };
    let s2 = s.clone();
    // State queries may take node locks: keep them off the async workers.
    match tokio::task::spawn_blocking(move || info::handle(&s2, &v)).await {
        Ok(Ok(out)) => json_response(StatusCode::OK, &out),
        Ok(Err(e)) => text_response(StatusCode::UNPROCESSABLE_ENTITY, e),
        Err(_) => text_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into()),
    }
}

async fn exchange_handler(
    State(s): State<Arc<Shared>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return json_response(
                StatusCode::OK,
                &render::err("Failed to deserialize the JSON body into the target type"),
            )
        }
    };
    let out = s.exchange(&v, addr.ip().to_string()).await;
    json_response(StatusCode::OK, &out)
}

async fn ws_handler(
    State(s): State<Arc<Shared>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
) -> Response {
    let ip = addr.ip().to_string();
    ws.max_message_size(4 << 20).on_upgrade(move |socket| ws::serve_conn(s, socket, ip))
}

async fn status_handler(State(s): State<Arc<Shared>>) -> Response {
    let st = tokio::task::spawn_blocking(move || s.backend.status()).await.unwrap_or_default();
    json_response(StatusCode::OK, &serde_json::to_value(st).unwrap_or(Value::Null))
}

async fn chaos_handler(State(s): State<Arc<Shared>>, body: Bytes) -> Response {
    let cfg = String::from_utf8_lossy(&body).to_string();
    match tokio::task::spawn_blocking(move || s.backend.set_chaos(&cfg)).await {
        Ok(Ok(())) => json_response(StatusCode::OK, &serde_json::json!({"status": "ok"})),
        Ok(Err(e)) => json_response(StatusCode::BAD_REQUEST, &serde_json::json!({"status": "err", "response": e})),
        Err(_) => text_response(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into()),
    }
}
