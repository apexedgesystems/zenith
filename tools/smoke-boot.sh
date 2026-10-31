#!/bin/bash
# ==============================================================================
# smoke-boot.sh - Boot a built zenith image and check what it serves.
#
# Runs the image as `docker run <image>` does with no mounts and no
# environment, so the config and target directories baked into it are what
# boots, on bridge networking with only the front door published, on a
# loopback port. The bundled targets dial and listen on the container's own
# loopback, find no peers there, and fail or wait; that is expected. Within
# a bounded wait it checks that:
#
#   - GET /api/health answers status "ok" and the version the workspace
#     Cargo.toml declares (tools/version-check.sh);
#   - GET /api/targets lists exactly the targets of the table below, each
#     under its positional id;
#   - each of them serves curated layouts with plotted channels, no more of
#     them undecodable than its ceiling in the table;
#   - each target the table marks as declaring a command catalog serves a
#     non-empty one (quick commands or components);
#   - GET / serves a page whose module script answers a JavaScript type.
#
# A failed check, or a response of the wrong shape, prints FAIL with the
# problem and the container log. The container is created first and removed
# by its id on every exit, so a name another run holds is never touched.
#
# Usage:
#   smoke-boot.sh --image <ref> [--port <port>] [--timeout <seconds>]
#                 [--verbose]
#
# Options:
#   --image <ref>         Image to boot, from the local store; never pulled
#                         (required; make smoke-boot passes the compose build)
#   --port <port>         Loopback host port for the front door
#                         (default: 18080)
#   --timeout <seconds>   Longest wait for /api/health (default: 60)
#   --verbose             Print the API responses and, on success, the
#                         container log
#   --help                Show this help
#
# ==============================================================================

set -euo pipefail

# ==============================================================================
# Constants
# ==============================================================================

readonly SCRIPT_NAME="smoke-boot"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIR

# The front door of the bundled config ([server] port).
readonly CONTAINER_PORT=8080

# The demo targets the image bundles (deploy/config.toml), one per line as
# id|name|ceiling|commands. Ids are positional in that file and key every
# stored sample, so a published image keeps each target under its id. The
# ceiling is the recorded number of plotted channels the bundled
# dictionaries leave undecodable (target-0: the WaveGenerator fields the
# decoder cannot place); any increase fails. commands is yes where the
# bundled config declares a command catalog. Changing the bundled targets
# changes this table with them.
readonly -a BUNDLED_TARGETS=(
  "target-0|Apex ops demo|8|yes"
  "target-1|cFS cpu1|0|no"
  "target-2|F-prime demo|0|no"
)

# Typed access for the jq filters: a field of the wrong type is an error,
# which read_json turns into a failed check.
readonly JQ_DEFS='
def str: if type == "string" then . else error("a string expected, \(type) found") end;
def arr: if type == "array" then . else error("an array expected, \(type) found") end;
def obj: if type == "object" then . else error("an object expected, \(type) found") end;
'

# ==============================================================================
# Logging
# ==============================================================================

log() { printf '\033[36m[%s]\033[0m %s\n' "$SCRIPT_NAME" "$1" >&2; }
log_ok() { printf '\033[32m[%s]\033[0m %s\n' "$SCRIPT_NAME" "$1" >&2; }
err() { printf '\033[31m[%s]\033[0m %s\n' "$SCRIPT_NAME" "$1" >&2; }
verbose() { if [[ "$VERBOSE" == "1" ]]; then log "$1"; fi; }

# ==============================================================================
# CLI Parsing
# ==============================================================================

IMAGE=""
PORT="18080"
TIMEOUT="60"
VERBOSE=0

usage() {
  sed -n '2,/^# ==/{ /^#/s/^# \{0,1\}//p }' "$0"
  exit 0
}

# Fail unless option $1 has a value; $2 is the count of arguments left.
need_value() {
  [[ "$2" -ge 2 ]] || {
    err "$1 needs a value"
    exit 1
  }
}

while [[ $# -gt 0 ]]; do
  case "$1" in
  --image)
    need_value "$1" "$#"
    IMAGE="$2"
    shift 2
    ;;
  --port)
    need_value "$1" "$#"
    PORT="$2"
    shift 2
    ;;
  --timeout)
    need_value "$1" "$#"
    TIMEOUT="$2"
    shift 2
    ;;
  --verbose)
    VERBOSE=1
    shift
    ;;
  --help | -h) usage ;;
  *)
    err "Unknown option: $1"
    exit 1
    ;;
  esac
done

