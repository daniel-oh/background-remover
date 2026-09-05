//! The HTTP face of the service:
//!
//! - `GET /health` → 200 `ok`
//! - `GET /version` → JSON: the crate version, the model's checksum, and
//!   whether the model is loaded right now
//! - `POST /remove` with raw image bytes and `content-type: image/jpeg`,
//!   `image/png` or `image/webp` → 200 `image/png` with alpha
//! - 415 for any other content type, 413 over 12 MiB, 503 when the queue is
//!   full, 408 when the whole request runs past 75 s, 500 when the picture
//!   cannot be decoded or the model fails.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use std::time::Instant;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower_http::timeout::TimeoutLayer;

use crate::config::{Config, MAX_BODY_BYTES};
use crate::imageops;
use crate::model::Model;

/// Requests waiting for the model before the service says "later".
pub const QUEUE: usize = 4;
/// The route gives up at 80 s; stop a little before it does.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(75);

#[derive(Clone)]
pub struct AppState {
    pub model: Arc<Model>,
    pub cfg: Arc<Config>,
    pub queue: Arc<Semaphore>,
}

/// What the route is allowed to send, exactly.
pub fn accepts(content_type: Option<&str>) -> bool {
    matches!(
        content_type,
        Some("image/jpeg" | "image/png" | "image/webp")
    )
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/remove", post(remove))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Serialize)]
struct Version {
    version: &'static str,
    model_sha256: String,
    loaded: bool,
}

async fn version(State(state): State<AppState>) -> Json<Version> {
    Json(Version {
        version: crate::VERSION,
        model_sha256: state.cfg.model_sha256.chars().take(16).collect(),
        loaded: state.model.is_loaded(),
    })
}

async fn remove(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    if !accepts(content_type) {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "send image/jpeg, image/png or image/webp",
        )
            .into_response();
    }
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty body").into_response();
    }
    if body.len() > MAX_BODY_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, "12 MiB at most").into_response();
    }
    let Ok(_permit) = state.queue.try_acquire() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "busy, try again shortly").into_response();
    };
    let model = state.model.clone();
    let fast = state.cfg.png_fast;
    let bytes_in = body.len();
    let started = Instant::now();
    let job = tokio::task::spawn_blocking(move || imageops::cutout(&body, &model, fast));
    let ms = |t: Instant| t.elapsed().as_millis();
    match job.await {
        Ok(Ok(png)) => {
            eprintln!(
                "POST /remove 200 in={bytes_in} out={} {}ms",
                png.len(),
                ms(started)
            );
            ([(header::CONTENT_TYPE, "image/png")], png).into_response()
        }
        Ok(Err(e)) => {
            eprintln!("POST /remove 500 in={bytes_in} {}ms: {e}", ms(started));
            (StatusCode::INTERNAL_SERVER_ERROR, "the removal failed").into_response()
        }
        Err(e) => {
            eprintln!(
                "POST /remove 500 in={bytes_in} {}ms: task: {e}",
                ms(started)
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "the removal failed").into_response()
        }
    }
}

/// Serve until SIGTERM or Ctrl-C, then let in-flight requests finish.
pub async fn serve(state: AppState, bind: &str, port: u16) -> std::io::Result<()> {
    let listener = TcpListener::bind((bind, port)).await?;
    eprintln!(
        "background-remover {} ready on {bind}:{port}; the model loads on first use",
        crate::VERSION
    );
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown())
        .await
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    eprintln!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_three_image_types_pass() {
        assert!(accepts(Some("image/jpeg")));
        assert!(accepts(Some("image/png")));
        assert!(accepts(Some("image/webp")));
        assert!(!accepts(Some("image/gif")));
        assert!(!accepts(Some("image/jpeg; charset=binary")));
        assert!(!accepts(Some("text/html")));
        assert!(!accepts(None));
    }
}
