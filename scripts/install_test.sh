#!/usr/bin/env bash
# install_test.sh — run scripts/install.sh against a release that exists.
#
# No gruntas release has been published yet, so the installer cannot be tried the
# way a user will try it. What it *can* be tried against is a release that
# exists on this machine: a local HTTP server stands in for GitHub and
# GRUNTAS_INSTALL_BASE_URL points the installer at it. That exercises the archive
# naming, the checksum verification, the extraction of both binaries, the bin
# directory, the version check and the skill copy — everything except resolving
# the latest tag, which needs a tag to resolve.
#
# Where the release comes from depends on what is already here. In CI the
# packaging workflow has just run `scripts/dist.sh`, so dist/
# holds the build's own archives and its own checksums.txt: those are served
# as they are, which is the point — the archiving and the checksums are then
# what gets tested, rather than a tarball this script rolled by hand. Only when
# there is no such archive does it build one, and then without --clean, because
# wiping dist/ would throw away the release it was supposed to be installing.
#
#   scripts/install_test.sh
#
# Needs bash, python3 and cargo. Run from
# anywhere; it finds the repository from its own path.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
REPO_ROOT=$(pwd)
# The skill the release carries, as the repository holds it.
SKILL_SRC=skills/gruntas/SKILL.md

host_os=$(uname -s | tr '[:upper:]' '[:lower:]')
case "$(uname -m)" in
  x86_64|amd64) host_arch=amd64 ;;
  aarch64|arm64) host_arch=arm64 ;;
  *) echo "  ✗ this test only installs for the host, and $(uname -m) is not one of the release's architectures" >&2; exit 1 ;;
esac

pass() { echo "  ✓ $1"; }
fail() { echo "  ✗ $1" >&2; exit 1; }

WORK=$(mktemp -d)
SERVER_PID=""
cleanup() {
  [[ -n "$SERVER_PID" ]] && kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "Syntax"
bash -n scripts/install.sh || fail "scripts/install.sh does not parse"
pass "scripts/install.sh parses"

# Sourced, not re-implemented: the helpers below are the installer's own, so the
# checksum this test writes is made by whatever tool the installer would have
# reached for, and the platform table it checks is the one detect_platform
# actually routes on. Sourcing runs nothing — install.sh only calls main when it
# is the program being run.
# shellcheck source=scripts/install.sh
source scripts/install.sh
resolve_sha256

if command -v shellcheck >/dev/null 2>&1; then
  shellcheck --severity=warning scripts/install.sh || fail "shellcheck"
  pass "shellcheck is happy"
else
  echo "  – shellcheck not installed, skipped"
fi

# The archive name and the platform table are read straight out of
# scripts/dist.sh, which writes the release, rather than reproduced from
# memory: this is the one place the installer and the release have to agree,
# so a change to either that the other does not follow should break here. The
# name is the one install.sh builds (gruntas_<version>_<goos>_<goarch>.<ext>).
echo
echo "Holding the installer to scripts/dist.sh"
for flavour in full lean; do
  for p in linux_amd64 windows_amd64; do
    [[ "$(scripts/dist.sh archive-name "${p%_*}" "${p#*_}" 1.2.3 "$flavour")" == "$(archive_for 1.2.3 "$p" "$flavour")" ]] \
      || fail "scripts/dist.sh names its $flavour archive for $p in a way scripts/install.sh does not build"
  done
done
[[ "$(scripts/dist.sh archive-name linux amd64 1.2.3)" == "gruntas_1.2.3_linux_amd64.tar.gz" \
   && "$(archive_for 1.2.3 linux_amd64 lean)" == "gruntas-lean_1.2.3_linux_amd64.tar.gz" ]] \
  || fail "the full build no longer carries the gruntas name, or the lean one lost its own"
grep -q 'checksums.txt' scripts/dist.sh \
  || fail "scripts/dist.sh no longer writes checksums.txt, which scripts/install.sh downloads"
pass "the archive and checksum names still match what the installer builds"

# The other half of that agreement is the platform table. A platform added to
# the release that the installer will not name 404s, and a platform the
# installer names that the release never built 404s the same way, with the
# same wrong-looking error. So the table is read and diffed against the arms.
yaml_built=$(scripts/dist.sh targets | awk '{print $2 "_" $3}' | sort -u)
[[ -n "$yaml_built" ]] || fail "scripts/dist.sh lists no targets, so this check is reading nothing"
yaml_goos=$(cut -d_ -f1 <<<"$yaml_built" | sort -u)
yaml_goarch=$(cut -d_ -f2 <<<"$yaml_built" | sort -u)

# What the case arms in detect_platform can answer with, as opposed to what they
# accept: `mingw*|msys*|cygwin*) os="windows"` is three spellings of one goos.
sh_goos=$(sed -n 's/^ *[^ ]*) *os="\([a-z0-9]*\)".*/\1/p' scripts/install.sh | sort -u)
sh_goarch=$(sed -n 's/^ *[^ ]*) *arch="\([a-z0-9]*\)".*/\1/p' scripts/install.sh | sort -u)

[[ "$yaml_goos" == "$sh_goos" ]] \
  || fail "scripts/dist.sh builds for [$(echo "$yaml_goos" | tr '\n' ' ')] and scripts/install.sh names [$(echo "$sh_goos" | tr '\n' ' ')]"
[[ "$yaml_goarch" == "$sh_goarch" ]] \
  || fail "scripts/dist.sh builds for [$(echo "$yaml_goarch" | tr '\n' ' ')] and scripts/install.sh names [$(echo "$sh_goarch" | tr '\n' ' ')]"
pass "the platform lists still agree: $(echo "$yaml_goos" | tr '\n' ' ')× $(echo "$yaml_goarch" | tr '\n' ' ')"


# The combinations the release does not build are the third piece: one the
# config does not build must be one the installer refuses to offer, and every
# one it does build must be one detect_platform can name.
yaml_ignored=$(for o in $yaml_goos; do for a in $yaml_goarch; do grep -qx "${o}_${a}" <<<"$yaml_built" || echo "${o}_${a}"; done; done | sort -u)

# detect_platform reads uname, so uname is what this stands in for. The stub is
# only live while FAKE_UNAME_* are set, and the installer itself runs in its own
# process, where it is not.
FAKE_UNAME_S=""
FAKE_UNAME_M=""
uname() {
  if [[ "${1:-}" == "-s" && -n "$FAKE_UNAME_S" ]]; then echo "$FAKE_UNAME_S"; return 0; fi
  if [[ "${1:-}" == "-m" && -n "$FAKE_UNAME_M" ]]; then echo "$FAKE_UNAME_M"; return 0; fi
  command uname "$@"
}

# How each goos and goarch reaches a machine as `uname -s` and `uname -m`. A
# platform the config gains and this table has not heard of fails loudly rather
# than going unchecked.
uname_s_for() {
  case "$1" in
    linux) echo "Linux" ;;
    darwin) echo "Darwin" ;;
    windows) echo "MINGW64_NT-10.0-22631" ;;
    *) return 1 ;;
  esac
}
uname_m_for() {
  case "$1" in
    amd64) echo "x86_64" ;;
    arm64) echo "aarch64" ;;
    *) return 1 ;;
  esac
}

