# syntax=docker/dockerfile:1

# Stage 1: build the single self-contained Rust binary
FROM rust:1-bookworm AS builder

WORKDIR /app

# ring (rustls) and libsqlite3-sys (bundled) need a C toolchain.
RUN apt-get update && \
    apt-get install -y --no-install-recommends build-essential && \
    rm -rf /var/lib/apt/lists/*

# Compile dependencies in their own layer, keyed by the lockfile. The stub
# sources only exist so `cargo build` has something to compile; they are
# replaced by the real tree on the next layer.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src static && \
    printf 'fn main() {}\n' > src/main.rs && \
    printf '<!doctype html>\n' > static/index.html && \
    touch static/manifest.json static/sw.js static/icon.svg && \
    cargo build --release --locked && \
    rm -rf src static

COPY src ./src
COPY static ./static
RUN cargo build --release --locked

# Stage 2: minimal production runtime image
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/* && \
    mkdir -p /data

WORKDIR /app
COPY --from=builder /app/target/release/github-radar /usr/local/bin/github-radar

ENV DATABASE_PATH=/data/radar.db
ENV BIND_ADDR=0.0.0.0:8080
ENV RUST_LOG=github_radar=info
# Set to 1 only when TLS terminates in front of the container, so the session
# cookie gets the Secure flag.
ENV COOKIE_SECURE=0

EXPOSE 8080
VOLUME ["/data"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -f http://localhost:8080/api/health || exit 1

CMD ["/usr/local/bin/github-radar"]
