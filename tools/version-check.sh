#!/usr/bin/env sh
# The declared version lives in one place, the workspace Cargo.toml;
# the frontend package must match it, and a release tag must match
# both. Exit 1 with the mismatch named.
#
#   tools/version-check.sh            # Cargo.toml vs package.json
#   tools/version-check.sh v0.0.2     # ...and vs a tag
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cargo_v=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$here/Cargo.toml" | head -1)
npm_v=$(sed -n 's/^  "version": "\([^"]*\)",/\1/p' "$here/frontend/package.json" | head -1)
[ -n "$cargo_v" ] || { echo "no workspace version in Cargo.toml"; exit 1; }
[ "$cargo_v" = "$npm_v" ] || {
  echo "version mismatch: Cargo.toml $cargo_v, frontend/package.json $npm_v"; exit 1; }
if [ "${1:-}" != "" ]; then
  tag_v=${1#v}
  [ "$cargo_v" = "$tag_v" ] || {
    echo "version mismatch: tag $1 but the code declares $cargo_v"; exit 1; }
fi
echo "version $cargo_v"
