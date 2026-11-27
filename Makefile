# ==============================================================================
# Zenith - Build System
#
# Build/run/test/coverage/lint targets run inside Docker (no local Rust/Node
# required; lint is clippy and ESLint, fmt-check is cargo fmt, both in the
# dev container). format and format-check run pre-commit on the host
# (one-time setup: `make hooks`).
#
# Usage:
#   make build          Build production Docker image
#   make run            Run with docker compose
#   make dev            Development mode (rebuild + run)
#   make test           Run all tests
#   make coverage       Run all tests instrumented; reports under coverage/
#   make format         Auto-fix formatting via pre-commit
#   make format-check   Check formatting: every pre-commit hook, check only
#   make fmt-check      Check Rust formatting with cargo fmt (as CI does)
#   make lint           Run clippy strict and ESLint
#   make smoke-boot     Boot the built image and check what it serves
#   make secret-scan    Scan the tree (and SECRET_SCAN_RANGE) for secrets
#   make hooks          Install git pre-commit hook
#   make clean          Remove build artifacts
#   make help           Show all targets
# ==============================================================================

.PHONY: build build-clean run run-clean dev stop test test-backend test-frontend \
	coverage coverage-backend coverage-frontend coverage-check coverage-summary \
	coverage-clean smoke-boot secret-scan bench clean format format-check fmt-check \
	lint hooks help

IMAGE_NAME := zenith
COMPOSE := docker compose

# ------------------------------------------------------------------------------
# Production
# ------------------------------------------------------------------------------

## Build production Docker image.
##
## IMPORTANT: this builds via docker compose so the resulting image is
## tagged as `zenith-zenith` (compose's auto-name from project+service),
## which is the tag that `make run` / `docker compose up` actually pulls.
## Building with `docker build -t zenith` directly creates a separately
## tagged image that compose ignores.
build:
	$(COMPOSE) build zenith

## Build with --no-cache to force fresh frontend + backend compile.
## Use this when frontend source has changed but Docker layer cache is
## hanging on to a stale dist. Plain `make build` will silently reuse
## the cached frontend stage if Docker can't tell anything in frontend/
## changed.
build-clean:
	$(COMPOSE) build --no-cache zenith

## Run with docker compose (detached)
run: build
	$(COMPOSE) up -d zenith

## Force-rebuild from scratch and run. Slower than `make run` but
## guaranteed to ship the latest frontend.
run-clean: build-clean
	$(COMPOSE) up -d zenith

## Stop running containers
stop:
	$(COMPOSE) down

## Rebuild and run in foreground
dev: build
	$(COMPOSE) up zenith

# ------------------------------------------------------------------------------
# Testing and Quality
# ------------------------------------------------------------------------------

## Run backend + frontend tests
test: test-backend test-frontend

## Run backend unit tests, the library's and the server binary's
test-backend:
	$(COMPOSE) run --rm -T dev bash -c \
		'cd backend && CARGO_TARGET_DIR=target-test cargo test --lib --bins --locked'

## Run frontend unit tests (Vitest)
test-frontend:
	$(COMPOSE) run --rm -T dev bash -c \
		'cd frontend && npm ci && npm run test'

# Coverage: each half's suite runs once, instrumented; a failing test fails
# its target and leaves no report for that half. Reports land under
# coverage/<half>/: summary.txt (the per-file table), lcov.info and html/.
# Each run counts only its own execution: the backend first removes the
# profiles and workspace builds of earlier runs (cargo llvm-cov clean
# --workspace; the dependency builds stay), since --no-report keeps them
# and its report would merge them, and the frontend removes its old report
# before installing, so a failed install leaves none behind.
# The backend measures the library's and the server binary's tests with the
# dev image's cargo-llvm-cov; llvm-cov counts the inline #[cfg(test)] modules
# as covered lines, so a production file's row says more than the total. The
# frontend measures every source file under src/, loaded by a test or not
# (vitest's v8 provider, configured in vite.config.ts). The container writes
# the reports as root, and llvm-cov makes its HTML directories readable by
# owner and group only, so coverage-backend opens its report to every user:
# the host and CI's upload step read it.
#
# coverage-check fails a half whose line coverage is under its floor. The
# floors stay 0, report only, until the suites have settled; then each sits a
# few points under a measured value and is only ever raised. `?=` takes a
# per-run override: make coverage-check COVERAGE_BACKEND_MIN_LINE=<percent>
COVERAGE_BACKEND_MIN_LINE  ?= 0
COVERAGE_FRONTEND_MIN_LINE ?= 0

