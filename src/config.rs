//! Configuration from the environment. Every value has a default; the
//! container image sets `MODEL_PATH` to its mounted volume, and outside it
//! the model lives in the user's cache directory (see [`crate::fetch`]).

use std::env;

use crate::profile::{self, Known, Profile};

/// The checksum of the default model (isnet-general-use). The other known
/// models carry theirs in [`crate::profile::KNOWN`].
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
    /// The model's name, as `MODEL` gave it: one of
    /// [`crate::profile::KNOWN`], or a name that is not known (the program
    /// refuses to start on one).
    pub model: String,
    /// Where the model's weights are fetched from.
    pub model_url: String,
    /// How the model is fed and read.
    pub profile: Profile,
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

    /// This configuration with another known model in place of its own:
    /// that model's file in the cache directory, its checksum, its source
    /// and its profile. What `--model NAME` does.
    pub fn with_model(&self, known: &Known) -> Config {
        Config {
            model: known.name.into(),
            model_url: known.url.into(),
            profile: known.profile,
            model_path: cache_path(known),
            model_path_set: false,
            model_sha256: known.sha256.into(),
            ..self.clone()
        }
    }

    /// The configuration from any source of variables: the environment in
    /// the program, a fixed table in a test.
    pub fn from_lookup(var: impl Fn(&str) -> Option<String>) -> Config {
        fn parse<T: std::str::FromStr>(v: Option<String>, fallback: T) -> T {
            v.and_then(|v| v.parse().ok()).unwrap_or(fallback)
        }
        let model = var("MODEL").unwrap_or_else(|| profile::DEFAULT.into());
        // An unknown name keeps the default's details here; `main` reports
        // it and stops, so nothing runs with a mixture.
        let known = profile::known(&model)
            .or(profile::known(profile::DEFAULT))
            .expect("the default model is known");
        let model_path = var("MODEL_PATH");
        Config {
            model,
            model_url: var("MODEL_URL").unwrap_or_else(|| known.url.into()),
            profile: {
                let p = var("MODEL_PROFILE")
                    .and_then(|p| Profile::named(&p))
                    .unwrap_or(known.profile);
                match var("MODEL_SIDE").and_then(|v| v.parse().ok()) {
                    Some(side) if side >= 32 => p.at(side),
                    _ => p,
                }
            },
            model_path_set: model_path.is_some(),
            // MODEL_DIR holds one folder per model, as the container image
            // lays them out: /models/<name>/<file>.
            model_path: model_path.unwrap_or_else(|| match var("MODEL_DIR") {
                Some(dir) => std::path::Path::new(&dir)
                    .join(known.name)
                    .join(known.file)
                    .to_string_lossy()
                    .into_owned(),
                None => cache_path(known),
            }),
            model_sha256: var("MODEL_SHA256").unwrap_or_else(|| known.sha256.into()),
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

/// Where a known model lives in the per-user cache directory.
fn cache_path(known: &Known) -> String {
    crate::fetch::cache_dir()
        .join(known.file)
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_ones() {
        // Not the real environment: a shell with PORT set must not fail this.
        let c = Config::from_lookup(|_| None);
        assert_eq!(c.max_pixels, None);
        assert_eq!(c.model, "isnet-general-use");
        assert_eq!(c.profile, Profile::ISNET);
        assert!(c.model_url.ends_with("/isnet-general-use.onnx"));
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
    fn naming_a_model_brings_its_file_checksum_source_and_profile() {
        let c = Config::from_lookup(|name| match name {
            "MODEL" => Some("birefnet-general-lite".into()),
            _ => None,
        });
        let lite = profile::known("birefnet-general-lite").unwrap();
        assert_eq!(c.profile, Profile::BIREFNET);
        assert_eq!(c.model_sha256, lite.sha256);
        assert_eq!(c.model_url, lite.url);
        assert!(c.model_path.ends_with("birefnet-general-lite.onnx"));
        // The same from a default configuration, as --model does it.
        let d = Config::from_lookup(|_| None).with_model(lite);
        assert_eq!(
            (d.model_path, d.model_sha256),
            (c.model_path, c.model_sha256)
        );
        assert_eq!(d.profile, Profile::BIREFNET);
    }

    #[test]
    fn a_models_folder_holds_one_folder_per_model() {
        let c = Config::from_lookup(|name| match name {
            "MODEL_DIR" => Some("/models".into()),
            "MODEL" => Some("birefnet-lite-768".into()),
            _ => None,
        });
        // Path::join writes backslashes on Windows.
        let path = |p: &str| p.replace('\\', "/");
        assert_eq!(
            path(&c.model_path),
            "/models/birefnet-lite-768/birefnet-lite-768.onnx"
        );
        assert_eq!(c.profile.side, 768);
        let d = Config::from_lookup(|name| (name == "MODEL_DIR").then(|| "/models".into()));
        assert_eq!(
            path(&d.model_path),
            "/models/isnet-general-use/isnet-general-use.onnx"
        );
    }

    #[test]
    fn a_file_of_ones_own_can_be_given_a_profile() {
        let c = Config::from_lookup(|name| match name {
            "MODEL_PATH" => Some("/mine.onnx".into()),
            "MODEL_SHA256" => Some("ab".repeat(32)),
            "MODEL_PROFILE" => Some("birefnet".into()),
            "MODEL_SIDE" => Some("512".into()),
            _ => None,
        });
        assert_eq!(c.profile, Profile::BIREFNET.at(512));
        assert_eq!(c.model_path, "/mine.onnx");
        assert_eq!(c.model_sha256, "ab".repeat(32));
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
