#!/usr/bin/env bash
# dist.sh — how a version becomes release archives. The one home of the
# platform table and of the release layout: CI builds one target per runner
# with it, in parallel, and `make dist` builds every target here in turn.
#
#   scripts/dist.sh targets                 the table: triple goos goarch runner
#   scripts/dist.sh matrix                  the table as a GitHub Actions matrix
#   scripts/dist.sh archive-name GOOS GOARCH VERSION [full|lean]
#   scripts/dist.sh build TRIPLE VERSION    both builds for one target, archived
#   scripts/dist.sh assemble VERSION        checksums.txt, metadata.json, artifacts.json
#   scripts/dist.sh all VERSION             build every target, then assemble
#
# The layout is what GoReleaser wrote, because npm/build.mjs, install.sh and the
# upgrader read it: dist/krowk_<version>_<goos>_<goarch>.tar.gz (.zip on Windows)
# holding krowk and krowk-mcp at its root, dist/checksums.txt in `sha256sum`
# form, and dist/<triple>/ holding the loose binaries.
#
# Two builds of krowk per target (R-PKG-3). The full build (`--features
# harness`: the agent, its TUI, the session store) is the one above, under the
# name every earlier release used, so links, npm and old upgraders keep
# getting what a person wants. The lean build (no features: the agent-container
# build, R-PKG-2) is dist/krowk-lean_<version>_<goos>_<goarch>.tar.gz beside
# it, and dist/<triple>-lean/ loose. krowk-mcp is the same lean binary in both.
# install.sh picks between them; an upgrade stays on the build it is.
#
# KROWK_FAST_BUILD=1 builds without LTO and with parallel codegen: the same
# binaries in every way a packaging test can see, minutes sooner. Releases
# never set it.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# triple goos goarch runner. Linux is musl, and static, because the binary lands
# in containers we did not build. Darwin builds on macOS, the only place its
# SDK is; the rest cross-compile with zig as the C toolchain for the bundled
# SQLite and ring. Nobody runs an agent container on Windows ARM yet.
TARGETS="x86_64-unknown-linux-musl linux amd64 ubuntu-latest
aarch64-unknown-linux-musl linux arm64 ubuntu-latest
x86_64-apple-darwin darwin amd64 macos-latest
aarch64-apple-darwin darwin arm64 macos-latest
x86_64-pc-windows-gnu windows amd64 ubuntu-latest"

die() { echo "dist.sh: $*" >&2; exit 1; }

row() {
  local found
  found=$(awk -v t="$1" '$1 == t' <<<"$TARGETS")
  [[ -n "$found" ]] || die "no target $1 — scripts/dist.sh targets lists them"
  echo "$found"
}

archive_name() {
  local ext="tar.gz" name="krowk"
  [[ "$1" == windows ]] && ext="zip"
  case "${4:-full}" in
    full) ;;
    lean) name="krowk-lean" ;;
    *) die "no build ${4} — full or lean" ;;
  esac
  echo "${name}_${3}_${1}_${2}.${ext}"
}

# archive DIR GOOS GOARCH VERSION FLAVOUR: the two binaries in DIR, archived.
archive() {
  local dir="$1" goos="$2" goarch="$3" version="$4" flavour="$5" ext="" archive
  [[ "$goos" == windows ]] && ext=".exe"
  archive="dist/$(archive_name "$goos" "$goarch" "$version" "$flavour")"
  rm -f "$archive"
  if [[ "$goos" == windows ]]; then
    (cd "$dir" && zip -q -X "$OLDPWD/$archive" "krowk$ext" "krowk-mcp$ext")
  else
    tar -czf "$archive" -C "$dir" krowk krowk-mcp
  fi
  echo "dist.sh: $archive"
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}

