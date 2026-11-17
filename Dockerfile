# background-remover: a static binary in a distroless image.
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

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /app/target/release/background-remover /usr/local/bin/background-remover
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
