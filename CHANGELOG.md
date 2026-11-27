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

### Breaking

- API clients: the command route, `POST /api/targets/{id}/command`,
  reads `full_uid` and `opcode` as hex only with a `0x` prefix and as
  decimal otherwise, as documented; it used to read any string of hex
  digits as hex. Add the prefix to a hex value: `"D000"` is now refused
  with 400, and an all-digit string changes meaning (`"1000"` is now
  1000, not 0x1000). The console already sends `0x` strings.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- API clients: a socket ticket from `POST /api/auth/ws-ticket` opens the
  telemetry socket and nothing else. On another route's `?ticket=`, or
  sent as `Authorization: Bearer`, it is now refused with 401; send the
  bearer token there instead.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- Operators: values the console used to coerce are refused, naming the
  field, and nothing is sent. Command forms refuse a blank field, an
  integer that is not a whole decimal or `0x` number or does not fit its
  field, and a floating-point value not written in decimal notation
  (`0x10`, `Infinity`). A tunable update refuses an integer outside its
  field's range (300 for a uint8 used to be written as 44), a bool other
  than 0 or 1, and a float32 beyond its range (written as infinity).
  Enter a value the field holds; Raw Mode still sends a payload as
  typed in hex.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- The image runs zenith as an unprivileged user (uid 10001) instead of
  root; its entrypoint hands an existing data volume to that user on
  first start. Started as root, the container needs CAP_SETUID and
  CAP_SETGID, plus CAP_CHOWN while a hand-over is pending: `--cap-drop
  ALL` alone stops with a message, and `--user 10001:10001` needs none of
  them. Mounted config files must be readable by uid 10001, and mounted
  target files and directories readable and searchable by it: the
  server refuses to start on a target path it cannot read, naming the
  path. A database outside `/var/lib/zenith`, and its directory, must be
  writable by it; ports below 1024 cannot be bound on the host network.
  To roll back, run 0.2.0 on a handed-over volume with its own release's
  compose file or Docker's default capabilities. Under this release's
  capability set 0.2.0 starts and reports healthy but cannot write its
  database: it runs as root and needs CAP_DAC_OVERRIDE to write files
  uid 10001 owns.
  ([#69](https://github.com/apexedgesystems/zenith/pull/69))
- The image carries only what zenith runs: no apt, bash or wget, and
  none of grep, sed, find, diff, gzip or perl; dpkg and its database
  stay, so a scanner sees every package. Its health check is built in
  (`zenith --health-check`), so the compose files declare none; a health
  check that calls wget fails.
  ([#69](https://github.com/apexedgesystems/zenith/pull/69))
- TPRM uploads use the v4 prelude and carry payloads beyond 64 KiB.
  Apex CSF vehicles built before apex_csf 2543279 refuse them; vehicles
  from that commit on require v4 and refuse zenith 0.2.0's v3 uploads.
  ([#50](https://github.com/apexedgesystems/zenith/pull/50))

### Added

- With auth on, the console signs in: a login page, a server-side
  session and Sign out. A session ends after `[auth] session_idle_min`
  without operator input (default 30; 0 disables) and `[auth]
  session_max_hours` after sign-in (default 10). Only operator input
  renews it. The console warns before each limit; after an expiry it
  asks for the password over the page it was on and resends nothing.
- `GET`, `POST` and `DELETE /api/auth/session`, and
  `POST /api/auth/session/refresh`. Bearer-token clients are unchanged.
- Generated target directories can be selected with `dir`, including
  at runtime for aproto-slip targets. Stream protocols and connect steps
  require `config.toml`.
  ([#49](https://github.com/apexedgesystems/zenith/pull/49))
- Apex targets without pushed telemetry can poll data blocks with
  `[[targets.poll]]` at up to 50 Hz.
  ([#49](https://github.com/apexedgesystems/zenith/pull/49))
- The development config includes the apex rover demo with telemetry
  and health rules.
  ([#49](https://github.com/apexedgesystems/zenith/pull/49))
- Release images are smoke-tested on each supported architecture before
  publication.
  ([#53](https://github.com/apexedgesystems/zenith/pull/53))
- Boot warns when the retention ladder is on and `[storage]
  retention_hours` (other than 0) ends at or before `[storage.tiers]
  mid_horizon_hours`: telemetry is deleted before it reaches the coarse
  tier, which stays empty. The warning names both values.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))

### Changed

- The container runs with a read-only root filesystem: the compose files
  set it, and `docker run --read-only` works with the data volume.
  ([#69](https://github.com/apexedgesystems/zenith/pull/69))
- A target path the server cannot read (a target directory, the mount
  above it, or an artifact named in config.toml) stops the boot with a
  message naming the path and the error; such a target used to load
  without those definitions, or as "not a directory".
  ([#69](https://github.com/apexedgesystems/zenith/pull/69))
- Demo target directories are regenerated from apex_csf d8d003c;
  SystemMonitor's 0x0100 command appears as HEALTH_SAMPLE.
  ([#50](https://github.com/apexedgesystems/zenith/pull/50))
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
- Command completions show component result names.
  ([#49](https://github.com/apexedgesystems/zenith/pull/49))
- Generated telemetry layouts follow their file order.
  ([#49](https://github.com/apexedgesystems/zenith/pull/49))
- Console screens suit large displays and distinguish an empty plot or
  idle connected link from a disconnected target.
  ([#49](https://github.com/apexedgesystems/zenith/pull/49))

### Security

- Session ids are random 256-bit values in an HttpOnly, SameSite=Strict
  cookie; the database stores only their SHA-256.
- Sign-in, refused sign-in (never under the typed text), sign-out,
  session expiry and session-ended socket closes are audited.

### Fixed

- `[storage] retention_hours = 0` keeps telemetry forever, as
  `audit_retention_days = 0` keeps the audit log, instead of deleting
  all of it every minute.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- Tunables reads int64 and bool fields as numbers instead of hex, and
  reads and writes a big-endian dictionary's fields in that byte order.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- Tunables applies a block holding arrays, strings or other fields it
  shows as hex: each field takes back the hex form it was read as, so an
  unedited block is written back byte for byte instead of the update
  being refused. A float that is not finite also reads as hex.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- `zenith --hash-password` hashes the password as typed, edge spaces
  included, as sign-in compares it; only the line ending is removed.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- An upload is checked as an RTS sequence only when it goes into a
  directory named `rts`, not whenever its path contains `rts/`
  (`parts/`), and the refusal reads without a run of spaces.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- Downsampling and the bearer-token login are audited: downsampling
  with the rows it converted, the login as a console sign-in is.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- Shutdown waits, up to 5 s, for each target's sample writer to write
  the samples it holds, instead of exiting with them unwritten.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- A trim with `count: 0` deletes nothing instead of a quarter of the
  target's samples.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- History queries over a window holding more samples than their limit
  return the newest ones, so a chart of a long window reaches the
  present instead of stopping at the limit's oldest samples. History
  and CSV export rows come in time order; CSV exports list the oldest
  row first.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- Tiered history no longer holds two points at one timestamp for a
  channel: a tier pass leaves the bucket its cutoff falls in for a
  later pass instead of writing it twice.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- After a dropped TM frame, a packet spanning three or more frames no
  longer yields bogus telemetry or swallows the next real packet:
  decoding resumes at the next frame in which a packet starts.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- A SLIP peer can no longer grow a frame past the 64 KiB bound with
  escaped bytes, and escaped bytes outside a frame are dropped.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- Socket tickets and bearer tokens are refused once expired, without
  the 60-second grace that let a 30-second ticket work for 90 seconds.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- An audit log request whose offset lies past the end returns no rows
  instead of the first page.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- The Telemetry page reads Live only while samples arrive and the link
  is up: a link reported down, or a stream silent for 10 s, shows Off,
  Listening or No data instead.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- Tunables shows and applies only the selected component's values: a
  slow read of the component left behind no longer replaces them, and
  Apply writes to the component the values were read from.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- The chart's hover readout shows no value for a channel before its
  first sample or after its last, instead of extrapolating one.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- Dashboard health cards for individual components load: the console
  asks for them with the uid and opcode in the hex form the command
  route reads.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- A library swap whose file cannot be read is logged as failed on the
  Operations page instead of staying pending.
  ([#70](https://github.com/apexedgesystems/zenith/pull/70))
- UDP targets show listening until first contact, and TCP targets no
  longer falsely remain connected after an immediate hangup or a
  disconnect racing an incoming session. Clients of the connect
  endpoint must accept `listening` / `already_listening` for waiting
  links. ([#68](https://github.com/apexedgesystems/zenith/pull/68))
- A shutdown that starts while a target's connect is still dialing, or
  its upload is running, keeps the API answering while it waits for
  that link, instead of stalling every request until the link frees.
  A connect asked for, or completing, once shutdown has begun is
  refused (503).
  ([#68](https://github.com/apexedgesystems/zenith/pull/68))
- Tunable updates to nested structs carry the layout hash the vehicle
  verifies when their dictionary exports none.
  ([#50](https://github.com/apexedgesystems/zenith/pull/50))
- A staged tunable update the vehicle refuses reads as refused, under
  the vehicle's reason, in the console and the audit log.
  ([#50](https://github.com/apexedgesystems/zenith/pull/50))
- Apex targets without their own health list flag a nonzero
  `frameOverrunCount`, `watchdogWarningCount` or `resyncDroppedTicks`
  on the dashboard.
  ([#50](https://github.com/apexedgesystems/zenith/pull/50))
- Telemetry socket clients that send keep-alive pings no longer lose
  the stream at their ping timeout; the server answers pings and ends
  a socket when its client closes.
- A config file that fails to parse, or a missing config path, refuses
  to boot instead of starting a default server with no targets.
- The image builds from the committed lockfiles (`--locked`, `npm ci`)
  and an allowlisted build context, so nothing from the host can enter
  it.
- Reconnected target links keep their connected status.
  ([#49](https://github.com/apexedgesystems/zenith/pull/49))
- Generated quick commands send their declared payloads.
  ([#49](https://github.com/apexedgesystems/zenith/pull/49))
- Command forms encode floating-point fields and refuse unsupported
  field types. Use Raw Mode for commands the form cannot encode.
  ([#49](https://github.com/apexedgesystems/zenith/pull/49))

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
