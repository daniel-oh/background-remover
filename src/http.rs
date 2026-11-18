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
//!   `?mask=1` for the mask alone as an 8-bit PNG, `?bg=RRGGBB` for the
//!   subject on a colour, `?refine=1` to clean the edges' colour, `?crop=1`
//!   to trim to the subject, `?orient=0` to leave the picture as stored
//! - 415 for any other content type, 413 over 12 MiB or over the pixel
//!   limit, 400 for an empty body or a bad option, 422 when the bytes are
//!   not a picture, 503 when the queue is full, 408 when the whole request
//!   runs past its time, 500 when the model fails. Errors are JSON:
//!   `{"error": "a_code", "message": "a sentence"}`.
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
use crate::imageops::{self, CutoutError, Focus, Options, Output};
use crate::model::Model;

/// Removals the service holds at once, the one being worked on included,
/// before it says "later".
pub const QUEUE: usize = 4;
/// Upper bounds, in seconds, of the buckets successful removals are counted
/// into for `/metrics`.
pub const BUCKETS: [f64; 8] = [0.5, 1.0, 2.0, 3.0, 5.0, 10.0, 30.0, 75.0];

/// Counters for `/metrics`. Plain atomics: a few numbers do not need a crate.
#[derive(Default)]
pub struct Metrics {
    /// Removals that returned 200.
    pub ok: AtomicU64,
    /// Removals refused: the request's own fault, or the queue was full
    /// (415, 413, 400, 422, 503).
    pub refused: AtomicU64,
    /// Removals that failed while running (500).
    pub failed: AtomicU64,
    /// Milliseconds spent in successful removals.
    pub millis: AtomicU64,
    /// Bytes received in successful removals.
    pub bytes_in: AtomicU64,
    /// Bytes sent in successful removals.
    pub bytes_out: AtomicU64,
    /// Successful removals by duration: one count per [`BUCKETS`] bound,
    /// then one for everything slower.
    pub durations: [AtomicU64; BUCKETS.len() + 1],
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
            queue: Arc::new(Semaphore::new(cfg.queue)),
            cfg: Arc::new(cfg),
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

/// The options of `POST /remove`, from its query string.
#[derive(Deserialize, Default)]
pub struct RemoveQuery {
    /// `png` (default) or `webp`.
    pub format: Option<String>,
    /// `1` or `true` for the mask alone.
    pub mask: Option<String>,
    /// `RRGGBB`: the subject on this colour, opaque.
    pub bg: Option<String>,
    /// `1` to clean the old background's colour out of the edges, `0` not
    /// to; unset means only with `bg`.
    pub refine: Option<String>,
    /// `1` to trim the output to the subject.
    pub crop: Option<String>,
    /// `0` to leave the picture as stored, ignoring its orientation tag.
    pub orient: Option<String>,
    /// Which model to run, by name: the default (`MODEL`) or one of
    /// `MODELS`; or `auto`, the default with a retry on the other when the
    /// result looks wrong.
    pub model: Option<String>,
    /// `x,y,w,h` in fractions: the subject inside this box.
    #[serde(rename = "box")]
    pub focus_box: Option<String>,
    /// `x,y` in fractions: the subject at this point.
    pub point: Option<String>,
    /// With `mask=1`: the long side of the mask returned.
    pub size: Option<String>,
}

/// Comma-separated fractions, each 0 to 1.
fn fractions<const N: usize>(text: &str, why: &'static str) -> Result<[f64; N], &'static str> {
    let parts: Vec<f64> = text
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| why)?;
    let parts: [f64; N] = parts.try_into().map_err(|_| why)?;
    if parts
        .iter()
        .any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
    {
        return Err(why);
    }
    Ok(parts)
}

/// Where a request asked the subject to be looked for.
fn focus_for(query: &RemoveQuery) -> Result<Focus, &'static str> {
    match (query.focus_box.as_deref(), query.point.as_deref()) {
        (Some(_), Some(_)) => Err("give box or point, not both"),
        (Some(b), None) => {
            let [x, y, w, h] = fractions::<4>(b, "box must be x,y,w,h as fractions 0 to 1")?;
            if w <= 0.0 || h <= 0.0 || x + w > 1.0 + 1e-9 || y + h > 1.0 + 1e-9 {
                return Err("box must have a size and lie inside the picture");
            }
            Ok(Focus::Box(x, y, w, h))
        }
        (None, Some(p)) => {
            let [x, y] = fractions::<2>(p, "point must be x,y as fractions 0 to 1")?;
            Ok(Focus::Point(x, y))
        }
        (None, None) => Ok(Focus::Whole),
    }
}

