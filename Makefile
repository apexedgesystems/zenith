# ==============================================================================
# Zenith - Build System
#
# Build/run/test/lint targets run inside Docker (no local Rust/Node required;
# lint is clippy and ESLint in the dev container). Format targets run
# pre-commit on the host (one-time setup: `make hooks`).
#
# Usage:
#   make build          Build production Docker image
#   make run            Run with docker compose
#   make dev            Development mode (rebuild + run)
#   make test           Run all tests
#   make format         Auto-fix formatting via pre-commit
#   make format-check   Check formatting (CI mode)
#   make lint           Run clippy strict
#   make smoke-boot     Boot the built image and check what it serves
#   make secret-scan    Scan the tree (and SECRET_SCAN_RANGE) for secrets
#   make hooks          Install git pre-commit hook
#   make clean          Remove build artifacts
#   make help           Show all targets
# ==============================================================================

.PHONY: build build-clean run run-clean dev stop test test-backend test-frontend smoke-boot secret-scan bench clean format format-check lint hooks help

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

## Run backend unit tests
test-backend:
	$(COMPOSE) run --rm -T dev bash -c \
		'cd backend && CARGO_TARGET_DIR=target-test cargo test --lib'

## Run frontend unit tests (Vitest)
test-frontend:
	$(COMPOSE) run --rm -T dev bash -c \
		'cd frontend && npm ci && npm run test'

## Verify the declared version agrees everywhere (and with a tag:
## make version-check TAG=v0.2.0). The release workflow runs this
## before publishing.
version-check:
	tools/version-check.sh $(TAG)

## Boot a built image as docker run does with no mounts and no environment,
## and check what it serves (tools/smoke-boot.sh; builds nothing): health
## and the declared version, the bundled demo targets under their ids,
## their curated plots within each target's recorded ceiling of
## undecodable channels, a non-empty command catalog where one is
## declared, and the console. CI runs it on every image it builds.
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
		'cd backend && cargo clippy --all-targets -- -D warnings'
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
	@echo "Builds, tests and lint run inside Docker; no local Rust or Node required."
	@echo "format, format-check and hooks run pre-commit on the host; smoke-boot"
	@echo "needs docker, curl and jq there, secret-scan docker, git and GNU tar"
	@echo "(docs/TESTING.md)."
