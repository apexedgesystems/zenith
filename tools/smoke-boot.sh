#!/bin/bash
# ==============================================================================
# smoke-boot.sh - Boot a built zenith image and check what it serves.
#
# Boots the image three times, one after another, each on bridge
# networking with only the front door published, on the same loopback
# port. The bundled targets dial and listen on the container's own
# loopback, find no peers there, and fail or wait; that is expected. Each
# boot is waited for, within a bounded wait, until GET /api/health
# answers.
#
# First, as `docker run <image>` does with no mounts and no environment,
# so the config and target directories baked into it are what boots. It
# checks that:
#
#   - GET /api/health answers status "ok" and the version the workspace
#     Cargo.toml declares (tools/version-check.sh);
#   - the server is the container's PID 1, so the stop signal reaches it,
#     and runs with no root identity (uid, gid and supplementary groups all
#     non-zero), no capabilities and no-new-privileges, as /proc/1/status
#     reads inside the container;
#   - the entrypoint handed nothing over: the image's own data directory is
#     already the account's, so a fresh named volume starts out owned by it;
#   - GET /api/targets lists exactly the targets of the table below, each
#     under its positional id;
#   - each of them serves curated layouts with plotted channels, no more of
#     them undecodable than its ceiling in the table;
#   - each target the table marks as declaring a command catalog serves a
#     non-empty one (quick commands or components);
#   - GET / serves a page whose module script answers a JavaScript type.
#
# Second, as the compose files run it (every capability dropped but
# CHOWN, SETUID and SETGID), with a root-owned tmpfs at
# /var/lib/zenith and one extra group (--group-add): the same posture,
# the extra group among the server's groups, and exactly one hand-over of
# the data directory in the container log. Third, as the account itself
# (--user 10001:10001): the same posture, which there rests on the
# entrypoint's last step alone.
#
# A failed check, or a response of the wrong shape, prints FAIL with the
# problem and the container log. Each container carries a label with a
# random token of this run's own (zenith-smoke-owner); a boot's container
# is removed by its id before the next boot takes the name and port, and
# the exit trap removes the containers it finds carrying the label, by
# their ids, never one by name, so a name another run holds is never
# touched. When a create had not returned and the first lookup finds none,
# the trap waits two seconds and looks once more: a retry delay, not a
# deadline for Docker. A create the daemon finishes after that last look
# leaves its labelled container behind, and the next run refuses the name:
# inspect that container and its owner label and remove only its confirmed
# id before reusing the port, or pick another port.
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
#   --timeout <seconds>   Longest wait, per boot, for /api/health
#                         (default: 60)
#   --verbose             Print the API responses and, on success, the
#                         container logs
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

# The data directory (deploy/zenith-entrypoint.sh) and the account's ids,
# as the README documents them for --user.
readonly DATA_DIR="/var/lib/zenith"
readonly ACCOUNT_IDS="10001:10001"

# The compose files' posture (every capability dropped but the three the
# entrypoint uses) and the extra group the second boot adds: dialout's
# number on Debian, the documented device example.
readonly -a COMPOSE_FLAGS=(--cap-drop ALL
  --cap-add CHOWN --cap-add SETUID --cap-add SETGID)
readonly EXTRA_GID=20

# The label every container this script creates carries, valued with the
# run's own random token: cleanup removes the containers it finds carrying
# it, never a name.
readonly OWNER_LABEL="zenith-smoke-owner"

# The delay before cleanup's single second lookup, when a create had not
# returned. A retry delay, not a deadline for Docker: a create that lands
# after the second lookup is left behind (see above).
readonly LATE_CREATE_WAIT=2

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
MODE=""
# none: no create yet; pending: create invoked, no id back; done: created.
CREATE_STATE="none"
DECLARED_VERSION=""
BODY=""
BODY_PATH=""
STATUS=""