/// A yes-or-no option: `1`/`true`, `0`/`false`, or not given.
fn flag(value: Option<&str>, name: &'static str) -> Result<Option<bool>, &'static str> {
    match value {
        None => Ok(None),
        Some("1" | "true") => Ok(Some(true)),
        Some("0" | "false") => Ok(Some(false)),
        Some(_) => Err(name),
    }
}

/// Everything a request asked for, from its query and its Accept header.
pub fn options_for(
    query: &RemoveQuery,
    accept: Option<&str>,
    cfg: &Config,
) -> Result<Options, &'static str> {
    let background = match query.bg.as_deref() {
        None => None,
        Some(text) => Some(imageops::colour_of(text).ok_or("bg must be a colour as RRGGBB")?),
    };
    Ok(Options {
        output: output_for(query, accept)?,
        png_fast: cfg.png_fast,
        max_pixels: cfg.max_pixels.unwrap_or(MAX_PIXELS),
        upright: flag(query.orient.as_deref(), "orient must be 0 or 1")?.unwrap_or(true),
        refine: flag(query.refine.as_deref(), "refine must be 0 or 1")?,
        background,
        crop: flag(query.crop.as_deref(), "crop must be 0 or 1")?.unwrap_or(false),
        model: match query.model.as_deref() {
            None | Some("auto") => None,
            Some(name) if name == cfg.model || cfg.choices.iter().any(|c| c == name) => {
                Some(name.to_string())
            }
            Some(_) => return Err("model is not one this service offers; GET /version lists them"),
        },
        auto: match query.model.as_deref() {
            Some("auto") if cfg.choices.is_empty() => {
                return Err("model=auto needs a second model in MODELS")
            }
            Some("auto") => true,
            _ => false,
        },
        focus: focus_for(query)?,
        mask_size: match query.size.as_deref() {
            None => None,
            Some(n) => Some(
                n.parse::<u32>()
                    .ok()
                    .filter(|n| (16..=8192).contains(n))
                    .ok_or("size must be a whole number from 16 to 8192")?,
            ),
        },
    })
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
        .route("/ready", get(ready))
        .route("/version", get(version))
        .route("/metrics", get(metrics))
        .route("/remove", post(remove))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(state.cfg.timeout_seconds),
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
            .allow_headers([header::CONTENT_TYPE, header::ACCEPT])
            .expose_headers(RESULT_HEADERS.map(HeaderName::from_static)),
    )
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Serialize)]
struct Version {
    version: &'static str,
    model: String,
    model_sha256: String,
    /// Every model a request may ask for with `?model=`, the default first.
    models: Vec<String>,
    /// The model in memory now, if any.
    loaded_model: Option<String>,
    loaded: bool,
}

async fn version(State(state): State<AppState>) -> Json<Version> {
    Json(Version {
        version: crate::VERSION,
        model: state.cfg.model.clone(),
        model_sha256: state.cfg.model_sha256.chars().take(16).collect(),
        models: state.model.names(),
        loaded_model: state.model.loaded_name(),
        loaded: state.model.is_loaded(),
    })
}

