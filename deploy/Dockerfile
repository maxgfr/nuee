# syntax=docker/dockerfile:1
# Nuée anchor image: `nuee serve --anchor` on distroless, as a non-root user.
# Build from the repository root: docker build -f deploy/Dockerfile .

# Base images are pinned by digest (tag kept for reading); Dependabot's docker ecosystem bumps both.
FROM rust:1.99-bookworm@sha256:114c7a4425406451c2866b6aafe69fe29b1b298832db1277d411ac73c82d04d6 AS builder
WORKDIR /build/nuee
# Only what the build needs is copied: the context's target/ directories stay out.
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY .cargo .cargo
COPY crates crates
COPY scripts scripts
COPY tasks/Cargo.toml tasks/Cargo.lock tasks/
COPY tasks/.cargo tasks/.cargo
COPY tasks/golden tasks/golden
COPY tasks/nuee-task tasks/nuee-task
COPY tasks/mandelbrot tasks/mandelbrot
COPY tasks/skycmd-arena tasks/skycmd-arena
COPY tasks/toy-trainer tasks/toy-trainer
# Jobs reach an anchor with their module, so no task is built here; `demo:mandelbrot` and
# `demo:train` use the golden modules embedded in the binary.
RUN cargo build --release --locked -p nuee \
    && mkdir -p /out/data /out/config /out/app \
    && cp target/release/nuee /out/nuee \
    && chmod 0700 /out/data /out/config

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
LABEL org.opencontainers.image.title="nuee" \
      org.opencontainers.image.description="Nuée anchor: persistent coordinator, relay, durable store and gateway" \
      org.opencontainers.image.source="https://github.com/maxgfr/nuee" \
      org.opencontainers.image.url="https://maxgfr.github.io/nuee/" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
COPY --from=builder /out/nuee /nuee
COPY --from=builder --chown=65532:65532 /out/app /app
COPY --from=builder --chown=65532:65532 --chmod=0700 /out/data /data
COPY --from=builder --chown=65532:65532 --chmod=0700 /out/config /config
ENV NUEE_DATA_DIR=/data \
    NUEE_CONFIG_DIR=/config \
    HOME=/data
WORKDIR /app
# The distroless "nonroot" user, numeric so the host can resolve it.
USER 65532:65532
VOLUME ["/data", "/config"]
# 80: ACME HTTP challenge and relay, 443: relay over HTTPS, 7842/udp: relay QUIC address
# discovery, 8080: gateway.
EXPOSE 80/tcp 443/tcp 7842/udp 8080/tcp
ENTRYPOINT ["/nuee"]
CMD ["serve", "--anchor"]
