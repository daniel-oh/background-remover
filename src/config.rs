//! Configuration from the environment. Every value has a default; the
//! container image sets `MODEL_PATH` to its mounted volume, and outside it
//! the model lives in the user's cache directory (see [`crate::fetch`]).

use std::env;

/// The isnet-general-use weights this build was verified against.
pub const MODEL_SHA256: &str = "60920e99c45464f2ba57bee2ad08c919a52bbf852739e96947fbb4358c0d964a";

/// Largest request body accepted.
pub const MAX_BODY_BYTES: usize = 12 * 1024 * 1024;

/// Largest picture the service decodes unless `MAX_PIXELS` says otherwise:
/// 25 megapixels, a 24-megapixel camera frame with room to spare. The body
/// limit alone does not bound memory, because a small file can declare a
/// very large picture.
pub const MAX_PIXELS: u64 = 25_000_000;

/// Everything the service reads from its environment.
#[derive(Clone, Debug)]
pub struct Config {
    /// Path to the ONNX model.
    pub model_path: String,
    /// Whether `MODEL_PATH` was set, rather than the cache default used.
    pub model_path_set: bool,
    /// The hash the model must have.
    pub model_sha256: String,
    /// Seconds without a request before the model is released.
    pub idle_seconds: u64,
    /// ONNX Runtime intra-op threads.
    pub threads: usize,
    /// Use the fast PNG encoder (the default; `PNG_FAST=0` for the smaller,
    /// slower one).
    pub png_fast: bool,
    /// Pixels the service decodes at most; `None` when `MAX_PIXELS` is not
    /// set (the service then uses [`MAX_PIXELS`], the command no limit), and
    /// `Some(0)` for no limit.
    pub max_pixels: Option<u64>,
    /// Removals the service holds at once, the running one included.
    pub queue: usize,
    /// Seconds a request may take before the service answers 408.
    pub timeout_seconds: u64,
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
        Config::from_lookup(|name| env::var(name).ok())
    }

    /// The configuration from any source of variables: the environment in
    /// the program, a fixed table in a test.
    pub fn from_lookup(var: impl Fn(&str) -> Option<String>) -> Config {
        fn parse<T: std::str::FromStr>(v: Option<String>, fallback: T) -> T {
            v.and_then(|v| v.parse().ok()).unwrap_or(fallback)
        }
        let model_path = var("MODEL_PATH");
        Config {
            model_path_set: model_path.is_some(),
            model_path: model_path.unwrap_or_else(|| {
                crate::fetch::default_model_path()
                    .to_string_lossy()
                    .into_owned()
            }),
            model_sha256: var("MODEL_SHA256").unwrap_or_else(|| MODEL_SHA256.into()),
            idle_seconds: parse(var("IDLE_SECONDS"), 300),
            threads: parse(var("THREADS"), 2),
            png_fast: var("PNG_FAST").is_none_or(|v| v != "0"),
            queue: parse(var("QUEUE"), 4).max(1),
            timeout_seconds: parse(var("REQUEST_TIMEOUT"), 75).max(1),
            max_pixels: var("MAX_PIXELS").and_then(|v| v.parse().ok()),
            bind: var("BIND").unwrap_or_else(|| "0.0.0.0".into()),
            port: parse(var("PORT"), 7000),
            cors_origins: var("CORS_ORIGINS")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_ones() {
        // Not the real environment: a shell with PORT set must not fail this.
        let c = Config::from_lookup(|_| None);
        assert_eq!(c.max_pixels, None);
        assert!(!c.model_path_set);
        assert_eq!(c.port, 7000);
        assert_eq!(c.threads, 2);
        assert_eq!(c.idle_seconds, 300);
        assert_eq!(c.model_sha256, MODEL_SHA256);
        assert!(c.png_fast);
        assert_eq!(c.queue, 4);
        assert_eq!(c.timeout_seconds, 75);
        assert!(c.cors_origins.is_empty());
        assert!(c.model_path.ends_with("isnet-general-use.onnx"));
    }

    #[test]
    fn variables_override_the_defaults() {
        let c = Config::from_lookup(|name| match name {
            "MODEL_PATH" => Some("/m.onnx".into()),
            "MAX_PIXELS" => Some("0".into()),
            "PNG_FAST" => Some("0".into()),
            "QUEUE" => Some("0".into()),
            "CORS_ORIGINS" => Some("https://a.example, https://b.example,".into()),
            "PORT" => Some("not a port".into()),
            _ => None,
        });
        assert_eq!(c.model_path, "/m.onnx");
        assert!(c.model_path_set);
        assert_eq!(c.max_pixels, Some(0));
        assert!(!c.png_fast);
        assert_eq!(c.queue, 1, "a queue of none would refuse everything");
        assert_eq!(c.cors_origins.len(), 2);
        assert_eq!(c.port, 7000, "an unreadable value falls back");
    }
}
