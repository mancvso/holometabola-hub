#!/usr/bin/env bash
# Print a Conventional-Commits changelog for BASE..HEAD as a markdown bullet list,
# grouped by commit type. Used by the pr-creator agent to fill the PR description
# without generating prose - the diff between BASE and HEAD is already ground truth.
#
# Usage: scripts/changelog.sh [base] [head]
#   base defaults to origin/main, head defaults to HEAD
#
# POSIX-ish bash (no associative arrays) so it also runs under macOS's stock bash 3.2.

set -euo pipefail

base="${1:-origin/main}"
head="${2:-HEAD}"

heading_for() {
  case "$1" in
    feat) echo "Features" ;;
    fix) echo "Fixes" ;;
    perf) echo "Performance" ;;
    refactor) echo "Refactors" ;;
    test) echo "Tests" ;;
    docs) echo "Docs" ;;
    style) echo "Style" ;;
    chore) echo "Chores" ;;
    *) echo "" ;;
  esac
}

commits="$(git log --reverse --pretty=format:'%s' "${base}..${head}")"

if [[ -z "$commits" ]]; then
  echo "No commits between ${base} and ${head}."
  exit 0
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT
other_file="${tmpdir}/other"
: > "$other_file"

order="feat fix perf refactor test docs style chore"
for type in $order; do
  : > "${tmpdir}/${type}"
done

conventional_re='^([a-z]+)(\(([^)]*)\))?!?: (.*)$'

while IFS= read -r subject; do
  [[ -z "$subject" ]] && continue
  if [[ "$subject" =~ $conventional_re ]]; then
    type="${BASH_REMATCH[1]}"
    scope="${BASH_REMATCH[3]}"
    rest="${BASH_REMATCH[4]}"
    if [[ -n "$scope" ]]; then
      line="- **${scope}:** ${rest}"
    else
      line="- ${rest}"
    fi
    if [[ -n "$(heading_for "$type")" ]]; then
      echo "$line" >> "${tmpdir}/${type}"
    else
      echo "- ${subject}" >> "$other_file"
    fi
  else
    echo "- ${subject}" >> "$other_file"
  fi
done <<< "$commits"

for type in $order; do
  if [[ -s "${tmpdir}/${type}" ]]; then
    echo "## $(heading_for "$type")"
    cat "${tmpdir}/${type}"
    echo
  fi
done

if [[ -s "$other_file" ]]; then
  echo "## Other"
  cat "$other_file"
fi
