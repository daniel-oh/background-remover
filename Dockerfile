# syntax=docker/dockerfile:1.10
# background-remover: a static binary in a distroless image.
#
# Two images come out of this file:
#   (default)          the program alone, 80 MB; models are mounted at /models
#   --target bundled   the program with isnet-general-use and birefnet-lite-768
#                      inside, about 450 MB; runs with nothing mounted
#
# Stage one compiles with a stub source first, so the dependency layer (which
# is where ONNX Runtime downloads and builds) is cached between changes to
# our own code. Stage two is glibc, libstdc++ and the binary, no shell, and
# runs as the distroless nonroot user (uid 65532).
#
# The real sources are touched after they are copied in, so they are newer
# than what the stub built and Cargo rebuilds the crate. Deleting the stub's
# build records by path did the same job until Cargo 1.100 moved them: the
# stub was then taken for the finished program, and v0.4.0's image shipped a
# binary that did nothing. Hence also the last line of the stage, which
# refuses to go on unless the binary built here says what version it is.

FROM rust:1.100-trixie AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir -p src tests \
  && echo 'fn main() {}' > src/main.rs \
  && echo '' > src/lib.rs \
  && cargo build --release --locked \
  && rm -rf src
COPY src ./src
RUN find src -name '*.rs' -exec touch {} + \
  && cargo build --release --locked \
  && test -n "$(target/release/background-remover --version)"

FROM gcr.io/distroless/cc-debian13:nonroot AS runtime
COPY --from=build /app/target/release/background-remover /usr/local/bin/background-remover
COPY NOTICE LICENSE-MIT LICENSE-APACHE /usr/share/doc/background-remover/
# MODEL picks the model; its weights are read from /models/<name>/<file>.
ENV MODEL_DIR=/models \
    IDLE_SECONDS=300 \
    THREADS=2 \
    PORT=7000 \
    MALLOC_ARENA_MAX=2
EXPOSE 7000
USER 65532:65532
HEALTHCHECK --interval=30s --timeout=3s --retries=3 --start-period=5s CMD ["/usr/local/bin/background-remover", "--health"]
ENTRYPOINT ["/usr/local/bin/background-remover"]

# The models built in, each fetched by the builder and refused unless it has
# the checksum the program expects (the same as src/profile.rs). The
# licences of both (Apache-2.0, MIT) are in /usr/share/doc/background-remover.
# BiRefNet by default, IS-Net for requests that ask for it: one model is in
# memory at a time; it peaks at 0.9 GB, so a 1200 MB limit covers both.
# --chmod sets the folders ADD creates as well as the file, so 0444 left the
# folders untraversable; 0555 reads for everyone and writes for no one.
FROM runtime AS bundled
ADD --link --chmod=0555 \
    --checksum=sha256:60920e99c45464f2ba57bee2ad08c919a52bbf852739e96947fbb4358c0d964a \
    https://github.com/danielgatis/rembg/releases/download/v0.0.0/isnet-general-use.onnx \
    /models/isnet-general-use/isnet-general-use.onnx
ADD --link --chmod=0555 \
    --checksum=sha256:16eedf6a83dc37dfa021651cb856590e640050938486fbdfc96404c2ed74418c \
    https://github.com/daniel-oh/background-remover/releases/download/models-1/birefnet-lite-768.onnx \
    /models/birefnet-lite-768/birefnet-lite-768.onnx
ENV MODEL=birefnet-lite-768 \
    MODELS=isnet-general-use

# Last, so a plain `docker build` still makes the program alone.
FROM runtime AS slim
