# ==============================================================================
# Zenith - Multi-stage Docker build
#
# Stage 1: Build Rust backend
# Stage 2: Build React frontend
# Stage 3: Runtime root (the base cut down to what zenith runs, the account,
#          the binary, the static frontend, the bundled config and targets)
# Stage 4: Runtime image (that root as one layer, and the image's settings)
#
# Usage: make run (docker compose build) -- see Makefile. Building with
# a raw docker build -t zenith produces a tag compose ignores.
#
# Each base image is pinned by tag and multi-architecture index digest
# (the final stage starts from scratch, which is empty and has nothing to
# pin), so a commit builds from the same base images on any day and host;
# dependabot proposes each update, a new tag or a rebuilt tag's new digest.
# ==============================================================================

# ------------------------------------------------------------------------------
# Stage 1: Rust backend
# ------------------------------------------------------------------------------
# Pinned to match docker/dev.Dockerfile -- one toolchain everywhere.
FROM rust:1.100-bookworm@sha256:ade39a710ef85f3506cffcfec3e8a24ba090badef1dff2fb798f71d878833fea AS backend

WORKDIR /build
# The committed lockfile is an input of the build: --locked refuses a
# lock that disagrees with the manifests instead of resolving anew.
COPY Cargo.toml Cargo.lock ./
COPY backend/ backend/

RUN cargo build --release --locked

# ------------------------------------------------------------------------------
# Stage 2: React frontend
# ------------------------------------------------------------------------------
FROM node:26-bookworm-slim@sha256:5df79989ed685e09f0a98019db4cebfb22fb9df1a53c0161664a011f7ff6b0f1 AS frontend

WORKDIR /build
# npm ci installs exactly what the lockfile says or fails; no fallback.
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci --ignore-scripts

COPY frontend/ .
RUN npm run build

# ------------------------------------------------------------------------------
# Stage 3: Runtime root
# ------------------------------------------------------------------------------
FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc AS runtime-root

# The server runs as the zenith account, uid and gid 10001: fixed numbers,
# so the host can give it a bind-mounted data directory or a device node
# by owner or group. Everything else names the account.
#
# The image then keeps only what it runs: the server's libraries, the
# entrypoint's sh, coreutils and setpriv (util-linux), what those depend
# on, and dpkg with its database, so a scanner still sees every package
# left. Four more stay that nothing here runs: base-files, whose
# os-release tells a scanner which system this is, base-passwd, and the
# two packages those need (mawk and libdebconfclient0). The rest of the
# base is purged. dpkg refuses to remove a package
# that a kept one declares a dependency on, and that is all it guards: a
# need no package declares is the smoke's to find (tools/smoke-boot.sh
# boots the image and runs its server, entrypoint and health check). The
# second purge holds back what the first one's maintainer scripts and
# dpkg itself still run. Between the two, every setuid and setgid bit
# left is cleared (su, which util-linux ships beside setpriv, and the
# empty /var/local and /var/mail), so no file in the image changes
# identity when run; the files stay where dpkg's lists name them. Last,
# the step compares the packages left with its list and fails, printing
# both, on any difference: a base update that adds, renames or drops a
# package stops the build until the list follows it.
RUN groupadd --gid 10001 zenith && \
    useradd --no-log-init --uid 10001 --gid zenith --no-create-home \
        --home-dir /var/lib/zenith --shell /bin/false zenith && \
    DEBIAN_FRONTEND=noninteractive dpkg --purge --force-remove-essential \
        adduser apt bsdutils debian-archive-keyring e2fsprogs gpgv gzip \
        hostname init-system-helpers libapt-pkg6.0 libcom-err2 libdb5.3 \
        libext2fs2 libffi8 libgnutls30 libhogweed6 libidn2-0 libnettle8 \
        libp11-kit0 libpam-modules libpam-modules-bin libpam-runtime \
        libseccomp2 libsemanage-common libsemanage2 libsepol2 libss2 \
        libstdc++6 libtasn1-6 libunistring2 libxxhash0 login logsave \
        mount ncurses-base ncurses-bin passwd sysvinit-utils tzdata \
        usr-is-merged && \
    find / -xdev -perm /6000 -exec chmod ug-s {} + && \
    dpkg --purge --force-remove-essential \
        bash diffutils findutils grep libc-bin perl-base sed && \
    listed="base-files base-passwd coreutils dash debconf debianutils dpkg \
        gcc-12-base libacl1 libattr1 libaudit-common libaudit1 libblkid1 \
        libbz2-1.0 libc6 libcap-ng0 libcap2 libcrypt1 libdebconfclient0 \
        libgcc-s1 libgcrypt20 libgmp10 libgpg-error0 liblz4-1 liblzma5 \
        libmd0 libmount1 libpam0g libpcre2-8-0 libselinux1 libsmartcols1 \
        libsystemd0 libtinfo6 libudev1 libuuid1 libzstd1 mawk tar \
        util-linux util-linux-extra zlib1g" && \
    left=$(dpkg-query -W -f '${Package} ') && \
    differ="" && \
    for p in $left; do \
        case " $listed " in *" $p "*) ;; *) differ="$differ +$p" ;; esac; \
    done && \
    for p in $listed; do \
        case " $left " in *" $p "*) ;; *) differ="$differ -$p" ;; esac; \
    done && \
    if [ -n "$differ" ]; then \
        printf 'packages left: %s\nlisted: %s\nleft (+) or listed (-) only:%s\n' \
            "$left" "$listed" "$differ" >&2; \
        exit 1; \
    fi

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

# ------------------------------------------------------------------------------
# Stage 4: Runtime image
# ------------------------------------------------------------------------------
# The runtime root copied whole, owners, modes and package database
# included, into one layer: what the purge removed is gone from the image,
# not hidden above a base layer that still carries it. Nothing is
# inherited from a base, so every setting the image needs is stated here.
FROM scratch

COPY --from=runtime-root / /

ENV PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# Working directory sits inside the persistent volume so even a
# relative storage path in config.toml resolves somewhere durable, and
# so SQLite's temporary files land there under a read-only root: SQLite
# falls back to the working directory when /var/tmp and /tmp are not
# writable.
WORKDIR /var/lib/zenith

EXPOSE 8080

# The server checks itself: --health-check asks the front door that the
# config and ZENITH_PORT declare, and the entrypoint runs the check as the
# account without touching the data directory. The server reads its whole
# database before it serves, so startup grows with the database: 24 to
# 30 s for 1.1 GB in the recorded amd64 runs, once 44.5 s under higher
# load, and the bundled config lets it grow to 2 GiB. Failed
# checks therefore count only after 180 s; a healthy server is reported
# healthy at its first passing check, so the long period delays only the
# verdict on a server that never answers.
HEALTHCHECK --interval=10s --timeout=3s --start-period=180s --retries=3 \
    CMD ["zenith-entrypoint", "--config", "/etc/zenith/config.toml", "--health-check"]

# The entrypoint starts as root only to hand over the data directory,
# then runs zenith as the account (deploy/zenith-entrypoint.sh).
ENTRYPOINT ["zenith-entrypoint"]
CMD ["--config", "/etc/zenith/config.toml"]