## Run all tests instrumented; reports under coverage/backend and coverage/frontend
coverage: coverage-backend coverage-frontend

## Measure backend coverage: the library's and the server binary's tests
coverage-backend:
	$(COMPOSE) run --rm -T dev bash -c 'set -eo pipefail; cd backend; \
		export CARGO_TARGET_DIR=target-cov; \
		rm -rf ../coverage/backend; \
		cargo llvm-cov clean --workspace; \
		cargo llvm-cov --lib --bins --locked --no-report; \
		mkdir -p ../coverage/backend; \
		cargo llvm-cov report --lcov --output-path ../coverage/backend/lcov.info; \
		cargo llvm-cov report --html --output-dir ../coverage/backend; \
		cargo llvm-cov report | tee ../coverage/backend/summary.txt; \
		chmod -R a+rX ../coverage/backend; \
		$(if $(COVERAGE_GATE),cargo llvm-cov report --fail-under-lines $(COVERAGE_BACKEND_MIN_LINE) >/dev/null \
			|| { echo "coverage-backend: line coverage is under the floor of $(COVERAGE_BACKEND_MIN_LINE)%" >&2; exit 1; })'

## Measure frontend coverage: every source file under frontend/src
coverage-frontend:
	$(COMPOSE) run --rm -T dev bash -c \
		'rm -rf coverage/frontend && cd frontend && npm ci && npm run test -- --coverage \
			$(if $(COVERAGE_GATE),--coverage.thresholds.lines=$(COVERAGE_FRONTEND_MIN_LINE))'

## Run coverage and fail a half under its line floor (COVERAGE_BACKEND_MIN_LINE,
## COVERAGE_FRONTEND_MIN_LINE; 0, the default, only reports)
coverage-check: COVERAGE_GATE := 1
coverage-check: coverage

## Print each half's coverage table as Markdown, for a CI job summary (host
## side: reads coverage/, writes nothing)
coverage-summary:
	@echo '## Coverage'; \
	for half in backend frontend; do \
		echo; \
		case $$half in \
			backend) echo '### Backend: library and server binary tests (cargo llvm-cov)'; echo; \
				echo 'Line counts include the inline test modules; read a production file by its row.' ;; \
			frontend) echo '### Frontend: every file under src/ (vitest, v8)' ;; \
		esac; \
		echo; \
		if [ -s coverage/$$half/summary.txt ]; then \
			echo '```text'; cat coverage/$$half/summary.txt; echo '```'; \
		else \
			echo "No report: coverage/$$half/summary.txt is missing (make coverage-$$half did not finish)."; \
		fi; \
	done

## Remove the coverage reports and the instrumented build (in the dev
## container, which owns them)
coverage-clean:
	$(COMPOSE) run --rm -T dev rm -rf coverage backend/target-cov

## Verify the declared version agrees everywhere (and with a tag:
## make version-check TAG=v0.2.0). The release workflow runs this
## before publishing.
version-check:
	tools/version-check.sh $(TAG)

