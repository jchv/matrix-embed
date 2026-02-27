FROM docker.io/library/rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.lock Cargo.toml /app/
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    libsqlite3-dev
RUN mkdir -p /app/src && \
    touch /app/src/lib.rs && \
    cargo build --release && \
    rm /app/src/lib.rs
COPY . .
RUN cargo build --release

FROM docker.io/library/debian:bookworm-slim
RUN apt-get update && apt-get install -y \
    ffmpeg \
    ca-certificates \
    libssl3 \
    libsqlite3-0 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/matrix-embed /matrix-embed
VOLUME ["/data"]
ENTRYPOINT ["/matrix-embed"]
