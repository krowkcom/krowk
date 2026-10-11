#!/usr/bin/env bash
# The nextest filter `make quick-check` runs: the tests of every crate changed
# since this branch left main, committed or not, and of every crate that
# depends on one (`rdeps`). A change outside crates/ (Cargo.lock, the nextest
# config, a script, tests/golden) could reach any test, so it prints `all()`.
# So does a branch nothing has changed on yet.
set -euo pipefail
cd "$(dirname "$0")/.."
base=$(git merge-base HEAD "${QUICK_BASE:-origin/main}" 2>/dev/null || true)
[ -n "$base" ] || { echo "all()"; exit 0; }
files=$( { git diff --name-only "$base"; git ls-files --others --exclude-standard; } | sort -u)
[ -n "$files" ] || { echo "all()"; exit 0; }
crates=()
while IFS= read -r f; do
  case "$f" in
    crates/*/*) c=${f#crates/}; crates+=("${c%%/*}") ;;
    *.md) ;;
    *) echo "all()"; exit 0 ;;
  esac
done <<< "$files"
[ ${#crates[@]} -gt 0 ] || { echo "none()"; exit 0; }
printf '%s\n' "${crates[@]}" | sort -u | sed 's/.*/rdeps(&)/' | paste -sd'|' | sed 's/|/ | /g'