OWNER_TOKEN=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n') || OWNER_TOKEN=""
[[ "$OWNER_TOKEN" =~ ^[0-9a-f]{32}$ ]] || {
  err "could not draw a random owner token from /dev/urandom"
  exit 1
}
readonly OWNER_TOKEN

# ==============================================================================
# Container
# ==============================================================================

# Force-remove every container labelled with this run's token; succeed if
# there was one.
remove_owned() {
  local ids id found=1
  ids=$(docker ps --all --quiet --no-trunc \
    --filter "label=$OWNER_LABEL=$OWNER_TOKEN" 2>/dev/null) || ids=""
  for id in $ids; do
    if [[ "$id" =~ ^[0-9a-f]{64}$ ]]; then
      docker rm --force "$id" >/dev/null 2>&1 || true
      found=0
    fi
  done
  return "$found"
}

# The EXIT trap: remove the containers this run owns that it can find. A
# create that had not returned may still land, so when nothing owned shows
# yet, look once more after the wait; one that lands later is left behind
# (see the header). INT and TERM are ignored meanwhile, so a second Ctrl-C
# cannot cut the cleanup short.
cleanup() {
  trap '' INT TERM
  [[ "$CREATE_STATE" != "none" ]] || return 0
  if ! remove_owned && [[ "$CREATE_STATE" == "pending" ]]; then
    sleep "$LATE_CREATE_WAIT"
    remove_owned || true
  fi
}

