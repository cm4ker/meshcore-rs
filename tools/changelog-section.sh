#!/usr/bin/env bash
# The section of CHANGELOG.md for a version (its heading "## <version>"), without the heading:
# the release's notes. Fails if there is none, or it is empty.
set -euo pipefail
version=${1:?usage: changelog-section.sh <version> [CHANGELOG.md]}
file=${2:-CHANGELOG.md}
section=$(awk -v v="$version" '
  /^## / { if (found) exit; split($0, h, " "); if (h[2] == v) { found = 1; next } }
  found { print }
' "$file")
if [ -z "$(printf '%s' "$section" | tr -d '[:space:]')" ]; then
  echo "$file has no section for $version (a heading \"## $version\")" >&2
  exit 1
fi
printf '%s\n' "$section" | sed -e '/./,$!d'
