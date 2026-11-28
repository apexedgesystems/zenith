# Zenith Testing Guide

How to run the tests and coverage, where the tests live, how to add
one, and what CI and the boot smoke check.

## Running

Tests, coverage, lint and the Rust format check run inside the dev
container (`docker compose run dev`); no local Rust or Node toolchain
is needed. `make format`, `make format-check` and
`make coverage-summary` run on the host.

| Target                   | What it does                                                                                       |
| ------------------------ | -------------------------------------------------------------------------------------------------- |
| `make test`              | Both suites                                                                                        |
| `make test-backend`      | The library's tests and the server binary's: `cargo test --lib --bins --locked`                    |
| `make test-frontend`     | `npm ci`, then Vitest with React Testing Library                                                   |
| `make coverage`          | Both suites once, instrumented; reports under `coverage/backend` and `coverage/frontend`           |
| `make coverage-backend`  | The backend half (cargo-llvm-cov, the dev image's pinned release)                                  |
| `make coverage-frontend` | The frontend half (Vitest's v8 provider)                                                           |
| `make coverage-check`    | Coverage, failing a half under its line floor (`COVERAGE_BACKEND_MIN_LINE`, `..._FRONTEND_...`)    |
| `make coverage-summary`  | Each half's table as Markdown (CI's job summary); reads `coverage/`, writes nothing                |
| `make coverage-clean`    | Remove the reports and the instrumented build (in the container, which owns them)                  |
| `make lint`              | clippy `--all-targets -D warnings`, then ESLint                                                    |
| `make fmt-check`         | `cargo fmt -- --check` in the container, as CI runs it                                             |
| `make format-check`      | Every pre-commit hook in check mode (Prettier, markdownlint, hadolint, cargo fmt, ASCII, no TODOs) |
| `make smoke-boot`        | Boot the built image and check what it serves and how it stops (below)                             |
| `make secret-scan`       | gitleaks over the tree, and over `SECRET_SCAN_RANGE` commits (below)                               |
| `make bench`             | The criterion benches: `protocol`, `storage`, `decoder`                                            |

The backend has 208 library tests and 32 in the server binary
(`main.rs`); the library's harness runs in about 2.2 s and the
binary's in about a second. The frontend has 154 tests in 21 files and
runs in about 4 s. `make test` takes about a minute, most of it
`npm ci` and starting the containers. These are the figures at the
time of writing; the harnesses print the current ones.

### Inside the dev container

For a single test, or output from a passing one:

```bash
docker compose run --rm dev bash

# Backend
cd backend
cargo test --lib --bins                     # Both harnesses
cargo test --lib protocol::slip::tests      # One module of the library
cargo test --bin zenith tests::path_uids    # Server tests by name
cargo test --lib -- --nocapture             # Show stdout of passing tests

# Frontend
cd frontend
npm ci
npm run test                                # All tests
npx vitest run src/utils/targets.test.ts    # One file
npx vitest run -t "reads Live"              # By name

# Benchmarks
cd backend
cargo bench --bench decoder -- --profile-time=10   # pprof flamegraph
```

## Coverage

`make coverage` runs each half's suite once, instrumented. A failing
test fails its half and leaves no report for it. Each half writes
`summary.txt` (the per-file table), `lcov.info` and `html/` under
`coverage/<half>/`. A report holds only its own run: the backend half
first removes earlier runs' profiles and workspace builds
(`cargo llvm-cov clean --workspace`; the dependency builds stay), and
the frontend half removes its old report before `npm ci`, so an install
that fails leaves no report behind. The floors default to 0, so
`make coverage-check` only reports until the suites have settled; a
floor then sits a few points under a measured value and is only raised:

```bash
make coverage-check COVERAGE_BACKEND_MIN_LINE=75 COVERAGE_FRONTEND_MIN_LINE=50
```

Reading the numbers:

- **Backend** (cargo-llvm-cov): a Rust file's row counts its inline
  test code with its production code, and the TOTAL counts every file's,
  so both read higher than production code alone. `main.rs` is the
  plainest case: its row includes its own test module, which runs in
  full. For a production figure, leave every line of every
  `#[cfg(test)]` item out of `lcov.info`: each file's test modules and
  any test-only item outside them, such as `repo_structs_path` in
  `config.rs`. At the time of writing the TOTAL row reads about 83% of
  lines; production code alone is about 67%: 92% of the library's lines
  and 36% of `main.rs`'s, where the boot sequence and the handlers that
  talk to a target need a running server or a live target.
- **Frontend** (Vitest, v8): every file under `src/` is in the table,
  loaded by a test or not, so a file no test reaches reads 0%. Test
  files, `src/test/` and declaration files are left out. At the time of
  writing, lines read about 59%.

## Where tests live

### Backend

Rust tests sit in a `#[cfg(test)] mod tests` block at the bottom of the
file they test, with access to its private items. The server binary is
built on the library crate
(`use zenith::{config, core, protocol, storage}` in `main.rs`), so the
library's tests run once, in the library's harness, and the binary's
harness holds only the tests written in `main.rs`. Both harnesses run
in `make test` and `make coverage`.

The tests in `main.rs` reach the server's router in process. Their
`Server` builds the state boot builds, over a temporary database: a
config written as TOML, each `[[targets]]` entry a target under its
positional id with its link built and never dialed (a request that
reaches it is refused "not connected"). `Server::call` answers one
request through `build_router` with `tower::ServiceExt::oneshot`, with
`MockConnectInfo` standing in for the client address; `Server::audit`
reads the audit log back. The connect admission tests poll
`do_connect_target` by hand with a no-op waker, so each interleaving of
a connect and the shutdown snapshot is one the test chose.

Tests that need files use `tempfile::TempDir`; several read the shipped
target directories under `targets/` and the TPRM vectors under
`compat/tprm/`.

| File                                   | Tests | What they protect                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| -------------------------------------- | ----: | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `backend/src/main.rs`                  |    32 | The server: tunable encoding (ranges, byte order, each form of an 8-byte integer, the read's hex form taken back), rails on 8-byte integers exactly and on float32s as float32, and on a field or array element sent as hex (one verdict for every form of the same bytes, a float32 applied at its rail applying again unedited), and the update route from JSON text to the uploaded bytes, an unedited Apply of every field type written back byte for byte; command and path number forms; connect admission at its three checks; the shutdown snapshot; the sample-writer flush, and a writer past its budget not holding the runtime; the auth middleware as a table, auth off included; socket ticket scope; the audit rows of downsampling and the bearer login; trim counts; the RTS directory check; `--hash-password`; retention 0 |
| `backend/src/config.rs`                |    19 | Config parsing and the boot refusals: target directories, artifact paths and permissions, poll rows, health rules, retired keys, TM frame sizes, environment overrides, listen and dial addresses, duplicate listen ports, the retention-horizon warning                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `backend/src/core/aproto_client.rs`    |    12 | The APROTO client against a fake target: round trips, QUEUED then completion, ACK timeouts and recovery, late ACKs discarded, remote close, the reader's routing, upload chunks and CRC, retiring a session's tasks on reconnect                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `backend/src/core/auth.rs`             |    10 | Credentials, token and ticket minting and checks (expiry with no grace, required claims, HS256 only, tickets only on a query string and never in a header), boot validation, credential precedence, the refused sign-in actor                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `backend/src/core/config_manager.rs`   |    12 | Parameter decode by the dictionary (every width, 8-byte integers as exact decimal text, both byte orders, header and entries blocks), data categories, quick-command payloads, connect-init and constraint rails (integers exactly), nested fields, record tables, byte-order and skip validation                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| `backend/src/core/metrics.rs`          |     1 | The counters' snapshot: averages and ages                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `backend/src/core/poll.rs`             |     2 | A polled read carries its category; a row with no usable period is skipped                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `backend/src/core/registration.rs`     |     3 | Curated layouts seeded in file order, the shipped rover directory, nothing seeded without a usable file                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `backend/src/core/sequence.rs`         |     1 | The RTS plan check that mirrors the vehicle's load rule, its refusal text included                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `backend/src/core/session.rs`          |    17 | Browser sessions: both deadlines, keep-alive within the absolute limit, lookups that never extend, end and sweep, reload after a restart, only the id's hash at rest, cookies, the same-origin rule, each expiry audited once                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `backend/src/core/stream_link.rs`      |    21 | Stream links: pipeline compositions, unroutable units, UDP listening and first contact, TCP dial and listen states, init steps (a TCP first step that cannot be sent included), generation guards and task retirement, TM drops and resync, the record stage                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `backend/src/core/telemetry.rs`        |    17 | The live decoder and router: dictionary joins, scalar widths in both byte orders, unreadable scalars, records, priority dedup, category selection, counters, the channel inventory, the shipped layouts' references                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `backend/src/core/telemetry_socket.rs` |    12 | The live socket over loopback: JSON samples, the session-end close (1008), pings, client close and drop, frame caps, a blocked send bounded                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| `backend/src/core/tprm.rs`             |     9 | TPRM v4 against the `compat/tprm/` vectors byte for byte, each prelude defect refused, bodies past 64 KiB, variable-length hashing, READBACK and VERIFY parsing, the staged-row judgement, a staged stop never a success                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| `backend/src/core/transport.rs`        |     5 | The state word, protocol names, connect answers (a connect crossing shutdown included), and the guard that generic modules name no protocol family or framework                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `backend/src/protocol/aproto.rs`       |     8 | APROTO header and packet build and parse, ACK parsing, status names, malformed input refused                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `backend/src/protocol/ccsds_spp.rs`    |     4 | Space packet headers at their extremes, implausible headers, reassembly and resync                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `backend/src/protocol/ccsds_tm.rs`     |     4 | TM frame CRC, deframing across feeds, a corrupt frame dropped and the stream realigned, the first-header pointer                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `backend/src/protocol/slip.rs`         |     5 | SLIP frames, the 64 KiB bound (escaped bytes included), empty frames, byte-at-a-time equal to bulk feeding                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `backend/src/storage/telemetry_db.rs`  |    46 | The SQLite store: history windows and limits (the newest rows, in time order, ties in insertion order, a cut among ties, bounds past the signed range, no sort in the plan), latest values, FIFO and fair eviction, retention, tier passes, the audit log, layouts and their migrations, the read pool, preferences, sessions                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |

### Frontend

Frontend tests sit next to the source they test as `*.test.ts` or
`*.test.tsx`; Vitest finds them. `vite.config.ts` runs them under
jsdom with Vitest's globals and `src/test/setup.ts`, which loads the
jest-dom matchers and, after every test, restores real timers, the real
globals and every spy, so no test inherits another's fakes.
`src/test/helpers.ts` holds the shared fixtures: `json()` for a JSON
answer, `stubFetch()` for a routed fetch stub that records each request
it was sent (anything it does not route gets a 404), `renderWithQuery()`
for a render under a query cache that never retries, and `target()` for
a target as `GET /api/targets` lists it.

| File                                   | Tests | What they protect                                                                                                                                                                                                                                                                                                                                                                                                                   |
| -------------------------------------- | ----: | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `src/pages/Tunables.test.tsx`          |    46 | Only the selected component's values shown and applied, past a late read or a late apply's answer; 64-bit integers exact: an unedited Apply posts back the strings read, an edit is a whole decimal or `0x` number in the field's range or is refused by name, and a value past 2^53 typed into a field of unknown type (a nested leaf, a variable-length field) goes as typed; field types from the struct the block was read with |
| `src/api/session.test.ts`              |    24 | The session owner: the boot read with backoff, sign-in and sign-out, deadlines and notices on the server's clock, input-only keep-alive, answers out of order                                                                                                                                                                                                                                                                       |
| `src/api/decode.test.ts`               |    10 | The field decoder: hex, every scalar width, 64-bit exactness, strings at their NUL, arrays, unknown types and reads past the buffer, formatting, enum labels                                                                                                                                                                                                                                                                        |
| `src/pages/Commanding.test.tsx`        |     9 | Command fields: integers only as whole decimal or 0x numbers in range, floats in decimal only, non-finite and unsupported types refused, blank fields refused, the form's payload or refusal                                                                                                                                                                                                                                        |
| `src/pages/Telemetry.test.tsx`         |     9 | The live stream's reconnect rules (none after a session end) and the status: Live, Paused, Off, Listening, and the 10 s stale rule                                                                                                                                                                                                                                                                                                  |
| `src/App.test.tsx`                     |     8 | The session gate (login page, auth off, connecting, in-place sign-in, prompts), the sidebar's link-state dot, the auto-reconnect cooldown                                                                                                                                                                                                                                                                                           |
| `src/pages/Operations.test.tsx`        |     6 | Link states, an unreadable library swap logged as failed, executive actions resolved by name or refused, the lock payload                                                                                                                                                                                                                                                                                                           |
| `src/api/apiFetch.test.ts`             |     5 | The one fetch path: each request sent once, never retried, a 401 reported with its session generation                                                                                                                                                                                                                                                                                                                               |
| `src/utils/cardPrefs.test.ts`          |     5 | Card order, moves and hiding                                                                                                                                                                                                                                                                                                                                                                                                        |
| `src/utils/wholeNumber.test.ts`        |     5 | The one whole-number rule for integer fields, shared by Tunables and Commanding: decimal and `0x` hex read exactly to each width's limits, surrounding space and leading zeros, one past either end of the range refused, anything not a whole number refused, and the range the refusal states                                                                                                                                     |
| `src/components/chartGeometry.test.ts` |     4 | Chart padding, pixel-to-time mapping, the hover readout (no value outside a channel's samples, a bucket's envelope)                                                                                                                                                                                                                                                                                                                 |
| `src/pages/Dashboard.test.tsx`         |     4 | Link states, and component health asked for in the command route's hex form                                                                                                                                                                                                                                                                                                                                                         |
| `src/pages/Login.test.tsx`             |     4 | The sign-in form and the ended-session dialog                                                                                                                                                                                                                                                                                                                                                                                       |
| `src/utils/storage.test.ts`            |     4 | Storage readouts: fill, rate, durations, time to cap                                                                                                                                                                                                                                                                                                                                                                                |
| `src/utils/health.test.ts`             |     3 | Health-rule evaluation at each boundary                                                                                                                                                                                                                                                                                                                                                                                             |
| `src/types/telemetry.test.ts`          |     2 | Channel names and grouping                                                                                                                                                                                                                                                                                                                                                                                                          |
| `src/utils/targets.test.ts`            |     2 | Byte and count formatting                                                                                                                                                                                                                                                                                                                                                                                                           |
| `src/api/client.test.ts`               |     1 | `request()` failing with an ApiError on a bad shape, a body that is not JSON, or no server                                                                                                                                                                                                                                                                                                                                          |
| `src/components/Clock.test.tsx`        |     1 | The UTC clock on mount and each second                                                                                                                                                                                                                                                                                                                                                                                              |
| `src/components/dialogs.test.tsx`      |     1 | `confirmDialog` resolving on OK and Cancel                                                                                                                                                                                                                                                                                                                                                                                          |
| `src/components/MultiChart.test.tsx`   |     1 | No hover reading past a channel's last sample                                                                                                                                                                                                                                                                                                                                                                                       |

## Adding a backend test

Append to the `mod tests` block of the file the behavior lives in,
with a `/// @test` doc comment that names the behavior:

```rust
/// @test A trim of 0 deletes nothing.
#[test]
fn a_zero_trim_deletes_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = TelemetryDb::open(&dir.path().join("t.db")).unwrap();
    // ... arrange / act / assert
}
```

A handler, the middleware or the boot-time state belongs in `main.rs`'s
module, driven through the router:

```rust
#[tokio::test]
async fn my_route_refuses_what_it_should() {
    let server = Server::new(APEX_TARGET);
    let (status, body) = server.post("/api/targets/target-0/command", json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}
```

Prefer small helper functions inside the module for fixtures (see
`spec_demo_decoder` in `core/telemetry.rs` and `Server` in `main.rs`).

## Adding a frontend test

### Pure function

Create a `.test.ts` next to the source:

```ts
// src/utils/foo.test.ts
import { describe, it, expect } from "vitest";
import { foo } from "./foo";

describe("foo", () => {
  it("does the thing", () => {
    expect(foo(1)).toBe(2);
  });
});
```

Vitest's globals are enabled in `vite.config.ts`, but importing
`describe`, `it` and `expect` explicitly is fine and the linter prefers
it.

### React component

Use `.test.tsx` and React Testing Library; stub the server with
`stubFetch()`:

```tsx
import { describe, it, expect } from "vitest";
import { screen } from "@testing-library/react";
import { json, renderWithQuery, stubFetch, target } from "../test/helpers";
import MyPage from "./MyPage";

describe("MyPage", () => {
  it("lists the targets", async () => {
    const server = stubFetch((req) =>
      req.path === "/api/targets" ? json({ targets: [target()] }) : undefined,
    );
    renderWithQuery(<MyPage />);
    expect(await screen.findByText("Target")).toBeInTheDocument();
    expect(server.calls()).toContain("GET /api/targets");
  });
});
```

For timers, `vi.useFakeTimers()` and `act(() => vi.advanceTimersByTime(...))`;
the setup file puts real timers back after each test. For user input,
`userEvent.setup()` from `@testing-library/user-event`.

## Test philosophy

- **Test the contract, not the implementation.** A test should still
  pass after a refactor that does not change behavior. Avoid asserting
  on class names, internal hooks, or call counts unless those are what
  you are guarding.
- **Cover the tricky parts.** Edge cases (empty input, off-by-one, NaN,
  maximum sizes), error paths, and logic that is hard to verify by eye
  (cache reuse, dedup, decoder priority, races between a request and
  shutdown).
- **Prefer pure functions.** When something is hard to test, that is
  often a sign the decision should be a function of its own
  (`connect_answer` in `core/transport.rs`, `readoutAt` in
  `components/chartGeometry.ts`).
- **Make each test able to fail.** A test that passes with the behavior
  removed protects nothing; break the behavior once and watch it fail.
- **Run them before pushing.** The backend suite takes about 3 s and the
  frontend's about 4 s once the containers are up.

## CI

GitHub Actions runs the gate on every pull request to `main`
(`.github/workflows/ci.yml`). The Test, Lint, Build job:

1. Build the dev image (`docker compose build dev`).
2. Run both suites once, instrumented (`make -k coverage`): a failing
   test fails the step, and `-k` lets the other half still write its
   report.
3. Write both coverage tables to the job summary
   (`make coverage-summary`) and upload `coverage/` as the `coverage`
   artifact for 14 days; both run even when a test failed.
4. Lint (`make lint`): clippy with `-D warnings`, then ESLint.
5. Check Rust formatting (`make fmt-check`).
6. Build the production image (`docker compose build zenith`).
7. Boot it (`make smoke-boot`).

The Secret scan job runs beside it: `make secret-scan` over the tree
and the pull request's commits. The release workflow boots each
architecture's image the same way before pushing it and, on a tag,
stops before recording the pushed digest unless the pushed image's
config is the booted build's (`tools/pushed-image-check.sh`).

### Smoke boot

`make smoke-boot` boots a built image three times, one after another,
each publishing only the front door on the same loopback port over
bridge networking. Each boot is waited for until `/api/health` answers
and Docker reports the container healthy by the image's own health
check; an unhealthy verdict, or none within the timeout, fails and
prints the last probes. Before any boot, the image's working directory
must be `/var/lib/zenith`, where SQLite keeps its temporary files under
a read-only root.

The first boot runs the image as `docker run <image>` does with no
mounts and no environment, so the config and demo targets baked into it
are what boots. It checks that:

- `/api/health` answers status `ok` with the version the workspace
  `Cargo.toml` declares;
- the server is the container's PID 1, so the stop signal reaches it,
  and runs with no root identity (uid, gid and supplementary groups all
  non-zero), no capabilities and no-new-privileges, as `/proc/1/status`
  reads inside the container (`docker exec`);
- the entrypoint handed nothing over: the image's own data directory is
  already the zenith account's, so a fresh named volume starts out owned
  by it;
- `/api/targets` lists the demo targets of `deploy/config.toml`, each
  under its positional id, and each reads the state it should once
  boot's auto-connect has run with no peer present: the cFS target
  (UDP) and the F-prime target (tcp-listen) listening, the Apex ops demo
  (it dials, and does not auto-connect) down;
- each of those targets serves curated layouts with plotted channels,
  within its recorded ceiling of undecodable channels (8 for the Apex
  ops demo, none for the other two);
- each target whose bundled config declares a command catalog (the
  Apex ops demo) serves a non-empty one;
- `/` serves a page whose module script answers a JavaScript type;
- the stop signal shuts the server down in order: stopped as
  `docker stop` and compose stop it (the stop signal, then a kill after
  10 s), it exits 0, logs `Zenith shutdown complete`, and logs
  `Disconnected target <id> during shutdown` for each listening target.

The second boot runs it as the compose files do (`--read-only
--cap-drop ALL --cap-add CHOWN --cap-add SETUID --cap-add SETGID`),
with a root-owned tmpfs at `/var/lib/zenith` and `--group-add 20`: the
same posture, group 20 among the server's groups, and exactly one
hand-over of the data directory in the container log. The third runs it
as the account (`--user 10001:10001`), where no-new-privileges rests on
the entrypoint's last step alone: the same posture. These two are
removed by force once checked. The compose deployment still differs in
its host networking, port variable and data volume.

A failed check, or a response of the wrong shape, names the problem
and prints the container log. The run labels its containers with a
random token of its own, removes each boot's container by its id before
the next boot, and, on exit, removes the containers it finds carrying
that label, by their ids, never a name, so a second run on the same
port never removes the first's. When the create had not returned
and the first lookup finds none, it looks once more two seconds later; a
retry delay, not a deadline for Docker. A create the daemon finishes
after that last look leaves its labelled container behind, and the next
run refuses the name: inspect that container and its
`zenith-smoke-owner` label and remove only its confirmed id before
reusing the port, or use another port.

It builds nothing and never pulls, so build (or pull) first. By
default it boots this checkout's compose build, the name `docker
compose config --images zenith` prints (compose names the image after
the checkout directory). The checks run on the host and need `docker`,
`curl` and `jq`:

```bash
make build                         # docker compose build zenith
make smoke-boot                    # boots that build on 127.0.0.1:18080
make smoke-boot SMOKE_PORT=18081   # another loopback port
make smoke-boot SMOKE_IMAGE=<ref>  # any image in the local store
```

### Secret scan

`make secret-scan` runs gitleaks, pinned by digest, with the rules in
`.gitleaks.toml`: the release's default rules (private keys, JWTs,
GitHub and cloud tokens, generic high-entropy keys) and zenith's own
credentials:

- the `[auth]` signing secret as a one-line TOML string that ends the
  value, basic (`"..."`, any character but an unescaped `"`) or literal
  (`'...'`, any character but `'`), written as `secret = ...` in the
  `[auth]` table, as the dotted `auth.secret = ...` (each part bare or
  quoted), or inside `auth = { ... }` (a field set in code, such as
  `auth.secret = "...".into()`, is left to the default rules);
- a literal value of `ZENITH_AUTH_SECRET` (after `=` or `:`, bare or
  quoted, a quoted one possibly holding spaces; an interpolation such as
  `${ZENITH_AUTH_SECRET}` is not one);
- an argon2 password hash (PHC string).

Zenith accepts a signing secret of 16 bytes or more (UTF-8, as Rust's
`String::len` counts them). Both signing-secret rules apply a
16-character floor to the value as written, and written in ASCII,
escapes included, a value is never shorter than the bytes it stands for
(an escape such as `\u03b1` is six characters for two bytes). For
ASCII-written values, including escapes, this length floor does not
exclude a value zenith accepts. Shorter decoded values can also meet the
floor. Gitleaks' inherited filters still apply. A value written with
literal (unescaped) multibyte characters can reach 16 bytes in fewer
characters and is outside the rules.

A multi-line TOML string (`"""` or `'''`, even on one line) is outside
these rules (the default generic rule still catches a high-entropy value
in one), and a YAML block scalar is matched by no rule. A shell ANSI-C
quoted value, `ZENITH_AUTH_SECRET=$'...'`, is read as an interpolation
and not reported. The rules are
built into the pinned release and both scans run with no network, so a
scan fetches nothing; the pin is bumped by hand (dependabot reads no
Makefile). It scans the tree as git sees it, tracked files plus
untracked ones no ignore rule covers, and with
`SECRET_SCAN_RANGE=<base>..<head>` every commit in that range, its diffs
read as text whatever `.gitattributes` says; that is how CI scans a pull
request, so a secret added and taken out again inside the range still
fails. The range scan reads the repository from inside a container and
needs a full checkout, not a git worktree: it refuses to run in a
worktree, and on a range git cannot resolve it stops with git's error,
both before any scan runs.

Exceptions live in `.gitleaks.toml` only, which holds one: the
placeholder signing secret `change-me-in-production`, in the four files
that ship it (the 16-character floor is written there as an allowlist
too). Inline `gitleaks:allow` comments are not honored, and the scan
refuses to run while a `.gitleaksignore` exists anywhere in the working
tree, since gitleaks would read one as a list of findings to drop.
Findings print with the secret redacted. A finding on a branch that was
already pushed means the value is on the server: rotate it, then remove
it. The scan needs `git`, GNU `tar` and `docker` on the host:

```bash
make secret-scan                                       # the tree
make secret-scan SECRET_SCAN_RANGE=origin/main..HEAD   # and this branch's commits
```
