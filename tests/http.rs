//! The HTTP contract, driven in-process through the router: no socket, no
//! model needed for the refusals. The one real removal runs only when
//! MODEL_PATH is set, as the golden test does.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use background_remover::config::{Config, MAX_BODY_BYTES};
use background_remover::http::{router, AppState, QUEUE};
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
async fn garbage_that_claims_to_be_a_jpeg_is_422_not_a_crash() {
    let req = Request::post("/remove")
        .header(header::CONTENT_TYPE, "image/jpeg")
        .body(Body::from(vec![0xFF, 0xD8, 0x00, 0x01, 0x02]))
        .unwrap();
    let (status, _, _) = send(app(), req).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

/// The same junk scripts/smoke.sh sends the built binary: a marker, then
/// pseudo-random bytes. Must be a 422 with the service still answering.
#[tokio::test]
async fn a_corrupt_jpeg_is_422_and_the_service_keeps_answering() {
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
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
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

/// `testdata/sample.jpg` with its frame header rewritten to claim 30000 by
/// 30000: under the body limit, far over what the memory limit could hold.
fn jpeg_claiming_900_megapixels() -> Vec<u8> {
    let mut jpeg =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/sample.jpg")).unwrap();
    let mut at = 2;
    loop {
        let len = u16::from_be_bytes([jpeg[at + 2], jpeg[at + 3]]) as usize;
        if matches!(jpeg[at + 1], 0xC0..=0xC2) {
            jpeg[at + 5..at + 7].copy_from_slice(&30_000u16.to_be_bytes());
            jpeg[at + 7..at + 9].copy_from_slice(&30_000u16.to_be_bytes());
            return jpeg;
        }
        at += 2 + len;
    }
}

#[tokio::test]
async fn a_picture_over_the_pixel_limit_is_413_and_counts_as_refused() {
    let cfg = Config {
        model_path: "/nonexistent.onnx".into(),
        max_pixels: None,
        ..Config::from_env()
    };
    let state = AppState::new(Model::new(cfg.clone()), cfg);
    let req = Request::post("/remove")
        .header(header::CONTENT_TYPE, "image/jpeg")
        .body(Body::from(jpeg_claiming_900_megapixels()))
        .unwrap();
    let (status, _, _) = send(router(state.clone()), req).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    let (_, body, _) = send(
        router(state),
        Request::get("/metrics").body(Body::empty()).unwrap(),
    )
    .await;
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("outcome=\"refused\"} 1"), "{text}");
    assert!(text.contains("outcome=\"failed\"} 0"), "{text}");
}

/// A request whose caller has gone keeps its place in the queue until its
/// work is really over, and does no work once its turn comes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
// Holding the lock across the awaits is the test: it stands in for a
// removal in progress.
#[allow(clippy::await_holding_lock)]
async fn an_abandoned_request_holds_its_place_until_its_turn_then_does_nothing() {
    let cfg = Config {
        model_path: "/nonexistent.onnx".into(),
        ..Config::from_env()
    };
    let state = AppState::new(Model::new(cfg.clone()), cfg);
    let sample =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/sample.jpg")).unwrap();
    // Somebody else's picture is being worked on.
    let turn = state.work.clone();
    let busy = turn.lock().unwrap();

    let req = Request::post("/remove")
        .header(header::CONTENT_TYPE, "image/jpeg")
        .body(Body::from(sample))
        .unwrap();
    let waiting = tokio::spawn(router(state.clone()).oneshot(req));
    while state.queue.available_permits() == QUEUE {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // The caller gives up; the job is still parked behind the lock.
    waiting.abort();
    let _ = waiting.await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        state.queue.available_permits(),
        QUEUE - 1,
        "the place is still taken"
    );

    drop(busy);
    for _ in 0..200 {
        if state.queue.available_permits() == QUEUE {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(state.queue.available_permits(), QUEUE, "the place is free");
    // It never ran: with no model, running would have counted a failure.
    let (_, body, _) = send(
        router(state),
        Request::get("/metrics").body(Body::empty()).unwrap(),
    )
    .await;
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("outcome=\"failed\"} 0"), "{text}");
}

/// Six at once: four are taken, two are told to come back, and the status
/// endpoints answer at once while the model is busy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_fills_the_queue_and_status_stays_quick() {
    if std::env::var("MODEL_PATH").is_err() {
        eprintln!("http: MODEL_PATH is not set, skipping the burst");
        return;
    }
    let app = app();
    let sample =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/sample.jpg")).unwrap();
    let removals: Vec<_> = (0..6)
        .map(|_| {
            let req = Request::post("/remove")
                .header(header::CONTENT_TYPE, "image/jpeg")
                .body(Body::from(sample.clone()))
                .unwrap();
            tokio::spawn(app.clone().oneshot(req))
        })
        .collect();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    for path in ["/version", "/metrics", "/health"] {
        let started = std::time::Instant::now();
        let (status, _, _) =
            send(app.clone(), Request::get(path).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            started.elapsed().as_millis() < 100,
            "{path} waited {}ms behind a removal",
            started.elapsed().as_millis()
        );
    }
    let mut ok = 0;
    let mut busy = 0;
    for r in removals {
        match r.await.unwrap().unwrap().status() {
            StatusCode::OK => ok += 1,
            StatusCode::SERVICE_UNAVAILABLE => busy += 1,
            other => panic!("unexpected {other}"),
        }
    }
    assert_eq!((ok, busy), (QUEUE, 6 - QUEUE));
}

#[tokio::test]
async fn errors_are_json_with_a_code_and_a_sentence() {
    let cases = [
        (
            Request::post("/remove")
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from("hello"))
                .unwrap(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_type",
        ),
        (
            Request::post("/remove?bg=orange")
                .header(header::CONTENT_TYPE, "image/jpeg")
                .body(Body::from(vec![1u8]))
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "bad_option",
        ),
        (
            Request::post("/remove")
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(b"not a picture".to_vec()))
                .unwrap(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "undecodable",
        ),
    ];
    for (req, want, code) in cases {
        let (status, body, ct) = send(app(), req).await;
        assert_eq!(status, want);
        assert_eq!(ct.as_deref(), Some("application/json"));
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains(&format!("\"error\":\"{code}\"")), "{text}");
        assert!(text.contains("\"message\":\""), "{text}");
    }
}

