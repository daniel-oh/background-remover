//! `background-remover`: the service, and the command.
//!
//! With no files it serves HTTP; with files it cuts them out and exits.
//! `--health` is the container's healthcheck (the runtime image has no shell
//! or curl); `--fetch-model`, `--version` and `--help` do what they say.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;

use background_remover::cli::{self, Command};
use background_remover::config::Config;
use background_remover::http::{serve, AppState};
use background_remover::model::{verify_checksum, Model};
use background_remover::profile;
use background_remover::VERSION;

const HELP: &str = "\
background-remover: background removal as a command and as a small HTTP service.

Usage:
  background-remover [OPTIONS] <FILE>...   cut out the subjects; - reads stdin
  background-remover                       run the HTTP service
  background-remover --fetch-model         download the model into the cache
  background-remover --health | --version | --help

Options for files:
  -o, --output <PATH>    where to write (one input); - for stdout
  -d, --out-dir <DIR>    write results into DIR instead of beside each input
                         (default name: NAME-cutout.png, or NAME-mask.png)
  -f, --format <png|webp> output format (default png, or from the -o extension)
      --mask             write the mask alone, an 8-bit greyscale PNG
      --bg <RRGGBB>      put the subject on this colour (opaque output);
                         cleans the edges' colour unless --no-refine
      --refine           clean the old background's colour out of soft edges
      --crop             trim the output to the subject
      --no-orient        ignore the photo's orientation tag
      --force            replace an output file that already exists
      --model <NAME|PATH> the model: isnet-general-use (default; quickest),
                         birefnet-lite-768 (better on people, hair, thin things
                         and busy scenes; 1.4 GB), birefnet-general-lite or
                         birefnet-general (best; slower, 3 to 4 GB), or a file.
                         A named model is fetched on first use
      --no-download      fail instead of fetching a missing model
  -j, --threads <N>      ONNX Runtime threads (default: up to eight cores)
  -q, --quiet            no progress lines on stderr

The service (default 0.0.0.0:7000):
  GET  /health    200 ok
  GET  /version   JSON: version, model checksum, whether the model is loaded
  GET  /metrics   Prometheus text: requests, seconds, bytes, model loaded
  POST /remove    raw image bytes (image/jpeg, image/png, image/webp) in,
                  PNG with alpha out; ?format=webp (or Accept: image/webp)
                  for a lossless WebP, ?mask=1 for the mask alone,
                  ?bg=RRGGBB, ?refine=1, ?crop=1, ?orient=0 as the options
                  above, ?model=NAME for one of MODELS; the x-model header
                  says which model ran; errors are JSON {error, message};
                  one picture at a time, four in the service at most

Configuration, by environment variable:
  MODEL          which model: isnet-general-use (default), birefnet-lite-768,
                 birefnet-general-lite or birefnet-general. Sets the file,
                 its checksum, its source and how it is fed and read
  MODELS         other models a request may ask for with ?model=NAME,
                 comma-separated. One model is in memory at a time; asking
                 for another swaps them (about a second)
  MODEL_DIR      a folder with one folder per model: MODEL_DIR/<name>/<file>.onnx
                 (the container image sets /models)
  MODEL_PATH     path to the ONNX model (default: MODEL_DIR, else the cache)
  MODEL_SHA256   expected checksum of the model; the process exits 1 on a mismatch
  MODEL_URL      where --fetch-model downloads from
  MODEL_PROFILE  for a file of your own: isnet or birefnet, how it is fed and read
  MODEL_SIDE     for a file of your own: the square it takes, if not 1024
  IDLE_SECONDS   release the model after this long without a request (default 300)
  THREADS        ONNX Runtime intra-op threads for the service (default 2)
  PNG_FAST       0 for the slow PNG encoder (about a tenth smaller files,
                 many times slower); the fast one is the default
  QUEUE          removals the service holds at once (default 4)
  REQUEST_TIMEOUT  seconds before a request is answered 408 (default 75)
  MAX_PIXELS     largest picture decoded, in pixels (the service: 25000000;
                 the command: no limit; 0 for no limit)
  BIND           address to listen on (default 0.0.0.0)
  PORT           port to listen on (default 7000)
  CORS_ORIGINS   comma-separated origins allowed to call from a browser, or *
                 (default none: no CORS headers)
";

fn main() {
    let cfg = Config::from_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args) {
        Ok(Command::Serve) => {}
        Ok(Command::Cutout(job)) => {
            if job.model_name.is_none() {
                refuse_an_unknown_model(&cfg);
            }
            exit(cli::run(&job, &cfg))
        }
        Ok(Command::FetchModel(name)) => {
            let cfg = match name.as_deref().and_then(profile::known) {
                Some(known) => cfg.with_model(known),
                None => cfg,
            };
            refuse_an_unknown_model(&cfg);
            let job = cli::Job::default();
            let mut log = |line: String| eprintln!("{line}");
            match cli::ensure_model(&job, &cfg, &mut log) {
                Ok(path) => println!("{}", path.display()),
                Err(e) => {
                    eprintln!("{e}");
                    exit(1);
                }
            }
            return;
        }
        Ok(Command::Health) => exit(if healthy(cfg.port) { 0 } else { 1 }),
        Ok(Command::Version) => {
            println!("background-remover {VERSION}");
            return;
        }
        Ok(Command::Help) => {
            print!("{HELP}");
            return;
        }
        Err(e) => {
            eprintln!("{e}\n\n{HELP}");
            exit(2);
        }
    }
    refuse_an_unknown_model(&cfg);
    if let Err(e) = verify_checksum(&cfg.model_path, &cfg.model_sha256) {
        eprintln!("{e}");
        if !cfg.model_path_set {
            eprintln!(
                "no MODEL_PATH is set; run `background-remover --fetch-model` once to put the model at {}",
                cfg.model_path
            );
        }
        exit(1);
    }
    // The other models a request may ask for: each must be one this program
    // knows, and on disk with the right checksum, before the service starts.
    let mut others = Vec::new();
    for name in &cfg.choices {
        let Some(known) = profile::known(name) else {
            eprintln!(
                "MODELS names {name}, which is not a model this program knows; it knows {}",
                profile::names()
            );
            exit(2);
        };
        let other = cfg.with_model(known);
        if let Err(e) = verify_checksum(&other.model_path, &other.model_sha256) {
            eprintln!("{name}: {e}");
            exit(1);
        }
        others.push(other);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    runtime.block_on(async move {
        let model = Arc::new(Model::with_choices(cfg.clone(), others));
        let reaper = model.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                let m = reaper.clone();
                let _ = tokio::task::spawn_blocking(move || m.unload_if_idle()).await;
            }
        });
        let state = AppState::with_model(model, cfg.clone());
        if let Err(e) = serve(state, &cfg.bind, cfg.port).await {
            eprintln!("server error: {e}");
            exit(1);
        }
    });
}

/// `MODEL` must be a name this program knows. A typo must not quietly run
/// the default model under another model's name.
fn refuse_an_unknown_model(cfg: &Config) {
    if profile::known(&cfg.model).is_none() {
        eprintln!(
            "MODEL={} is not a model this program knows; it knows {}.\n\
             For a file of your own, leave MODEL unset and give MODEL_PATH, MODEL_SHA256 and MODEL_PROFILE.",
            cfg.model,
            profile::names()
        );
        exit(2);
    }
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
