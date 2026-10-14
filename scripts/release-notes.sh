#!/usr/bin/env bash
# Print the CHANGELOG.md section for one version, for use as the body of a GitHub release.
#
#   scripts/release-notes.sh 0.1.0 [CHANGELOG.md]
#
# Exits 1, naming the version, when the changelog has no "## [VERSION]" heading, so a release
# cannot go out without notes. The release workflow calls this.
set -euo pipefail

version="${1:?usage: release-notes.sh <version> [changelog]}"
changelog="${2:-$(dirname "$0")/../CHANGELOG.md}"

notes="$(awk -v v="$version" '
  /^## \[/ {
    if (found) exit
    if (index($0, "## [" v "]") == 1) { found = 1; next }
  }
  # The link definitions at the foot of the changelog are not part of any release.
  /^\[[^]]+\]: / { if (found) exit }
  found { print }
  END { if (!found) exit 3 }
' "$changelog")" || {
  echo "release-notes: no \"## [$version]\" section in $changelog" >&2
  exit 1
}

# Trim blank lines at both ends.
printf '%s\n' "$notes" | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}' | sed -e '/./,$!d'
