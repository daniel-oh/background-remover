# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.5.0] - 2026-11-17

### Added

- **Newer models.** `MODEL` (service) and `--model NAME` (command) choose
  one by name: `isnet-general-use` (still the default), `birefnet-lite-768`,
  `birefnet-general-lite` or `birefnet-general`. A name brings the file,
  its checksum, where to fetch it and how it is fed and read. BiRefNet (MIT,
  2024) cuts out people, hair, thin structures and busy scenes visibly
  better: on six hard photos IS-Net got three visibly wrong and BiRefNet
  none. README, "Models", has the comparison and the measurements.
- **`birefnet-lite-768`**: BiRefNet_lite exported at 768 by 768 and
  published as the `models-1` release. Nearly the quality of the 1024
  export for about a third of the time and memory (3.5 s and 1.2 GB on an
  M1 Pro against 11 s and 3.2 GB), which is what lets it run on a small
  server. `scripts/export-birefnet.py` rebuilds it.
- `MODEL_DIR`: a folder of models, one folder each, `MODEL_DIR/<name>/<file>`.
  The container image sets `/models` instead of fixing `MODEL_PATH` to the
  IS-Net file.
- `MODEL_PROFILE` and `MODEL_SIDE` for a model file of one's own.
- `GET /version` names the model.
- A golden test for BiRefNet against the same model run in Python
  (alpha within one level), in CI with the real weights.

### Changed

- A model whose input size does not match what it is run at is reported as
  that, instead of as an ONNX Runtime error about dimensions.
- `MODEL=` with a name the program does not know stops it (exit 2) instead
  of running the default under the wrong name.
- For library users: `Profile`, `profile::KNOWN`, `imageops::tensor_with`,
  `mask_with`, `to_side`, `mask_from_side`, `Config::with_model`; `Config`
  has `model`, `model_url` and `profile`; `cli::Command::FetchModel` takes
  the model's name; `cli::Job` has `model_name`.

### Notes

- The resampler matches Pillow 11 and 12 bit for bit. Pillow 13.0 resizes
  some sizes differently; references must be made with 12 or earlier.

## [0.4.1] - 2026-11-17

**Use this instead of 0.4.0.** 0.4.0's container image holds a program
that does nothing, and its binaries were never published. Nothing else
differs: 0.4.1 is 0.4.0 as it was meant to go out.

### Fixed

- **The 0.4.0 image was empty.** The Dockerfile compiles a stub first so
  the dependencies are cached, then removed the stub's build records by
  path so the real sources would be compiled. Cargo 1.100 keeps those
  records somewhere else; the stub was taken for the finished program and
  shipped. The container started, did nothing, exited 0 and was restarted,
  for ever, with an empty log. The real sources are now touched after they
  are copied in, which does not depend on where Cargo keeps anything, and
  the build stops unless the binary it made prints its version.
- **The Windows binary did not build with Rust 1.100**, which is why 0.4.0
  has no binaries, crate or formula. A build step of `ort-sys`
  (`copy-dylibs`, which places shared libraries beside the binary) assumes
  the build directory layout Cargo had before 1.100. The runtime is linked
  into the binary, so the step had nothing to do, and the feature is no
  longer enabled.

### Changed

- CI runs the container it builds, the way the hardened compose example
  does, and puts a photo through it. Before, it built the image and
  scanned it: 0.4.0's built and scanned clean.
- CI builds and tests on Windows and macOS on every pull request, with the
  real model. Before, a release was the first time either was compiled.
- The release workflow runs each image and each binary and checks the
  version it reports before anything is tagged or published, and takes an
  optional `rust` when run by hand, to rebuild a tag with a Rust other
  than the one the tag names.

## [0.4.0] - 2026-11-17

Output that is right for more photos and more uses, and an API that says
what went wrong. For a photo without an orientation tag and a request
without the new options, the pixels are the same as before.

### Added

- **Phone portraits come out upright.** A phone stores a portrait on its
  side and says so in an orientation tag; the tag was ignored, and a PNG
  cannot carry it, so the cutout came back sideways (and the model saw it
  sideways). The picture is now turned first, as rembg's `remove` does.
  `?orient=0` and `--no-orient` keep the stored pixels.
- **Edge colour clean-up**, `?refine=1` and `--refine`. Soft edges (hair,
  fur, cloth) carry the old background's colour, which shows as a halo on a
  new one. The subject's own colour is estimated for those pixels; opaque
  and transparent pixels are untouched.
- **A background colour**, `?bg=RRGGBB` and `--bg`: the subject on that
  colour, opaque. Cleans the edges first unless told not to.
- **Crop to the subject**, `?crop=1` and `--crop`.
- `/metrics`: a histogram of request durations
  (`background_remover_request_duration_seconds`), the number of removals
  in the service and the queue's size.
- `QUEUE` and `REQUEST_TIMEOUT` set the queue's size and the time limit.
- The command accepts `--name=value`, and `--` before file names that
  start with a dash.

### Changed

- **A body that is not a picture is `422`, not `500`,** and counts as
  refused, not failed: `outcome="failed"` now means the service's own
  failures and is worth alerting on.
- **Errors are JSON**, `{"error": "a_code", "message": "a sentence"}`,
  instead of a bare sentence.
- **The fast PNG encoder is the default.** Measured: 30 ms instead of
  470 ms on a 1600 by 2000 cutout, for a file 6% larger; at 24 megapixels
  0.2 s instead of 2.8 s, 11% larger. The pixels are identical. `PNG_FAST=0`
  brings the smaller files back.
- **The command no longer replaces an existing output file** unless
  `--force` is given. It says so before doing the work.
- Two commands fetching the model at once no longer write the same
  temporary file.
- Rust 1.100 for the build and the image.
- For library users: `Options` has `upright`, `refine`, `background` and
  `crop`; `Stages` has `refine`; `Config` has `queue` and
  `timeout_seconds`; `http::REQUEST_TIMEOUT` is gone (it is configuration
  now) and `RemoveQuery` has the new options.

### Fixed

- The release workflow built the macOS binary on `macos-14`, an image
  GitHub has retired; the job was cancelled and v0.3.2's binaries were not
  published with its images. It now uses `macos-latest`.


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

[Unreleased]: https://github.com/daniel-oh/background-remover/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/daniel-oh/background-remover/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/daniel-oh/background-remover/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/daniel-oh/background-remover/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/daniel-oh/background-remover/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/daniel-oh/background-remover/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/daniel-oh/background-remover/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/daniel-oh/background-remover/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/daniel-oh/background-remover/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/daniel-oh/background-remover/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/daniel-oh/background-remover/releases/tag/v0.1.0
