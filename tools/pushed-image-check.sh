#!/bin/bash
# ==============================================================================
# pushed-image-check.sh - Check that an image pushed by digest is the build
# the smoke booted.
#
# Reads the pushed image index, selects its one linux/<arch> image manifest
# (attestation manifests carry unknown/unknown and are passed over), reads
# that manifest's config digest and compares it with the config digest of
# the build the smoke booted, as that build's metadata records it
# (containerimage.config.digest). The config names the image's filesystem
# layers and its runtime settings, so equal digests mean the same image.
# Prints both digests either way; exits 1 when they differ, when the index
# holds no single linux/<arch> image manifest, or when a read fails.
#
# Usage:
#   pushed-image-check.sh --pushed <repository@sha256:...> --arch <arch>
#                         --expected-config <sha256:...> [--verbose]
#
# Options:
#   --pushed <ref>               The pushed image index, by digest
#   --arch <arch>                Architecture of the image manifest to select
#                                (amd64, arm64)
#   --expected-config <digest>   Config digest of the booted build
#   --verbose                    Print the index and the manifest read
#   --help                       Show this help
#
# ==============================================================================

set -euo pipefail

# ==============================================================================
# Constants
# ==============================================================================

readonly SCRIPT_NAME="pushed-image-check"

# Typed access for the jq filters: a field of the wrong type is an error.
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

PUSHED=""
ARCH=""
EXPECTED=""
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
  --pushed)
    need_value "$1" "$#"
    PUSHED="$2"
    shift 2
    ;;
  --arch)
    need_value "$1" "$#"
    ARCH="$2"
    shift 2
    ;;
  --expected-config)
    need_value "$1" "$#"
    EXPECTED="$2"
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

[[ "$PUSHED" =~ ^[^@[:space:]]+@sha256:[0-9a-f]{64}$ ]] || {
  err "--pushed must be <repository>@sha256:<64 hex digits>, not '$PUSHED'"
  exit 1
}
[[ "$ARCH" =~ ^[a-z0-9]+$ ]] || {
  err "--arch must be an architecture name such as amd64, not '$ARCH'"
  exit 1
}
[[ "$EXPECTED" =~ ^sha256:[0-9a-f]{64}$ ]] || {
  err "--expected-config must be sha256:<64 hex digits>, not '$EXPECTED'"
  exit 1
}
for tool in docker jq; do
  command -v "$tool" >/dev/null 2>&1 || {
    err "$tool is required"
    exit 1
  }
done

REPOSITORY="${PUSHED%@*}"

# ==============================================================================
# Check
# ==============================================================================

index=$(docker buildx imagetools inspect --raw "$PUSHED" 2>&1) || {
  err "cannot read the pushed index $PUSHED: $index"
  exit 1
}
verbose "index: $index"
manifest_digest=$(jq -er --arg arch "$ARCH" "$JQ_DEFS"'
  [.manifests | arr | .[] | obj
    | select(.platform.os == "linux" and .platform.architecture == $arch)]
  | if length == 1 then .[0].digest | str
    else error("\(length) linux/\($arch) image manifests in the index, not one") end' \
  <<<"$index" 2>&1) || {
  err "FAIL: $PUSHED: $manifest_digest"
  exit 1
}
manifest=$(docker buildx imagetools inspect --raw "$REPOSITORY@$manifest_digest" 2>&1) || {
  err "cannot read the linux/$ARCH manifest $manifest_digest: $manifest"
  exit 1
}
verbose "manifest: $manifest"
pushed_config=$(jq -er "$JQ_DEFS"'.config | obj | .digest | str' <<<"$manifest" 2>&1) || {
  err "FAIL: the linux/$ARCH manifest $manifest_digest names no config digest: $pushed_config"
  exit 1
}
log "pushed linux/$ARCH manifest $manifest_digest, config $pushed_config"
log "booted build's config $EXPECTED"
if [[ "$pushed_config" != "$EXPECTED" ]]; then
  err "FAIL: the pushed linux/$ARCH image (config $pushed_config) is not the build the smoke booted (config $EXPECTED)"
  exit 1
fi
log_ok "PASS: the pushed linux/$ARCH image is the build the smoke booted"
