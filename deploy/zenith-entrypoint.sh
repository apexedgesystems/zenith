#!/bin/sh
# ==============================================================================
# zenith-entrypoint.sh - Start zenith as the zenith account, never as root.
#
# The image's entrypoint. Started as root, the default, it hands the data
# directory to the zenith account when the directory, or any entry directly
# in it, belongs to anyone else (a volume written by an image that ran as
# root, a fresh bind mount, a tmpfs, a hand-over cut short), then executes
# itself again as that account: its uid and gid, the supplementary groups
# the container was started with except root's (so a device's group given
# with --group-add reaches the server), and no capabilities; the bounding
# set stays as the runtime gave it. Started as another user (docker run
# --user), it changes no ownership and no identity.
#
# Either way it then checks that the data directory is writable, naming
# the cause and the remedy when it is not, and replaces itself with zenith
# under no-new-privileges: the server runs as PID 1, receives the
# container's stop signal, and cannot gain privileges, whatever the start
# mode. The binary's client mode --hash-password never opens the
# database: it gets the switch to the account and nothing else.
#
# The hand-over never reaches outside the data directory: chown -R does
# not follow symbolic links, so a link planted in the directory is handed
# over itself and its target keeps its owner. Deciding whether a hand-over
# is needed reads the directory and its own entries only, so a correctly
# owned database is never walked, and a hand-over that was cut short is
# found and finished at the next start. A read-only mount is reported
# before any hand-over is tried, and a directory that root cannot read
# with the container's capabilities stops the hand-over with its name.
# The root phase needs CAP_SETUID and CAP_SETGID, and CAP_CHOWN for a
# hand-over, and stops with a message naming what is missing; the server
# keeps no capability.
#
# Usage:
#   zenith-entrypoint [zenith arguments]
#
# The arguments reach zenith untouched; the image passes
# --config /etc/zenith/config.toml when none are given.
#
# ==============================================================================

set -eu

# ==============================================================================
# Constants
# ==============================================================================

readonly SCRIPT_NAME="zenith-entrypoint"
readonly ACCOUNT="zenith"
readonly DATA_DIR="/var/lib/zenith"

# Capability numbers (linux/capability.h).
readonly CAP_CHOWN=0
readonly CAP_DAC_OVERRIDE=1
readonly CAP_DAC_READ_SEARCH=2
readonly CAP_SETGID=6
readonly CAP_SETUID=7

# ==============================================================================
# Logging
# ==============================================================================

log() { printf '%s: %s\n' "$SCRIPT_NAME" "$1" >&2; }
err() { printf '%s: error: %s\n' "$SCRIPT_NAME" "$1" >&2; }

# ==============================================================================
# Checks
# ==============================================================================

# Print field $1 of this process's /proc/self/status.
status_field() {
  while read -r key value; do
    if [ "$key" = "$1:" ]; then
      printf '%s\n' "$value"
      return 0
    fi
  done </proc/self/status
  return 1
}

# Succeed if this process holds capability $1 in its effective set.
has_cap() {
  [ $(((0x$(status_field CapEff) >> $1) & 1)) -eq 1 ]
}

