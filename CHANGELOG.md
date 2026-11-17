# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.3.2] - 2026-11-17

A review of the service found three ways a request could take it down or
past its limits. All three are closed; a valid request gets the same bytes
back as before.

### Security

- **A small file could exhaust the memory.** Only the upload's size was
  limited (12 MiB), not the picture's. A JPEG well under that can declare
  hundreds of megapixels, and decoding it needs gigabytes: in a container
  with a memory limit the kernel kills the process. The size is now read
  from the header and checked before anything is decoded. The service
  refuses more than 25 megapixels with `413` (`MAX_PIXELS` changes it, `0`
  removes it); the command has no limit unless `MAX_PIXELS` is set.
- **The queue stopped counting after a timeout.** A request's place in the
  queue was held by its handler. When the 75 s timeout fired the place was
  freed, but the work could not be cancelled and went on, so slow requests
  could pile up past four. The place now belongs to the work and is freed
  when the work ends. A request whose caller has gone before its turn came
  is dropped without running.
- **Several pictures were in memory at once.** Only the model ran one at a
  time; up to four pictures were decoded and resized together. One picture
  is now worked on at a time, from decode to encode.
- tokio 1.54.1 (RUSTSEC-2026-0386), rustls-webpki 0.103.17
  (RUSTSEC-2026-0382) and webpki-roots 1.0.12 (RUSTSEC-2026-0381). The TLS
  ones touch only the command's model download, which is checksummed.
- Workflow actions are pinned to a commit instead of a tag, which can be
  moved. Release images carry build provenance and an SBOM.

### Fixed

- `GET /version` and `GET /metrics` waited for the running removal to end,
  on one of two worker threads: two status calls during a removal held up
  every other request, `/health` included. They now answer at once.
- The release workflow wrote the GitHub release from four build jobs at
  once. They raced, and 0.3.1 was first published with two of its eight
  files. Binaries are still built in parallel but one job now publishes
  them, one file at a time with retries (GitHub rejects concurrent uploads
  to a release, including the release action's own), all eight or none. An
  existing tag can be published again with
  `gh workflow run release.yml -f tag=vX.Y.Z`.

### Added

- The log line of each removal says where the time went:
  `1600x2000 in=745238 out=3745894 2826ms (decode=34ms resize=108ms model=2210ms encode=473ms)`.
- `imageops::cutout_with`, taking `Options` and returning the picture's size
  and stage timings with the bytes; `decode_within`; `AppState::with_model`;
  `Config::from_lookup`. `cutout`, `cutout_as` and `decode` are unchanged.

### Changed

- `CutoutError` has a `TooLarge` variant and is now `#[non_exhaustive]`;
  `AppState` has a `work` field and `Config` has `max_pixels` and
  `model_path_set`. Code that matches the error exhaustively or builds
  those structs field by field needs a line.
- The plain RGB and alpha copies are released before encoding, and a JPEG
  is decoded into its final buffer instead of being copied once more.
- Rust 1.99 for the build and the image; every dependency current (`ort`
  stays pinned). Dependabot sends one grouped pull request a week for
  crates and one for actions: separate ones blocked each other whenever two
  advisories were open, which is how six sat failing for a month.
- The PNG_FAST note in the README is now a measurement (6 to 11% larger,
  15 times quicker), and "four waiting" is corrected to four in the
  service, the running one included.

## [0.3.1] - 2026-09-17

### Security

- rustls 0.23.43 to 0.23.45 for RUSTSEC-2026-0285 (TLS 1.3 handshake
  messages accepted across encryption-level boundaries). rustls is what the
  command-line mode downloads the model over; the service never makes
  outbound connections. The handshake transcript is still authenticated, so
  the advisory does not let an attacker alter a download, and the model is
  checksummed after fetching either way.

### Changed

- Every other dependency to its latest compatible release (ureq 3.4.2 and
  ten transitive crates). `ort` stays pinned: the static ONNX Runtime
  binaries are matched to that release.
- CI: current majors for the Docker and artifact actions (upload and
  download moved together; they are a pair), hadolint-action 3.5.0, and
  `crate-ci/typos` on a release tag instead of `master`. Jobs run with a
  read-only token; `audit` gets `checks: write` so an advisory is reported
  instead of failing on a permissions error.
- Dependabot no longer offers to raise `dtolnay/rust-toolchain@1.88`. That
  ref is the minimum supported Rust version, not an action version.

### Fixed

- README: the command's output matches the service's byte for byte at the
  same thread count, not at any thread count.

## [0.3.0] - 2026-09-06

### Added

- A command-line mode: `background-remover photo.jpg` writes
  `photo-cutout.png` beside it; `-` reads stdin and writes stdout; several
  files run through one loaded model; `-o`, `-d`, `-f webp`, `--mask`,
  `-j`, `-q`. Same pipeline and parity as the service. It uses up to eight
  cores by default (1.3 s a photo on an M1 Pro), where the service keeps
  its two-thread default.
- The model is fetched on first use in command-line mode, checksummed, into
  the user's cache directory (`~/Library/Caches/background-remover`,
  `$XDG_CACHE_HOME/background-remover`, `%LOCALAPPDATA%\background-remover`);
  `--fetch-model` does it explicitly and prints the path; `--no-download`
  refuses. The service never downloads: it still needs the model in place
  and exits with a hint otherwise.
- `MODEL_PATH` now defaults to that cache path outside the container image,
  so the Homebrew service needs no configuration after `--fetch-model`.

## [0.2.2] - 2026-09-06

### Added

- A Homebrew tap (`brew install daniel-oh/tap/background-remover`) with the
  prebuilt binaries and a `brew services` definition; `scripts/formula.sh`
  writes the formula and the release workflow pushes it.
- Publishing to crates.io as `background-remover-server` (the plain name was
  taken by an unrelated crate); the binary is still `background-remover`.
- CI packages the crate on every change so a publish cannot fail on tag day.

### Changed

- Cargo package name is `background-remover-server`. Nothing else is renamed.

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

[Unreleased]: https://github.com/daniel-oh/background-remover/compare/v0.3.2...HEAD
[0.3.2]: https://github.com/daniel-oh/background-remover/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/daniel-oh/background-remover/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/daniel-oh/background-remover/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/daniel-oh/background-remover/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/daniel-oh/background-remover/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/daniel-oh/background-remover/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/daniel-oh/background-remover/releases/tag/v0.1.0