print_log() {
  log "container log ($NAME, $MODE):"
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

# Create, then start, as the boot described by $1 with the docker flags
# that follow it. Only containers carrying this run's token are ever
# removed, so losing the name to a concurrent run (its create fails at the
# conflict) removes nothing.
start_container() {
  local out
  MODE="$1"
  shift
  if docker container inspect "$NAME" >/dev/null 2>&1; then
    err "a container named $NAME already exists; remove it or use another --port"
    exit 1
  fi
  CREATE_STATE="pending"
  out=$(docker create --pull never --label "$OWNER_LABEL=$OWNER_TOKEN" \
    --name "$NAME" --publish "127.0.0.1:$PORT:$CONTAINER_PORT" "$@" "$IMAGE" 2>&1) ||
    fail "docker create: $out"
  CREATE_STATE="done"
  CONTAINER_ID=$(tail -n 1 <<<"$out")
  if [[ ! "$CONTAINER_ID" =~ ^[0-9a-f]{64}$ ]]; then
    CONTAINER_ID=""
    fail "docker create printed no container id: $out"
  fi
  out=$(docker start "$CONTAINER_ID" 2>&1) || fail "docker start: $out"
  log "booted $IMAGE as $NAME on $BASE_URL ($MODE)"
}

# Remove this boot's container by its id, so the next boot can take the
# name and the port.
remove_container() {
  local out
  if [[ "$VERBOSE" == "1" ]]; then
    print_log
  fi
  out=$(docker rm --force "$CONTAINER_ID" 2>&1) || fail "docker rm $NAME: $out"
  CONTAINER_ID=""
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

# Read field $2 of PID 1's status (STATUS) into the variable named $1, its
# value only; a field the status lacks fails the smoke, naming it.
read_status() {
  local _var="$1" _key="$2" _line
  local -a _fields
  _line=$(grep -m 1 "^$_key:" <<<"$STATUS") ||
    fail "the container's /proc/1/status has no $_key field: ${STATUS:0:200}"
  read -r -a _fields <<<"${_line#"$_key":}"
  printf -v "$_var" '%s' "${_fields[*]}"
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

# The posture the entrypoint establishes (deploy/zenith-entrypoint.sh), read
# from /proc/1/status inside the container, so the ids are the container's
# own whatever user namespace the daemon runs it in.
check_posture() {
  local name uids gids groups id key cap nnp
  STATUS=$(docker exec "$CONTAINER_ID" cat /proc/1/status 2>&1) ||
    fail "docker exec $NAME cat /proc/1/status: $STATUS"
  read_status name Name
  [[ "$name" == "zenith" ]] ||
    fail "PID 1 in the container is '$name', not zenith: the entrypoint must exec the server so it receives the stop signal"
  read_status uids Uid
  read_status gids Gid
  read_status groups Groups
  [[ "$uids $gids" =~ ^[0-9]+( [0-9]+){7}$ ]] ||
    fail "/proc/1/status answered Uid '$uids' and Gid '$gids', not four ids each"
  [[ "$groups" =~ ^([0-9]+( [0-9]+)*)?$ ]] ||
    fail "/proc/1/status answered Groups '$groups', not a list of ids"
  for id in $uids; do
    [[ "$id" != 0 ]] ||
      fail "zenith runs as uid 0 (real, effective, saved, filesystem: $uids): the image must not run the server as root"
  done
  for id in $gids; do
    [[ "$id" != 0 ]] ||
      fail "zenith runs with gid 0 (real, effective, saved, filesystem: $gids): the image must not run the server in root's group"
  done
  for id in $groups; do
    [[ "$id" != 0 ]] ||
      fail "zenith keeps root's group 0 among its supplementary groups ($groups)"
  done
  for key in CapInh CapPrm CapEff CapAmb; do
    read_status cap "$key"
    [[ "$cap" =~ ^[0-9a-f]{16}$ ]] ||
      fail "/proc/1/status answered $key '$cap', not a capability mask"
    [[ "$cap" =~ ^0+$ ]] ||
      fail "zenith holds capabilities ($key $cap): the server must keep none"
  done
  read_status nnp NoNewPrivs
  [[ "$nnp" == "1" ]] ||
    fail "zenith runs without no-new-privileges (NoNewPrivs '$nnp'): a setuid program it started could regain root"
  log_ok "posture: zenith is PID 1 as uid ${uids%% *}, gid ${gids%% *}, no capabilities, no-new-privileges"
}

# The group given with --group-add ($1) reaches the server; reads the
# status check_posture read.
check_group() {
  local gid="$1" groups id
  read_status groups Groups
  for id in $groups; do
    if [[ "$id" == "$gid" ]]; then
      log_ok "groups: $gid (--group-add) among zenith's groups ($groups)"
      return 0
    fi
  done
  fail "zenith's supplementary groups (${groups:-none}) lack $gid, given with --group-add: a device's group would not reach the server"
}

# The entrypoint logs each hand-over of the data directory once; $1 is the
# number of hand-over lines this boot's log must hold.
check_handovers() {
  local expected="$1" count
  count=$(docker logs "$CONTAINER_ID" 2>&1 | grep -c "^zenith-entrypoint: handing $DATA_DIR ") || true
  [[ "$count" == "$expected" ]] ||
    fail "the entrypoint logged ${count:-no} hand-overs of $DATA_DIR, not $expected"
  log_ok "entrypoint: hand-overs of $DATA_DIR logged: $count, as expected"
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

docker image inspect "$IMAGE" >/dev/null 2>&1 || {
  err "image $IMAGE is not in the local image store; build or pull it first"
  exit 1
}
read_declared_version

start_container "plain docker run"
wait_for_health
check_health
check_posture
check_handovers 0
check_targets
check_definitions
check_commands
check_console
remove_container

start_container "compose posture, root-owned tmpfs, --group-add $EXTRA_GID" \
  "${COMPOSE_FLAGS[@]}" --tmpfs "$DATA_DIR:mode=0755" --group-add "$EXTRA_GID"
wait_for_health
check_posture
check_group "$EXTRA_GID"
check_handovers 1
remove_container

start_container "--user $ACCOUNT_IDS" --user "$ACCOUNT_IDS"
wait_for_health
check_posture
remove_container

log_ok "PASS: $IMAGE boots its bundled config and serves version $DECLARED_VERSION, as a plain run, under the compose posture and as the account"
