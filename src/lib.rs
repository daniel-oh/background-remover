//! Background removal as a small HTTP service.
//!
//! Raw image bytes in, a PNG with alpha out. The matting model
//! (`isnet-general-use`, or any model of the same shape) runs through ONNX
//! Runtime; the image work before and after it does exactly what Pillow and
//! rembg's `DisSession` do, so results are reproducible against the Python
//! reference implementation to within a level of alpha.

#![warn(missing_docs)]

pub mod config;
pub mod http;
pub mod imageops;
pub mod model;
pub mod resample;

/// The crate version, for `--version` and `GET /version`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
