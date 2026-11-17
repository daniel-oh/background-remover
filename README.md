# background-remover

[![CI](https://github.com/daniel-oh/background-remover/actions/workflows/ci.yml/badge.svg)](https://github.com/daniel-oh/background-remover/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/daniel-oh/background-remover?display_name=tag)](https://github.com/daniel-oh/background-remover/releases)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Background removal as a command and as a small HTTP service. Give it a photo,
get back a PNG with the subject on a transparent background. One Rust binary, a 76 MB image,
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

## Command line

```sh
brew install daniel-oh/tap/background-remover      # or a release binary, or cargo install

background-remover photo.jpg                        # writes photo-cutout.png beside it
background-remover photo.jpg -o cutout.webp         # lossless WebP, from the extension
background-remover --mask photo.jpg                 # the mask alone, photo-mask.png
background-remover -d out/ *.jpg                    # a batch through one loaded model
background-remover --bg ffffff --crop photo.jpg     # on white, trimmed to the subject
cat photo.jpg | background-remover - > cutout.png   # stdin to stdout
```

`--model birefnet-lite-768` (or `birefnet-general`) uses a newer model that
cuts out people, hair and busy scenes better; see [Models](#models).

The first run fetches the model (178 MB) into the cache directory and
verifies its checksum; `background-remover --fetch-model` does that on its
own and prints the path. The command uses up to eight cores: on an M1 Pro
a 1600 px photo takes 1.3 s once the model is loaded (0.2 s), and a batch
runs at that rate. Output is the same bytes the service produces at the same
thread count (`-j 2`); ONNX Runtime sums in a different order across thread
counts, which on the test photo moves 19 of 1.15 million alpha values by one
level. An output file that already exists is left alone unless `--force`
is given. `--help` lists every option.

## Service quick start

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
| `POST /remove?bg=RRGGBB` | the subject on that colour, opaque. Cleans the edges' colour first (see `refine`) unless `refine=0` |
| `POST /remove?refine=1` | the old background's colour taken out of the soft edges, so hair and fur do not carry a halo onto a new background |
| `POST /remove?crop=1` | the output trimmed to the subject |
| `POST /remove?orient=0` | the picture as stored, ignoring its orientation tag (by default a phone portrait comes back upright) |
| any other content type | `415` |
| body over 12 MiB, or a picture over `MAX_PIXELS` (25 megapixels) | `413` |
| empty body, or a bad option | `400` |
| the body is not a picture, or a broken one | `422` |
| four removals already in the service (one running, three waiting) | `503` |
| a request running past 75 s | `408` |
| the model fails | `500` |

Errors are JSON, `{"error": "too_many_pixels", "message": "the picture has
too many pixels"}`: a code for a program, a sentence for a person. The codes
are `unsupported_type`, `bad_option`, `empty_body`, `too_large`,
`too_many_pixels`, `undecodable`, `busy` and `failed`.

One picture is worked on at a time, from decode to encode, so memory in use
is one picture's however many are waiting; a small queue absorbs bursts. A
picture's size is read from its header and checked before anything is
decoded, because a file of a few kilobytes can declare a picture of a
gigabyte. A request keeps its place in the queue until its work has really
ended, also when its caller has stopped waiting. Nothing is stored; the
picture lives in memory for the duration of the request.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `MODEL` | `isnet-general-use` | which model: `isnet-general-use`, `birefnet-lite-768`, `birefnet-general-lite` or `birefnet-general` (see [Models](#models)). Brings its file name, checksum, source and how it is fed and read; an unknown name stops the program |
| `MODEL_DIR` | unset; the image sets `/models` | a folder with one folder per model, `MODEL_DIR/<name>/<file>.onnx` |
| `MODEL_PATH` | `MODEL_DIR`, else the cache directory (`~/Library/Caches/background-remover/` on macOS, `$XDG_CACHE_HOME` or `~/.cache` on Linux) | the ONNX file itself; checksummed at start. The service never downloads it; the command does, once |
| `MODEL_PROFILE`, `MODEL_SIDE` | from `MODEL` | for a file of your own: `isnet` or `birefnet` (how the picture is scaled in and the mask read out), and the square it takes |
| `MODEL_URL` | the rembg release asset | where the command fetches the model from |
| `MODEL_SHA256` | the isnet-general-use hash | what the model must hash to; the process exits 1 otherwise |
| `IDLE_SECONDS` | `300` | release the model after this long without a request; the next request reloads it in about 0.3 s |
| `THREADS` | `2` for the service; up to eight cores for the command (`-j`) | ONNX Runtime intra-op threads |
| `PNG_FAST` | on | `0` for the slow PNG encoder. Measured on an M1 Pro: a 1600×2000 cutout encodes in 30 ms fast and 470 ms slow, and the slow file is 6% smaller; at 24 megapixels, 0.2 s against 2.8 s for 11% |
| `QUEUE` | `4` | removals the service holds at once, the running one included; one more gets `503` |
| `REQUEST_TIMEOUT` | `75` | seconds before a request is answered `408` |
| `MAX_PIXELS` | `25000000` for the service; no limit for the command | the largest picture decoded, in pixels (`0` for no limit). At the default the service stays inside the example's 768 MB |
| `BIND` | `0.0.0.0` | listen address |
| `PORT` | `7000` | listen port |
| `CORS_ORIGINS` | unset | comma-separated origins allowed to call from a browser, or `*`; unset means no CORS headers |
| `MALLOC_ARENA_MAX` | `2` (set in the image) | keeps glibc from holding freed memory in extra arenas |

`background-remover --help` lists the command-line options; `--version`,
`--fetch-model` and `--health` (the container healthcheck; exit 0 when
`/health` answers) are the other flags.

## How a picture is processed

The steps are the ones rembg's `DisSession` takes, done so the bytes match:

1. Decode to RGB, alpha dropped. JPEGs go through libjpeg-turbo (via
   `mozjpeg`), the decoder Pillow ships, with its defaults; PNG and WebP
   through the `image` crate. Then the picture is turned the way its
   orientation tag says, as rembg's `remove` does with
   `ImageOps.exif_transpose`: a phone stores a portrait on its side with a
   tag, and a PNG cannot carry the tag along.
2. Resize to 1024×1024 with a port of Pillow's Lanczos resampler
   (`src/resample.rs`): the same 22-bit fixed-point coefficients, the same
   rounding, horizontal then vertical, an 8-bit intermediate. Bit-identical
   to `Image.resize(..., Image.LANCZOS)` in Pillow 11 and 12, which the
   references were made with. Pillow 13.0 resizes some sizes differently
   (up to 24 levels on the test photo at 1024 square, none at others).
3. `/255`, then `(x − 0.5) / 1.0`, as NCHW float32.
4. Run the model; take the first output's single plane.
5. Rescale the plane to 0..1 by its own min and max, `× 255`, uint8 by
   truncation (numpy's `astype("uint8")`).
6. Resize the mask back to the picture's size with the same resampler, use
   it as the alpha of the original RGB, encode PNG.

Up to here the output is the reference's. Three optional steps go further,
each off unless asked for:

- **`refine`** estimates the subject's own colour for partly transparent
  pixels (`src/refine.rs`: the blur fusion of Forte and Pitié, two passes).
  An edge pixel is a mix of subject and old background; the mask says how
  much of each, but the stored colour is still the mix. Opaque and
  transparent pixels are not touched. Measured on an M1 Pro: 0.2 s on a 1600×2000
  picture, 0.8 s at 24 megapixels.
- **`bg`** composites onto a colour and writes an opaque picture.
- **`crop`** trims to the box around the subject (alpha of 5% and up).

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

## Models

| `MODEL` | Size | Licence | |
|---|---|---|---|
| `isnet-general-use` (default) | 178 MB | Apache-2.0 | IS-Net (2022), as rembg ships it. Quickest and lightest |
| `birefnet-lite-768` | 187 MB | MIT | BiRefNet_lite at 768 by 768, exported by this project ([`models-1`](https://github.com/daniel-oh/background-remover/releases/tag/models-1), `scripts/export-birefnet.py`). The best cutout that fits a small server |
| `birefnet-general-lite` | 214 MB | MIT | The same model at 1024, the author's export |
| `birefnet-general` | 928 MB | MIT | BiRefNet with the large backbone, the author's export. The best cutout here |

Compared by eye on six hard photos (a portrait against the sea, an old
portrait with wild hair, a bicycle in a street, a cat, a dog, a dandelion
in grass), IS-Net made a visible mistake on three: a piece of the sleeve
cut away, a ghost of the van parked behind the bicycle, the dandelion's
stem lost and grass kept. The three BiRefNet models got all six right; the
1024 ones are a little cleaner between bicycle spokes than the 768 one.

Measured on an M1 Pro, two threads, a 1920 by 2397 portrait:

| | Time | Peak memory |
|---|---|---|
| `isnet-general-use` | 2.1 s | 0.7 GB |
| `birefnet-lite-768` | 3.5 s | 1.2 GB in the container, 1.4 GB as the command |
| `birefnet-general-lite` | 11 s | 3.2 GB |
| `birefnet-general` | 24 s | 3.8 GB |

The BiRefNet models take ImageNet-scaled input and give logits, so the
program scales and reads them differently (`src/profile.rs`); what comes out
is checked against the same model run in Python
(`testdata/reference-birefnet-lite-768-mask.png`, alpha within one level).

A model of your own works if it takes a square float32 picture and gives a
single-plane first output: set `MODEL_PATH`, `MODEL_SHA256`, `MODEL_PROFILE`
and, if it is not 1024, `MODEL_SIDE`. The input name is read from the model.

In the container image the models live in `/models/<name>/<file>.onnx`:

```sh
mkdir -p models/birefnet-lite-768
curl -sSL -o models/birefnet-lite-768/birefnet-lite-768.onnx \
  https://github.com/daniel-oh/background-remover/releases/download/models-1/birefnet-lite-768.onnx
docker run --rm -p 7000:7000 -e MODEL=birefnet-lite-768 -v "$PWD/models:/models:ro" \
  ghcr.io/daniel-oh/background-remover:latest
```

## Checks

Every push runs, on Linux with the real model:

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo doc` with warnings as errors
- unit tests, the HTTP contract tests (in-process, no socket) and the golden parity test on both fixtures
- the limits: a small file that declares a 900-megapixel picture is refused from its header, a burst of six gets four answers and two "later", an abandoned request keeps its place until its turn, and the status endpoints answer while the model is busy
- a smoke test of the built binary as a process: a good JPEG, a corrupt one, WebP and mask outputs, the process still alive afterwards, and the command-line mode on a file, on stdin and on junk (`scripts/smoke.sh`)
- the command-line mode as a real process against the golden reference, and the model fetch against a local HTTP server, with a wrong checksum rejected
- the same suite on a 64-bit Arm runner, and a build on the minimum supported Rust (1.88)
- `cargo deny` (licences, advisories, duplicate crates, sources), `cargo audit`, `typos`, `hadolint`
- a container build and a Trivy scan of it for known vulnerabilities

The log line for each removal says where its time went (`decode`, `resize`,
`model`, `encode`, in milliseconds).

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

The container image builds on `rust:1.100-trixie` and runs on
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
- Requests are bounded: 12 MiB, 25 megapixels (checked from the header,
  before decoding), three content types, one picture in memory at a time,
  four in the service, 75 s per request. `/metrics` has the queue's depth
  and a histogram of durations to alert on.
- The workflows that build and publish run actions pinned to a commit, and
  release images carry signed build provenance and an SBOM.
- The service writes nothing to disk and logs only method, status, byte
  counts and milliseconds. Only the command-line mode ever downloads the
  model, and only when no `MODEL_PATH` or `--model` was given.
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
