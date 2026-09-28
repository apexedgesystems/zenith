# Changelog

Notable changes to zenith, newest first. Format after
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions
follow semantic versioning. The declared version lives in the
workspace Cargo.toml, the frontend package matches it, and a release
tag must equal both (tools/version-check.sh). While the major
version is 0, a minor release may contain breaking changes; each is
called out in its entry. The history starts at the 2026-08
architecture review; work before that is the v0.0.1 tag.

## Unreleased

### Fixed

- A config file that fails to parse, or a missing config path, refuses
  to boot with the error instead of starting a default server with no
  targets.
- The image build uses the committed lockfiles: the backend builds
  with `--locked` and the frontend installs with `npm ci` alone, so a
  lockfile that disagrees with its manifest fails the build.

### Changed

- Authentication is documented as covering API clients. The browser
  console does not log in when auth is on; the remote-deployment
  section describes an authenticating reverse proxy for operators.

## v0.2.0 - 2026-09-27

Zenith grows from an Apex CSF console into one console for three
flight frameworks, published as a container that runs as pulled.
Apex CSF targets have the full command surface; NASA cFS and F-prime
targets are telemetry only in this release, and their command pages
say so.

### Breaking

- Target config keys: `arm_hex` is replaced by a generated
  `connect_init` file of named steps; `udp_listen_port` is renamed
  `listen_port`; `health_nonzero_bad` is renamed `health` (boot
  refuses each old key and names the new one).
- The container runs on the host network. Each target's definition
  declares its own ports; nothing is published per target anymore.

### Added

- NASA cFS targets over UDP, with a dictionary generator that reads
  the flight build's debug info and a connect-time step that arms the
  downlink.
- F-prime targets: the deployment dials in, TM frames are verified,
  and a record dictionary generated from the topology dictionary
  names every channel.
- Byte order stamped in dictionaries; big-endian wires decode without
  configuration.
- A pull-not-build image with a default config and demo targets for
  all three frameworks; `ZENITH_PORT`, `ZENITH_DB_PATH` and
  `ZENITH_AUTH_SECRET` override the file from the environment.
- Retention tiers (age-based envelope ladder) with chart bands for
  downsampled history, and a Storage page with a capacity gauge and
  live pipeline accounting.
- Command lifecycle: READBACK results, verify-before-apply on
  tunables, RTS plans validated before upload.
- Configurable dashboard cards, command favorites, saved preferences.
- A listening target shows as listening; telemetry-only targets show
  component status by telemetry heard.
- Dashboard health rules can compare (`{ field = "x", ge = 2 }`), not
  only test nonzero; enum-typed values show their name beside the
  number when the dictionary names the enum.
- Walkthroughs for standing up the cFS and F-prime demo rigs.

### Changed

- Everything framework-specific comes from generated target files;
  a target declares its protocol, carrier, ports and connect-time
  init in config.

### Security

- Argon2 credentials, JWT sessions with short-lived WebSocket
  tickets, per-IP rate limiting, bounded uploads and history
  queries, audit entries attributed to the operator.

### Fixed

- Ingest survives write failures and the database survives a crash;
  retention removes what it says it removes.
- Every dropped sample is counted at the stage that dropped it, and
  the health page reflects it.
- Commands that never get an ACK time out instead of hanging.

## v0.0.1 - 2026-04-10

First tagged build: the Apex CSF operations console.
