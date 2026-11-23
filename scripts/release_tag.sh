#!/usr/bin/env bash
# release.yml's first step: is tag $1 a release tag, and which version does it
# cut? Prints the version (the tag without its `v`) or fails with an error.
#
# Versions are calendar versions, YYYY.M.N: the release's year, its month
# without a leading zero, and N counting the month's releases from 0
# (2026.11.0, 2026.11.1, 2026.12.0). A prerelease adds -rcK (2026.11.0-rc1).
# A leading zero is refused here, before anything is built or published:
# npm would refuse it later (npm/build.mjs), and gruntas upgrade never offers
# it (cli/upgrade.rs, is_release).
set -euo pipefail

tag=${1:?usage: release_tag.sh <tag>}

if [[ ! "$tag" =~ ^v[0-9]{4}\.([1-9]|1[0-2])\.(0|[1-9][0-9]*)(-rc[1-9][0-9]*)?$ ]]; then
  echo "::error::$tag is not a release tag: want vYYYY.M.N or vYYYY.M.N-rcK, the month and N without a leading zero (v2026.11.0, v2026.1.3, v2026.11.0-rc1)" >&2
  exit 1
fi
echo "${tag#v}"