for goos in $yaml_goos; do
  for goarch in $yaml_goarch; do
    want="${goos}_${goarch}"
    fake_s=$(uname_s_for "$goos") || fail "this test has no uname -s spelling for $goos; add one beside the others"
    fake_m=$(uname_m_for "$goarch") || fail "this test has no uname -m spelling for $goarch; add one beside the others"

    got=""
    if got=$(FAKE_UNAME_S="$fake_s" FAKE_UNAME_M="$fake_m" detect_platform 2>/dev/null); then
      named=yes
    else
      named=no
    fi

    if grep -qx "$want" <<<"$yaml_ignored"; then
      [[ "$named" == "no" ]] \
        || fail "scripts/dist.sh does not build $want, but scripts/install.sh offers it as $got"
    else
      [[ "$named" == "yes" ]] \
        || fail "scripts/dist.sh builds $want, but scripts/install.sh refuses to name it"
      [[ "$got" == "$want" ]] \
        || fail "scripts/dist.sh builds $want, but scripts/install.sh calls that platform $got"
    fi
  done
done
pass "every platform the release builds is one the installer names, and no others"

# A cosign of this machine's would check the stand-in release's signature,
# which nothing here can make: every install below runs without one, and the
# signature's own tests put a stub of theirs first on PATH.
no_cosign=""
IFS=: read -ra path_dirs <<<"$PATH"
for dir in "${path_dirs[@]}"; do
  [[ -x "$dir/cosign" ]] || no_cosign+="${no_cosign:+:}$dir"
done
PATH="$no_cosign"

# The release to install from.
echo
echo "Laying out a release to install from"
RELEASE="$WORK/release"
mkdir -p "$RELEASE"

# dist/ first. In CI the packaging workflow has already run a full snapshot two
# steps up, so the build's own archive and its own checksums.txt are sitting
# there — serving those is what makes this a test of the release rather than of
# a tarball assembled here. They are copied rather than linked because the
# tampering test below writes a byte into the archive it serves.
dist_archive=""
if [[ -f dist/checksums.txt ]]; then
  for candidate in dist/gruntas_*_"${host_os}"_"${host_arch}".tar.gz; do
    if [[ -f "$candidate" ]]; then
      dist_archive="$candidate"
      break
    fi
  done
fi

