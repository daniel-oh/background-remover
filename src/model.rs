//! The model: one ONNX Runtime session, built on first use and let go after a
//! quiet spell, because the box it runs on is shared and short of memory.

use std::fs::File;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use ort::ep::CPU;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::imageops::SIDE;

/// The model session, built on first use and released when idle.
pub struct Model {
    cfg: Config,
    session: Mutex<Option<Session>>,
    /// Seconds since `epoch` at the last request; 0 means never.
    last_used: AtomicU64,
    epoch: Instant,
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
            last_used: AtomicU64::new(0),
            epoch: Instant::now(),
        }
    }

    fn build(&self) -> Result<Session, String> {
        let t = Instant::now();
        let session = self.build_inner().map_err(|e| e.to_string())?;
        eprintln!("model loaded in {:.1}s", t.elapsed().as_secs_f32());
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

    /// Run the model on one NCHW tensor of 3 by SIDE by SIDE; returns the
    /// first output's single plane. One inference at a time.
    pub fn infer(&self, input: Vec<f32>) -> Result<Vec<f32>, String> {
        let mut guard = self
            .session
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        if guard.is_none() {
            *guard = Some(self.build()?);
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
        let tensor = Tensor::from_array(([1usize, 3, SIDE as usize, SIDE as usize], input))
            .map_err(|e| e.to_string())?;
        let outputs = session
            .run(ort::inputs![input_name.as_str() => tensor])
            .map_err(|e| e.to_string())?;
        let (_, data) = outputs[output_name.as_str()]
            .try_extract_tensor::<f32>()
            .map_err(|e| e.to_string())?;
        let plane = (SIDE * SIDE) as usize;
        if data.len() < plane {
            return Err(format!(
                "model output has {} values, expected {plane}",
                data.len()
            ));
        }
        Ok(data[..plane].to_vec())
    }

    /// Whether the session is resident right now.
    pub fn is_loaded(&self) -> bool {
        self.session.lock().map(|g| g.is_some()).unwrap_or(false)
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
        drop(guard);
        // glibc keeps freed pages unless asked; ask, so the RSS actually falls.
        #[cfg(target_os = "linux")]
        unsafe {
            libc::malloc_trim(0);
        }
        eprintln!("model released after {}s idle", self.cfg.idle_seconds);
        true
    }
}
