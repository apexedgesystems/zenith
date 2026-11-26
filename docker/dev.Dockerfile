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
# versions. The tag and its index digest are one pin (dependabot
# proposes updates: a new tag, or a rebuilt tag's digest); keep it in
# step with the backend stage in ../Dockerfile.
FROM rust:1.100-bookworm@sha256:ade39a710ef85f3506cffcfec3e8a24ba090badef1dff2fb798f71d878833fea

# Install Node.js 26 -- keep in step with the frontend stage in
# ../Dockerfile (node:26-bookworm-slim): the node that runs tests
# must be the node that builds the shipped bundle. Dependabot bumps
# the prod stage; this line moves with it by hand.
RUN curl -fsSL https://deb.nodesource.com/setup_26.x | bash - && \
    apt-get install -y --no-install-recommends nodejs && \
    rm -rf /var/lib/apt/lists/*

# Rust components for linting
RUN rustup component add clippy rustfmt

WORKDIR /build
CMD ["bash"]
