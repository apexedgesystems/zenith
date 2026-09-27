# Zenith

Real-time operations interface for flight software: [Apex CSF](https://github.com/apexedgesystems/apex_csf), NASA cFS, and F-prime.

Zenith connects to targets over per-target wire protocols -- one
console for Apex CSF (TCP/APROTO), NASA cFS (CCSDS SPP over UDP),
and F-prime (CCSDS TM frames over a listening TCP port), all
running side by side -- provides a REST API for commanding and
telemetry, and serves a real-time web UI for visualization,
configuration, and system management. Zero hardcoded
component knowledge -- all application-specific behavior comes from
per-target build artifacts (struct dictionaries, app manifest, plot
layouts, command catalog) loaded at deploy time, and the layers above
the transport are protocol-neutral by construction: each target
declares its full transport (protocol, carrier, ports, connect-time
init) in config, command surfaces a protocol lacks answer 501, and a CI
boundary test forbids generic code from referencing any one protocol
family or flight framework.

```
+--------------------+   TCP: SLIP + APROTO    \
|  Apex Application  | <---------------------->  \
|  (Pi, Thor, ...)   |      full command surface   \
+--------------------+                              \   +--------------------------+
                                                     >  |        Zenith            |
+--------------------+   UDP: CCSDS space packets   /   |   Rust backend (axum)    |
|    NASA cFS        | ---------------------------->    |   React frontend         |
|  (CI_LAB/TO_LAB)   |   zenith arms the downlink  /    |   SQLite + WAL pool      |
+--------------------+                            /     |                          |
                                                 /      |   - REST API             |
+--------------------+   TCP: CCSDS TM frames   /       |   - WebSocket telemetry  |
|     F-prime        | ------------------------/        |   - Multi-target         |
|   (deployment)     |   dials IN; zenith listens       +--------------------------+
+--------------------+                                             |
                                            Per-target definitions (TOML) +
                                            generated artifacts (JSON)
                                            +--------------------------+
                                            |  app_manifest.json       |
                                            |  structs/*.json          |
                                            |     or records.json      |
                                            |  telemetry.json          |
                                            |  commands.json           |
                                            |  on_connect.json         |
                                            +--------------------------+
```

## Per-Target Plugin Architecture

Zenith ships with zero application knowledge. Each target you want to
control gets its own config directory with the build artifacts that
describe what's running on that target:

```
targets/
  pi-ops-demo/            # An Apex application
    app_manifest.json     # Component registry: fullUid, name, type, instance
    structs/              # apex_data_gen output, one JSON per component;
                          #   a field may name an entry in the file's
                          #   enums table ("enum": "DriveMode") and the
                          #   UI shows the value's name beside the number
      ApexExecutive.json  #   - struct definitions with field types and offsets
      Scheduler.json      #   - categories: STATIC_PARAM / TUNABLE_PARAM /
      WaveGenerator.json  #     STATE / INPUT / OUTPUT / TELEMETRY
      SystemMonitor.json
      ...
    telemetry.json        # Plot layout presets (which channels to chart)
    commands.json         # Per-component command catalog (quick commands,
                          #   typed field forms, response decoding hints)

  cfs-cpu1/               # A cFS instance
    app_manifest.json     # Same schema, different generator (cfs-dictgen)
    structs/              # Layouts extracted from the flight build's DWARF
    telemetry.json
    on_connect.json       # Named steps sent on connect (enable the downlink)

  fprime-demo/            # An F-prime deployment
    records.json          # Record dictionary: one table of channel id, name,
                          #   type, size + the packet/record header shape, byte
                          #   order, and the wire layout of undecoded records
    telemetry.json
```

The same backend binary serves any configured target. Adding a
new target is a new entry in `config.toml` plus a new config
directory.

The config directory is always generator output, one generator per
framework, all emitting the same neutral format: apex targets use
`apex_data_gen`; cFS targets use `tools/cfs-dictgen` (extracts
exact struct layouts from the flight build's DWARF debug info --
the same binaries the software runs, so dictionaries cannot drift
from the wire); F-prime targets use `tools/fprime-dictgen` (a pure
transform of the deployment's build-generated JSON dictionary into
a record dictionary -- record-shaped telemetry is a first-class
dictionary form, one readable table of channel ids, names, types,
and sizes, plus the byte layout of every channel it does not
decode so the walker steps over them -- plus display layouts).
Dictionaries carry a generator-measured `byte_order` stamp, so
big-endian wires (F-prime by spec, big-endian flight processors by
ELF ident) decode correctly with zero configuration.

## Pages

| Page              | Purpose                                                                                                                                                                                                                                                                                                                                                 |
| ----------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Dashboard**     | Per-target health cards auto-discovered from struct dicts. Executive summary banner. Component registry with status dots (probed by command on apex targets; "telemetry heard in the last 30 s" on telemetry-only links). Connect / disconnect / add target.                                                                                            |
| **Telemetry**     | Multi-signal strip charts with hover crosshair, per-plot time windows, threshold lines, drag-to-reorder, layout presets from `telemetry.json`, user-saved layouts in DB, pause/resume, 2-column grid, PNG/CSV export, historical data backfill. Tiered (downsampled) history renders as min/max envelope bands with mean [min..max] crosshair readouts. |
| **Operations**    | System controls: Sleep/Wake, Pause/Resume, Set Verbosity, Restart Executive (with auto-reconnect). Per-component Lock/Unlock with visual lock state. Library hot-swap (lock + upload .so + reload + auto-unlock). In-page audit feed of issued commands.                                                                                                |
| **Command**       | Generic APROTO command console: pick a component from the catalog, fill typed fields, send. Quick command presets. Response display with "Interpret as..." dropdown that decodes the raw bytes against any per-target struct of matching size.                                                                                                          |
| **Tunables**      | Edit TUNABLE_PARAM blocks for any component (auto-discovered). Decoded field table with editable values per the struct dict types. Apply button does TPRM upload + RELOAD_TPRM. Variable-length TPRM support for Scheduler-style header + entries layouts.                                                                                              |
| **INSPECT**       | Browse any registered data block on any component for any category (STATIC_PARAM / TUNABLE_PARAM / STATE / INPUT / OUTPUT). Decoded field table with type info. Auto-refresh toggle (1 Hz) for live state debugging.                                                                                                                                    |
| **File Transfer** | Drag-and-drop file upload to any path on the target via APROTO. Single-file with size cap. Per-target export of telemetry as CSV.                                                                                                                                                                                                                       |
| **Storage**       | Live capacity gauge against the configured cap with fill rate and time-to-cap projection, per-target usage bars with trim/delete controls, the pipeline accounting table (decoded = written + counted drops), FIFO/retention counters, retention-ladder card (per-band populations), manual downsample.                                                 |
| **Audit Log**     | Append-only log of operator actions: every command, file upload, target connect/disconnect/add/remove, library swap, storage trim. Filterable by actor / target / IP / status. Auto-refresh option.                                                                                                                                                     |

## Sidebar Features

- Live target list with connection state dots (green connected; amber
  listening, for a tcp-listen target waiting for its deployment to
  dial in; grey down)
- **Per-target storage strip** -- shows samples + bytes per target
- **Right-click target menu**: Connect / Disconnect, Copy address,
  **Auto-reconnect toggle** (persisted across browser sessions),
  Export telemetry CSV, Trim oldest 25%, Remove target
- Add Target form for runtime target creation

## Stack

| Layer         | Technology                                                                              |
| ------------- | --------------------------------------------------------------------------------------- |
| Backend       | Rust (axum, tokio, rusqlite)                                                            |
| Frontend      | React 19, TypeScript strict, Canvas API                                                 |
| Storage       | SQLite (WAL mode) with read connection pool (1 writer + N readers)                      |
| Protocol      | APROTO over TCP + SLIP framing                                                          |
| Tests         | `cargo test --lib` (128 unit tests) + Vitest with React Testing Library (56 unit tests) |
| Benches       | criterion + pprof flamegraphs                                                           |
| Auth          | JWT bearer middleware (config-disabled by default)                                      |
| Rate limiting | Per-IP token bucket on POST endpoints (when auth is on)                                 |
| Deploy        | Docker (multi-stage Rust + Node -> Debian slim)                                         |

## Build Artifacts (from Apex release)

| Artifact            | Generator                             | Purpose                                                             |
| ------------------- | ------------------------------------- | ------------------------------------------------------------------- |
| Struct dict JSONs   | `apex_data_gen`                       | Field-level decoding of telemetry, params, state, inspect responses |
| `app_manifest.json` | `make zenith-target`                  | Component registry: fullUid, name, type, instance index             |
| `telemetry.json`    | `make zenith-target` (then customize) | Plot layout presets (auto-generated defaults, user-customizable)    |
| `commands.json`     | `make zenith-target`                  | Per-component command catalog with field types                      |
| TPRM binaries       | `apex_tprm_compile`                   | Runtime configuration loaded by apex on startup                     |

Uploaded tunable payloads carry the TPRM format A v3 prelude (magic,
version, size, target uid, layout hash, body CRC), which the vehicle
verifies before any payload reaches a component. The byte-level
contract lives in `compat/tprm/` -- a deliberate copy of the
producing repo's golden vectors; the backend conformance suite fails
CI on any divergence, and the copy's README documents the refresh
procedure.

## Refreshing a Target Directory

Everything under `targets/<name>/` is disposable generator output --
no file in it is hand-maintained. When the producing repo's
components change:

```bash
# in the apex repo
make apex-data-db && make zenith-target APP=MyApp
# in zenith
rm -rf targets/<name> && cp -r <apex>/build/*/zenith_targets/MyApp targets/<name>
```

On the next startup zenith re-syncs config-seeded layouts to the new
file (stale ones removed, changed ones updated) and leaves DB-saved
user layouts untouched. Operator-curated layouts belong in the
database (save them from the Telemetry page); the layout picker
flags any saved layout whose channels no longer exist in the
refreshed dictionaries. Back curation up with
`GET /api/targets/{id}/telemetry/layouts/export`.

## Quickstart

### Run the published image

Nothing to build. The image carries a default config and the demo
target directories for all three frameworks, so it boots as is:

```bash
docker run -d --name zenith --network host \
  -v zenith-data:/var/lib/zenith \
  ghcr.io/apexedgesystems/zenith:latest
# http://localhost:8080
```

Or with compose, which adds the health check and restart policy:

```bash
curl -O https://raw.githubusercontent.com/apexedgesystems/zenith/main/deploy/docker-compose.yml
docker compose up -d
```

Pin a version with `ZENITH_IMAGE=ghcr.io/apexedgesystems/zenith:v0.2.0`.
Three settings come from the environment when set: `ZENITH_PORT`
(the front door, default 8080), `ZENITH_DB_PATH` (default
`/var/lib/zenith/zenith.db`, inside the persistent volume), and
`ZENITH_AUTH_SECRET` (see Authentication and Audit). Everything else
is the config file.

To run your own targets, write a `config.toml` (the Configuration
section below is the reference; the bundled default is
[deploy/config.toml](deploy/config.toml)) and mount it with the
target directories:

```bash
docker run -d --name zenith --network host \
  -v zenith-data:/var/lib/zenith \
  -v "$PWD/config.toml:/etc/zenith/config.toml:ro" \
  -v "$PWD/targets:/data/targets:ro" \
  ghcr.io/apexedgesystems/zenith:latest
```

The container runs on the host network on purpose: each target's
definition describes its own transport (what it dials, what it
listens on) and the transport layer binds exactly that, so no
per-target port publishing is ever needed.

To run the flight side of the bundled demos yourself, two
walkthroughs stand up the stock frameworks from scratch and end at
zenith charts: [docs/DEMO_CFS.md](docs/DEMO_CFS.md) and
[docs/DEMO_FPRIME.md](docs/DEMO_FPRIME.md). The apex ops demo ships
with Apex CSF.

### Build from source

```bash
# 1. Generate the target config directory from the apex build
#    (in the apex repo; emits manifest, struct dicts, commands, and
#    a default telemetry layout in one step)
make apex-data-db && make zenith-target APP=MyApp
cp -r build/*/zenith_targets/MyApp targets/my-target

# 2. Optional: curate plot layouts IN THE UI and save them -- saved
#    layouts live in zenith's database, not in the target directory.

# 3. Configure
cat > config.toml << 'EOF'
[server]
host = "0.0.0.0"
port = 8080

[storage]
path = "/var/lib/zenith/zenith.db"
retention_hours = 24
max_db_size_mb = 2048

# Optional age-based retention ladder: newest window at full
# resolution, older data as envelope buckets (mean + min/max +
# count) so spikes stay visible while the cap holds far more
# history. See config.toml for the [storage.tiers] keys.

[[targets]]
name = "My Target"
host = "192.168.1.100"
port = 9000
manifest = "/data/targets/my-target/app_manifest.json"
structs_dir = "/data/targets/my-target/structs"
telemetry_config = "/data/targets/my-target/telemetry.json"
commands_config = "/data/targets/my-target/commands.json"
auto_connect = false
EOF

# 4. Build and run
make run

# 5. Open browser
# http://localhost:8080
```

## Build, Test, Bench

| Command              | Purpose                                                                                        |
| -------------------- | ---------------------------------------------------------------------------------------------- |
| `make run`           | Build production Docker image and start the container                                          |
| `make run-clean`     | Same but with `--no-cache` -- use when frontend source changed and Docker layer cache is stale |
| `make stop`          | Stop the running container                                                                     |
| `make dev`           | Build + run in foreground (logs to stdout)                                                     |
| `make test`          | Run **both** backend and frontend test suites                                                  |
| `make test-backend`  | Backend only: `cargo test --lib` (currently 128 unit tests)                                    |
| `make test-frontend` | Frontend only: `vitest run` (currently 56 unit tests)                                          |
| `make bench`         | Run criterion benches (`protocol`, `storage`, `decoder`)                                       |
| `make format`        | Run rustfmt across the backend                                                                 |
| `make lint`          | Run clippy with `-D warnings`                                                                  |

All commands run inside Docker -- no local Rust or Node toolchain
required.

The `make run` target uses `docker compose build` (not raw
`docker build`) so the resulting image is tagged correctly for compose.
Mixing `docker build -t zenith` with `docker compose up` produces two
unrelated images and silently runs the stale one. The Makefile guards
against this.

## Configuration

```toml
[server]
host = "0.0.0.0"
port = 8080

[auth]
enabled = false                # JWT auth + rate limiting (default off)
secret = "change-me-in-production"

[storage]
path = "./data/zenith.db"
retention_hours = 24
max_db_size_mb = 2048          # Size-based FIFO kicks in above this

[[targets]]
name = "Pi - Ops Demo"
host = "192.168.1.119"
port = 9000
auto_connect = false           # Auto-connect on startup
manifest = "/data/targets/pi-ops-demo/app_manifest.json"
structs_dir = "/data/targets/pi-ops-demo/structs"
telemetry_config = "/data/targets/pi-ops-demo/telemetry.json"
commands_config = "/data/targets/pi-ops-demo/commands.json"

[[targets]]
name = "cFS cpu1"              # A NASA cFS instance, same engine
host = "127.0.0.1"             # Commands dial out to CI_LAB here
port = 1234
protocol = "ccsds-spp"         # Space packets...
carrier = "udp"                # ...as datagrams (default: "tcp")
listen_port = 2234             # Local port TO_LAB pushes telemetry to
connect_init = "/data/targets/cfs-cpu1/on_connect.json"
                               # Named init steps sent on connect
                               # (e.g. enable the downlink); generated
manifest = "/data/targets/cfs-cpu1/app_manifest.json"
structs_dir = "/data/targets/cfs-cpu1/structs"
telemetry_config = "/data/targets/cfs-cpu1/telemetry.json"
[targets.apid_map]             # Wire APID -> component uid routing
"0x000" = "0x00C00000"

[[targets]]
name = "F-prime Demo"          # An F-prime deployment, same engine
host = "127.0.0.1"
port = 50050
protocol = "tm+ccsds-spp+records"  # CCSDS TM frames -> space
                               # packets -> id-addressed records
carrier = "tcp-listen"         # The deployment dials IN to zenith
listen_port = 50050
tm_frame_size = 1024           # Mission constant of the build
records_config = "/data/targets/fprime-demo/records.json"
telemetry_config = "/data/targets/fprime-demo/telemetry.json"
```

A target's definition fully describes its transport: the protocol
(`aproto-slip`, `ccsds-spp`, `slip+ccsds-spp`, `tm+ccsds-spp`,
`tm+ccsds-spp+records`, `raw-slip`), the
carrier (`tcp` dials host:port and reads the stream; `udp` binds
`listen_port` for inbound datagrams and sends outbound ones to
host:port; `tcp-listen` binds `listen_port` on every interface and accepts
the target dialing in -- the push-to-ground pattern, with the
connected state tracking the live session, not the bound listener), and an optional `connect_init`
sequence -- a generated
on_connect.json of named steps (bytes + per-step delays) sent in
order on every connect, for stacks that emit nothing until a ground
message enables their downlink. Steps are opaque bytes to zenith;
their names appear in logs and in the connect audit record, and a
step that fails to send tears the link down rather than leaving it
half-armed. Unknown protocols or carriers, a UDP carrier without a
listen port, a broken connect_init file, and two targets claiming
one listen port all refuse to boot. The deployment needs no per-target network config: the
container runs on the host network and binds exactly what
definitions declare.

A target's `health` list is the dashboard's display policy: which
fields read as bad, and when. A bare name means bad when nonzero
(the apex error counters are the default list); a table names one
comparison against a number:

```toml
health = [
  "is_slipping",                          # bad when nonzero
  { field = "last_cmd_result", ge = 2 },  # 1 is an ACK; 2 and up are NACKs
  { field = "board_link", eq = 2 },       # 0 never used, 1 up, 2 lost
]
```

Operators are `eq`, `ne`, `ge`, `gt`, `le`, `lt`, one per rule; a
rule with none or several refuses to boot. Field names match
lowercased with underscores stripped. This is ground-side judgement,
not vehicle truth, so it stays in the zenith config rather than the
generated bundle.

## Config Validation

On startup, zenith validates all loaded artifacts and logs warnings
for issues. Warnings are non-fatal.

- **Manifest:** application name present, Executive component
  (UID `0x000000`) exists, no duplicate UIDs, valid hex format.
- **Struct dicts:** fields don't extend past struct size, structs
  with nonzero size have field definitions.
- **Telemetry config:** layouts have names, plots have channels.

## Concurrency Model

- **One writer connection** to the SQLite DB (Mutex<Connection>) for
  inserts, FIFO deletions, layout writes.
- **Pool of up to 8 reader connections** (Mutex<Vec<Connection>>) for
  history queries, latest values, layout reads, audit log reads.
  Multiple readers run truly in parallel against WAL.
- **One AprotoClient per target**, owned by the target's task. Holds
  a writer and reader split over the same TCP socket: ACK responses
  flow back through an `mpsc::channel` to the command caller, push
  telemetry flows through a `broadcast::channel` to the WebSocket
  subscribers.
- **Per-target sample broadcast** -> writer task that batches into
  `insert_batch` calls (~50 samples or empty rx, whichever first).

## API Summary

```
# System
GET    /api/health                               # DB writability + per-target state, "degraded" on DB failure
GET    /api/metrics                              # Per-target pipeline counters (drops, failures, latency)
GET    /api/version
GET    /api/audit?limit=N&offset=N
POST   /api/auth/login
POST   /api/auth/ws-ticket                       # Trade a bearer token for a 30s WebSocket ticket

# Targets
GET    /api/targets
POST   /api/targets/add
POST   /api/targets/{id}/connect
POST   /api/targets/{id}/disconnect
POST   /api/targets/{id}/remove
POST   /api/targets/{id}/restart                 # Restart executive (deferred ACK before execve)

# Commands
POST   /api/targets/{id}/noop
GET    /api/targets/{id}/health
GET    /api/targets/{id}/inspect/{uid}?category=N&offset=N&length=N
POST   /api/targets/{id}/command                 # Generic (uid, opcode, payload_hex)
GET    /api/targets/{id}/registry
GET    /api/targets/{id}/commands

# Parameters (TUNABLE_PARAM)
GET    /api/targets/{id}/params
GET    /api/targets/{id}/params/{uid}
POST   /api/targets/{id}/params/{uid}/update     # v3-stamped upload; verify-before-apply on readback-capable targets
POST   /api/targets/{id}/params/{uid}/verify     # vehicle-side VERIFY of the staged payload, no apply
GET    /api/targets/{id}/tprm/staged             # staged-bank digest (declared identity + verdict per file)

# Telemetry
WS     /api/targets/{id}/telemetry/live
GET    /api/targets/{id}/telemetry/latest
GET    /api/targets/{id}/telemetry/history?channel=&start_ms=&end_ms=&limit=
GET    /api/targets/{id}/telemetry/csv
GET    /api/telemetry/stats                      # totals, cap, net fill rate, time-to-cap projection
POST   /api/telemetry/downsample

# Operator Preferences (display policy; per target or global)
GET    /api/prefs/{scope}/{kind}                 # scope: global | target:{id}
GET    /api/prefs/{scope}/{kind}/{name}
PUT    /api/prefs/{scope}/{kind}/{name}          # JSON value, 32KB cap
DELETE /api/prefs/{scope}/{kind}/{name}

# Telemetry Layouts
GET    /api/targets/{id}/telemetry/layouts
POST   /api/targets/{id}/telemetry/layouts/save
DELETE /api/targets/{id}/telemetry/layouts/{layout_id}
GET    /api/targets/{id}/telemetry/layouts/export

# Storage
GET    /api/targets/{id}/storage                 # sample count, channel count, byte estimate
POST   /api/targets/{id}/storage/trim            # Delete oldest N samples
POST   /api/targets/{id}/storage/delete          # Delete all samples for target

# Files
POST   /api/targets/{id}/upload                  # Generic file upload (base64)
POST   /api/targets/{id}/components/{uid}/library  # Lock + upload .so + reload + auto-unlock

# Struct Dictionaries (per-target)
GET    /api/targets/{id}/structs
GET    /api/targets/{id}/structs/{component}
GET    /api/structs                              # Global fallback dict (usually empty)
GET    /api/structs/{component}                  # Same
```

## Authentication and Audit

Setup: generate a password hash with `zenith --hash-password` (reads
the password from stdin, prints an argon2 PHC string), then set
`[auth] enabled = true`, `username`, `password_hash`, and a `secret`
of at least 16 characters. The secret signs tokens and is never a
login credential. Startup refuses to boot with the default secret,
a missing hash, or a malformed hash while auth is enabled.

When auth is enabled:

- All `/api/*` routes except `/api/auth/login` and `/api/health`
  require `Authorization: Bearer <jwt>`. Tokens carry `sub` and
  `exp` (24 h) and the subject is recorded as the actor on every
  audited action.
- WebSocket upgrades (which browsers cannot attach headers to) use
  `POST /api/auth/ws-ticket` to trade the bearer token for a 30 s
  single-purpose ticket passed as `?ticket=`. Long-lived tokens on
  a query string are rejected so they can never reach request logs
  (which record method and path only, never query strings).
- Per-IP token bucket rate limit of 10 req/sec (burst 30) on POST
  endpoints. Returns 429 when exceeded. Idle buckets are evicted.

When auth is disabled (the default for development), the middleware
is a pass-through, all endpoints are open, no rate limiting applies,
and audit entries record the anonymous actor "operator". This is a
deliberate trusted-LAN development posture -- enable auth for any
deployment where the network is not fully trusted.

Independent of auth, every request is bounded: history and CSV row
limits clamp at 200k, uploads cap at `[server] upload_max_mb`
(default 50) with remote paths required to be relative and
traversal-free, cross-origin API access is denied unless
`[server] cors_allowed_origins` lists origins, and audit rows prune
after `[storage] audit_retention_days` (default 90; 0 keeps
forever).

The audit log captures every state-changing action regardless of
whether auth is on. View it at `GET /api/audit` or via the Audit Log
page in the UI. Each entry has timestamp, actor, action, target,
detail, status, and source IP.

## Deploying Beyond a Trusted LAN

The default posture is a trusted LAN: auth off, every endpoint open.
For any host others can reach, the deployment enables auth and puts
TLS in front:

1. Generate a password hash: `docker run --rm -i
ghcr.io/apexedgesystems/zenith:latest --hash-password` (reads the
   password from stdin, prints an argon2 PHC string).
2. In `config.toml`, set `[auth] enabled = true`, `username`, and
   `password_hash`. Leave `secret` out of the file and pass it as
   `ZENITH_AUTH_SECRET` (at least 16 characters, generated, never
   reused); startup refuses the default secret while auth is on.
3. Bind the front door to the loopback (`[server] host =
"127.0.0.1"`) and terminate TLS in a reverse proxy on the same
   host that forwards to it. The proxy must pass WebSocket upgrades
   through for `/ws` (the UI's telemetry stream); tokens never ride
   the query string, so request logs stay clean.
4. If the UI is served from another origin, list it in `[server]
cors_allowed_origins`; otherwise leave it empty (same-origin only).

With auth on, every `/api/*` route except login and health requires a
bearer token, the per-IP rate limit applies to POSTs, and every
audited action carries the operator's name. The audit log is on
regardless.

## Releases

The declared version lives in the workspace `Cargo.toml`;
`frontend/package.json` must match it, and `make version-check`
proves both (`make version-check TAG=vX.Y.Z` also proves a tag).
[CHANGELOG.md](CHANGELOG.md) keeps an `Unreleased` section that each
feature branch adds to; a release turns it into a `vX.Y.Z - date`
section. Entries are written for the person upgrading: what changed
for them, one line each, breaking changes first. Engineering detail
stays in commit messages.

To cut a release: bump the two version fields, retitle the
changelog section, merge, then tag `vX.Y.Z` on main and push the
tag. The release workflow checks the tag against the declared
version and the changelog before any build starts, builds the image
natively for amd64 and arm64 with SBOM and provenance attestations,
publishes the multi-arch manifest as
`ghcr.io/apexedgesystems/zenith:vX.Y.Z` and `:latest`, and creates
the GitHub release with that changelog section as its body and the
compose file, the image manifest and SHA256SUMS attached. A
pre-release tag (`vX.Y.Z-rc1`) publishes under its own tag, marks
the release as a pre-release, and leaves `:latest` alone.

Rehearse before tagging: `gh workflow run release.yml --ref <branch>`
runs every step except publishing, with a throwaway version, so a
green rehearsal is proof the next tag will publish. Workflow actions
are pinned by commit; dependabot proposes their updates.

## License

MIT
