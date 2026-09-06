# background-remover

[![CI](https://github.com/daniel-oh/background-remover/actions/workflows/ci.yml/badge.svg)](https://github.com/daniel-oh/background-remover/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/daniel-oh/background-remover?display_name=tag)](https://github.com/daniel-oh/background-remover/releases)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Background removal as a small HTTP service. Send a photo, get back a PNG with
the subject on a transparent background. One Rust binary, a 76 MB image,
about 1 MB of memory while it waits and 400 MB while it works, and output
that matches the Python reference implementation (rembg's `DisSession` over
the isnet-general-use model) to within one level of alpha.

It exists because the usual way to run this model is a 4 GB Python image
that holds half a gigabyte whether or not anyone is using it. This does the
same job with the same numbers, and gets out of the way when idle.

## Install

| | |
|---|---|
| Container image | `ghcr.io/daniel-oh/background-remover`, one tag for `linux/amd64` and `linux/arm64` (Raspberry Pi 5, Graviton, Apple silicon under Docker) |
| Homebrew (macOS Apple silicon, Linux) | `brew install daniel-oh/tap/background-remover`, then `brew services start background-remover` after the one-time model download it prints |
| Binaries, on each release | Linux x86_64, Linux aarch64, macOS Apple silicon, Windows x86_64, each with a sha256, on the [releases page](https://github.com/daniel-oh/background-remover/releases) |
| Cargo | `cargo install background-remover-server` (the crate name; the binary is `background-remover`) |
| From source | any platform with a Rust toolchain that links ONNX Runtime, Intel Macs included; see Building |

## Quick start

```sh
# 1. The model (178 MB, once), into a directory you will mount read-only.
mkdir -p models/isnet-general-use
curl -sSL -o models/isnet-general-use/isnet-general-use.onnx \
  https://github.com/danielgatis/rembg/releases/download/v0.0.0/isnet-general-use.onnx
echo "60920e99c45464f2ba57bee2ad08c919a52bbf852739e96947fbb4358c0d964a  models/isnet-general-use/isnet-general-use.onnx" | sha256sum -c -

# 2. Run it.
docker run --rm -p 7000:7000 -v "$PWD/models:/models:ro" ghcr.io/daniel-oh/background-remover:latest

# 3. Use it.
curl -s --data-binary @photo.jpg -H 'content-type: image/jpeg' http://127.0.0.1:7000/remove > cutout.png
```

The model directory must be readable by uid 65532 (the image runs as
distroless `nonroot`); `chmod -R a+rX models` if in doubt. See
[`docker-compose.example.yml`](docker-compose.example.yml) for a hardened
service definition.

## HTTP contract

| Request | Response |
|---|---|
| `GET /health` | `200 ok` |
| `GET /version` | `200` JSON: `version`, `model_sha256` (first 16 hex), `loaded` |
| `GET /metrics` | `200` Prometheus text: requests by outcome, seconds, bytes in and out, model loaded |
| `POST /remove`, body: raw image bytes, `content-type: image/jpeg`, `image/png` or `image/webp` | `200`, `content-type: image/png`, RGBA at the picture's size |
| `POST /remove?format=webp` (or `Accept: image/webp`) | `200`, `image/webp`, lossless with alpha, about 20% smaller than the PNG on the test photo |
| `POST /remove?mask=1` | `200`, `image/png`, the mask alone as 8-bit greyscale, for pipelines that composite themselves |
| any other content type | `415` |
| body over 12 MiB | `413` |
| empty body, or an unknown `format` | `400` |
| more than four requests waiting | `503` |
| a request running past 75 s | `408` |
| the picture cannot be decoded, or the model fails | `500` |

One inference runs at a time; a small queue absorbs bursts. Nothing is
stored; the picture lives in memory for the duration of the request.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `MODEL_PATH` | `/models/isnet-general-use/isnet-general-use.onnx` | the ONNX model; checksummed at start |
| `MODEL_SHA256` | the isnet-general-use hash | what the model must hash to; the process exits 1 otherwise |
| `IDLE_SECONDS` | `300` | release the model after this long without a request; the next request reloads it in about 0.3 s |
| `THREADS` | `2` | ONNX Runtime intra-op threads |
| `PNG_FAST` | unset | `1` for the fast PNG encoder (about a quarter larger files, several times quicker) |
| `BIND` | `0.0.0.0` | listen address |
| `PORT` | `7000` | listen port |
| `CORS_ORIGINS` | unset | comma-separated origins allowed to call from a browser, or `*`; unset means no CORS headers |
| `MALLOC_ARENA_MAX` | `2` (set in the image) | keeps glibc from holding freed memory in extra arenas |

`background-remover --help`, `--version` and `--health` (the container
healthcheck; exit 0 when `/health` answers) are the only flags.

## How a picture is processed

The steps are the ones rembg's `DisSession` takes, done so the bytes match:

1. Decode to RGB, alpha dropped, no orientation applied. JPEGs go through
   libjpeg-turbo (via `mozjpeg`), the decoder Pillow ships, with its
   defaults; PNG and WebP through the `image` crate.
2. Resize to 1024×1024 with a port of Pillow's Lanczos resampler
   (`src/resample.rs`): the same 22-bit fixed-point coefficients, the same
   rounding, horizontal then vertical, an 8-bit intermediate. Bit-identical
   to `Image.resize(..., Image.LANCZOS)`.
3. `/255`, then `(x − 0.5) / 1.0`, as NCHW float32.
4. Run the model; take the first output's single plane.
5. Rescale the plane to 0..1 by its own min and max, `× 255`, uint8 by
   truncation (numpy's `astype("uint8")`).
6. Resize the mask back to the picture's size with the same resampler, use
   it as the alpha of the original RGB, encode PNG.

### Parity, tested

`tests/golden.rs` runs a photo through the service and compares the result
with what the Python implementation produced for the same bytes
(`testdata/reference*.png`). It requires identical colour channels, alpha
within 2 levels on every pixel and under 0.1 on average. Measured: alpha max
difference 1, mean 0.0001, on both a JPEG and a PNG fixture, on x86 (ONNX
Runtime 1.28, static) and Apple silicon (1.29, dynamic).

## Numbers

Measured on a 4-core AMD EPYC (Rome) VPS, x86_64, against the Python
service it replaced:

| | Python (rembg image) | background-remover |
|---|---|---|
| Image | 4.2 GB | 76 MB |
| Start to ready | ~20 s | 2 s (checksum of the weights) |
| Idle memory | 29 MB | 1 MB fresh, 30 MB after a release |
| Working memory | ~500 MB | 415 MB |
| Cold request (load + run) | 2.8 s | 3.1 s |
| Warm request, 1600 px photo | 2.4 s | 2.4 s |
| Runs as | root | uid 65532, read-only root, no capabilities |

The time is the model's; the language around it does not move it. What the
port buys is the image, the idle footprint, the start time and the absence
of a Python package tree.

## Other models

Any model with a `[1, 3, 1024, 1024]` float32 input and a single-plane
`[1, 1, 1024, 1024]` first output that expects the same normalisation (the
DIS / isnet family) works: point `MODEL_PATH` at it and set `MODEL_SHA256`
to its hash. The input name is read from the model, so it need not be
`input_image`.

## Checks

Every push runs, on Linux with the real model:

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo doc` with warnings as errors
- unit tests, the HTTP contract tests (in-process, no socket) and the golden parity test on both fixtures
- a smoke test of the built binary as a process: a good JPEG, a corrupt one, WebP and mask outputs, and the process still alive afterwards (`scripts/smoke.sh`)
- the same suite on a 64-bit Arm runner, and a build on the minimum supported Rust (1.88)
- `cargo deny` (licences, advisories, duplicate crates, sources), `cargo audit`, `typos`, `hadolint`
- a container build and a Trivy scan of it for known vulnerabilities

Dependabot keeps the crates, the actions and the base images current. `main` only
takes pull requests that pass all of it; release tags are immutable.

## Building

Linux (or any machine that links pyke's prebuilt ONNX Runtime):

```sh
cargo build --release
cargo test --release             # unit tests
MODEL_PATH=/path/to/isnet-general-use.onnx cargo test --release   # + golden
```

On a Mac with an older Xcode the static runtime does not link; use the
dynamic feature with Homebrew's ONNX Runtime:

```sh
brew install onnxruntime
export ORT_DYLIB_PATH=/opt/homebrew/lib/libonnxruntime.dylib
cargo test --release --no-default-features --features dynamic
GOLDEN=jpeg MODEL_PATH=... cargo test --release --no-default-features --features dynamic --test golden -- --nocapture
```

The container image builds on `rust:1.98-trixie` and runs on
`gcr.io/distroless/cc-debian13:nonroot` (the static ONNX Runtime needs
glibc 2.38 and GCC 13's libstdc++). `docker build -t background-remover .`

## Using it from code

```sh
# a lossless WebP instead of a PNG
curl -s --data-binary @photo.jpg -H 'content-type: image/jpeg' 'http://127.0.0.1:7000/remove?format=webp' > cutout.webp
# just the mask
curl -s --data-binary @photo.jpg -H 'content-type: image/jpeg' 'http://127.0.0.1:7000/remove?mask=1' > mask.png
```

```js
// Node or a browser (with CORS_ORIGINS set for the browser)
const res = await fetch("http://127.0.0.1:7000/remove", { method: "POST", headers: { "content-type": "image/jpeg" }, body: bytes });
const png = new Uint8Array(await res.arrayBuffer());
```

```python
import requests
png = requests.post("http://127.0.0.1:7000/remove", data=open("photo.jpg", "rb").read(),
                    headers={"content-type": "image/jpeg"}).content
```

## Security model

- Runs as an unprivileged user in a distroless image: no shell, no package
  manager, read-only root, all capabilities dropped (see the compose
  example).
- The model file is checksummed before the server starts; a mismatch is a
  hard failure.
- Requests are bounded: 12 MiB, three content types, one inference at a
  time, a queue of four, 75 s per request.
- Nothing is written to disk; nothing is logged but method, status, byte
  counts and milliseconds.
- Intended to sit on a private network behind your application, which does
  the authentication and rate limiting.

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md)
for how to set up, test (including regenerating a golden reference) and
what a good change looks like.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Third-party notices, including
the Pillow resampler this project ports and the model it is verified
against, are in [NOTICE](NOTICE).

Copyright © 2026 Daniel Oh.