# Set WRONG to what in the data directory belongs to someone other than
# OWNER, reading the directory and its own entries only (never deeper), or
# to nothing when all of it is the account's. An entry removed after the
# listing is skipped.
find_wrong_owner() {
  WRONG=""
  current=$(stat -c %u:%g "$DATA_DIR")
  if [ "$current" != "$OWNER" ]; then
    WRONG="owner $current"
    return 0
  fi
  set -- "$DATA_DIR"/* "$DATA_DIR"/.[!.]* "$DATA_DIR"/..?*
  for entry in "$@"; do
    [ -e "$entry" ] || [ -L "$entry" ] || continue
    current=$(stat -c %u:%g "$entry" 2>/dev/null) || continue
    if [ "$current" != "$OWNER" ]; then
      WRONG="${entry#"$DATA_DIR"/} owned by $current"
      return 0
    fi
  done
}

# Set DATA_MOUNT to "mount" when the data directory is a mount point of its
# own (a volume, a bind mount, a tmpfs) and to "image" otherwise, and
# DATA_RO to 1 when the filesystem holding it is read-only, by its mount's
# options or its superblock's.
find_data_fs() {
  DATA_MOUNT="image"
  DATA_RO=""
  best=""
  while read -r _id _parent _dev _root point options rest; do
    case "$DATA_DIR" in
    "$point" | "$point"/*) ;;
    *) [ "$point" = / ] || continue ;;
    esac
    [ ${#point} -ge ${#best} ] || continue
    best="$point"
    case ",$options,${rest##* }," in
    *,ro,*) DATA_RO=1 ;;
    *) DATA_RO="" ;;
    esac
  done </proc/self/mountinfo
  [ "$best" != "$DATA_DIR" ] || DATA_MOUNT="mount"
}

# Explain why the data directory is not writable by this process.
explain_unwritable() {
  find_data_fs
  owned=$(stat -c '%u:%g, mode %a' "$DATA_DIR")
  account="$(id -u "$ACCOUNT"):$(id -g "$ACCOUNT")"
  if [ -n "$DATA_RO" ] && [ "$DATA_MOUNT" = mount ]; then
    err "$DATA_DIR is mounted read-only: mount it read-write"
  elif [ -n "$DATA_RO" ]; then
    err "$DATA_DIR is the image's own directory, read-only under --read-only: mount a volume there"
  elif [ "$DATA_MOUNT" = image ]; then
    err "$DATA_DIR is the image's own directory (owner $owned), not writable by uid $(id -u): mount a volume there that this uid can write, or start the container as root or as $ACCOUNT (--user $account)"
  elif [ "$(stat -c %u "$DATA_DIR")" = "$(id -u)" ]; then
    who="uid $(id -u)"
    [ "$(id -u)" != "$(id -u "$ACCOUNT")" ] || who="$ACCOUNT"
    case $(stat -c %A "$DATA_DIR") in
    ??w*) err "$DATA_DIR (owner $owned) belongs to $who and its mode lets the owner write, yet $who cannot: look for an immutable attribute or a security module on the host" ;;
    *) err "$DATA_DIR (owner $owned) belongs to $who, but its mode does not let the owner write: give the owner write permission on the host (chmod u+w; for a tmpfs, mode=0755)" ;;
    esac
  elif [ "$(id -u)" = "$(id -u "$ACCOUNT")" ]; then
    err "$DATA_DIR (owner $owned) is not writable by $ACCOUNT ($account): start the container once as root, without --user, so this entrypoint hands it over, or chown it to $account on the host"
  else
    err "$DATA_DIR (owner $owned) is not writable by uid $(id -u) (gid $(id -g)): make it writable by that uid on the host, or start the container as root so this entrypoint hands it to $ACCOUNT"
  fi
}

# Explain a hand-over that chown -R could not finish; $1 is what it printed.
explain_handover() {
  unreadable=""
  while IFS= read -r line; do
    case $line in
    *"cannot read directory '"*"': Permission denied")
      unreadable=${line#*"cannot read directory '"}
      unreadable=${unreadable%"': Permission denied"}
      break
      ;;
    esac
  done <<EOF
$1
EOF
  if [ -z "$unreadable" ]; then
    err "cannot hand $DATA_DIR to $ACCOUNT ($OWNER), as chown reports above: chown it to $OWNER on the host, or start the container as a user that can already write it (--user)"
  elif has_cap "$CAP_DAC_READ_SEARCH" || has_cap "$CAP_DAC_OVERRIDE"; then
    err "root cannot read $unreadable, so $DATA_DIR cannot be handed to $ACCOUNT ($OWNER): chown the tree to $OWNER on the host"
  else
    err "root cannot read $unreadable with the container's capabilities, so $DATA_DIR cannot be handed to $ACCOUNT ($OWNER): chown the tree to $OWNER on the host, or start the container once with Docker's default capabilities"
  fi
}

# ==============================================================================
# Root phase: hand over the data directory, then drop to the account
# ==============================================================================

# The binary's client mode never opens the database: it gets the switch
# to the account and nothing else.
USES_DATA=1
for arg in "$@"; do
  case $arg in
  --hash-password) USES_DATA="" ;;
  esac
done

if [ -n "$USES_DATA" ] && [ ! -d "$DATA_DIR" ]; then
  err "$DATA_DIR is not a directory"
  exit 1
fi

if [ "$(id -u)" = 0 ]; then
  OWNER="$(id -u "$ACCOUNT"):$(id -g "$ACCOUNT")"
  WRONG=""
  [ -z "$USES_DATA" ] || find_wrong_owner
  if [ -n "$WRONG" ]; then
    find_data_fs
    if [ -n "$DATA_RO" ]; then
      explain_unwritable
      exit 1
    fi
  fi
  if ! has_cap "$CAP_SETUID" || ! has_cap "$CAP_SETGID"; then
    if [ -n "$WRONG" ]; then
      err "the root phase needs CAP_SETUID and CAP_SETGID to switch to $ACCOUNT ($OWNER), and CAP_CHOWN to hand over $DATA_DIR ($WRONG): add them (--cap-add SETUID --cap-add SETGID --cap-add CHOWN), or chown the directory to $OWNER on the host and start the container as the account (--user $OWNER), which needs none"
    else
      err "the root phase needs CAP_SETUID and CAP_SETGID to switch to $ACCOUNT ($OWNER): add them (--cap-add SETUID --cap-add SETGID), or start the container as the account itself (--user $OWNER), which needs none"
    fi
    exit 1
  fi
  if [ -n "$WRONG" ]; then
    if ! has_cap "$CAP_CHOWN"; then
      err "handing $DATA_DIR ($WRONG) to $ACCOUNT ($OWNER) needs CAP_CHOWN: add it (--cap-add CHOWN), or chown the directory to $OWNER on the host"
      exit 1
    fi
    log "handing $DATA_DIR ($WRONG) to $ACCOUNT ($OWNER)"
    if ! failed=$(chown -R "$OWNER" "$DATA_DIR" 2>&1); then
      printf '%s\n' "$failed" >&2
      explain_handover "$failed"
      exit 1
    fi
  fi
  groups=""
  for gid in $(id -G); do
    [ "$gid" = 0 ] || groups="${groups:+$groups,}$gid"
  done
  if [ -n "$groups" ]; then
    exec setpriv --reuid="$ACCOUNT" --regid="$ACCOUNT" --groups="$groups" \
      --no-new-privs --inh-caps=-all -- "$0" "$@"
  fi
  exec setpriv --reuid="$ACCOUNT" --regid="$ACCOUNT" --clear-groups \
    --no-new-privs --inh-caps=-all -- "$0" "$@"
fi

# ==============================================================================
# Start the server
# ==============================================================================

if [ -n "$USES_DATA" ] && [ ! -w "$DATA_DIR" ]; then
  explain_unwritable
  exit 1
fi

exec setpriv --no-new-privs -- zenith "$@"
