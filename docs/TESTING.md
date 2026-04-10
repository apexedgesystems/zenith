# Zenith Testing Guide

How to run tests, where they live, and how to add new ones.

## Running

All test commands run inside Docker via `docker compose`. No local
Rust or Node toolchain required.

```bash
# Run all tests (backend + frontend)
make test

# Backend only (42 unit tests, Rust)
make test-backend

# Frontend only (35 unit tests, Vitest + React Testing Library)
make test-frontend

# Lint (clippy with -D warnings)
make lint

# Benchmarks (criterion)
make bench
```

### Direct invocation (inside dev container)

For interactive debugging or running individual tests:

```bash
# Start a dev shell
docker compose run --rm dev bash

# Backend
cd backend
cargo test --lib                          # All tests
cargo test --lib protocol::slip::tests    # Single module
cargo test --lib decode_8b_output         # Single test by name
cargo test --lib -- --nocapture           # Show stdout for passing tests

# Frontend
cd frontend
npm ci --silent
npm run test                              # All tests
npx vitest run src/utils/targets.test.ts  # Single file
npx vitest run -t "encodes hex"           # Pattern match

# Benchmarks
cd backend
cargo bench --bench decoder -- --profile-time=10  # pprof flamegraph
```

## Where tests live

### Backend

Rust tests live in `#[cfg(test)] mod tests { ... }` blocks at the
bottom of the source file they test. This is the standard Rust idiom:
the tests have private-field access and there's no separate test
discovery to manage.

| File                                  | Tests cover                                                                                                                                                                                                                                |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `backend/src/protocol/slip.rs`        | SLIP encode/decode round-trip, partial feed, escape across boundary, max-frame-size enforcement, byte-by-byte vs bulk equivalence                                                                                                          |
| `backend/src/protocol/aproto.rs`      | Header parse, payload round-trip, ACK parsing, status code mapping, rejection of malformed input                                                                                                                                           |
| `backend/src/core/telemetry.rs`       | Decoder construction, struct-aware decode, OUTPUT vs STATE priority dedup, generic fallback, padding/reserved filtering, channel-name Arc cache reuse                                                                                      |
| `backend/src/storage/telemetry_db.rs` | insert_batch + query round-trip, time window filtering, limit clamp, query_latest correctness, delete_oldest ordering, delete_target isolation, target_stats span, global_stats accuracy, layout save/load, config layout deletion refusal |

Integration tests (DB + storage + actual SQLite file) use `tempfile::TempDir`
and don't need any external setup. Each test creates its own DB.

### Frontend

Frontend tests live in `*.test.ts` / `*.test.tsx` files **co-located**
with the source they test. Vitest auto-discovers them.

| Test file                       | What it covers                                                                        |
| ------------------------------- | ------------------------------------------------------------------------------------- |
| `src/utils/targets.test.ts`     | `targetsEqual` (used by App.tsx to dedupe poll responses)                             |
| `src/types/telemetry.test.ts`   | `fieldName`, `groupChannels`                                                          |
| `src/pages/Commanding.test.ts`  | `encodeField` (uint8/16/32 encoding, hex input, clamping, masking, NaN safe fallback) |
| `src/components/Clock.test.tsx` | Component-test pattern: render, fake timers, advance, unmount                         |

## Adding a backend test

Append to the existing `mod tests` block in the file you're testing:

```rust
#[test]
fn my_new_test() {
    let db = TelemetryDb::open(&...).unwrap();
    // ... arrange / act / assert
}
```

For test fixtures that need setup, prefer small helper fns inside the
`mod tests` block (see `make_wavegen_dict` in `core/telemetry.rs`).

Use `tempfile::TempDir` for file-system fixtures so tests are isolated
and self-cleaning.

## Adding a frontend test

### Pure function

For a pure function, create a `.test.ts` next to the source:

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

Vitest globals (`describe`, `it`, `expect`) are enabled by the
`globals: true` setting in `vite.config.ts`, but importing them
explicitly is fine and the linter prefers it.

### React component

Use `.test.tsx` and React Testing Library:

```tsx
import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import MyComponent from "./MyComponent";

describe("MyComponent", () => {
  it("renders the title", () => {
    render(<MyComponent title="Hello" />);
    expect(screen.getByText("Hello")).toBeInTheDocument();
  });
});
```

For components that use timers (`setInterval`, `setTimeout`):

```tsx
import { vi, beforeEach, afterEach } from "vitest";
import { act } from "@testing-library/react";

beforeEach(() => vi.useFakeTimers());
afterEach(() => vi.useRealTimers());

it("ticks", () => {
  render(<Clock />);
  act(() => {
    vi.advanceTimersByTime(1000);
  });
  expect(screen.getByText(/12:34:57/)).toBeInTheDocument();
});
```

For components that fetch data, mock `globalThis.fetch`:

```tsx
beforeEach(() => {
  vi.stubGlobal(
    "fetch",
    vi.fn(
      async () => new Response(JSON.stringify({ ok: true }), { status: 200 }),
    ),
  );
});
afterEach(() => vi.unstubAllGlobals());
```

For user interactions:

```tsx
import userEvent from "@testing-library/user-event";

const user = userEvent.setup();
await user.click(screen.getByRole("button", { name: "Submit" }));
```

## Test philosophy

- **Test the contract, not the implementation.** A test should still
  pass after a refactor that doesn't change behavior. Avoid asserting
  on class names, internal hooks, or call counts unless those are
  what you're guarding.
- **Cover the tricky parts.** Don't write 100% coverage tests for
  thin glue code. Do write tests for edge cases (empty input,
  off-by-one, NaN, max-size), error paths, and any logic that's
  hard to verify visually (cache reuse, dedup, decoder priority).
- **Prefer pure functions.** When something is hard to test, that's
  often a signal the logic should be extracted to a pure function
  (e.g. extracted `targetsEqual` and `Clock` to make them
  testable without mounting the full App).
- **Run them locally before pushing.** Both suites take well under a
  second; there's no excuse for `cargo test --lib && (cd frontend && npm test)`
  to not be muscle memory before a commit.

## CI

GitHub Actions runs on every push/PR to `main` (`.github/workflows/ci.yml`):

1. Build dev image (`docker compose build dev`)
2. Backend tests (`cargo test --lib`)
3. Frontend tests (`npm ci && npm run test`)
4. Clippy (`cargo clippy --lib --bins -- -D warnings`)
5. Cargo fmt check (`cargo fmt -- --check`)
6. Build production image (`docker compose build zenith`)
