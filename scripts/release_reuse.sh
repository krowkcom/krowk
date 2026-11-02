#!/usr/bin/env bash
# release.yml's `passed` job: has workflow $1 (full or bench) already passed on
# the tagged commit, or on an earlier one that differs from it only by what a
# release commit changes? Walks first-parent history back from $SHA (HEAD) and
# stops at the first commit that fails the "only these files" test.
# Prints `true` or `false` on the last line of stdout; the log goes to stderr.
#
# Release commits touch CHANGELOG.md (and other *.md) and the three manifests.
# crates/krowk-bench/budgets.toml is not ignorable for either workflow: bench
# holds the build to it and the unit tests include_str! it.
set -euo pipefail

wf=${1:?workflow name}
sha=${SHA:-$(git rev-parse HEAD)}
repo=${GITHUB_REPOSITORY:?}
max=${MAX_WALK:-30}

case "$wf" in
  full | bench) ignorable='^(.*\.md|\.claude-plugin/plugin\.json|gemini-extension\.json|server\.json)$' ;;
  *) echo "unknown workflow $wf" >&2; exit 2 ;;
esac

cand=$sha
for ((i = 0; i < max; i++)); do
  n=$(gh api "repos/$repo/actions/workflows/$wf.yml/runs?head_sha=$cand&status=success&per_page=1" --jq '.total_count' || echo 0)
  if [ "${n:-0}" -gt 0 ]; then
    echo "$wf.yml passed on $cand; reusing it for $sha" >&2
    echo true
    exit 0
  fi
  parent=$(git rev-parse --verify -q "$cand^1" || true)
  [ -n "$parent" ] || break
  # What $parent..$sha changed in total, not just this step, so the test
  # holds for the whole walk.
  other=$(git diff --name-only "$parent" "$sha" | grep -Ev "$ignorable" || true)
  if [ -n "$other" ]; then
    echo "$wf.yml: stopped before $parent, $sha differs from it in: $(echo "$other" | head -3 | tr '\n' ' ')" >&2
    break
  fi
  cand=$parent
done
echo "$wf.yml has not passed on $sha or on a commit that differs only by release files" >&2
echo false