#[tokio::test]
async fn metrics_carry_a_histogram_and_the_queue() {
    let (_, body, _) = send(app(), Request::get("/metrics").body(Body::empty()).unwrap()).await;
    let text = String::from_utf8(body).unwrap();
    for line in [
        "background_remover_in_service 0",
        "background_remover_queue_size 4",
        "background_remover_request_duration_seconds_bucket{le=\"0.5\"} 0",
        "background_remover_request_duration_seconds_bucket{le=\"+Inf\"} 0",
        "background_remover_request_duration_seconds_count 0",
    ] {
        assert!(text.contains(line), "missing {line} in\n{text}");
    }
}

/// The options on a real removal: a colour behind the subject makes the
/// output opaque, crop makes it smaller, and a phone portrait is upright.
#[tokio::test]
async fn background_crop_and_orientation_on_a_real_removal() {
    if std::env::var("MODEL_PATH").is_err() {
        eprintln!("http: MODEL_PATH is not set, skipping the options");
        return;
    }
    let sample =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/sample.jpg")).unwrap();
    let remove = |query: &'static str, bytes: Vec<u8>| async move {
        let req = Request::post(format!("/remove{query}"))
            .header(header::CONTENT_TYPE, "image/jpeg")
            .body(Body::from(bytes))
            .unwrap();
        let (status, body, _) = send(app(), req).await;
        assert_eq!(status, StatusCode::OK, "{query}");
        image::load_from_memory(&body).unwrap()
    };

    let plain = remove("", sample.clone()).await.to_rgba8();
    let on_green = remove("?bg=00ff00", sample.clone()).await;
    assert_eq!(on_green.color(), image::ColorType::Rgb8, "opaque");
    let on_green = on_green.to_rgb8();
    assert_eq!(on_green.dimensions(), plain.dimensions());
    // Where the cutout is fully transparent the colour is the background's;
    // where it is fully opaque it is the photo's.
    for (cut, flat) in plain.pixels().zip(on_green.pixels()) {
        match cut[3] {
            0 => assert_eq!(flat.0, [0, 255, 0]),
            255 => assert_eq!(flat.0, [cut[0], cut[1], cut[2]]),
            _ => {}
        }
    }

    // Cropped: exactly the box around the subject in the plain cutout, with
    // the same pixels in it. (In this photo the subject reaches the frame,
    // so the box may be the whole picture; the arithmetic is what is tested.)
    let cropped = remove("?crop=1", sample.clone()).await.to_rgba8();
    let alpha: Vec<u8> = plain.pixels().map(|p| p[3]).collect();
    let (x, y, w, h) =
        background_remover::imageops::subject_box(&alpha, plain.width(), plain.height()).unwrap();
    assert_eq!(cropped.dimensions(), (w, h));
    assert_eq!(cropped.get_pixel(0, 0), plain.get_pixel(x, y));
    assert_eq!(
        cropped.get_pixel(w - 1, h - 1),
        plain.get_pixel(x + w - 1, y + h - 1)
    );

    // The same photo saved by a phone held upright: stored on its side with
    // a tag. It must come back the size a viewer shows it at.
    let mut tagged = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x22];
    tagged.extend(b"Exif\0\0MM\0\x2A\0\0\0\x08\0\x01");
    tagged.extend([0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0, 0]);
    tagged.extend(&sample[2..]);
    let upright = remove("", tagged.clone()).await;
    assert_eq!((upright.width(), upright.height()), (1200, 960));
    let stored = remove("?orient=0", tagged).await;
    assert_eq!((stored.width(), stored.height()), (960, 1200));
}

