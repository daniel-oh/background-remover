//! The HTTP face of the service:
//!
//! - `GET /health` → 200 `ok`
//! - `GET /version` → JSON: the crate version, the model's checksum, and
//!   whether the model is loaded right now
//! - `GET /metrics` → Prometheus text: requests by outcome, seconds spent,
//!   bytes in and out, whether the model is loaded
//! - `POST /remove` with raw image bytes and `content-type: image/jpeg`,
//!   `image/png` or `image/webp` → 200 `image/png` with alpha;
//!   `?format=webp` (or `Accept: image/webp`) for a lossless WebP,
//!   `?mask=1` for the mask alone as an 8-bit PNG
//! - 415 for any other content type, 413 over 12 MiB or over the pixel
//!   limit, 400 for an empty body or an unknown format, 503 when four
//!   requests are already in the service, 408 when the whole request runs
//!   past 75 s, 500 when the picture cannot be decoded or the model fails.
//!
//! One picture is worked on at a time, from decode to encode, so the memory
//! in use is one picture's however many requests are waiting.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{header, HeaderMap, HeaderName, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::timeout::TimeoutLayer;

use crate::config::{Config, MAX_BODY_BYTES, MAX_PIXELS};
use crate::imageops::{self, CutoutError, Options, Output};
use crate::model::Model;

/// Removals the service holds at once, the one being worked on included,
/// before it says "later".
pub const QUEUE: usize = 4;
/// Callers typically give up around 80 s; stop a little before that.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(75);

/// Counters for `/metrics`. Plain atomics: six numbers do not need a crate.
#[derive(Default)]
pub struct Metrics {
    /// Removals that returned 200.
    pub ok: AtomicU64,
    /// Removals refused before running (415, 413, 400, 503).
    pub refused: AtomicU64,
    /// Removals that failed while running (500).
    pub failed: AtomicU64,
    /// Milliseconds spent in successful removals.
    pub millis: AtomicU64,
    /// Bytes received in successful removals.
    pub bytes_in: AtomicU64,
    /// Bytes sent in successful removals.
    pub bytes_out: AtomicU64,
}

/// What every handler can reach.
#[derive(Clone)]
pub struct AppState {
    /// The model, shared by all requests.
    pub model: Arc<Model>,
    /// The configuration the process started with.
    pub cfg: Arc<Config>,
    /// The queue: one permit for each removal in the service. A permit is
    /// held until the work itself ends, not until the caller stops waiting.
    pub queue: Arc<Semaphore>,
    /// Held while a picture is being worked on, so only one is in memory.
    pub work: Arc<Mutex<()>>,
    /// Request counters.
    pub metrics: Arc<Metrics>,
}

impl AppState {
    /// State for a fresh process.
    pub fn new(model: Model, cfg: Config) -> AppState {
        AppState::with_model(Arc::new(model), cfg)
    }

    /// State around a model something else also holds (the idle reaper).
    pub fn with_model(model: Arc<Model>, cfg: Config) -> AppState {
        AppState {
            model,
            cfg: Arc::new(cfg),
            queue: Arc::new(Semaphore::new(QUEUE)),
            work: Arc::new(Mutex::new(())),
            metrics: Arc::new(Metrics::default()),
        }
    }
}

/// What the route is allowed to send, exactly.
pub fn accepts(content_type: Option<&str>) -> bool {
    matches!(
        content_type,
        Some("image/jpeg" | "image/png" | "image/webp")
    )
}

/// `?format=png|webp` and `?mask=1` on `POST /remove`.
#[derive(Deserialize, Default)]
pub struct RemoveQuery {
    /// `png` (default) or `webp`.
    pub format: Option<String>,
    /// `1` or `true` for the mask alone.
    pub mask: Option<String>,
}