## Boot a built image and check what it serves (tools/smoke-boot.sh;
## builds nothing), three times: as docker run does with no mounts and no
## environment, under the compose posture with a root-owned tmpfs data
## directory and an extra group, and as the account (--user). The image's
## working directory must be its data directory; each boot must answer
## /api/health and turn healthy by the image's health check, with the
## server as an unprivileged PID 1 (no root identity, no capabilities,
## no-new-privileges). The first also checks the declared version, that
## nothing was handed over, the bundled demo targets under their ids and
## in their states after auto-connect (two listening, one down), their
## curated plots within each target's recorded ceiling of undecodable
## channels, a non-empty command catalog where one is declared, and the
## console, and is then stopped with the stop signal: it must exit 0, log
## its shutdown-complete line and a disconnect for each listening target.
## The second checks the extra group and one hand-over of the data
## directory. CI runs it on every image it builds.
## SMOKE_IMAGE defaults to this checkout's compose build (docker compose
## config --images zenith); SMOKE_PORT and SMOKE_TIMEOUT override the
## script's defaults.
smoke-boot: SMOKE_IMAGE ?= $(shell $(COMPOSE) config --images zenith 2>/dev/null)
smoke-boot: SMOKE_ARGS = --image "$(SMOKE_IMAGE)" \
	$(if $(SMOKE_PORT),--port "$(SMOKE_PORT)") \
	$(if $(SMOKE_TIMEOUT),--timeout "$(SMOKE_TIMEOUT)")
smoke-boot:
	@if [ -z "$(SMOKE_IMAGE)" ]; then \
		if [ "$(origin SMOKE_IMAGE)" = file ]; then \
			echo "smoke-boot: docker compose config --images zenith printed no image; set SMOKE_IMAGE" >&2; \
		else \
			echo "smoke-boot: SMOKE_IMAGE is set but empty ($(origin SMOKE_IMAGE)); name an image, or unset it for the compose build" >&2; \
		fi; \
		exit 1; \
	fi
	tools/smoke-boot.sh $(strip $(SMOKE_ARGS))

# gitleaks by its multi-arch index digest, bumped by hand (dependabot reads
# no Makefile). Its rules are built into the release, extended by
# .gitleaks.toml, and both scans run with no network.
GITLEAKS_IMAGE := ghcr.io/gitleaks/gitleaks:v8.30.1@sha256:c00b6bd0aeb3071cbcb79009cb16a60dd9e0a7c60e2be9ab65d25e6bc8abbb7f
GITLEAKS_FLAGS := --config /repo/.gitleaks.toml --no-banner --redact --verbose --ignore-gitleaks-allow

## Scan for secrets with gitleaks (rules and exceptions in .gitleaks.toml):
## the tree as git sees it, tracked files plus untracked ones no ignore rule
## covers, and with SECRET_SCAN_RANGE=<base>..<head> every commit in that
## range, diffs read as text. Before any scan it refuses a .gitleaksignore
## anywhere in the working tree and, with a range, anything but a full
## checkout and a range git cannot resolve on the host. CI scans each pull
## request's commits. Findings print with the secret redacted.
secret-scan: SHELL := /bin/bash
secret-scan: .SHELLFLAGS := -euo pipefail -c
secret-scan:
	@found=$$(find . -path ./.git -prune -o -name .gitleaksignore -print -quit 2>/dev/null || true); \
	if [ -n "$$found" ] || [ -e .gitleaksignore ]; then \
		echo "secret-scan: $${found:-./.gitleaksignore} would drop findings unreviewed;" \
			"exceptions belong in .gitleaks.toml" >&2; \
		exit 1; \
	fi
ifneq ($(SECRET_SCAN_RANGE),)
	@test -d .git || { echo "secret-scan: the range scan reads the repository inside" \
		"a container and needs a full checkout; .git here is not a directory" >&2; exit 1; }
	@n=$$(git rev-list --count $(SECRET_SCAN_RANGE)); echo "secret-scan: $$n commits in $(SECRET_SCAN_RANGE)"
