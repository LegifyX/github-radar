# Stage 1: Build the single self-contained Rust binary
FROM rust:1.85-bookworm AS builder

WORKDIR /app

# Install build dependencies for bundled C libraries (rusqlite / ring)
RUN apt-get update && \
    apt-get install -y --no-install-recommends pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

# Copy manifest, source, and embedded PWA frontend
COPY Cargo.toml ./
COPY src ./src
COPY static ./static

# Build release binary
RUN cargo build --release --verbose

# Stage 2: Minimal production runtime image
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

EXPOSE 8080
VOLUME ["/data"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD curl -f http://localhost:8080/api/health || exit 1

CMD ["/usr/local/bin/github-radar"]