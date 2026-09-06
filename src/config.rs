//! Configuration from the environment. Every value has a default; the
//! container image sets `MODEL_PATH` to its mounted volume, and outside it
//! the model lives in the user's cache directory (see [`crate::fetch`]).

use std::env;

/// The isnet-general-use weights this build was verified against.
pub const MODEL_SHA256: &str = "60920e99c45464f2ba57bee2ad08c919a52bbf852739e96947fbb4358c0d964a";

/// Largest request body accepted.
pub const MAX_BODY_BYTES: usize = 12 * 1024 * 1024;

/// Everything the service reads from its environment.
#[derive(Clone, Debug)]
pub struct Config {
    /// Path to the ONNX model.
    pub model_path: String,
    /// The hash the model must have.
    pub model_sha256: String,
    /// Seconds without a request before the model is released.
    pub idle_seconds: u64,
    /// ONNX Runtime intra-op threads.
    pub threads: usize,
    /// Use the fast PNG encoder.
    pub png_fast: bool,
    /// Address to listen on.
    pub bind: String,
    /// Port to listen on.
    pub port: u16,
    /// Origins allowed to call from a browser; empty means no CORS headers.
    pub cors_origins: Vec<String>,
}

impl Config {
    /// Read the configuration, falling back to the documented defaults.
    pub fn from_env() -> Config {
        Config {
            model_path: env::var("MODEL_PATH").unwrap_or_else(|_| {
                crate::fetch::default_model_path()
                    .to_string_lossy()
                    .into_owned()
            }),
            model_sha256: env::var("MODEL_SHA256").unwrap_or_else(|_| MODEL_SHA256.into()),
            idle_seconds: parse("IDLE_SECONDS", 300),
            threads: parse("THREADS", 2),
            png_fast: env::var("PNG_FAST").map(|v| v == "1").unwrap_or(false),
            bind: env::var("BIND").unwrap_or_else(|_| "0.0.0.0".into()),
            port: parse("PORT", 7000),
            cors_origins: env::var("CORS_ORIGINS")
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

fn parse<T: std::str::FromStr>(name: &str, fallback: T) -> T {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_ones() {
        let c = Config::from_env();
        assert_eq!(c.port, 7000);
        assert_eq!(c.threads, 2);
        assert_eq!(c.idle_seconds, 300);
        assert_eq!(c.model_sha256, MODEL_SHA256);
        assert!(!c.png_fast);
        assert!(c.cors_origins.is_empty());
        assert!(c.model_path.ends_with("isnet-general-use.onnx"));
    }
}
