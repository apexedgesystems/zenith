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
FROM rust:1.100-bookworm AS backend

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

# The server runs as the zenith account, uid and gid 10001: fixed numbers,
# so the host can give it a bind-mounted data directory or a device node
# by owner or group. Everything else names the account.
RUN groupadd --gid 10001 zenith && \
    useradd --no-log-init --uid 10001 --gid zenith --no-create-home \
        --home-dir /var/lib/zenith --shell /usr/sbin/nologin zenith

COPY --from=backend /build/target/release/zenith /usr/local/bin/zenith
COPY --from=frontend /build/dist/ /usr/local/share/zenith/static/
COPY --chmod=0755 deploy/zenith-entrypoint.sh /usr/local/bin/zenith-entrypoint

# The image boots on its own: a default config declaring the bundled
# demo targets. A mount at /etc/zenith/config.toml replaces the
# config; a mount at /data/targets replaces the target directories.
COPY deploy/config.toml /etc/zenith/config.toml
COPY targets/ /data/targets/

# The data directory belongs to the account, so a fresh named volume
# mounted here starts out owned by it; the entrypoint hands over any
# directory someone else owns. The config and the targets get modes of
# their own, readable by the account whatever the build host's umask
# left in the checkout.
RUN install -d -o zenith -g zenith -m 0755 /var/lib/zenith && \
    chmod -R u=rwX,go=rX /etc/zenith /data/targets

# Working directory sits inside the persistent volume so even a
# relative storage path in config.toml resolves somewhere durable, and
# so SQLite's temporary files land there under a read-only root: SQLite
# falls back to the working directory when /var/tmp and /tmp are not
# writable.
WORKDIR /var/lib/zenith

EXPOSE 8080

# The entrypoint starts as root only to hand over the data directory,
# then runs zenith as the account (deploy/zenith-entrypoint.sh).
ENTRYPOINT ["zenith-entrypoint"]
CMD ["--config", "/etc/zenith/config.toml"]
