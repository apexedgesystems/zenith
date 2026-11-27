# ==============================================================================
# Zenith dev image -- Rust + Node for build, test, lint, coverage, bench
#
# The Makefile runs its test, lint, fmt-check, coverage and bench targets
# in this image (docker compose run --rm -T dev ...); for a shell in it:
#   docker compose run --rm dev
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

# Rust components: clippy and rustfmt for lint and the format check, and
# llvm-tools-preview for coverage (the llvm-profdata and llvm-cov of this
# toolchain, which cargo-llvm-cov drives)
RUN rustup component add clippy rustfmt llvm-tools-preview

# cargo-llvm-cov, the coverage driver (make coverage-backend): the
# project's release binary for this machine, checked against the sha256
# recorded here before it is unpacked. The version and both digests are
# bumped together by hand (dependabot reads no ARG).
ARG CARGO_LLVM_COV_VERSION=0.9.1
ARG CARGO_LLVM_COV_SHA256_X86_64=b3f68e625481fed9b16444174f3fa5ebcdbde4a1878803a35eabe2dcefcdc41a
ARG CARGO_LLVM_COV_SHA256_AARCH64=abf5f13c1520f8756d2192bfaaeadb0208f5b7eaa8cadc15bf847befa42b7360
RUN arch="$(uname -m)" && \
    case "$arch" in \
        x86_64) sha256="$CARGO_LLVM_COV_SHA256_X86_64" ;; \
        aarch64) sha256="$CARGO_LLVM_COV_SHA256_AARCH64" ;; \
        *) echo "cargo-llvm-cov: no digest recorded for $arch" >&2; exit 1 ;; \
    esac && \
    tarball="cargo-llvm-cov-$arch-unknown-linux-gnu.tar.gz" && \
    curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --connect-timeout 30 \
        -o "/tmp/$tarball" \
        "https://github.com/taiki-e/cargo-llvm-cov/releases/download/v$CARGO_LLVM_COV_VERSION/$tarball" && \
    printf '%s  %s\n' "$sha256" "/tmp/$tarball" > /tmp/cargo-llvm-cov.sha256 && \
    sha256sum -c /tmp/cargo-llvm-cov.sha256 && \
    tar -xzf "/tmp/$tarball" -C /usr/local/cargo/bin --no-same-owner cargo-llvm-cov && \
    rm "/tmp/$tarball" /tmp/cargo-llvm-cov.sha256 && \
    cargo llvm-cov --version

WORKDIR /build
CMD ["bash"]
