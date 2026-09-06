# ==============================================================================
# Zenith - Multi-stage Docker build
#
# Stage 1: Build Rust backend
# Stage 2: Build React frontend
# Stage 3: Slim runtime image (backend binary + static frontend assets)
#
# Usage: make run (docker compose build) -- see Makefile. Building with
# a raw docker build -t zenith produces a tag compose ignores.
# ==============================================================================

# ------------------------------------------------------------------------------
# Stage 1: Rust backend
# ------------------------------------------------------------------------------
# Pinned to match docker/dev.Dockerfile -- one toolchain everywhere.
FROM rust:1.97-bookworm AS backend

WORKDIR /build
COPY Cargo.toml Cargo.toml
COPY backend/ backend/

RUN cargo build --release

# ------------------------------------------------------------------------------
# Stage 2: React frontend
# ------------------------------------------------------------------------------
FROM node:22-bookworm-slim AS frontend

WORKDIR /build
COPY frontend/package.json frontend/package-lock.json* ./
RUN npm ci --ignore-scripts 2>/dev/null || npm install

COPY frontend/ .
RUN npm run build

# ------------------------------------------------------------------------------
# Stage 3: Runtime
# ------------------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates wget && \
    rm -rf /var/lib/apt/lists/*

COPY --from=backend /build/target/release/zenith /usr/local/bin/zenith
COPY --from=frontend /build/dist/ /usr/local/share/zenith/static/

RUN mkdir -p /var/lib/zenith /etc/zenith

# Working directory sits inside the persistent volume so even a
# relative storage path in config.toml resolves somewhere durable.
WORKDIR /var/lib/zenith

EXPOSE 8080

ENTRYPOINT ["zenith"]
CMD ["--config", "/etc/zenith/config.toml"]
