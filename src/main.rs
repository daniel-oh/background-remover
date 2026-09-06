//! `background-remover`: the service binary.
//!
//! `background-remover --health` is the container's healthcheck (the runtime
//! image has no shell or curl); `--version` and `--help` do what they say.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;

use background_remover::config::Config;
use background_remover::http::{serve, AppState};
use background_remover::model::{verify_checksum, Model};
use background_remover::VERSION;

const HELP: &str = "\
background-remover: background removal as a small HTTP service.

Usage: background-remover [--health | --version | --help]

Runs an HTTP server (default 0.0.0.0:7000) with:
  GET  /health    200 ok
  GET  /version   JSON: version, model checksum, whether the model is loaded
  GET  /metrics   Prometheus text: requests, seconds, bytes, model loaded
  POST /remove    raw image bytes (image/jpeg, image/png, image/webp) in,
                  PNG with alpha out; ?format=webp (or Accept: image/webp)
                  for a lossless WebP, ?mask=1 for the mask alone

Configuration, by environment variable:
  MODEL_PATH     path to the ONNX model (default /models/isnet-general-use/isnet-general-use.onnx)
  MODEL_SHA256   expected checksum of the model; the process exits 1 on a mismatch
  IDLE_SECONDS   release the model after this long without a request (default 300)
  THREADS        ONNX Runtime intra-op threads (default 2)
  PNG_FAST       1 for the fast PNG encoder (larger files, quicker)
  BIND           address to listen on (default 0.0.0.0)
  PORT           port to listen on (default 7000)
  CORS_ORIGINS   comma-separated origins allowed to call from a browser, or *
                 (default none: no CORS headers)
";

fn main() {
    let cfg = Config::from_env();
    match std::env::args().nth(1).as_deref() {
        Some("--health") => exit(if healthy(cfg.port) { 0 } else { 1 }),
        Some("--version" | "-V") => {
            println!("background-remover {VERSION}");
            return;
        }
        Some("--help" | "-h") => {
            print!("{HELP}");
            return;
        }
        Some(other) => {
            eprintln!("unknown option {other}\n\n{HELP}");
            exit(2);
        }
        None => {}
    }
    if let Err(e) = verify_checksum(&cfg.model_path, &cfg.model_sha256) {
        eprintln!("{e}");
        exit(1);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async move {
        let model = Arc::new(Model::new(cfg.clone()));
        let reaper = model.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                let m = reaper.clone();
                let _ = tokio::task::spawn_blocking(move || m.unload_if_idle()).await;
            }
        });
        let state = AppState {
            model,
            ..AppState::new(Model::new(cfg.clone()), cfg.clone())
        };
        if let Err(e) = serve(state, &cfg.bind, cfg.port).await {
            eprintln!("server error: {e}");
            exit(1);
        }
    });
}

/// A plain HTTP/1.0 GET to our own /health, without pulling in a client.
fn healthy(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    if stream
        .write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = String::new();
    let _ = stream.read_to_string(&mut buf);
    buf.starts_with("HTTP/1.1 200") || buf.starts_with("HTTP/1.0 200")
}
