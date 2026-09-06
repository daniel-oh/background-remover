# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
uses [Semantic Versioning](https://semver.org/).

## [0.2.1] - 2026-09-06

### Fixed

- A corrupt JPEG (a valid marker followed by junk) killed the process. libjpeg
  reports errors by unwinding through the mozjpeg crate, and the release
  profile was built with `panic = "abort"`, so the catch never ran; tests
  never saw it because cargo always builds tests with unwinding. The profile
  no longer aborts, the decoder catches the unwind explicitly, and
  `scripts/smoke.sh` runs the built binary against such a file in CI.

## [0.2.0] - 2026-09-06

### Added

- `?format=webp` (or `Accept: image/webp`) returns a lossless WebP with
  alpha, about 20% smaller than the PNG on the test photo; `?mask=1` returns the mask alone
  as an 8-bit greyscale PNG.
- `GET /metrics`, Prometheus text: requests by outcome, seconds spent, bytes
  in and out, whether the model is loaded.
- `CORS_ORIGINS` for calling the service from a browser.
- Multi-architecture images (`linux/amd64`, `linux/arm64`) under one tag, and
  release binaries for Linux x86_64 and aarch64, macOS Apple silicon and
  Windows x86_64.
- HTTP contract tests driven in-process, a floating-point reference test for
  the resampler, rustdoc warnings as errors, a minimum-Rust build, licence and
  advisory policy (`cargo deny`), spelling and Dockerfile checks, and a Trivy
  scan of the image in CI.
- Code owners, issue templates with private security routing, and rulesets
  protecting `main` and release tags.

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

[0.2.1]: https://github.com/daniel-oh/background-remover/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/daniel-oh/background-remover/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/daniel-oh/background-remover/releases/tag/v0.1.0
