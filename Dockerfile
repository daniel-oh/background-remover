# background-remover: a static binary in a distroless image.
#
# Stage one compiles with a stub source first, so the dependency layer (which
# is where ONNX Runtime downloads and builds) is cached between changes to
# our own code. Stage two is glibc, libstdc++ and the binary, no shell, and
# runs as the distroless nonroot user (uid 65532).

FROM rust:1.98-trixie AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir -p src tests \
  && echo 'fn main() {}' > src/main.rs \
  && echo '' > src/lib.rs \
  && cargo build --release --locked \
  && rm -rf src target/release/.fingerprint/background* target/release/deps/background* target/release/deps/libbackground* target/release/background-remover*
COPY src ./src
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /app/target/release/background-remover /usr/local/bin/background-remover
ENV MODEL_PATH=/models/isnet-general-use/isnet-general-use.onnx \
    IDLE_SECONDS=300 \
    THREADS=2 \
    PORT=7000 \
    MALLOC_ARENA_MAX=2
EXPOSE 7000
USER 65532:65532
HEALTHCHECK --interval=30s --timeout=3s --retries=3 --start-period=5s CMD ["/usr/local/bin/background-remover", "--health"]
ENTRYPOINT ["/usr/local/bin/background-remover"]
