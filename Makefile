# ==============================================================================
# Zenith - Build System
#
# Build/run/test targets run inside Docker (no local Rust/Node required).
# Format/lint targets run pre-commit on the host (one-time setup: `make hooks`).
#
# Usage:
#   make build          Build production Docker image
#   make run            Run with docker compose
#   make dev            Development mode (rebuild + run)
#   make test           Run all tests
#   make format         Auto-fix formatting via pre-commit
#   make format-check   Check formatting (CI mode)
#   make lint           Run clippy strict
#   make hooks          Install git pre-commit hook
#   make clean          Remove build artifacts
#   make help           Show all targets
# ==============================================================================

.PHONY: build build-clean run run-clean dev stop test test-backend test-frontend bench clean format format-check lint hooks help

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
## tagged image that compose ignores -- hit this on 2026-04-09.
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

## Run backend unit tests
test-backend:
	$(COMPOSE) run --rm -T dev bash -c \
		'cd backend && CARGO_TARGET_DIR=target-test cargo test --lib'

## Run frontend unit tests (Vitest)
test-frontend:
	$(COMPOSE) run --rm -T dev bash -c \
		'cd frontend && npm ci --silent && npm run test'

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

## Check formatting (no fixes), show diffs on failure -- CI mode
format-check:
	pre-commit run --all-files --show-diff-on-failure

## Install the git pre-commit hook so commits run formatters automatically
hooks:
	pre-commit install
	@echo "pre-commit hooks installed. Commits will now run formatters."

## Lint code (Rust clippy with -D warnings, runs in Docker)
##
## Not part of pre-commit by default because clippy can take 30s+ on a
## cold cache. Run this manually before pushing or in CI.
lint:
	$(COMPOSE) run --rm -T dev bash -c \
		'cd backend && cargo clippy --lib --bins -- -D warnings'
	$(COMPOSE) run --rm -T dev bash -c \
		'cd frontend && npx eslint . --max-warnings 200'

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
	@echo "All targets run inside Docker. No local Rust or Node required."
