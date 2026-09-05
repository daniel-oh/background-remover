# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
uses [Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-09-05

### Added

- The service: `POST /remove` (JPEG, PNG or WebP in, PNG with alpha out),
  `GET /health`, `GET /version`.
- isnet-general-use through ONNX Runtime, statically linked; a `dynamic`
  feature for machines that cannot link the static build.
- Pillow's Lanczos resampler ported to Rust so masks land exactly where
  Pillow's would; JPEG decoding through libjpeg-turbo for the same reason.
- A golden test against the Python reference implementation's output, run in
  CI with the real model.
- The model released after a configurable idle time; checksum verified at
  start; bounded requests; graceful shutdown.
- A distroless, non-root container image published to GitHub Container
  Registry.

[0.1.0]: https://github.com/daniel-oh/background-remover/releases/tag/v0.1.0
