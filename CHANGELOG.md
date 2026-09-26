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

### Added

- NASA cFS targets: CCSDS space packets over a UDP carrier, a
  dictionary generator that reads the flight build's debug info, and
  generated connect-time init sequences that arm the downlink.
- F-prime targets: a tcp-listen carrier for deployments that dial in,
  CCSDS TM transfer-frame deframing with CRC verification, a
  record-dictionary form for id-addressed telemetry, and a generator
  that transforms the deployment's topology dictionary.
- Dictionaries carry a byte-order stamp; big-endian wires decode
  without configuration.
- Retention tiers: an age-based envelope ladder, chart bands for
  downsampled history, and a storage panel with a capacity gauge, usage
  bars, and live pipeline accounting.
- Command lifecycle with READBACK consumption and verify-before-apply
  on tunables; RTS plan validation before upload.
- Preference store, configurable dashboard cards, command favorites.
- Argon2 credentials, JWT sessions with WebSocket tickets, attributed
  audit entries, per-IP rate limiting, bounded requests.
- Environment overrides for the front-door port, database path, and
  auth secret; a default config and the demo target directories
  bundled in the image; a pull-not-build compose file.

### Changed

- Zenith is framework-neutral above the transport: each target
  declares its protocol, carrier, ports and connect-time init in
  config, and a CI boundary test forbids generic modules from naming
  any framework.
- Target directories are named once (`dir =`) with conventional
  artifact names inside; explicit paths still override.
- Telemetry-only targets report component status by telemetry heard
  and show a listening link as listening.
- Toolchain pinned across dev and prod images; dependabot groups paired
  majors so proposed updates can merge.

### Fixed

- Failure-safe ingest and durable storage with correct retention.
- Bounded ACK waits; no lock guards held across target I/O.
- Counted drops at every pipeline stage and truthful health.
- Layout hashes aware of field offsets; nested-struct vocabulary.
- Frontend geometry, id handling, and dead code under a lint gate.

## v0.0.1 - 2026-04-10

First tagged build: the Apex CSF operations console.