# Both builds' gruntas, unpacked, to tell which one an install put down.
mkdir -p "$WORK/full" "$WORK/lean"
if [[ -n "$dist_archive" ]]; then
  ARCHIVE=$(basename "$dist_archive")
  VERSION=${ARCHIVE#gruntas_}
  VERSION=${VERSION%"_${host_os}_${host_arch}.tar.gz"}
  LEAN_ARCHIVE=$(archive_for "$VERSION" "${host_os}_${host_arch}" lean)
  [[ -f "dist/$LEAN_ARCHIVE" ]] || fail "dist/ holds $ARCHIVE but not $LEAN_ARCHIVE: the release builds both"
  cp "$dist_archive" "$RELEASE/$ARCHIVE"
  cp "dist/$LEAN_ARCHIVE" "$RELEASE/$LEAN_ARCHIVE"
  cp dist/checksums.txt "$RELEASE/checksums.txt"
  tar -xzf "$RELEASE/$ARCHIVE" -C "$WORK/full"
  tar -xzf "$RELEASE/$LEAN_ARCHIVE" -C "$WORK/lean"
  SOURCE="the release already in dist/"
else
  # No release here to install, so the host's binaries are built and archived
  # the way scripts/dist.sh archives them, into this run's own directory: the
  # repository's dist/ is neither read nor written. The lean build first, and
  # copied out before the full build overwrites target/release/gruntas.
  cargo build --release --locked -p gruntas --bin gruntas --bin gruntas-mcp >"$WORK/cargo.log" 2>&1 \
    || { cat "$WORK/cargo.log"; fail "cargo could not build the lean build"; }
  cp target/release/gruntas target/release/gruntas-mcp "$WORK/lean/"
  cargo build --release --locked -p gruntas --bin gruntas --features harness >"$WORK/cargo.log" 2>&1 \
    || { cat "$WORK/cargo.log"; fail "cargo could not build the full build"; }
  cp target/release/gruntas "$WORK/full/"
  cp "$WORK/lean/gruntas-mcp" "$WORK/full/"
  SOURCE="cargo build"
  VERSION="9.9.9"
  ARCHIVE=$(archive_for "$VERSION" "${host_os}_${host_arch}" full)
  LEAN_ARCHIVE=$(archive_for "$VERSION" "${host_os}_${host_arch}" lean)
  tar -czf "$RELEASE/$ARCHIVE" -C "$WORK/full" gruntas gruntas-mcp
  tar -czf "$RELEASE/$LEAN_ARCHIVE" -C "$WORK/lean" gruntas gruntas-mcp
  (cd "$RELEASE" && "${SHA256_CMD[@]}" "$ARCHIVE" "$LEAN_ARCHIVE" >checksums.txt)
fi
if cmp -s "$WORK/full/gruntas" "$WORK/lean/gruntas"; then fail "the full and lean archives hold the same gruntas"; fi

cp "$SKILL_SRC" "$RELEASE/SKILL.md"
# Only the stub cosign below reads it.
echo '{}' >"$RELEASE/checksums.txt.sigstore.json"
pass "serving $ARCHIVE, $LEAN_ARCHIVE + checksums.txt, from $SOURCE"

# The server. Port 0 so parallel runs do not collide.
python3 -u -m http.server 0 --bind 127.0.0.1 --directory "$RELEASE" >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
BASE=""
# Up to 30 s: a cold python3 on a busy macOS runner has taken longer than 5.
for _ in $(seq 1 300); do
  port=$(sed -n 's/.*port \([0-9]*\).*/\1/p' "$WORK/server.log" | head -1)
  if [[ -n "$port" ]]; then
    BASE="http://127.0.0.1:${port}"
    break
  fi
  sleep 0.1
done
[[ -n "$BASE" ]] || { cat "$WORK/server.log" >&2; fail "the local release server never came up"; }
pass "serving the release at $BASE"

echo
echo "Installing"
BIN="$WORK/bin"
CLAUDE="$WORK/claude"
mkdir -p "$CLAUDE/skills"
# The shared directory gruntas and Codex read: the skill goes there as well.
SHARED="$WORK/home/.agents/skills"
mkdir -p "$SHARED"

# HOME is redirected so a failing test cannot write to the real one, and SHELL is
# named so the PATH branch picks a file inside the redirected HOME.
env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
  CLAUDE_CONFIG_DIR="$CLAUDE" GRUNTAS_INSTALL_TTY=/dev/null \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
  bash "$REPO_ROOT/scripts/install.sh" >"$WORK/install.log" 2>&1 \
  || { cat "$WORK/install.log"; fail "the installer exited non-zero"; }

[[ -x "$BIN/gruntas" ]]     || fail "gruntas was not installed"
[[ -x "$BIN/gruntas-mcp" ]] || fail "gruntas-mcp was not installed"
pass "both binaries landed in $BIN"

cmp -s "$BIN/gruntas" "$WORK/full/gruntas" || { cat "$WORK/install.log"; fail "R-PKG-3: a workstation install did not get the full build"; }
grep -q "(full build)" "$WORK/install.log" || fail "the installer did not say which build it installed"
pass "R-PKG-3: a workstation install gets the full build"
grep -q "sign in and connect a model" "$WORK/install.log" || { cat "$WORK/install.log"; fail "a full install did not say bare gruntas sets the machine up"; }
pass "a full install says bare gruntas sets the machine up"

"$BIN/gruntas" --version >/dev/null || fail "the installed gruntas does not run"
pass "the installed gruntas runs"

grep -q "Checksum matches" "$WORK/install.log" || fail "the installer did not report verifying the checksum"
pass "the checksum was verified"
# The release under test is signed or not by its version: a local run's
# fake 9.9.9 is, CI's 0.0.0-snapshot (from dist/) predates signing.
if signed_since "$VERSION"; then
  grep -q "cosign is not installed" "$WORK/install.log" && grep -q "cosign verify-blob checksums.txt" "$WORK/install.log" \
    || { cat "$WORK/install.log"; fail "R-OSS-3: without cosign the installer did not say how to check the signature"; }
  pass "R-OSS-3: without cosign, the signature check is printed rather than required"
else
  grep -q "predates signed releases, so only its checksum is checked" "$WORK/install.log" \
    || { cat "$WORK/install.log"; fail "R-OSS-3: an unsigned release was not said to be checked by its checksum alone"; }
  pass "R-OSS-3: a release from before signing is said to be checked by its checksum alone"
fi

[[ -f "$CLAUDE/skills/gruntas/SKILL.md" ]] || fail "the agent skill was not written"
diff -q "$SKILL_SRC" "$CLAUDE/skills/gruntas/SKILL.md" >/dev/null \
  || fail "the installed skill differs from the one in the repository"
pass "the agent skill was written to CLAUDE_CONFIG_DIR"

diff -q "$SKILL_SRC" "$SHARED/gruntas/SKILL.md" >/dev/null \
  || fail "the agent skill was not written to ~/.agents/skills"
[[ -f "$SHARED/gruntas/.managed-by-gruntas-cli" ]] \
  || fail "the installer did not mark ~/.agents/skills/gruntas as its own"
grep -qF "gruntas skill installed: $CLAUDE/skills/gruntas, ~/.agents/skills/gruntas" "$WORK/install.log" \
  || { cat "$WORK/install.log"; fail "one line did not name every directory the skill went to"; }
pass "the agent skill was written to ~/.agents/skills too, and one line names both"

[[ -f "$CLAUDE/skills/gruntas/.managed-by-gruntas-cli" ]] \
  || fail "the installer did not mark the skill directory as its own"
# The installer's own constant, from sourcing it above: the sentence is
# asserted against the definition rather than against a copy of it here.
[[ "$(cat "$CLAUDE/skills/gruntas/.managed-by-gruntas-cli")" == "$MANAGED_MARKER_CONTENT" ]] \
  || fail "the ownership marker says something other than the sentence gruntas writes"
[[ "$(cat "$CLAUDE/skills/gruntas/.installed-version")" == "$VERSION" ]] \
  || fail "the version stamp says $(cat "$CLAUDE/skills/gruntas/.installed-version"), want $VERSION"
pass "the skill directory carries the ownership marker and the version stamp"

# The whole point of the marker: a skill directory somebody else wrote is not
# gruntas's to overwrite, and one gruntas wrote is.
echo
echo "Ownership of the skill directory"
MINE="$WORK/claude-mine"
mkdir -p "$MINE/skills/gruntas"
printf '# my own skill\n' >"$MINE/skills/gruntas/SKILL.md"
# Something the pre-marker installer never wrote, which is what makes this
# somebody's own directory rather than an old install to adopt.
printf 'mine\n' >"$MINE/skills/gruntas/notes.md"
env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
  CLAUDE_CONFIG_DIR="$MINE" \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
  bash "$REPO_ROOT/scripts/install.sh" >"$WORK/unowned.log" 2>&1 \
  || { cat "$WORK/unowned.log"; fail "the installer exited non-zero over a skill directory it does not own"; }

[[ "$(cat "$MINE/skills/gruntas/SKILL.md")" == "# my own skill" ]] \
  || fail "the installer overwrote a skill directory it did not write"
[[ ! -e "$MINE/skills/gruntas/.managed-by-gruntas-cli" ]] \
  || fail "the installer claimed a skill directory it did not write"
grep -q "was not written by gruntas" "$WORK/unowned.log" \
  || { cat "$WORK/unowned.log"; fail "the installer did not say why it left the skill directory alone"; }
pass "a populated skill directory gruntas did not write is left untouched, and said so"

# And the one it did write is refreshed in place, marker and all.
printf '# stale\n' >"$CLAUDE/skills/gruntas/SKILL.md"
env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
  CLAUDE_CONFIG_DIR="$CLAUDE" \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
  bash "$REPO_ROOT/scripts/install.sh" >"$WORK/refresh.log" 2>&1 \
  || { cat "$WORK/refresh.log"; fail "the installer exited non-zero refreshing its own skill directory"; }

diff -q "$SKILL_SRC" "$CLAUDE/skills/gruntas/SKILL.md" >/dev/null \
  || fail "the installer did not refresh a skill directory it wrote"
[[ -f "$CLAUDE/skills/gruntas/.managed-by-gruntas-cli" ]] \
  || fail "the refresh dropped the ownership marker"
pass "a marked skill directory is refreshed in place"

# A symlink where the skill directory should be points at a directory this
# installer never inspected, so it is not one to write through.
LINKED="$WORK/claude-linked"
mkdir -p "$LINKED/skills" "$WORK/link-target"
ln -s "$WORK/link-target" "$LINKED/skills/gruntas"
env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
  CLAUDE_CONFIG_DIR="$LINKED" \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
  bash "$REPO_ROOT/scripts/install.sh" >"$WORK/linked.log" 2>&1 \
  || { cat "$WORK/linked.log"; fail "the installer exited non-zero over a symlinked skill directory"; }
[[ -z "$(ls -A "$WORK/link-target")" ]] \
  || fail "the installer wrote through a symlinked skill directory"
grep -q "is a symlink" "$WORK/linked.log" \
  || { cat "$WORK/linked.log"; fail "the installer did not say it refused a symlinked skill directory"; }
pass "a symlinked skill directory is left alone"

# Inside a directory gruntas does own, a symlink in a managed file's name is
# refused rather than followed — the same answer internal/harness gives, and
# for the same reason: its target was never inspected, so whatever it points at
# is somebody else's file.
printf 'do not touch\n' >"$WORK/victim"
rm -f "$CLAUDE/skills/gruntas/.installed-version"
ln -s "$WORK/victim" "$CLAUDE/skills/gruntas/.installed-version"
env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
  CLAUDE_CONFIG_DIR="$CLAUDE" \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
  bash "$REPO_ROOT/scripts/install.sh" >"$WORK/victim.log" 2>&1 \
  || { cat "$WORK/victim.log"; fail "the installer exited non-zero over a symlinked version stamp"; }
[[ "$(cat "$WORK/victim")" == "do not touch" ]] \
  || fail "the installer wrote through a symlinked version stamp"
grep -q "is a symlink" "$WORK/victim.log" \
  || { cat "$WORK/victim.log"; fail "the installer did not say it refused the symlinked version stamp"; }
diff -q "$SKILL_SRC" "$CLAUDE/skills/gruntas/SKILL.md" >/dev/null \
  || fail "the refused stamp took the skill down with it"
rm -f "$CLAUDE/skills/gruntas/.installed-version"
pass "a symlink in a managed file's name is refused, and its target untouched"

# A directory that cannot be listed is refused rather than assumed empty.
if [[ "$(id -u)" == "0" ]]; then
  echo "  – running as root, where an unreadable directory is still readable; skipped"
else
  BLIND="$WORK/claude-blind"
  mkdir -p "$BLIND/skills/gruntas"
  printf '# mine\n' >"$BLIND/skills/gruntas/SKILL.md"
  chmod 300 "$BLIND/skills/gruntas"
  env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
    CLAUDE_CONFIG_DIR="$BLIND" \
    GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
    bash "$REPO_ROOT/scripts/install.sh" >"$WORK/blind.log" 2>&1 \
    || { cat "$WORK/blind.log"; fail "the installer exited non-zero over an unreadable skill directory"; }
  grep -q "cannot be read" "$WORK/blind.log" \
    || { cat "$WORK/blind.log"; fail "the installer did not refuse an unreadable skill directory"; }
  chmod 700 "$BLIND/skills/gruntas"
  [[ "$(cat "$BLIND/skills/gruntas/SKILL.md")" == "# mine" ]] \
    || fail "the installer wrote into a directory it could not read"
  pass "a skill directory that cannot be listed is left alone"
fi

# The one shape the installer adopts: what a pre-marker krowk wrote, which is
# a SKILL.md and nothing else. That file was overwritten by every run of the
# old installer anyway, so adopting it takes nothing from anyone.
OLD="$WORK/claude-old"
mkdir -p "$OLD/skills/gruntas"
printf '# an older krowk wrote this\n' >"$OLD/skills/gruntas/SKILL.md"
env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
  CLAUDE_CONFIG_DIR="$OLD" \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
  bash "$REPO_ROOT/scripts/install.sh" >"$WORK/old.log" 2>&1 \
  || { cat "$WORK/old.log"; fail "the installer exited non-zero upgrading a pre-marker skill"; }
[[ -f "$OLD/skills/gruntas/.managed-by-gruntas-cli" ]] \
  || { cat "$WORK/old.log"; fail "a pre-marker skill directory was not adopted"; }
diff -q "$SKILL_SRC" "$OLD/skills/gruntas/SKILL.md" >/dev/null \
  || fail "the adopted skill was not refreshed"
pass "a skill directory a pre-marker krowk wrote is adopted and refreshed"

# The rename: the skill the krowk installer wrote, in skills/krowk under its
# marker, gives way to skills/gruntas. One it did not write stays, untouched.
RENAMED="$WORK/claude-renamed"
mkdir -p "$RENAMED/skills/krowk"
printf '# krowk\n' >"$RENAMED/skills/krowk/SKILL.md"
printf '%s\n' "$OLD_MANAGED_MARKER_CONTENT" >"$RENAMED/skills/krowk/.managed-by-krowk-cli"
printf '0.18.0\n' >"$RENAMED/skills/krowk/.installed-version"
env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
  CLAUDE_CONFIG_DIR="$RENAMED" \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
  bash "$REPO_ROOT/scripts/install.sh" >"$WORK/renamed.log" 2>&1 \
  || { cat "$WORK/renamed.log"; fail "the installer exited non-zero replacing the krowk skill"; }
[[ ! -e "$RENAMED/skills/krowk" ]] || { cat "$WORK/renamed.log"; fail "the krowk skill the old installer wrote was not removed"; }
diff -q "$SKILL_SRC" "$RENAMED/skills/gruntas/SKILL.md" >/dev/null && [[ -f "$RENAMED/skills/gruntas/.managed-by-gruntas-cli" ]] \
  || { cat "$WORK/renamed.log"; fail "the gruntas skill did not take the krowk one's place"; }
grep -q "Removed the old krowk skill" "$WORK/renamed.log" || fail "the installer did not say it removed the krowk skill"
pass "a managed krowk skill is replaced by the gruntas one"

for shape in unmarked extra; do
  KEPT="$WORK/claude-kept-$shape"
  mkdir -p "$KEPT/skills/krowk"
  printf '# mine\n' >"$KEPT/skills/krowk/SKILL.md"
  if [[ "$shape" == extra ]]; then
    printf '%s\n' "$OLD_MANAGED_MARKER_CONTENT" >"$KEPT/skills/krowk/.managed-by-krowk-cli"
    printf 'mine\n' >"$KEPT/skills/krowk/notes.md"
  fi
  env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
    CLAUDE_CONFIG_DIR="$KEPT" \
    GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
    bash "$REPO_ROOT/scripts/install.sh" >"$WORK/kept.log" 2>&1 \
    || { cat "$WORK/kept.log"; fail "the installer exited non-zero beside a krowk skill it did not write ($shape)"; }
  [[ "$(cat "$KEPT/skills/krowk/SKILL.md")" == "# mine" ]] \
    || fail "a krowk skill the old installer did not write ($shape) was touched"
  [[ -f "$KEPT/skills/gruntas/SKILL.md" ]] || fail "the gruntas skill was not written beside a kept krowk one ($shape)"
done
pass "an unmanaged krowk skill, or one holding files of its own, is left alone"

# A permissive umask must not leave the skill directory world-writable: gruntas's
# marker would then be vouching for a directory any local user can rewrite.
LOOSE="$WORK/claude-loose"
mkdir -p "$LOOSE/skills"
( umask 000
  env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
    CLAUDE_CONFIG_DIR="$LOOSE" \
    GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
    bash "$REPO_ROOT/scripts/install.sh" >"$WORK/umask.log" 2>&1 ) \
  || { cat "$WORK/umask.log"; fail "the installer exited non-zero under umask 000"; }
[[ -z "$(find "$LOOSE/skills/gruntas" -maxdepth 0 -perm /022)" ]] \
  || fail "the skill directory gruntas created under umask 000 is writable by others"
pass "a skill directory gruntas creates is not writable by others, whatever the umask says"

# A marker much larger than a marker is not a marker, however it begins. The
# padding is newlines on purpose: those are what a naive read strips before
# measuring, which would let this pass.
BIG="$WORK/claude-big"
mkdir -p "$BIG/skills/gruntas"
{ printf '%s\n' "$MANAGED_MARKER_CONTENT"; for _ in $(seq 1 1000); do printf '\n'; done; } \
  >"$BIG/skills/gruntas/.managed-by-gruntas-cli"
printf '# mine\n' >"$BIG/skills/gruntas/SKILL.md"
env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
  CLAUDE_CONFIG_DIR="$BIG" \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
  bash "$REPO_ROOT/scripts/install.sh" >"$WORK/big.log" 2>&1 \
  || { cat "$WORK/big.log"; fail "the installer exited non-zero over an oversized marker"; }
grep -q "carries a marker gruntas did not write" "$WORK/big.log" \
  || { cat "$WORK/big.log"; fail "the installer accepted a marker far larger than a marker"; }
[[ "$(cat "$BIG/skills/gruntas/SKILL.md")" == "# mine" ]] \
  || fail "the installer wrote into a directory whose marker it refused"
pass "a marker padded past the read bound is refused"

# A FIFO in a managed file's name: not a file this installer wrote, and not one
# a rename should quietly replace.
if ! command -v mkfifo >/dev/null 2>&1; then
  echo "  – mkfifo not available, so a non-regular file cannot be staged; skipped"
else
  FIFO="$WORK/claude-fifo"
  mkdir -p "$FIFO/skills/gruntas"
  printf '%s\n' "$MANAGED_MARKER_CONTENT" >"$FIFO/skills/gruntas/.managed-by-gruntas-cli"
  mkfifo "$FIFO/skills/gruntas/SKILL.md"
  env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
    CLAUDE_CONFIG_DIR="$FIFO" \
    GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
    bash "$REPO_ROOT/scripts/install.sh" >"$WORK/fifo.log" 2>&1 \
    || { cat "$WORK/fifo.log"; fail "the installer exited non-zero over a FIFO in the skill's name"; }
  grep -q "not a regular file gruntas wrote" "$WORK/fifo.log" \
    || { cat "$WORK/fifo.log"; fail "the installer did not refuse a FIFO in the skill's name"; }
  [[ -p "$FIFO/skills/gruntas/SKILL.md" ]] \
    || fail "the installer replaced a FIFO it should have left alone"
  pass "a FIFO in a managed file's name is refused"
fi

# A directory in a managed file's name: `mv` would move the new file inside it
# and report success, leaving the skill at a path nobody named.
DIRNAME="$WORK/claude-dirname"
mkdir -p "$DIRNAME/skills/gruntas/SKILL.md"
printf '%s\n' "$MANAGED_MARKER_CONTENT" >"$DIRNAME/skills/gruntas/.managed-by-gruntas-cli"
env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
  CLAUDE_CONFIG_DIR="$DIRNAME" \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
  bash "$REPO_ROOT/scripts/install.sh" >"$WORK/dirname.log" 2>&1 \
  || { cat "$WORK/dirname.log"; fail "the installer exited non-zero over a directory in SKILL.md's name"; }
[[ -z "$(ls -A "$DIRNAME/skills/gruntas/SKILL.md")" ]] \
  || fail "the installer moved the skill inside a directory named SKILL.md"
grep -q "is a directory" "$WORK/dirname.log" \
  || { cat "$WORK/dirname.log"; fail "the installer did not say it refused a directory in the skill file's name"; }
pass "a directory in a managed file's name is refused, with nothing moved into it"

# Nothing this installer writes may be left behind on a refusal: a stranded
# temporary file would make the next run's adoption check refuse the directory.
stranded=("$DIRNAME/skills/gruntas"/.gruntas-*)
[[ ! -e "${stranded[0]}" ]] \
  || fail "a temporary file was stranded in the skill directory: ${stranded[0]}"
pass "no temporary file was left behind"

# A skill directory belonging to somebody else is not gruntas's to manage,
# whatever its mode allows. Only root can stage that.
if [[ "$(id -u)" != "0" ]]; then
  echo "  – not root, so a directory owned by another user cannot be staged; skipped"
else
  OTHER="$WORK/claude-other"
  mkdir -p "$OTHER/skills/gruntas"
  chown 65534:65534 "$OTHER/skills/gruntas"
  env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
    CLAUDE_CONFIG_DIR="$OTHER" \
    GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
    bash "$REPO_ROOT/scripts/install.sh" >"$WORK/other.log" 2>&1 \
    || { cat "$WORK/other.log"; fail "the installer exited non-zero over a directory owned by another user"; }
  grep -q "belongs to another user" "$WORK/other.log" \
    || { cat "$WORK/other.log"; fail "the installer did not refuse a directory owned by another user"; }
  [[ -z "$(ls -A "$OTHER/skills/gruntas")" ]] \
    || fail "the installer wrote into a directory owned by another user"
  pass "a skill directory owned by another user is left alone"
fi

grep -q "gruntas push screenshot.png" "$WORK/install.log" || fail "the next steps were not printed"
pass "the next steps were printed"

# NO_COLOR was set above, so nothing may have emitted an escape sequence.
if grep -q $'\033' "$WORK/install.log"; then fail "the installer emitted colour with NO_COLOR set"; fi
pass "NO_COLOR was honoured"

# R-PKG-3: which build lands, end to end, for each way of asking. Each line
# is what the environment says, the installer's arguments, and the build
# that must land.
echo
echo "The full build for people, the lean one for CI and containers (R-PKG-3)"
BUILD_BIN="$WORK/bin-build"
mkdir -p "$WORK/docker-root" "$WORK/podman-root/run"
: >"$WORK/docker-root/.dockerenv"
: >"$WORK/podman-root/run/.containerenv"
while IFS='|' read -r envs args want; do
  rm -rf "$BUILD_BIN"
  # shellcheck disable=SC2086  # envs and args are word lists on purpose.
  # A terminal stands in (/dev/null opens where /dev/tty would), so each
  # case is decided by what it sets, not by how this test was started.
  env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 GRUNTAS_SKIP_SKILL=1 GRUNTAS_INSTALL_TTY=/dev/null \
    GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BUILD_BIN" $envs \
    bash "$REPO_ROOT/scripts/install.sh" $args >"$WORK/build.log" 2>&1 \
    || { cat "$WORK/build.log"; fail "the installer exited non-zero with [$envs] [$args]"; }
  cmp -s "$BUILD_BIN/gruntas" "$WORK/$want/gruntas" \
    || { cat "$WORK/build.log"; fail "[$envs] [$args] installed the wrong build; want $want"; }
  pass "${envs:-no CI, no container}${args:+ $args}: the $want build"
done <<CASES
CI=true||lean
CI=1||lean
CI=false||full
GITHUB_ACTIONS=true||lean
GITLAB_CI=true||lean
GRUNTAS_INSTALL_FS_ROOT=$WORK/docker-root||lean
GRUNTAS_INSTALL_FS_ROOT=$WORK/podman-root||lean
container=podman||lean
KUBERNETES_SERVICE_HOST=10.0.0.1||lean
|--lean|lean
CI=true|--full|full
CI=true GRUNTAS_LEAN=0||full
GRUNTAS_LEAN=1||lean
GRUNTAS_INSTALL_TTY=$WORK/no-such-tty|--full|full
GRUNTAS_INSTALL_TTY=$WORK/no-such-tty||lean
CASES
grep -q "Pass --full, or set GRUNTAS_LEAN=0" "$WORK/build.log" \
  || { cat "$WORK/build.log"; fail "a lean install did not say how to get the full build"; }
pass "a lean install says why, and how to get the full build instead"

if "$BUILD_BIN/gruntas" sessions >"$WORK/lean-sessions.log" 2>&1; then fail "the lean build ran gruntas sessions"; fi
grep -q "not_in_build" "$WORK/lean-sessions.log" || { cat "$WORK/lean-sessions.log"; fail "the lean build did not say what it leaves out"; }
pass "the lean build is the agent build: no session store"

# A Dockerfile RUN under BuildKit: no /.dockerenv, no CI, and no terminal.
rm -rf "$BUILD_BIN"
env -i PATH="$PATH" HOME="$WORK/home" NO_COLOR=1 GRUNTAS_SKIP_SKILL=1 GRUNTAS_INSTALL_TTY="$WORK/no-such-tty" \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BUILD_BIN" \
  bash "$REPO_ROOT/scripts/install.sh" >"$WORK/notty.log" 2>&1 || { cat "$WORK/notty.log"; fail "the installer failed with no terminal"; }
grep -q "no terminal, so no person" "$WORK/notty.log" || { cat "$WORK/notty.log"; fail "a terminal-less install did not say why it is lean"; }
pass "no controlling terminal (a Dockerfile RUN under BuildKit): the lean build, and why"

# A release from before the lean build: one archive, one checksum, and a
# gruntas with no agent in it — stood in for by the lean binaries under the
# old name. A lean install of it must install that, not 404 on
# gruntas-lean_…, and must not advertise an agent the binary does not have.
OLD_REL="$RELEASE/old"
mkdir -p "$OLD_REL"
tar -czf "$OLD_REL/$ARCHIVE" -C "$WORK/lean" gruntas gruntas-mcp
cp "$RELEASE/SKILL.md" "$OLD_REL/"
(cd "$OLD_REL" && "${SHA256_CMD[@]}" "$ARCHIVE" >checksums.txt)
rm -rf "$BUILD_BIN"
env -i PATH="$PATH" HOME="$WORK/home" NO_COLOR=1 GRUNTAS_SKIP_SKILL=1 CI=true \
  GRUNTAS_INSTALL_BASE_URL="$BASE/old" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BUILD_BIN" \
  bash "$REPO_ROOT/scripts/install.sh" --lean >"$WORK/old-release.log" 2>&1 \
  || { cat "$WORK/old-release.log"; fail "a lean install of a release that predates the lean build failed"; }
cmp -s "$BUILD_BIN/gruntas" "$WORK/lean/gruntas" || fail "a release without a lean build installed something other than its one build"
grep -q "predates the lean build" "$WORK/old-release.log" || { cat "$WORK/old-release.log"; fail "the fallback to an old release's one build was not said"; }
if grep -q "Open gruntas's agent" "$WORK/old-release.log"; then cat "$WORK/old-release.log"; fail "the next steps advertised an agent the installed gruntas does not have"; fi
pass "a release that predates the lean build installs its one build, and says so"

if env -i PATH="$PATH" HOME="$WORK/home" NO_COLOR=1 GRUNTAS_LEAN=maybe \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BUILD_BIN" \
  bash "$REPO_ROOT/scripts/install.sh" >"$WORK/lean-bad.log" 2>&1; then
  fail "the installer accepted GRUNTAS_LEAN=maybe"
fi
if env -i PATH="$PATH" HOME="$WORK/home" NO_COLOR=1 \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BUILD_BIN" \
  bash "$REPO_ROOT/scripts/install.sh" --tiny >"$WORK/arg-bad.log" 2>&1; then
  fail "the installer accepted --tiny"
fi
pass "a GRUNTAS_LEAN or an argument that means neither build is refused"

echo
echo "The release's signature, with cosign (R-OSS-3)"
# A stub cosign: it records what it was asked and answers as COSIGN_SAYS says,
# so what the installer asks cosign for, and what it does with the answer, is
# what is tested; cosign's own checking is SECURITY.md's command's.
STUB="$WORK/cosign-stub"
mkdir -p "$STUB"
cat >"$STUB/cosign" <<'STUB_EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$COSIGN_ARGS"
[[ "$COSIGN_SAYS" == ok ]] || { echo "Error: none of the expected identities matched" >&2; exit 1; }
echo "Verified OK" >&2
STUB_EOF
chmod +x "$STUB/cosign"
# Only a signed release has a bundle to check: a local run's fake 9.9.9 is,
# CI's 0.0.0-snapshot from dist/ is not, and has none to download.
if signed_since "$VERSION"; then
for says in ok refused; do
  rm -rf "${BIN:?}"/*
  if env -i PATH="$STUB:$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
    COSIGN_SAYS="$says" COSIGN_ARGS="$WORK/cosign-$says.args" \
    GRUNTAS_SKIP_SKILL=1 GRUNTAS_INSTALL_TTY=/dev/null \
    GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
    bash "$REPO_ROOT/scripts/install.sh" >"$WORK/cosign-$says.log" 2>&1; then
    [[ "$says" == ok ]] || { cat "$WORK/cosign-$says.log"; fail "R-OSS-3: the installer installed a release whose signature cosign refused"; }
  else
    [[ "$says" == refused ]] || { cat "$WORK/cosign-$says.log"; fail "R-OSS-3: the installer failed on a signature cosign verified"; }
  fi
  { grep -qx -- "https://github.com/gruntas/gruntas/.github/workflows/release.yml@refs/tags/v$VERSION" "$WORK/cosign-$says.args" \
    && grep -qx -- "https://token.actions.githubusercontent.com" "$WORK/cosign-$says.args" \
    && grep -qx -- "--bundle" "$WORK/cosign-$says.args" \
    && grep -A1 -x -- "--certificate-github-workflow-repository" "$WORK/cosign-$says.args" | grep -qx "gruntas/gruntas" \
    && grep -A1 -x -- "--certificate-github-workflow-trigger" "$WORK/cosign-$says.args" | grep -qx "push"; } \
    || { cat "$WORK/cosign-$says.args"; fail "R-OSS-3: cosign was not asked for the release workflow's identity at v$VERSION"; }
  if [[ "$says" == ok ]]; then
    { grep -q "Signature verified" "$WORK/cosign-ok.log" && [[ -x "$BIN/gruntas" ]]; } || { cat "$WORK/cosign-ok.log"; fail "R-OSS-3: a verified signature was not said"; }
    pass "R-OSS-3: with cosign, the signature is checked as the release workflow at the version's tag"
  else
    { [[ ! -e "$BIN/gruntas" ]] && grep -q "not signed by" "$WORK/cosign-refused.log"; } || { cat "$WORK/cosign-refused.log"; fail "R-OSS-3: a refused signature did not stop the install"; }
    pass "R-OSS-3: a signature cosign refuses stops the install, and installs nothing"
  fi
done
# The old release laid out above has no bundle; at this version it should.
if env -i PATH="$STUB:$PATH" HOME="$WORK/home" NO_COLOR=1 GRUNTAS_SKIP_SKILL=1 CI=true \
  COSIGN_SAYS=ok COSIGN_ARGS="$WORK/cosign-missing.args" \
  GRUNTAS_INSTALL_BASE_URL="$BASE/old" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
  bash "$REPO_ROOT/scripts/install.sh" --lean >"$WORK/cosign-missing.log" 2>&1; then
  fail "R-OSS-3: with cosign, a signed release's missing bundle was installed past"
fi
grep -q "would not download" "$WORK/cosign-missing.log" || { cat "$WORK/cosign-missing.log"; fail "R-OSS-3: a missing bundle failed for another reason"; }
pass "R-OSS-3: with cosign, a signed release without its bundle is refused"
# GRUNTAS_REQUIRE_SIGNATURE=1: no cosign, or a release from before signing, is
# refused rather than installed on its checksum.
rm -rf "${BIN:?}"/*
if env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 GRUNTAS_REQUIRE_SIGNATURE=1 \
  GRUNTAS_SKIP_SKILL=1 GRUNTAS_INSTALL_TTY=/dev/null \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
  bash "$REPO_ROOT/scripts/install.sh" >"$WORK/strict-nocosign.log" 2>&1; then
  fail "R-OSS-3: GRUNTAS_REQUIRE_SIGNATURE=1 installed without cosign"
fi
{ [[ ! -e "$BIN/gruntas" ]] && grep -q "install cosign" "$WORK/strict-nocosign.log"; } || { cat "$WORK/strict-nocosign.log"; fail "R-OSS-3: GRUNTAS_REQUIRE_SIGNATURE=1 without cosign did not say to install it"; }
pass "R-OSS-3: GRUNTAS_REQUIRE_SIGNATURE=1 refuses without cosign"
else
  echo "  - R-OSS-3: the cosign checks need a signed release; $VERSION predates signing (a local run checks them on its own)"
fi
{ signed_since 0.12.0-rc2 && signed_since 1.0.0 && ! signed_since 0.11.2; } || fail "the first signed release is not 0.12.0"
pass "R-OSS-3: releases before 0.12.0 have no bundle"
verify_signature_log=$( (REQUIRE_SIGNATURE=1 verify_signature "$BASE" "$WORK" 0.11.2) 2>&1 ) && fail "R-OSS-3: GRUNTAS_REQUIRE_SIGNATURE=1 accepted a release from before signing"
[[ "$verify_signature_log" == *"predates signed releases"*"install 0.12.0 or later"* ]] || fail "R-OSS-3: an unsigned old release under GRUNTAS_REQUIRE_SIGNATURE=1 was refused for another reason: $verify_signature_log"
pass "R-OSS-3: GRUNTAS_REQUIRE_SIGNATURE=1 refuses a release from before signing"

echo
echo "Refusing a bad download"
rm -rf "${BIN:?}"/*
# One byte of the archive changed, checksums.txt untouched: exactly the shape of
# a tampered or truncated download.
printf 'x' >>"$RELEASE/$ARCHIVE"
if env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
  GRUNTAS_SKIP_SKILL=1 GRUNTAS_INSTALL_TTY=/dev/null \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="$VERSION" GRUNTAS_BIN_DIR="$BIN" \
  bash "$REPO_ROOT/scripts/install.sh" >"$WORK/bad.log" 2>&1; then
  fail "the installer accepted an archive whose checksum does not match"
fi
if [[ -e "$BIN/gruntas" ]]; then fail "the installer left a binary behind after a checksum failure"; fi
grep -q "not the file the release signed for" "$WORK/bad.log" \
  || { cat "$WORK/bad.log"; fail "the installer failed for some other reason than the checksum"; }
pass "a mismatched checksum stops the install, and installs nothing"

echo
echo "Refusing a missing version"
if env -i PATH="$PATH" HOME="$WORK/home" SHELL=/bin/bash NO_COLOR=1 \
  GRUNTAS_INSTALL_BASE_URL="$BASE" GRUNTAS_VERSION="not-a-version" GRUNTAS_BIN_DIR="$BIN" \
  bash "$REPO_ROOT/scripts/install.sh" >"$WORK/version.log" 2>&1; then
  fail "the installer accepted GRUNTAS_VERSION=not-a-version"
fi
pass "a version that is not one is refused"

echo
echo "Another gruntas earlier on PATH"
old="$WORK/shadow/old" new="$WORK/shadow/new"
mkdir -p "$old" "$new"
printf '#!/bin/sh\necho 0.0.1\n' >"$old/gruntas"
printf '#!/bin/sh\necho 9.9.9\n' >"$new/gruntas"
chmod +x "$old/gruntas" "$new/gruntas"
said=$(BIN_DIR="$new" PATH="$old:$new:$PATH" warn_shadowed linux_amd64)
[[ "$said" == *"Another gruntas comes first on your PATH: $old/gruntas (0.0.1)"* && "$said" == *"put $new before $old"* ]] \
  || fail "a gruntas earlier on PATH was not named: $said"
[[ -z "$(BIN_DIR="$new" PATH="$new:$old:$PATH" warn_shadowed linux_amd64)" ]] || fail "the installed gruntas, first on PATH, was taken for another"
[[ -z "$(BIN_DIR="$new" PATH="$old:$PATH" warn_shadowed linux_amd64)" ]] || fail "warned while BIN_DIR is not on PATH, which the profile line puts first"
pass "a gruntas that shadows the one installed is named, with the fix"

echo
echo "All checks passed."
