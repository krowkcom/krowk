#!/usr/bin/env bash
# Republishes docs/security/ from Canon (R-OSS-1): `engineering/crypto.md`
# and `engineering/threat-model.md`, copied unchanged but for one note under
# each title saying where it came from and that the other design files it
# cites aren't published. Canon is only read (`git show`), never fetched or
# checked out, so fetch it first for its latest.
#
#   scripts/publish_security_docs.sh [canon checkout] [ref]
#
# The checkout defaults to $KROWK_CANON, else ../krowk-canon beside this
# repository's main checkout; the ref to origin/main.
set -euo pipefail
canon=${1:-${KROWK_CANON:-}}
[ -z "$canon" ] || canon=$(cd "$canon" && pwd)
cd "$(dirname "$0")/.."
main=$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")
canon=${canon:-$main/../krowk-canon}
ref=${2:-origin/main}
sha=$(git -C "$canon" rev-parse --short=7 --verify "$ref^{commit}")

# Both are written to a scratch directory first, so a copy that fails
# leaves docs/security/ as it was.
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
publish() {
  local name=$1 other=$2 body
  body=$(git -C "$canon" show "$sha:engineering/$name.md")
  {
    printf '%s\n\n' "$(head -n 1 <<< "$body")"
    printf '> Published from Canon, krowk'\''s private design repository (`engineering/%s.md` at %s). `%s.md` is in this directory. The other design files it cites, such as `relay.md`, `harness.md` and `devices.md`, are not published.\n' "$name" "$sha" "$other"
    tail -n +2 <<< "$body"
  } > "$out/$name.md"
}

publish threat-model crypto
publish crypto threat-model
mv "$out/threat-model.md" "$out/crypto.md" docs/security/
echo "docs/security/ published from Canon at $sha"
