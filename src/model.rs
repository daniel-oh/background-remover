//! The model: one ONNX Runtime session, built on first use and let go after a
//! quiet spell, because the box it runs on is shared and short of memory.
//!
//! A [`Model`] can hold several choices (the default first). Only one is
//! ever in memory: running another drops the session that is there and
//! builds the one asked for, which takes about a second. Inferences are one
//! at a time, so two models never sit in memory together.

use std::fs::File;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use ort::ep::CPU;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use sha2::{Digest, Sha256};

use crate::config::Config;

/// No choice is in memory.
const NONE: usize = usize::MAX;

/// The model session, built on first use and released when idle.
pub struct Model {
    /// The choices, the default first.
    choices: Vec<Config>,
    /// The session in memory, and which choice it is.
    session: Mutex<Option<(usize, Session)>>,
    /// Which choice is in memory, readable without the lock; [`NONE`] when
    /// none is.
    current: AtomicUsize,
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
        Model::with_choices(cfg, Vec::new())
    }

    /// The default model and others a request may ask for by name. None is
    /// loaded yet; each is loaded when first asked for.
    pub fn with_choices(default: Config, others: Vec<Config>) -> Model {
        let mut choices = vec![default];
        choices.extend(others);
        Model {
            choices,
            session: Mutex::new(None),
            current: AtomicUsize::new(NONE),
            loaded: AtomicBool::new(false),
            last_used: AtomicU64::new(0),
            epoch: Instant::now(),
            quiet: false,
        }
    }

    /// How the default model is fed and read.
    pub fn profile(&self) -> crate::profile::Profile {
        self.choices[0].profile
    }

    /// How a choice is fed and read.
    pub fn profile_of(&self, choice: usize) -> crate::profile::Profile {
        self.choices[choice].profile
    }

    /// The index of the choice with this name.
    pub fn choice(&self, name: &str) -> Option<usize> {
        self.choices.iter().position(|c| c.model == name)
    }

    /// A choice's name.
    pub fn name_of(&self, choice: usize) -> &str {
        &self.choices[choice].model
    }

    /// The names of the choices, the default first.
    pub fn names(&self) -> Vec<String> {
        self.choices.iter().map(|c| c.model.clone()).collect()
    }

    /// The name of the choice in memory, if any. Never waits.
    pub fn loaded_name(&self) -> Option<String> {
        let i = self.current.load(Ordering::Relaxed);
        self.choices.get(i).map(|c| c.model.clone())
    }

    /// Say nothing on stderr about loading and releasing (the command's `-q`).
    pub fn quiet(mut self, quiet: bool) -> Model {
        self.quiet = quiet;
        self
    }

    fn build(&self, choice: usize) -> Result<Session, String> {
        let t = Instant::now();
        let session = self.build_inner(choice).map_err(|e| e.to_string())?;
        if !self.quiet {
            if self.choices.len() > 1 {
                eprintln!(
                    "model {} loaded in {:.1}s",
                    self.name_of(choice),
                    t.elapsed().as_secs_f32()
                );
            } else {
                eprintln!("model loaded in {:.1}s", t.elapsed().as_secs_f32());
            }
        }
        Ok(session)
    }

    fn build_inner(&self, choice: usize) -> ort::Result<Session> {
        let cfg = &self.choices[choice];
        // No arena: the default one keeps growing with every request and is
        // what pushed the Python service past its limit.
        let mut builder = Session::builder()?
            .with_execution_providers([CPU::default().with_arena_allocator(false).build()])?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_intra_threads(cfg.threads)?
            .with_inter_threads(1)?
            .with_memory_pattern(false)?;
        builder.commit_from_file(&cfg.model_path)
    }

    /// Run the model on one NCHW tensor of 3 by side by side (the side of
    /// its profile); returns the first output's single plane. One inference
    /// at a time.
    pub fn infer(&self, input: Vec<f32>) -> Result<Vec<f32>, String> {
        self.infer_with(0, input)
    }

    /// [`Model::infer`] with a chosen model. When another is in memory it is
    /// dropped first, so only one ever is.
    pub fn infer_with(&self, choice: usize, input: Vec<f32>) -> Result<Vec<f32>, String> {
        if choice >= self.choices.len() {
            return Err(format!("no model choice {choice}"));
        }
        let mut guard = self
            .session
            .lock()
            .map_err(|_| "model lock poisoned".to_string())?;
        if let Some((held, _)) = guard.as_ref() {
            if *held != choice {
                let held = *held;
                *guard = None;
                self.forget();
                if !self.quiet {
                    eprintln!(
                        "model {} released for {}",
                        self.name_of(held),
                        self.name_of(choice)
                    );
                }
            }
        }
        if guard.is_none() {
            *guard = Some((choice, self.build(choice)?));
            self.current.store(choice, Ordering::Relaxed);
            self.loaded.store(true, Ordering::Relaxed);
        }
        self.last_used
            .store(self.epoch.elapsed().as_secs().max(1), Ordering::Relaxed);
        let session = &mut guard.as_mut().expect("just built").1;
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
        let side = self.choices[choice].profile.side as usize;
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
        let idle = self.choices[0].idle_seconds;
        if self.epoch.elapsed().as_secs().saturating_sub(last) < idle {
            return false;
        }
        *guard = None;
        drop(guard);
        self.forget();
        if !self.quiet {
            eprintln!("model released after {idle}s idle");
        }
        true
    }

    /// After a session is dropped: nothing is in memory, and glibc is asked
    /// to give the freed pages back, so the RSS actually falls.
    fn forget(&self) {
        self.loaded.store(false, Ordering::Relaxed);
        self.current.store(NONE, Ordering::Relaxed);
        #[cfg(target_os = "linux")]
        unsafe {
            libc::malloc_trim(0);
        }
    }
}
