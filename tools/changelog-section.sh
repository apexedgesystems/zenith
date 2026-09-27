#!/usr/bin/env sh
# Print the CHANGELOG.md section for one version (heading excluded),
# for use as a release body. Exit 1 if the version has no section:
# a release without notes is a release cut too early.
#
#   tools/changelog-section.sh v0.0.2
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
tag=${1:?usage: changelog-section.sh vX.Y.Z}
tag=${tag%%-*}   # a pre-release (vX.Y.Z-rc1) ships the notes of its base version
out=$(awk -v tag="$tag" '
  /^## / { if (found) exit; if ($2 == tag) { found = 1; next } }
  found { print }
' "$here/CHANGELOG.md")
[ -n "$(printf '%s' "$out" | tr -d '[:space:]')" ] || {
  echo "CHANGELOG.md has no section for $tag" >&2; exit 1; }
printf '%s\n' "$out"
