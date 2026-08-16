# Zenith

Real-time operations interface for [Apex CSF](https://github.com/apexedgesystems/apex_csf) applications.

Zenith connects to Apex executables over TCP/APROTO, provides a REST API
for commanding and telemetry, and serves a real-time web UI for
visualization, configuration, and system management. Zero hardcoded
component knowledge -- all application-specific behavior comes from
per-target build artifacts (struct dictionaries, app manifest, plot
layouts, command catalog) loaded at deploy time.

```
+--------------------+        TCP / APROTO         +--------------------------+
|  Apex Application  | <-------------------------> |       Zenith             |
|  (Pi, Thor, ...)   |        (port 9000+)         |   Rust backend (axum)    |
|                    |                             |   React frontend         |
|  Executive         |                             |   SQLite + WAL pool      |
|  Scheduler         |                             |                          |
|  Interface         |                             |   - REST API             |
|  Components...     |                             |   - WebSocket telemetry  |
+--------------------+                             |   - Multi-target         |
                                                   +--------------------------+
                                                              |
                                            Per-target config (JSON / TOML)
                                            +--------------------------+
                                            |  app_manifest.json       |
                                            |  structs/*.json          |
                                            |  telemetry.json          |
                                            |  commands.json           |
                                            +--------------------------+
```

## Per-Target Plugin Architecture

Zenith ships with zero apex-application knowledge. Each target you
want to control gets its own config directory with the build artifacts
that describe what's running on that target:

```
targets/
  pi-ops-demo/
    app_manifest.json     # Component registry: fullUid, name, type, instance
    structs/              # apex_data_gen output, one JSON per component
      ApexExecutive.json  #   - struct definitions with field types and offsets
      Scheduler.json      #   - categories: STATIC_PARAM / TUNABLE_PARAM /
      WaveGenerator.json  #     STATE / INPUT / OUTPUT / TELEMETRY
      SystemMonitor.json
      ...
    telemetry.json        # Plot layout presets (which channels to chart)
    commands.json         # Per-component command catalog (quick commands,
                          #   typed field forms, response decoding hints)

  thor-ops-demo-a/
    ...                   # Different application: different artifacts
  thor-ops-demo-b/
    ...
```

The same backend binary serves any configured target. Adding a
new target is a new entry in `config.toml` plus a new config
directory.

## Pages

| Page              | Purpose                                                                                                                                                                                                                                                    |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Dashboard**     | Per-target health cards auto-discovered from struct dicts. Executive summary banner. Component registry with reachability dots. Connect / disconnect / add target.                                                                                         |
| **Telemetry**     | Multi-signal strip charts with hover crosshair, per-plot time windows, threshold lines, drag-to-reorder, layout presets from `telemetry.json`, user-saved layouts in DB, pause/resume, 2-column grid, PNG/CSV export, historical data backfill.            |
| **Operations**    | System controls: Sleep/Wake, Pause/Resume, Set Verbosity, Restart Executive (with auto-reconnect). Per-component Lock/Unlock with visual lock state. Library hot-swap (lock + upload .so + reload + auto-unlock). In-page audit feed of issued commands.   |
| **Command**       | Generic APROTO command console: pick a component from the catalog, fill typed fields, send. Quick command presets. Response display with "Interpret as..." dropdown that decodes the raw bytes against any per-target struct of matching size.             |
| **Tunables**      | Edit TUNABLE_PARAM blocks for any component (auto-discovered). Decoded field table with editable values per the struct dict types. Apply button does TPRM upload + RELOAD_TPRM. Variable-length TPRM support for Scheduler-style header + entries layouts. |
| **INSPECT**       | Browse any registered data block on any component for any category (STATIC_PARAM / TUNABLE_PARAM / STATE / INPUT / OUTPUT). Decoded field table with type info. Auto-refresh toggle (1 Hz) for live state debugging.                                       |
| **File Transfer** | Drag-and-drop file upload to any path on the target via APROTO. Single-file with size cap. Per-target export of telemetry as CSV.                                                                                                                          |
| **Audit Log**     | Append-only log of operator actions: every command, file upload, target connect/disconnect/add/remove, library swap, storage trim. Filterable by actor / target / IP / status. Auto-refresh option.                                                        |

## Sidebar Features

- Live target list with connection state dots
- **Per-target storage strip** -- shows samples + bytes per target
- **Right-click target menu**: Connect / Disconnect, Copy address,
  **Auto-reconnect toggle** (persisted across browser sessions),
  Export telemetry CSV, Trim oldest 25%, Remove target
- Add Target form for runtime target creation

## Stack

| Layer         | Technology                                                                               |
| ------------- | ---------------------------------------------------------------------------------------- |
| Backend       | Rust (axum, tokio, rusqlite)                                                             |
| Frontend      | React 19, TypeScript strict, Canvas API                                                  |
| Storage       | SQLite (WAL mode) with read connection pool (1 writer + N readers)                       |
| Protocol      | APROTO over TCP + SLIP framing                                                           |
| Tests         | `cargo test --lib` (39+ unit tests) + Vitest with React Testing Library (35+ unit tests) |
| Benches       | criterion + pprof flamegraphs                                                            |
| Auth          | JWT bearer middleware (config-disabled by default)                                       |
| Rate limiting | Per-IP token bucket on POST endpoints (when auth is on)                                  |
| Deploy        | Docker (multi-stage Rust + Node -> Debian slim)                                          |

## Build Artifacts (from Apex release)

| Artifact            | Generator                             | Purpose                                                             |
| ------------------- | ------------------------------------- | ------------------------------------------------------------------- |
| Struct dict JSONs   | `apex_data_gen`                       | Field-level decoding of telemetry, params, state, inspect responses |
| `app_manifest.json` | `make zenith-target`                  | Component registry: fullUid, name, type, instance index             |
| `telemetry.json`    | `make zenith-target` (then customize) | Plot layout presets (auto-generated defaults, user-customizable)    |
| `commands.json`     | `make zenith-target`                  | Per-component command catalog with field types                      |
| TPRM binaries       | `apex_tprm_compile`                   | Runtime configuration loaded by apex on startup                     |

## Quickstart

```bash
# 1. Create target config directory and copy in build artifacts
mkdir -p targets/my-target/structs
cp /path/to/apex/build/apex_data_db/*.json targets/my-target/structs/
cp /path/to/apex/app_manifest.json targets/my-target/

# 2. Optional: write a telemetry.json plot layout and a commands.json
#    Both are optional -- zenith works without them. See targets/pi-ops-demo/
#    in this repo for working examples.

# 3. Configure
cat > config.toml << 'EOF'
[server]
host = "0.0.0.0"
port = 8080

[storage]
path = "./data/zenith.db"
retention_hours = 24
max_db_size_mb = 2048

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
| `make test-backend`  | Backend only: `cargo test --lib` (currently 42 unit tests)                                     |
| `make test-frontend` | Frontend only: `vitest run` (currently 35 unit tests)                                          |
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
```

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
GET    /api/targets/{id}/schedule
GET    /api/targets/{id}/commands

# Parameters (TUNABLE_PARAM)
GET    /api/targets/{id}/params
GET    /api/targets/{id}/params/{uid}
POST   /api/targets/{id}/params/{uid}/update     # TPRM upload + RELOAD_TPRM

# Telemetry
WS     /api/targets/{id}/telemetry/live
GET    /api/targets/{id}/telemetry/latest
GET    /api/targets/{id}/telemetry/history?channel=&start_ms=&end_ms=&limit=
GET    /api/targets/{id}/telemetry/csv
GET    /api/telemetry/stats
POST   /api/telemetry/downsample

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

When `[auth] enabled = true` in `config.toml`:

- All `/api/*` routes except `/api/auth/login` and `/api/health`
  require `Authorization: Bearer <jwt>` (or `?token=<jwt>` for
  WebSocket upgrades, since browsers can't set Authorization on
  WS).
- Per-IP token bucket rate limit of 10 req/sec (burst 30) on POST
  endpoints. Returns 429 when exceeded.
- Login: `POST /api/auth/login` with `{username, password}`.
  Returns `{token, expires_in}`. The hash compares against
  `[auth] secret`.

When auth is disabled (the default for development), the middleware
is a pass-through. All endpoints are open and no rate limiting
applies.

The audit log captures every state-changing action regardless of
whether auth is on. View it at `GET /api/audit` or via the Audit Log
page in the UI. Each entry has timestamp, actor, action, target,
detail, status, and source IP.

## License

MIT