[[ -n "$IMAGE" ]] || {
  err "--image is required (an image in the local store)"
  exit 1
}
if [[ ! "$PORT" =~ ^[1-9][0-9]{0,4}$ ]] || ((10#$PORT > 65535)); then
  err "--port must be a port number (1-65535), not '$PORT'"
  exit 1
fi
[[ "$TIMEOUT" =~ ^[1-9][0-9]{0,4}$ ]] || {
  err "--timeout must be a whole number of seconds, not '$TIMEOUT'"
  exit 1
}
for tool in docker curl jq; do
  command -v "$tool" >/dev/null 2>&1 || {
    err "$tool is required"
    exit 1
  }
done

NAME="zenith-smoke-$PORT"
BASE_URL="http://127.0.0.1:$PORT"
CONTAINER_ID=""
DECLARED_VERSION=""
BODY=""
BODY_PATH=""

# ==============================================================================
# Container
# ==============================================================================

cleanup() {
  if [[ -n "$CONTAINER_ID" ]]; then
    docker rm --force "$CONTAINER_ID" >/dev/null 2>&1 || true
  fi
}

print_log() {
  log "container log ($NAME):"
  docker logs "$CONTAINER_ID" 2>&1 | sed 's/^/  /' >&2 || true
}

# Report a failed check with the container log and stop; the EXIT trap
# removes the container.
fail() {
  err "FAIL: $1"
  if [[ -n "$CONTAINER_ID" ]]; then
    print_log
  fi
  exit 1
}

# The declared version (the workspace Cargo.toml's), read by the tool the
# release workflow proves a tag with.
read_declared_version() {
  local out
  out=$("$SCRIPT_DIR/version-check.sh" 2>&1) || {
    err "tools/version-check.sh: $out"
    exit 1
  }
  DECLARED_VERSION="${out#version }"
}

# Create, then start: only the container this run created is ever removed,
# by its id, so losing the name to a concurrent run removes nothing.
start_container() {
  local out
  docker image inspect "$IMAGE" >/dev/null 2>&1 || {
    err "image $IMAGE is not in the local image store; build or pull it first"
    exit 1
  }
  if docker container inspect "$NAME" >/dev/null 2>&1; then
    err "a container named $NAME already exists; remove it or use another --port"
    exit 1
  fi
  out=$(docker create --pull never --name "$NAME" \
    --publish "127.0.0.1:$PORT:$CONTAINER_PORT" "$IMAGE" 2>&1) ||
    fail "docker create: $out"
  CONTAINER_ID=$(tail -n 1 <<<"$out")
  if [[ ! "$CONTAINER_ID" =~ ^[0-9a-f]{64}$ ]]; then
    CONTAINER_ID=""
    fail "docker create printed no container id: $out"
  fi
  out=$(docker start "$CONTAINER_ID" 2>&1) || fail "docker start: $out"
  log "booted $IMAGE as $NAME on $BASE_URL"
}

# Poll /api/health until it answers, the container stops, or the timeout
# passes; a stopped container fails at once rather than at the deadline.
wait_for_health() {
  local start=$SECONDS
  local state
  while true; do
    state=$(docker container inspect --format '{{.State.Status}} {{.State.ExitCode}}' "$CONTAINER_ID" 2>&1) ||
      fail "container $NAME is gone: $state"
    if [[ "${state%% *}" != "running" ]]; then
      fail "the container stopped (${state%% *}, exit code ${state##* }) before /api/health answered"
    fi
    if curl --silent --fail --max-time 2 --output /dev/null "$BASE_URL/api/health"; then
      log "/api/health answered after $((SECONDS - start))s"
      return 0
    fi
    if ((SECONDS - start >= TIMEOUT)); then
      fail "/api/health did not answer within ${TIMEOUT}s"
    fi
    sleep 1
  done
}

# GET a JSON object into BODY; an HTTP error, or a body that is not a JSON
# object, fails the smoke.
fetch_json() {
  local path="$1"
  local out
  out=$(curl --silent --show-error --fail --max-time 5 "$BASE_URL$path" 2>&1) ||
    fail "GET $path: $out"
  jq --exit-status 'type == "object"' >/dev/null 2>&1 <<<"$out" ||
    fail "GET $path did not answer a JSON object: ${out:0:200}"
  BODY="$out"
  BODY_PATH="$path"
  verbose "GET $path: $BODY"
}

# Read BODY through a jq filter into the variable named $1. A filter that
# fails on the body, a field of the wrong type, fails the smoke and names
# the response instead of ending the script silently.
read_json() {
  local _var="$1" _filter="$2" _out
  _out=$(jq -r "$JQ_DEFS $_filter" <<<"$BODY" 2>&1) ||
    fail "GET $BODY_PATH answered an unexpected shape (${_out}): ${BODY:0:200}"
  printf -v "$_var" '%s' "$_out"
}

# ==============================================================================
# Checks
# ==============================================================================

check_health() {
  local status writable version
  fetch_json /api/health
  read_json status '.status | str'
  read_json writable '.db_writable'
  read_json version '.version | str'
  [[ "$status" == "ok" ]] ||
    fail "/api/health reports status '$status' (db_writable: $writable), not 'ok'"
  [[ "$version" == "$DECLARED_VERSION" ]] ||
    fail "/api/health reports version $version; the workspace Cargo.toml declares $DECLARED_VERSION"
  log_ok "health: status ok, version $version"
}

check_targets() {
  local entry id name expected="" listed
  for entry in "${BUNDLED_TARGETS[@]}"; do
    IFS='|' read -r id name _ _ <<<"$entry"
    expected+="$id=$name"$'\n'
  done
  expected="${expected%$'\n'}"
  fetch_json /api/targets
  read_json listed '.targets | arr | .[] | obj | "\(.id | str)=\(.name | str)"'
  [[ "$listed" == "$expected" ]] ||
    fail "/api/targets does not list the bundled targets
expected:
$expected
listed:
${listed:-(none)}"
  log_ok "targets: the ${#BUNDLED_TARGETS[@]} bundled targets, each under its id"
}

# Every bundled target ships curated layouts with plotted channels; a target
# without them (one with commands only, say) would need a rule of its own. A
# target whose definition files did not load is listed all the same: it
# serves no layouts, or the layouts endpoint reports its plotted channels as
# unknown_channels, the ones its loaded dictionaries cannot decode (the
# console flags them to the operator). The ceiling allows the recorded
# number of undecodable channels and fails on any more.
check_definitions() {
  local entry id name ceiling plotted unknown sample
  for entry in "${BUNDLED_TARGETS[@]}"; do
    IFS='|' read -r id name ceiling _ <<<"$entry"
    fetch_json "/api/targets/$id/telemetry/layouts"
    read_json plotted '[.layouts | arr | .[] | obj | .plots | arr | .[] | obj | .channels | arr | .[] | str] | unique | length'
    read_json unknown '[.layouts | arr | .[] | obj | .unknown_channels | arr | .[] | str] | unique | length'
    ((plotted > 0)) ||
      fail "$id ($name) serves no curated layouts with plotted channels: its telemetry definition did not load"
    if ((unknown > ceiling)); then
      read_json sample '[.layouts[].unknown_channels[]] | unique | .[:4] | join(", ")'
      fail "$id ($name): $unknown of its $plotted plotted channels are undecodable, above its recorded ceiling of $ceiling ($sample, ...): its dictionaries did not load in full"
    fi
    log_ok "$id ($name): $((plotted - unknown)) of $plotted plotted channels decodable (ceiling $ceiling undecodable)"
  done
}

check_commands() {
  local entry id name commands count
  for entry in "${BUNDLED_TARGETS[@]}"; do
    IFS='|' read -r id name _ commands <<<"$entry"
    [[ "$commands" == "yes" ]] || continue
    fetch_json "/api/targets/$id/commands"
    read_json count '(.quickCommands | arr | length) + (.components | obj | length)'
    ((count > 0)) ||
      fail "$id ($name) serves an empty command catalog: its commands file did not load"
    log_ok "$id ($name): command catalog served ($count quick commands and components)"
  done
}

check_console() {
  local page script type
  page=$(curl --silent --show-error --fail --max-time 5 "$BASE_URL/" 2>&1) ||
    fail "GET /: $page"
  script=$(grep -o '<script type="module"[^>]* src="[^"]*"' <<<"$page" | head -n 1 |
    sed 's/.* src="//; s/"$//') || true
  [[ -n "$script" ]] ||
    fail "GET / served a page with no module script (<script type=\"module\" src=...>): ${page:0:200}"
  type=$(curl --silent --show-error --fail --max-time 5 --output /dev/null \
    --write-out '%{content_type}' "$BASE_URL$script" 2>&1) ||
    fail "GET $script: $type"
  [[ "$type" == *javascript* ]] ||
    fail "GET $script answered $type, not the console script: the bundle is incomplete"
  log_ok "console: / and $script served"
}

# ==============================================================================
# Smoke
# ==============================================================================

trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

read_declared_version
start_container
wait_for_health
check_health
check_targets
check_definitions
check_commands
check_console
if [[ "$VERBOSE" == "1" ]]; then
  print_log
fi
log_ok "PASS: $IMAGE boots its bundled config and serves version $DECLARED_VERSION"