endif
	git ls-files -z --cached --others --exclude-standard \
		| { grep -zv -E '(^|/)\.gitleaksignore$$' || true; } \
		| tar --create --file=- --null --no-recursion --ignore-failed-read --files-from=- \
		| docker run --rm -i --network none --entrypoint sh $(GITLEAKS_IMAGE) -c \
			'mkdir /repo && tar -xf - -C /repo && cd /repo && gitleaks dir . $(GITLEAKS_FLAGS)'
ifneq ($(SECRET_SCAN_RANGE),)
	docker run --rm --network none -v "$(CURDIR):/repo:ro" $(GITLEAKS_IMAGE) git /repo \
		--log-opts="--text $(SECRET_SCAN_RANGE)" $(GITLEAKS_FLAGS)
endif

## Run criterion benchmarks
bench:
	$(COMPOSE) run --rm -T dev bash -c \
		'cd backend && CARGO_TARGET_DIR=target-bench cargo bench --bench protocol --bench storage --bench decoder'

# ------------------------------------------------------------------------------
# Formatting and Linting (pre-commit driven)
# ------------------------------------------------------------------------------
#
# We use pre-commit (https://pre-commit.com) as the canonical formatter and
# linter driver. The .pre-commit-config.yaml at the repo root defines the
# full hook list:
#
#   - General hygiene (trailing whitespace, line endings, JSON/YAML/TOML check)
#   - Prettier for TS/TSX/CSS/MD/JSON/YAML
#   - Markdownlint
#   - Hadolint (Dockerfile)
#   - cargo fmt for Rust
#   - ASCII-only enforcement
#   - TODO/FIXME/HACK forbidden enforcement
#
# Install with `make hooks` once per clone, then `make format` auto-fixes
# everything pre-commit knows how to fix and `make format-check` runs in
# check-only mode (suitable for CI).

## Auto-fix formatting issues across the whole repo
format:
	pre-commit run --all-files

## Run every pre-commit hook; fail, showing the diff, when any changes a file (CI mode)
format-check:
	pre-commit run --all-files --show-diff-on-failure

## Check Rust formatting with cargo fmt in the dev container (the check CI
## runs; format-check runs it on the host among the other hooks)
fmt-check:
	$(COMPOSE) run --rm -T dev bash -c 'cd backend && cargo fmt -- --check'

## Install the git pre-commit hook so commits run formatters automatically
hooks:
	pre-commit install
	@echo "pre-commit hooks installed. Commits will now run formatters."

## Lint code (Rust clippy with -D warnings, then ESLint; runs in Docker)
##
## Not part of pre-commit by default because clippy can take 30s+ on a
## cold cache. Run this manually before pushing or in CI. ESLint uses the
## installed frontend dependencies, installing them first when there are
## none (a fresh checkout).
lint:
	$(COMPOSE) run --rm -T dev bash -c \
		'cd backend && cargo clippy --all-targets --locked -- -D warnings'
	$(COMPOSE) run --rm -T dev bash -c \
		'cd frontend && { [ -d node_modules ] || npm ci; } && npx eslint . --max-warnings 200'

# ------------------------------------------------------------------------------
# Cleanup
# ------------------------------------------------------------------------------

## Remove build artifacts and containers
clean:
	$(COMPOSE) down -v --rmi local 2>/dev/null || true
	docker rmi $(IMAGE_NAME) 2>/dev/null || true

# ------------------------------------------------------------------------------
# Help
# ------------------------------------------------------------------------------

## Show available targets
help:
	@echo "Zenith - Real-time Operations Interface"
	@echo ""
	@echo "Usage: make <target>"
	@echo ""
	@echo "Targets:"
	@grep -E '^## ' Makefile | sed 's/^## /  /'
	@echo ""
	@echo "Builds, tests and lint run inside Docker; no local Rust or Node required."
	@echo "format, format-check and hooks run pre-commit on the host, and"
	@echo "coverage-summary reads coverage/ there; smoke-boot needs docker, curl"
	@echo "and jq there, secret-scan docker, git and GNU tar (docs/TESTING.md)."
