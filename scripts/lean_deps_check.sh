#!/usr/bin/env bash
# R-PKG-2, R-OSS-7: the agent build — `krowk` with no features, the one a container
# compiles from source — keeps its dependency promise. The crates it links are
# listed in crates/krowk/lean-deps.txt, by name; any change to that set, from
# a new feature leaking into the default build or a dependency growing a new
# one, fails `make check` until the list is updated on purpose. The list is
# resolved for every target (`--target all`), not the host's, so the same
# list holds on the Linux and macOS runners and covers every platform the
# release ships:
#
#   LEAN_DEPS_UPDATE=1 scripts/lean_deps_check.sh
set -euo pipefail
# One collation everywhere: sort order is the locale's, and a list written
# under en_US would not match one sorted under CI's C locale.
export LC_ALL=C
cd "$(dirname "$0")/.."
want=crates/krowk/lean-deps.txt
got=$(cargo tree -p krowk -e normal --prefix none --locked --target all | awk '{print $1}' | sort -u)
if [ "${LEAN_DEPS_UPDATE:-}" = 1 ]; then
  printf '%s\n' "$got" > "$want"
  echo "wrote $want ($(wc -l < "$want") crates)"
  exit 0
fi
if ! diff -u "$want" <(printf '%s\n' "$got"); then
  echo "the agent build's dependencies changed (above: - listed, + now linked)." >&2
  echo "If that is intended, run LEAN_DEPS_UPDATE=1 scripts/lean_deps_check.sh and say why in the PR." >&2
  exit 1
fi
echo "agent build: $(printf '%s\n' "$got" | wc -l) crates, as listed"
