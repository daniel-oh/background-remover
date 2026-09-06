//! The HTTP contract, driven in-process through the router: no socket, no
//! model needed for the refusals. The one real removal runs only when
//! MODEL_PATH is set, as the golden test does.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use background_remover::config::{Config, MAX_BODY_BYTES};
use background_remover::http::{router, AppState};
use background_remover::model::Model;
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app() -> axum::Router {
    let cfg = Config {
        model_path: std::env::var("MODEL_PATH").unwrap_or_else(|_| "/nonexistent.onnx".into()),
        ..Config::from_env()
    };
    router(AppState::new(Model::new(cfg.clone()), cfg))
}

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, Vec<u8>, Option<String>) {
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let ct = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, body, ct)
}

#[tokio::test]
async fn health_says_ok() {
    let (status, body, _) = send(app(), Request::get("/health").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"ok");
}

#[tokio::test]
async fn version_reports_itself_and_an_unloaded_model() {
    let (status, body, ct) =
        send(app(), Request::get("/version").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct.as_deref(), Some("application/json"));
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.contains(&format!("\"version\":\"{}\"", background_remover::VERSION)),
        "{text}"
    );
    assert!(text.contains("\"loaded\":false"), "{text}");
}

#[tokio::test]
async fn wrong_content_type_is_415() {
    let req = Request::post("/remove")
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from("hello"))
        .unwrap();
    let (status, _, _) = send(app(), req).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn empty_body_is_400() {
    let req = Request::post("/remove")
        .header(header::CONTENT_TYPE, "image/jpeg")
        .body(Body::empty())
        .unwrap();
    let (status, _, _) = send(app(), req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn oversized_body_is_413() {
    let req = Request::post("/remove")
        .header(header::CONTENT_TYPE, "image/jpeg")
        .body(Body::from(vec![0u8; MAX_BODY_BYTES + 1]))
        .unwrap();
    let (status, _, _) = send(app(), req).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn garbage_that_claims_to_be_a_jpeg_is_500_not_a_crash() {
    let req = Request::post("/remove")
        .header(header::CONTENT_TYPE, "image/jpeg")
        .body(Body::from(vec![0xFF, 0xD8, 0x00, 0x01, 0x02]))
        .unwrap();
    let (status, _, _) = send(app(), req).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

/// The same junk scripts/smoke.sh sends the built binary: a marker, then
/// pseudo-random bytes. Must be a 500 with the service still answering.
#[tokio::test]
async fn a_corrupt_jpeg_is_500_and_the_service_keeps_answering() {
    let mut x: u32 = 0x1234_5678;
    let mut junk = vec![0xFF, 0xD8, 0xFF, 0xE0];
    junk.extend((0..5000).map(|_| {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (x >> 24) as u8
    }));
    let app = app();
    let req = Request::post("/remove")
        .header(header::CONTENT_TYPE, "image/jpeg")
        .body(Body::from(junk))
        .unwrap();
    let (status, _, _) = send(app.clone(), req).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (status, body, _) = send(app, Request::get("/health").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"ok");
}

#[tokio::test]
async fn metrics_are_prometheus_text() {
    let (status, body, ct) =
        send(app(), Request::get("/metrics").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.as_deref().unwrap_or("").starts_with("text/plain"));
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.contains("background_remover_requests_total{outcome=\"ok\"} 0"),
        "{text}"
    );
    assert!(text.contains("background_remover_model_loaded 0"), "{text}");
}

#[tokio::test]
async fn a_bad_format_is_400() {
    let req = Request::post("/remove?format=gif")
        .header(header::CONTENT_TYPE, "image/jpeg")
        .body(Body::from(vec![1u8, 2, 3]))
        .unwrap();
    let (status, _, _) = send(app(), req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn cors_headers_appear_only_when_configured() {
    let res = app()
        .oneshot(
            Request::get("/health")
                .header(header::ORIGIN, "https://example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(res.headers().get("access-control-allow-origin").is_none());
    let cfg = Config {
        cors_origins: vec!["https://example.com".into()],
        ..Config::from_env()
    };
    let app = router(AppState::new(Model::new(cfg.clone()), cfg));
    let req = Request::get("/health")
        .header(header::ORIGIN, "https://example.com")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://example.com")
    );
}

#[tokio::test]
async fn unknown_paths_are_404() {
    let (status, _, _) = send(app(), Request::get("/nope").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_real_removal_returns_a_png_with_alpha() {
    if std::env::var("MODEL_PATH").is_err() {
        eprintln!("http: MODEL_PATH is not set, skipping the real removal");
        return;
    }
    let sample =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/sample.jpg")).unwrap();
    let req = Request::post("/remove")
        .header(header::CONTENT_TYPE, "image/jpeg")
        .body(Body::from(sample))
        .unwrap();
    let (status, body, ct) = send(app(), req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ct.as_deref(), Some("image/png"));
    let img = image::load_from_memory(&body).unwrap().to_rgba8();
    assert_eq!(img.dimensions(), (960, 1200));
    assert!(
        img.pixels().any(|p| p[3] == 0),
        "some of the background is transparent"
    );
    assert!(
        img.pixels().any(|p| p[3] == 255),
        "some of the subject is opaque"
    );
}