/// Which output a request asked for, from its query and its Accept header.
pub fn output_for(query: &RemoveQuery, accept: Option<&str>) -> Result<Output, &'static str> {
    if matches!(query.mask.as_deref(), Some("1" | "true")) {
        return Ok(Output::MaskPng);
    }
    match query.format.as_deref() {
        Some("png") => Ok(Output::Png),
        Some("webp") => Ok(Output::Webp),
        Some(_) => Err("format must be png or webp"),
        None => {
            let prefers_webp =
                accept.is_some_and(|a| a.contains("image/webp") && !a.contains("image/png"));
            Ok(if prefers_webp {
                Output::Webp
            } else {
                Output::Png
            })
        }
    }
}

/// The service's routes and layers, ready to serve or to drive in a test.
pub fn router(state: AppState) -> Router {
    let cors = cors_for(&state.cfg.cors_origins);
    let router = Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/metrics", get(metrics))
        .route("/remove", post(remove))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ));
    match cors {
        Some(layer) => router.layer(layer).with_state(state),
        None => router.with_state(state),
    }
}

/// CORS only when asked for: `*`, or a list of origins.
fn cors_for(origins: &[String]) -> Option<CorsLayer> {
    if origins.is_empty() {
        return None;
    }
    let allow = if origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(origins.iter().filter_map(|o| o.parse().ok()))
    };
    Some(
        CorsLayer::new()
            .allow_origin(allow)
            .allow_methods([Method::GET, Method::POST])
            .allow_headers([header::CONTENT_TYPE, header::ACCEPT]),
    )
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

/// Prometheus text exposition, by hand.
async fn metrics(State(state): State<AppState>) -> ([(HeaderName, &'static str); 1], String) {
    let m = &state.metrics;
    let load = Ordering::Relaxed;
    let text = format!(
        "# HELP background_remover_requests_total Removal requests by outcome.\n\
         # TYPE background_remover_requests_total counter\n\
         background_remover_requests_total{{outcome=\"ok\"}} {}\n\
         background_remover_requests_total{{outcome=\"refused\"}} {}\n\
         background_remover_requests_total{{outcome=\"failed\"}} {}\n\
         # HELP background_remover_seconds_total Seconds spent in successful removals.\n\
         # TYPE background_remover_seconds_total counter\n\
         background_remover_seconds_total {:.3}\n\
         # HELP background_remover_bytes_total Bytes in and out of successful removals.\n\
         # TYPE background_remover_bytes_total counter\n\
         background_remover_bytes_total{{direction=\"in\"}} {}\n\
         background_remover_bytes_total{{direction=\"out\"}} {}\n\
         # HELP background_remover_model_loaded Whether the model is resident (1) or released (0).\n\
         # TYPE background_remover_model_loaded gauge\n\
         background_remover_model_loaded {}\n",
        m.ok.load(load),
        m.refused.load(load),
        m.failed.load(load),
        m.millis.load(load) as f64 / 1000.0,
        m.bytes_in.load(load),
        m.bytes_out.load(load),
        u8::from(state.model.is_loaded()),
    );
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        text,
    )
}

async fn remove(
    State(state): State<AppState>,
    Query(query): Query<RemoveQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let refuse = |status: StatusCode, why: &'static str| {
        state.metrics.refused.fetch_add(1, Ordering::Relaxed);
        (status, why).into_response()
    };
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    if !accepts(content_type) {
        return refuse(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "send image/jpeg, image/png or image/webp",
        );
    }
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    let output = match output_for(&query, accept) {
        Ok(o) => o,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, why),
    };
    if body.is_empty() {
        return refuse(StatusCode::BAD_REQUEST, "empty body");
    }
    if body.len() > MAX_BODY_BYTES {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "12 MiB at most");
    }
    let Ok(permit) = state.queue.clone().try_acquire_owned() else {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "busy, try again shortly");
    };
    let model = state.model.clone();
    let work = state.work.clone();
    let opts = Options {
        output,
        png_fast: state.cfg.png_fast,
        max_pixels: state.cfg.max_pixels.unwrap_or(MAX_PIXELS),
    };
    let bytes_in = body.len();
    let started = Instant::now();
    // Set when this handler is dropped: the caller went away or the timeout
    // fired. A job still waiting its turn then has nobody to answer.
    let gone = Arc::new(AtomicBool::new(false));
    let _leaving = Leaving(gone.clone());
    let job = tokio::task::spawn_blocking(move || {
        // The permit lives as long as the work, so the queue counts what is
        // really in the service. A blocking task cannot be cancelled; were
        // the permit the handler's, a timed-out request would free its place
        // while its work went on.
        let _permit = permit;
        let _turn = work.lock().unwrap_or_else(|e| e.into_inner());
        if gone.load(Ordering::Relaxed) {
            return None;
        }
        Some(imageops::cutout_with(&body, &model, &opts))
    });
    let ms = |t: Instant| t.elapsed().as_millis();
    match job.await {
        Ok(Some(Ok(cutout))) => {
            let bytes = cutout.bytes;
            let m = &state.metrics;
            m.ok.fetch_add(1, Ordering::Relaxed);
            m.millis.fetch_add(ms(started) as u64, Ordering::Relaxed);
            m.bytes_in.fetch_add(bytes_in as u64, Ordering::Relaxed);
            m.bytes_out.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            eprintln!(
                "POST /remove 200 {output:?} {}x{} in={bytes_in} out={} {}ms ({})",
                cutout.width,
                cutout.height,
                bytes.len(),
                ms(started),
                cutout.stages
            );
            ([(header::CONTENT_TYPE, output.content_type())], bytes).into_response()
        }
        Ok(Some(Err(e @ CutoutError::TooLarge { .. }))) => {
            eprintln!("POST /remove 413 in={bytes_in}: {e}");
            refuse(
                StatusCode::PAYLOAD_TOO_LARGE,
                "the picture has too many pixels",
            )
        }
        // Only reachable when nobody is listening for the answer.
        Ok(None) => StatusCode::REQUEST_TIMEOUT.into_response(),
        Ok(Some(Err(e))) => {
            state.metrics.failed.fetch_add(1, Ordering::Relaxed);
            eprintln!("POST /remove 500 in={bytes_in} {}ms: {e}", ms(started));
            (StatusCode::INTERNAL_SERVER_ERROR, "the removal failed").into_response()
        }
        Err(e) => {
            state.metrics.failed.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "POST /remove 500 in={bytes_in} {}ms: task: {e}",
                ms(started)
            );
            (StatusCode::INTERNAL_SERVER_ERROR, "the removal failed").into_response()
        }
    }
}