#[tokio::test]
async fn a_model_the_service_does_not_offer_is_400() {
    for name in ["birefnet-general", "nonsense"] {
        let req = Request::post(format!("/remove?model={name}"))
            .header(header::CONTENT_TYPE, "image/jpeg")
            .body(Body::from(vec![1u8, 2, 3]))
            .unwrap();
        let (status, body, _) = send(app(), req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
        assert!(String::from_utf8(body).unwrap().contains("bad_option"));
    }
}

/// Two models offered, requests alternating between them: each answer says
/// which model made it, and only one model is ever in memory.
#[tokio::test]
async fn a_request_can_choose_the_model_and_only_one_is_in_memory() {
    let (Ok(isnet), Ok(birefnet)) = (
        std::env::var("MODEL_PATH"),
        std::env::var("BIREFNET_768_PATH"),
    ) else {
        eprintln!("http: MODEL_PATH or BIREFNET_768_PATH is not set, skipping");
        return;
    };
    let base = Config::from_lookup(|_| None);
    let known = background_remover::profile::known("birefnet-lite-768").unwrap();
    let default = Config {
        model_path: birefnet,
        model_path_set: true,
        choices: vec!["isnet-general-use".into()],
        ..base.with_model(known)
    };
    let other = Config {
        model_path: isnet,
        model_path_set: true,
        ..base.clone()
    };
    let state = AppState::new(Model::with_choices(default.clone(), vec![other]), default);
    let sample =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/sample.jpg")).unwrap();
    let mut masks = Vec::new();
    for (query, want) in [
        ("?mask=1", "birefnet-lite-768"),
        ("?mask=1&model=isnet-general-use", "isnet-general-use"),
        ("?mask=1&model=birefnet-lite-768", "birefnet-lite-768"),
    ] {
        let req = Request::post(format!("/remove{query}"))
            .header(header::CONTENT_TYPE, "image/jpeg")
            .body(Body::from(sample.clone()))
            .unwrap();
        let res = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{query}");
        assert_eq!(
            res.headers().get("x-model").and_then(|v| v.to_str().ok()),
            Some(want),
            "{query}"
        );
        assert_eq!(state.model.loaded_name().as_deref(), Some(want));
        masks.push(res.into_body().collect().await.unwrap().to_bytes());
    }
    assert_ne!(masks[0], masks[1], "the two models give different masks");
    assert_eq!(
        masks[0], masks[2],
        "and switching back gives the same answer"
    );

    let (_, body, _) = send(
        router(state),
        Request::get("/version").body(Body::empty()).unwrap(),
    )
    .await;
    let text = String::from_utf8(body).unwrap();
    assert!(
        text.contains(r#""models":["birefnet-lite-768","isnet-general-use"]"#),
        "{text}"
    );
}