/// Prometheus text exposition, by hand.
async fn metrics(State(state): State<AppState>) -> ([(HeaderName, &'static str); 1], String) {
    let m = &state.metrics;
    let load = Ordering::Relaxed;
    let mut text = format!(
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
         background_remover_model_loaded {}\n\
         # HELP background_remover_in_service Removals in the service now, the running one included.\n\
         # TYPE background_remover_in_service gauge\n\
         background_remover_in_service {}\n\
         # HELP background_remover_queue_size Removals the service holds before answering 503.\n\
         # TYPE background_remover_queue_size gauge\n\
         background_remover_queue_size {}\n\
         # HELP background_remover_request_duration_seconds Duration of successful removals.\n\
         # TYPE background_remover_request_duration_seconds histogram\n",
        m.ok.load(load),
        m.refused.load(load),
        m.failed.load(load),
        m.millis.load(load) as f64 / 1000.0,
        m.bytes_in.load(load),
        m.bytes_out.load(load),
        u8::from(state.model.is_loaded()),
        state.cfg.queue - state.queue.available_permits().min(state.cfg.queue),
        state.cfg.queue,
    );
    // The process's memory, so a deployer can set its limit from what it
    // really uses (#35). Linux only: read from /proc.
    if let Some((now, peak)) = memory() {
        text.push_str(&format!(
            "# HELP process_resident_memory_bytes Resident memory size in bytes.\n\
             # TYPE process_resident_memory_bytes gauge\n\
             process_resident_memory_bytes {now}\n\
             # HELP background_remover_resident_memory_peak_bytes The most resident memory this process has held.\n\
             # TYPE background_remover_resident_memory_peak_bytes gauge\n\
             background_remover_resident_memory_peak_bytes {peak}\n"
        ));
    }
    // Prometheus buckets are cumulative: each holds everything at or under it.
    let mut total = 0;
    for (i, count) in m.durations.iter().enumerate() {
        total += count.load(load);
        let bound = BUCKETS.get(i).map_or("+Inf".to_string(), |b| b.to_string());
        text.push_str(&format!(
            "background_remover_request_duration_seconds_bucket{{le=\"{bound}\"}} {total}\n"
        ));
    }
    text.push_str(&format!(
        "background_remover_request_duration_seconds_sum {:.3}\n\
         background_remover_request_duration_seconds_count {total}\n",
        m.millis.load(load) as f64 / 1000.0,
    ));
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
    let refuse = |status: StatusCode, code: &'static str, why: &'static str| {
        state.metrics.refused.fetch_add(1, Ordering::Relaxed);
        problem(status, code, why)
    };
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    if !accepts(content_type) {
        return refuse(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_type",
            "send image/jpeg, image/png or image/webp",
        );
    }
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    let opts = match options_for(&query, accept, &state.cfg) {
        Ok(o) => o,
        Err(why) => return refuse(StatusCode::BAD_REQUEST, "bad_option", why),
    };
    let output = opts.output;
    if body.is_empty() {
        return refuse(StatusCode::BAD_REQUEST, "empty_body", "empty body");
    }
    if body.len() > MAX_BODY_BYTES {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "12 MiB at most");
    }
    let Ok(permit) = state.queue.clone().try_acquire_owned() else {
        let mut busy = refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "busy, try again shortly",
        );
        busy.headers_mut()
            .insert(header::RETRY_AFTER, retry_after(&state).into());
        return busy;
    };
    let model = state.model.clone();
    let work = state.work.clone();
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
            let described = result_headers(&cutout);
            let bytes = cutout.bytes;
            let m = &state.metrics;
            m.ok.fetch_add(1, Ordering::Relaxed);
            m.millis.fetch_add(ms(started) as u64, Ordering::Relaxed);
            m.bytes_in.fetch_add(bytes_in as u64, Ordering::Relaxed);
            m.bytes_out.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            let seconds = started.elapsed().as_secs_f64();
            let bucket = BUCKETS.iter().position(|&b| seconds <= b);
            m.durations[bucket.unwrap_or(BUCKETS.len())].fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "POST /remove 200 {} {output:?} {}x{} in={bytes_in} out={} {}ms ({})",
                cutout.model,
                cutout.width,
                cutout.height,
                bytes.len(),
                ms(started),
                cutout.stages
            );
            let mut res = ([(header::CONTENT_TYPE, output.content_type())], bytes).into_response();
            for (name, value) in described {
                if let Ok(value) = header::HeaderValue::from_str(&value) {
                    res.headers_mut()
                        .insert(HeaderName::from_static(name), value);
                }
            }
            res
        }
        Ok(Some(Err(e @ CutoutError::TooLarge { .. }))) => {
            eprintln!("POST /remove 413 in={bytes_in}: {e}");
            refuse(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_many_pixels",
                "the picture has too many pixels",
            )
        }
        // Not a picture, or a broken one: the sender's mistake, not ours.
        Ok(Some(Err(e @ CutoutError::Decode(_)))) => {
            eprintln!("POST /remove 422 in={bytes_in}: {e}");
            refuse(
                StatusCode::UNPROCESSABLE_ENTITY,
                "undecodable",
                "the body is not a picture this service can read",
            )
        }
        // Only reachable when nobody is listening for the answer.
        Ok(None) => StatusCode::REQUEST_TIMEOUT.into_response(),
        Ok(Some(Err(e))) => {
            state.metrics.failed.fetch_add(1, Ordering::Relaxed);
            eprintln!("POST /remove 500 in={bytes_in} {}ms: {e}", ms(started));
            problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed",
                "the removal failed",
            )
        }
        Err(e) => {
            state.metrics.failed.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "POST /remove 500 in={bytes_in} {}ms: task: {e}",
                ms(started)
            );
            problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed",
                "the removal failed",
            )
        }
    }
}

