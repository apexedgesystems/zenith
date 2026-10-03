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
  session, and Sign out. A session ends after `[auth]
  session_idle_min` without operator input (default 30 minutes; 0
  turns the idle limit off) and `[auth] session_max_hours` after
  sign-in (default 10 hours). Only operator input renews a session;
  polling, reloads and the telemetry stream do not. The console offers
  to stay signed in two minutes before the idle limit and gives the
  end time ten minutes before the absolute one. After an expiry it
  asks for the password over the current page, keeps what was typed
  there, and resends nothing.
- `GET`, `POST` and `DELETE /api/auth/session` (session status, sign
  in, sign out) and `POST /api/auth/session/refresh` (keep-alive).

### Changed

- The session cookie is Secure by default: serve the console over
  HTTPS through a reverse proxy (the README's remote-deployment
  steps), or set `[auth] cookie_secure = false` for plain HTTP on a
  trusted network.
- A request that carries the session cookie with any method but GET,
  HEAD and OPTIONS, or that opens the telemetry socket, must come from
  the console's own origin; anything else is refused with 403. Bearer-token requests are not
  affected. `POST /api/auth/ws-ticket` is for bearer-token clients and
  refuses a signed-in console.
- When a session ends, the server closes its telemetry streams with
  WebSocket close code 1008. Target links and recording are not
  affected.
- With auth on, a request without credentials is refused with "missing
  token or session" instead of "missing token".

### Security

- Session ids are random 256-bit values in an HttpOnly, SameSite=Strict
  cookie. The database stores only their SHA-256, so sessions survive a
  restart without the ids being kept anywhere.
- Sign-in, refused sign-in, sign-out and streams closed by a session
  end are audited. A refused sign-in is recorded under the configured
  user name or "(unknown user)", never under the text that was typed.

### Fixed

- A config file that fails to parse, or a missing config path, refuses
  to boot with the error instead of starting a default server with no
  targets.
- The image build uses the committed lockfiles: the backend builds
  with `--locked` and the frontend installs with `npm ci` alone, so a
  lockfile that disagrees with its manifest fails the build.
- The image build context excludes local dependencies, build outputs,
  runtime data and the git history, so a host's node_modules or target
  directory cannot enter the image.

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
