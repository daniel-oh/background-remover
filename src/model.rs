//! The model: one ONNX Runtime session, built on first use and let go after a
//! quiet spell, because the box it runs on is shared and short of memory.

use std::fs::File;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use ort::ep::CPU;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use sha2::{Digest, Sha256};

use crate::config::Config;

/// The model session, built on first use and released when idle.
pub struct Model {
    cfg: Config,
    session: Mutex<Option<Session>>,
    /// Whether `session` holds a session, readable without the lock: the
    /// lock is held for the whole of an inference, and a status endpoint
    /// must not wait for one.
    loaded: AtomicBool,
    /// Seconds since `epoch` at the last request; 0 means never.
    last_used: AtomicU64,
    epoch: Instant,
    /// Suppress the load and release lines.
    quiet: bool,
}

/// The weights on disk must be the ones this build was checked against.
/// Streamed, so the 178 MB file is never held twice.
pub fn verify_checksum(path: &str, expected: &str) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| format!("cannot open the model at {path}: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("cannot read the model: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hex::encode(hasher.finalize());
    if got != expected {
        return Err(format!(
            "model checksum {got} does not match the expected {expected}"
        ));
    }
    Ok(())
}

impl Model {
    /// A model that has not been loaded yet.
    pub fn new(cfg: Config) -> Model {
        Model {
            cfg,
            session: Mutex::new(None),
            loaded: AtomicBool::new(false),
            last_used: AtomicU64::new(0),
            epoch: Instant::now(),
            quiet: false,
        }
    }

    /// How this model is fed and read.
    pub fn profile(&self) -> crate::profile::Profile {
        self.cfg.profile
    }

    /// Say nothing on stderr about loading and releasing (the command's `-q`).
    pub fn quiet(mut self, quiet: bool) -> Model {
        self.quiet = quiet;
        self
    }

    fn build(&self) -> Result<Session, String> {
        let t = Instant::now();
        let session = self.build_inner().map_err(|e| e.to_string())?;
        if !self.quiet {
            eprintln!("model loaded in {:.1}s", t.elapsed().as_secs_f32());
        }
        Ok(session)
    }

    fn build_inner(&self) -> ort::Result<Session> {
        // No arena: the default one keeps growing with every request and is
        // what pushed the Python service past its limit.
        let mut builder = Session::builder()?
            .with_execution_providers([CPU::default().with_arena_allocator(false).build()])?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_intra_threads(self.cfg.threads)?
            .with_inter_threads(1)?
            .with_memory_pattern(false)?;
        builder.commit_from_file(&self.cfg.model_path)
    }

    /// Run the model on one NCHW tensor of 3 by side by side (the side of
    /// its profile); returns the first output's single plane. One inference
    /// at a time.
    pub fn infer(&self, input: Vec<f32>) -> Result<Vec<f32>, String> {
        let mut guard = self
            .session
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        if guard.is_none() {
            *guard = Some(self.build()?);
            self.loaded.store(true, Ordering::Relaxed);
        }
        self.last_used
            .store(self.epoch.elapsed().as_secs().max(1), Ordering::Relaxed);
        let session = guard.as_mut().expect("just built");
        // The model's own tensor names, so any model of this shape works
        // whatever its author called them.
        let input_name = session
            .inputs()
            .first()
            .map(|i| i.name().to_string())
            .ok_or("model has no inputs")?;
        let output_name = session
            .outputs()
            .first()
            .map(|o| o.name().to_string())
            .ok_or("model has no outputs")?;
        let side = self.cfg.profile.side as usize;
        // A model built for another size fails inside ONNX Runtime with a
        // message about dimensions; say what is wrong instead.
        if let Some(dims) = session
            .inputs()
            .first()
            .and_then(|i| i.dtype().tensor_shape())
        {
            let fixed: Vec<i64> = dims.iter().copied().collect();
            if fixed.len() == 4 && fixed[2] > 0 && fixed[2] as usize != side {
                return Err(format!(
                    "the model takes {}x{} pictures but its profile says {side}; set MODEL_SIDE",
                    fixed[2], fixed[3]
                ));
            }
        }
        let tensor =
            Tensor::from_array(([1usize, 3, side, side], input)).map_err(|e| e.to_string())?;
        let outputs = session
            .run(ort::inputs![input_name.as_str() => tensor])
            .map_err(|e| e.to_string())?;
        let (_, data) = outputs[output_name.as_str()]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        let plane = side * side;
        if data.len() < plane {
            return Err(format!(
                "model output has {} values, expected {plane}",
                data.len()
            ));
        }
        Ok(data[..plane].to_vec())
    }

    /// Whether the session is resident right now. Never waits, even while
    /// an inference is running.
    pub fn is_loaded(&self) -> bool {
        self.loaded.load(Ordering::Relaxed)
    }

    /// Drop the session once it has sat unused for the configured time.
    /// Returns true when something was released.
    pub fn unload_if_idle(&self) -> bool {
        let Ok(mut guard) = self.session.lock() else {
            return false;
        };
        if guard.is_none() {
            return false;
        }
        let last = self.last_used.load(Ordering::Relaxed);
        if self.epoch.elapsed().as_secs().saturating_sub(last) < self.cfg.idle_seconds {
            return false;
        }
        *guard = None;
        self.loaded.store(false, Ordering::Relaxed);
        drop(guard);
        // glibc keeps freed pages unless asked; ask, so the RSS actually falls.
        #[cfg(target_os = "linux")]
        unsafe {
            libc::malloc_trim(0);
        }
        if !self.quiet {
            eprintln!("model released after {}s idle", self.cfg.idle_seconds);
        }
        true
    }
}
