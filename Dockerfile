FROM rust:1.92-bookworm@sha256:e90e846de4124376164ddfbaab4b0774c7bdeef5e738866295e5a90a34a307a2 AS builder

ARG TRUNK_VERSION=0.21.14
ARG TARGETARCH
RUN case "$TARGETARCH" in \
        amd64) TRUNK_ARCH=x86_64 ;; \
        arm64) TRUNK_ARCH=aarch64 ;; \
        *) echo "Unsupported target architecture: $TARGETARCH (expected amd64 or arm64)" >&2; exit 1 ;; \
    esac \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl libudev-dev pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && curl --fail --location --silent --show-error \
        --output /trunk.tar.gz \
        "https://github.com/trunk-rs/trunk/releases/download/v${TRUNK_VERSION}/trunk-${TRUNK_ARCH}-unknown-linux-gnu.tar.gz" \
    && tar -xzf /trunk.tar.gz -C /usr/local/bin trunk \
    && rm /trunk.tar.gz \
    && rustup target add wasm32-unknown-unknown

WORKDIR /build
COPY . .
RUN cargo build --locked --release --no-default-features --features server --bin ebc-server
RUN EBC_WASM_DEFAULT_TRANSPORT=remote trunk build --locked --release

FROM debian:bookworm-slim@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587

LABEL org.opencontainers.image.source="https://github.com/cnrd/ebc-battery-tester" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.description="Headless EBC Battery Tester server with remote browser UI"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libudev1 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /build/target/release/ebc-server /usr/local/bin/ebc-server
COPY --from=builder /build/dist /app/dist

ENV EBC_HTTP_ADDR=0.0.0.0:8080 \
    EBC_SERIAL_PORT=/dev/ttyUSB0 \
    EBC_DATA_DIR=/data \
    EBC_STATIC_DIR=/app/dist \
    EBC_MOCK=false \
    RUST_LOG=info

EXPOSE 8080
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 CMD ["ebc-server", "--healthcheck"]
ENTRYPOINT ["ebc-server"]