/// Marks a request as abandoned when its handler is dropped.
struct Leaving(Arc<AtomicBool>);

impl Drop for Leaving {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
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

    #[test]
    fn output_follows_the_query_then_the_accept_header() {
        let q = |format: Option<&str>, mask: Option<&str>| RemoveQuery {
            format: format.map(String::from),
            mask: mask.map(String::from),
        };
        assert_eq!(output_for(&q(None, None), None).unwrap(), Output::Png);
        assert_eq!(
            output_for(&q(Some("webp"), None), None).unwrap(),
            Output::Webp
        );
        assert_eq!(
            output_for(&q(None, Some("1")), None).unwrap(),
            Output::MaskPng
        );
        assert_eq!(
            output_for(&q(None, None), Some("image/webp")).unwrap(),
            Output::Webp
        );
        assert_eq!(
            output_for(&q(None, None), Some("image/png, image/webp")).unwrap(),
            Output::Png
        );
        assert_eq!(
            output_for(&q(Some("png"), None), Some("image/webp")).unwrap(),
            Output::Png
        );
        assert!(output_for(&q(Some("gif"), None), None).is_err());
    }

    #[test]
    fn cors_is_off_unless_configured() {
        assert!(cors_for(&[]).is_none());
        assert!(cors_for(&["*".to_string()]).is_some());
        assert!(cors_for(&["https://example.com".to_string()]).is_some());
    }
}
