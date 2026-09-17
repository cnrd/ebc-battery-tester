FROM rust:1.92-bookworm AS builder

ARG TRUNK_VERSION=0.21.14
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl libudev-dev pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && curl --fail --location --silent --show-error \
        "https://github.com/trunk-rs/trunk/releases/download/v${TRUNK_VERSION}/trunk-x86_64-unknown-linux-gnu.tar.gz" \
        | tar -xz -C /usr/local/bin trunk \
    && rustup target add wasm32-unknown-unknown

WORKDIR /build
COPY . .
RUN cargo build --locked --release --no-default-features --features server --bin ebc-server
RUN EBC_WASM_DEFAULT_TRANSPORT=remote trunk build --locked --release

FROM debian:bookworm-slim

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
