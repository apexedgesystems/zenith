# ==============================================================================
# Zenith dev image -- Rust + Node for build, test, lint, bench
#
# Usage (via docker compose):
#   docker compose run --rm dev make test
#   docker compose run --rm dev cargo clippy --lib --bins -- -D warnings
# ==============================================================================

FROM rust:bookworm

# Install Node.js 22
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    apt-get install -y --no-install-recommends nodejs && \
    rm -rf /var/lib/apt/lists/*

# Rust components for linting
RUN rustup component add clippy rustfmt

WORKDIR /build
CMD ["bash"]
