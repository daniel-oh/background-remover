# Contributing

Thanks for looking. This is a small project with a narrow job, so the bar for
a change is: it keeps the output identical to the reference, it keeps the
service small, and it comes with a test.

## Setting up

You need a Rust toolchain (`rust-toolchain.toml` pins the version; `rustup`
will pick it up) and, for the golden test, the model:

```sh
mkdir -p .model
curl -sSL -o .model/isnet-general-use.onnx \
  https://github.com/danielgatis/rembg/releases/download/v0.0.0/isnet-general-use.onnx
echo "60920e99c45464f2ba57bee2ad08c919a52bbf852739e96947fbb4358c0d964a  .model/isnet-general-use.onnx" | sha256sum -c -
export MODEL_PATH="$PWD/.model/isnet-general-use.onnx"
```

On Linux, `cargo test --release` builds against pyke's prebuilt ONNX Runtime.
On a Mac whose Xcode is older than that build's SDK, use the dynamic feature
with Homebrew's runtime (see the README's Building section).

## Checks a pull request must pass

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo test --release                    # unit, HTTP and golden tests (golden when MODEL_PATH is set)
GOLDEN=jpeg cargo test --release --test golden
cargo deny check                        # licences, advisories, duplicates, sources
typos && hadolint Dockerfile            # spelling, Dockerfile
```

CI runs all of these plus `cargo audit`, a build on the minimum Rust
(1.88) and a Trivy scan of the container image. Install the tools once with
`cargo install cargo-deny typos-cli` (or Homebrew) and `hadolint` from your
package manager.

## The golden test

`tests/golden.rs` is the contract with the Python implementation. It feeds
`testdata/sample.png` (and, with `GOLDEN=jpeg`, `testdata/sample.jpg`)
through the service and compares the PNG with `testdata/reference-png.png`
and `testdata/reference.png`, which were produced by rembg's `DisSession`
(`isnet-general-use`, no alpha matting, no post-processing) from the same
bytes. Colour must be identical; alpha may differ by at most 2 levels
anywhere and 0.1 on average.

If you change anything on the image path and the test moves, that is the
test doing its job: explain the difference in the pull request. To
regenerate a reference for a new fixture, run the Python side once:

```python
from rembg import new_session, remove
session = new_session("isnet-general-use")
open("reference.png", "wb").write(remove(open("sample.jpg", "rb").read(), session=session))
```

## What a good change looks like

- One thing per pull request, with the reason in the description.
- Comments say why, not what. Names are plain words.
- No new dependency without a sentence on why it earns its place; the
  service's size and surface are features.
- Performance claims come with a measurement.

## Reporting a bug

Open an issue with the template. If it is a security matter, read
[SECURITY.md](SECURITY.md) first.