#[derive(Serialize)]
struct Problem {
    error: &'static str,
    message: &'static str,
}

/// An error a program can switch on and a person can read.
fn problem(status: StatusCode, error: &'static str, message: &'static str) -> Response {
    (status, Json(Problem { error, message })).into_response()
}

/// The headers that describe a cutout (also exposed over CORS).
const RESULT_HEADERS: [&str; 7] = [
    "x-model",
    "x-retried",
    "x-model-runs",
    "x-subject-coverage",
    "x-subject-parts",
    "x-soft-fraction",
    "x-bbox",
];

/// Facts about a cutout as response headers, so a caller can judge or
/// explain it without decoding the image (#34).
fn result_headers(cutout: &imageops::Cutout) -> Vec<(&'static str, String)> {
    let f = &cutout.facts;
    let mut out = vec![
        ("x-model", cutout.model.clone()),
        ("x-retried", u8::from(cutout.retried).to_string()),
        ("x-model-runs", cutout.runs.to_string()),
        ("x-subject-coverage", format!("{:.4}", f.coverage)),
        ("x-subject-parts", f.parts.to_string()),
        ("x-soft-fraction", format!("{:.4}", f.soft)),
    ];
    if let Some((x, y, w, h)) = f.bbox {
        out.push(("x-bbox", format!("{x:.4},{y:.4},{w:.4},{h:.4}")));
    }
    out
}

/// Seconds a refused caller should wait: the average removal so far times
/// the removals ahead of it, at least one second.
fn retry_after(state: &AppState) -> u64 {
    let m = &state.metrics;
    let done = m.ok.load(Ordering::Relaxed);
    let average = if done == 0 {
        5.0
    } else {
        m.millis.load(Ordering::Relaxed) as f64 / done as f64 / 1000.0
    };
    let ahead = state.cfg.queue - state.queue.available_permits().min(state.cfg.queue);
    ((average * ahead.max(1) as f64).ceil() as u64).max(1)
}

#[derive(Serialize)]
struct Ready {
    ready: bool,
    loaded_model: Option<String>,
}

/// 200 when a model is in memory, 503 when the next request will load one
/// first ("warming up"). For a caller's display, not an orchestrator's
/// readiness probe: the model loads on the first request, so a probe on
/// this would keep a fresh service out of rotation for ever.
async fn ready(State(state): State<AppState>) -> Response {
    let body = Ready {
        ready: state.model.is_loaded(),
        loaded_model: state.model.loaded_name(),
    };
    let status = if body.ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(body)).into_response()
}

/// Resident memory now and at its highest, in bytes, from /proc/self/status
/// (`VmRSS`, `VmHWM`). `None` where there is no /proc.
fn memory() -> Option<(u64, u64)> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |name: &str| {
        status
            .lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
            .map(|kb| kb * 1024)
    };
    Some((field("VmRSS:")?, field("VmHWM:")?))
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
            ..RemoveQuery::default()
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
    fn options_come_from_the_query() {
        let cfg = Config::from_lookup(|_| None);
        let plain = options_for(&RemoveQuery::default(), None, &cfg).unwrap();
        assert!(plain.upright && !plain.crop);
        assert_eq!((plain.refine, plain.background), (None, None));
        assert_eq!(plain.max_pixels, MAX_PIXELS);
        let q = RemoveQuery {
            bg: Some("ff8000".into()),
            refine: Some("0".into()),
            crop: Some("1".into()),
            orient: Some("false".into()),
            ..RemoveQuery::default()
        };
        let o = options_for(&q, None, &cfg).unwrap();
        assert_eq!(o.background, Some([255, 128, 0]));
        assert_eq!(o.refine, Some(false));
        assert!(o.crop && !o.upright);
        for bad in [
            RemoveQuery {
                bg: Some("orange".into()),
                ..RemoveQuery::default()
            },
            RemoveQuery {
                crop: Some("yes".into()),
                ..RemoveQuery::default()
            },
        ] {
            assert!(options_for(&bad, None, &cfg).is_err());
        }
    }

    #[test]
    fn cors_is_off_unless_configured() {
        assert!(cors_for(&[]).is_none());
        assert!(cors_for(&["*".to_string()]).is_some());
        assert!(cors_for(&["https://example.com".to_string()]).is_some());
    }
}
