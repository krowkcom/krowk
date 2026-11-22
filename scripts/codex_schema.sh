#!/usr/bin/env bash
# The JSON Schema of `codex app-server`, pinned (R-BACK-3). gruntas drives
# Codex over this protocol, so the schema the pinned Codex generates is
# checked in beside the code that speaks it — crates/gruntas-harness/schema/codex/,
# with the Codex version it came from in VERSION — and the backend's tests
# validate every message against it. Experimental methods and fields are
# included: gruntas opts into them for dynamic tools.
#
#   scripts/codex_schema.sh --check   fail when the pinned copy is stale
#                                     against the pinned Codex (CI runs this)
#   scripts/codex_schema.sh --update  regenerate it from the Codex installed,
#                                     and pin that Codex's version
#
# CODEX names the binary (codex on PATH by default); CODEX_SCHEMA_DIR the
# pinned directory, for this script's own test. Codex runs with a fresh,
# empty CODEX_HOME: generating the schema needs no login, and reads none.
set -euo pipefail

mode="${1:---check}"
case "$mode" in
--check | --update) ;;
*)
  echo "usage: $0 [--check | --update]" >&2
  exit 64
  ;;
esac
root="$(cd "$(dirname "$0")/.." && pwd)"
dir="${CODEX_SCHEMA_DIR:-$root/crates/gruntas-harness/schema/codex}"
codex="${CODEX:-codex}"
bundle=codex_app_server_protocol.schemas.json

if ! have="$("$codex" --version 2>/dev/null)"; then
  echo "codex_schema: $codex is not installed — npm install -g @openai/codex@$(cat "$dir/VERSION" 2>/dev/null || echo latest)" >&2
  exit 2
fi
have="${have##* }"
pinned="$(cat "$dir/VERSION" 2>/dev/null || true)"
if [ "$mode" = --check ] && [ "$have" != "$pinned" ]; then
  echo "codex_schema: the schema is pinned from Codex $pinned, and $codex is $have — install the pinned one (npm install -g @openai/codex@$pinned), or move the pin with --update" >&2
  exit 2
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/home"
if ! CODEX_HOME="$tmp/home" "$codex" app-server generate-json-schema --experimental --out "$tmp/out" >"$tmp/said" 2>&1; then
  cat "$tmp/said" >&2
  echo "codex_schema: $codex could not generate the schema" >&2
  exit 1
fi

if [ "$mode" = --update ]; then
  mkdir -p "$dir"
  cp "$tmp/out/$bundle" "$dir/$bundle"
  printf '%s\n' "$have" >"$dir/VERSION"
  echo "pinned the app-server schema of Codex $have in $dir"
  exit 0
fi
if ! cmp -s "$tmp/out/$bundle" "$dir/$bundle"; then
  echo "codex_schema: $dir/$bundle is stale against Codex $have — regenerate it with scripts/codex_schema.sh --update, and check what changed in the protocol gruntas speaks:" >&2
  diff -u "$dir/$bundle" "$tmp/out/$bundle" | head -40 >&2 || true
  exit 1
fi
echo "the pinned app-server schema matches Codex $have"
