# ==============================================================================
# Zenith dev image -- Rust + Node for build, test, lint, bench
#
# Usage (via docker compose):
#   docker compose run --rm dev make test
#   docker compose run --rm dev cargo clippy --lib --bins -- -D warnings
# ==============================================================================

# Pinned toolchain: local `make lint` must mean exactly what CI means.
# A floating tag skews by pull date -- CI builds fresh while local
# images sit cached, so the same Dockerfile yields different clippy
# versions. Bump deliberately (dependabot proposes updates); keep in
# step with the backend stage in ../Dockerfile.
FROM rust:1.98-bookworm

# Install Node.js 22
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    apt-get install -y --no-install-recommends nodejs && \
    rm -rf /var/lib/apt/lists/*

# Rust components for linting
RUN rustup component add clippy rustfmt

WORKDIR /build
CMD ["bash"]
