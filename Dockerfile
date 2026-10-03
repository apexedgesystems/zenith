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
FROM rust:1.98-bookworm AS backend

WORKDIR /build
# The committed lockfile is an input of the build: --locked refuses a
# lock that disagrees with the manifests instead of resolving anew.
COPY Cargo.toml Cargo.lock ./
COPY backend/ backend/

RUN cargo build --release --locked

# ------------------------------------------------------------------------------
# Stage 2: React frontend
# ------------------------------------------------------------------------------
FROM node:26-bookworm-slim AS frontend

WORKDIR /build
# npm ci installs exactly what the lockfile says or fails; no fallback.
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci --ignore-scripts

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

# The image boots on its own: a default config declaring the bundled
# demo targets. A mount at /etc/zenith/config.toml replaces the
# config; a mount at /data/targets replaces the target directories.
COPY deploy/config.toml /etc/zenith/config.toml
COPY targets/ /data/targets/

RUN mkdir -p /var/lib/zenith

# Working directory sits inside the persistent volume so even a
# relative storage path in config.toml resolves somewhere durable.
WORKDIR /var/lib/zenith

EXPOSE 8080

ENTRYPOINT ["zenith"]
CMD ["--config", "/etc/zenith/config.toml"]
