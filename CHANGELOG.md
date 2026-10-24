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

- With auth on, the console signs in: a login page, a server-side
  session and Sign out. A session ends after `[auth] session_idle_min`
  without operator input (default 30; 0 disables) and `[auth]
  session_max_hours` after sign-in (default 10). Only operator input
  renews it. The console warns before each limit; after an expiry it
  asks for the password over the page it was on and resends nothing.
- `GET`, `POST` and `DELETE /api/auth/session`, and
  `POST /api/auth/session/refresh`. Bearer-token clients are unchanged.
- A target names its generated directory with `dir`: its manifest,
  dictionaries, layouts, commands and connect steps are found inside.
  Add Target takes the same directory for an aproto-slip target;
  stream protocols and connect steps stay in `config.toml`.
- `[[targets.poll]]` reads a data block (`uid`, `category`, `rate_hz`
  up to 50) on a timer into telemetry, for an apex target that pushes
  nothing.
- A demo target for the apex rover in the development config
  (`targets/rover-mcu`, host loopback port 9010), with health rules
  for its command result and board link.

### Changed

- The session cookie is Secure by default: serve the console over
  HTTPS (see the README's remote-deployment steps) or set
  `[auth] cookie_secure = false` on a trusted network.
- A request with the session cookie that is not GET, HEAD or OPTIONS,
  and any telemetry socket upgrade, must come from the console's own
  origin (403 otherwise). A reverse proxy must pass `Host` through.
- A session end closes its telemetry sockets with code 1008; target
  links and recording are unaffected. A connection whose send cannot
  complete within 10 s is dropped. Clients may send at most 4 KiB per
  frame.
- A console opened while the server is restarting retries its first
  session read (after 1 s, doubling to 10 s) instead of staying blank.
- Without credentials the API answers "missing token or session".
- Command completion frames are named from the component's result set
  (for example INVALID_ARGUMENT, EXEC_FAILED); immediate and queued
  frames keep the interface's names.
- Layouts from a target's `telemetry.json` are listed in the order the
  file declares them.
- Screens suit a large display; empty plots say "Waiting for data", and
  a link that is up with nothing to push shows "No data", not "Off".

### Security

- Session ids are random 256-bit values in an HttpOnly, SameSite=Strict
  cookie; the database stores only their SHA-256.
- Sign-in, refused sign-in (never under the typed text), sign-out,
  session expiry and session-ended socket closes are audited.

### Fixed

- Telemetry socket clients that send keep-alive pings no longer lose
  the stream at their ping timeout; the server answers pings and ends
  a socket when its client closes.
- A config file that fails to parse, or a missing config path, refuses
  to boot instead of starting a default server with no targets.
- The image builds from the committed lockfiles (`--locked`, `npm ci`)
  and an allowlisted build context, so nothing from the host can enter
  it.
- A link that reconnects after the remote hung up stays connected
  instead of reading as disconnected moments later.
- Quick commands generated with `payloadHex` send their payload
  instead of an empty one.

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