build() {
  local triple="$1" version="$2" goos goarch ext="" tool=zigbuild out
  read -r _ goos goarch _ <<<"$(row "$triple")"
  [[ "$goos" == windows ]] && ext=".exe"
  # Apple's own toolchain for Apple targets on a Mac; zig everywhere else.
  [[ "$goos" == darwin && "$(uname -s)" == Darwin ]] && tool=build
  # R-OSS-3: the same tag builds the same bytes wherever it is checked out.
  # The only machine-specific input a release binary would otherwise carry is
  # the absolute path of its sources, in panic locations and in the C that
  # SQLite and ring compile, so both are rewritten to fixed prefixes: this
  # checkout to /krowk, cargo's registry to /cargo. The standard library's
  # paths are already /rustc/<commit>. The toolchain versions are the other
  # input, and they are pinned in the workflows.
  local cargo_home="${CARGO_HOME:-$HOME/.cargo}"
  export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$PWD=/krowk --remap-path-prefix=$cargo_home=/cargo"
  export CFLAGS="${CFLAGS:-} -ffile-prefix-map=$PWD=/krowk -ffile-prefix-map=$cargo_home=/cargo"
  if [[ "${KROWK_FAST_BUILD:-}" == 1 ]]; then
    export CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
  fi
  # The lean build first, both binaries: krowk-mcp never touches the session
  # store, so it is the lean build in both archives. Its krowk is copied out
  # before the full build overwrites target/…/krowk.
  KROWK_VERSION="$version" cargo "$tool" --release --locked --target "$triple" -p krowk --bin krowk --bin krowk-mcp
  local lean="dist/$triple-lean"
  rm -rf "$lean" && mkdir -p "$lean"
  for bin in krowk krowk-mcp; do
    cp "target/$triple/release/$bin$ext" "$lean/"
  done
  KROWK_VERSION="$version" cargo "$tool" --release --locked --target "$triple" -p krowk --bin krowk --features harness

  out="dist/$triple"
  rm -rf "$out" && mkdir -p "$out"
  cp "target/$triple/release/krowk$ext" "$lean/krowk-mcp$ext" "$out/"
  archive "$out" "$goos" "$goarch" "$version" full
  archive "$lean" "$goos" "$goarch" "$version" lean
}

assemble() {
  local version="$1" archives=() triple goos goarch ext
  while read -r triple goos goarch _; do
    local archive flavour
    for flavour in full lean; do
      archive=$(archive_name "$goos" "$goarch" "$version" "$flavour")
      [[ -f "dist/$archive" ]] || die "dist/$archive is missing — build $triple first"
      archives+=("$archive")
    done
  done <<<"$TARGETS"
  (cd dist && sha256 "${archives[@]}" >checksums.txt)

  printf '{"project_name":"krowk","tag":"v%s","version":"%s"}\n' "$version" "$version" >dist/metadata.json
  {
    echo "["
    local first=1
    while read -r triple goos goarch _; do
      ext=""
      [[ "$goos" == windows ]] && ext=".exe"
      # The lean krowk is listed as its own ID, which npm/build.mjs does not
      # package: npm carries the full build, as it always has.
      for entry in "krowk $triple krowk" "krowk-mcp $triple krowk-mcp" "krowk-lean $triple-lean krowk"; do
        read -r id dir bin <<<"$entry"
        [[ $first == 1 ]] || echo ","
        first=0
        printf '  {"name":"%s","path":"dist/%s/%s%s","goos":"%s","goarch":"%s","type":"Binary","extra":{"ID":"%s"}}' \
          "$bin" "$dir" "$bin" "$ext" "$goos" "$goarch" "$id"
      done
    done <<<"$TARGETS"
    echo
    echo "]"
  } >dist/artifacts.json
  echo "dist.sh: dist/checksums.txt, dist/metadata.json, dist/artifacts.json"
}

case "${1:-}" in
  targets) echo "$TARGETS" ;;
  matrix)
    printf '{"include":['
    awk 'NR > 1 { printf "," } { printf "{\"target\":\"%s\",\"runner\":\"%s\"}", $1, $4 }' <<<"$TARGETS"
    printf ']}\n'
    ;;
  archive-name) archive_name "${2:?goos}" "${3:?goarch}" "${4:?version}" "${5:-full}" ;;
  build) build "${2:?triple}" "${3:?version}" ;;
  assemble) assemble "${2:?version}" ;;
  all)
    rm -rf dist && mkdir -p dist
    while read -r triple _; do build "$triple" "${2:?version}"; done <<<"$TARGETS"
    assemble "$2"
    ;;
  *) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2 ;;
esac
