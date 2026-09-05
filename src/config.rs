//! Configuration from the environment. Every value has a default that makes a
//! bare `background-remover` run the way the container does.

use std::env;

/// The isnet-general-use weights this build was verified against.
pub const MODEL_SHA256: &str = "60920e99c45464f2ba57bee2ad08c919a52bbf852739e96947fbb4358c0d964a";

/// Largest request body accepted.
pub const MAX_BODY_BYTES: usize = 12 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Config {
    pub model_path: String,
    pub model_sha256: String,
    pub idle_seconds: u64,
    pub threads: usize,
    pub png_fast: bool,
    pub bind: String,
    pub port: u16,
}

impl Config {
    pub fn from_env() -> Config {
        Config {
            model_path: env::var("MODEL_PATH")
                .unwrap_or_else(|_| "/models/isnet-general-use/isnet-general-use.onnx".into()),
            model_sha256: env::var("MODEL_SHA256").unwrap_or_else(|_| MODEL_SHA256.into()),
            idle_seconds: parse("IDLE_SECONDS", 300),
            threads: parse("THREADS", 2),
            png_fast: env::var("PNG_FAST").map(|v| v == "1").unwrap_or(false),
            bind: env::var("BIND").unwrap_or_else(|_| "0.0.0.0".into()),
            port: parse("PORT", 7000),
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
    }
}
