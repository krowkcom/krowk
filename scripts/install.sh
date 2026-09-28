#!/usr/bin/env bash
# install.sh — put krowk and krowk-mcp on this machine.
#
#   curl -fsSL https://krowk.com/install | bash
#
# Both binaries come down together because scripts/dist.sh ships them in one
# archive: an agent container that wants krowk usually wants krowk-mcp too, and
# one download is one thing to get wrong.
#
# Which krowk comes down depends on who is installing it (R-PKG-3). A person
# gets the full build: krowk's own agent (bare `krowk` opens it), its session
# store, everything. CI and containers get the lean build, the one agent
# containers run: a few megabytes, no SQLite, nothing to fail on. CI is read
# from the variables CI systems set (CI, GITHUB_ACTIONS, GITLAB_CI, …) and a
# container from the markers container runtimes leave (/.dockerenv,
# /run/.containerenv, $container, a Kubernetes service host), or from there
# being no controlling terminal at all — a Dockerfile RUN, a provisioning
# script. `--lean` or
# KROWK_LEAN=1 asks for the lean build anywhere; `--full` or KROWK_LEAN=0 for
# the full one:
#
#   curl -fsSL https://krowk.com/install | bash -s -- --lean
#
# There is no wizard at the end and nothing to log into. A keyless push works the
# moment the binary lands, so the last thing this script does is say what to run,
# not ask a question nobody is there to answer — the common caller is `curl |
# bash` inside a container build, where stdin is not a terminal.
#
# Options, all through the environment because a piped script has no argv:
#   KROWK_BIN_DIR     Where the binaries go (default: ~/bin if it is on PATH,
#                     else ~/.local/bin if it is; otherwise ~/bin on Windows and
#                     ~/.local/bin everywhere else)
#   KROWK_VERSION     A version to install, e.g. 0.1.0 (default: the latest release)
#   KROWK_SKIP_SKILL  1 to leave the agent skill alone
#   KROWK_LEAN        1 for the lean build, 0 for the full one (default: lean in
#                     CI and containers, full everywhere else)
#
#   KROWK_INSTALL_BASE_URL
#                     Test-only. A directory holding the archives, checksums.txt
#                     and SKILL.md, instead of the GitHub release. It exists so
#                     scripts/install_test.sh can run this file end to end
#                     against a local server; it is not a supported knob, and it
#                     requires KROWK_VERSION since there is no release to ask.
#   KROWK_INSTALL_FS_ROOT
#                     Test-only. Where the container markers are looked for,
#                     instead of /, so the detection can be tested on a machine
#                     that is, or is not, a container.
#   KROWK_INSTALL_TTY Test-only. The terminal device opened to tell whether a
#                     person is there (default /dev/tty), so a test run with no
#                     terminal can stand in for one that has one.

set -euo pipefail

# KROWK_REPO is how the GitHub Action names the repository it was taken from;
# anyone else gets the canonical one.
REPO="${KROWK_REPO:-krowkcom/krowk}"
BIN_DIR="${KROWK_BIN_DIR:-}"
VERSION="${KROWK_VERSION:-}"
BASE_URL_OVERRIDE="${KROWK_INSTALL_BASE_URL:-}"
FS_ROOT="${KROWK_INSTALL_FS_ROOT:-}"
TTY_DEVICE="${KROWK_INSTALL_TTY:-/dev/tty}"
# full or lean, and why; main settles both from the arguments, KROWK_LEAN and
# what this machine is, before anything is downloaded.
BUILD=""
BUILD_REASON=""
# yes once the installed krowk is found to carry the agent (`krowk -p`).
HAS_AGENT=""
CURL_SCHANNEL_FALLBACK_FLAG=""
# Both of these are files rather than variables, and main fills them in. See
# curl_run for why.
CURL_ERROR_FILE=""
CURL_FALLBACK_NOTED_FILE=""
# The SHA-256 command, as an array because `shasum -a 256` is three words.
# resolve_sha256 fills it once, before anything is downloaded.
SHA256_CMD=()

# The binaries this installs. krowk is first because it is the one that gets
# checked afterwards, and the one the next steps talk about.
BINARIES=(krowk krowk-mcp)

# Color helpers — NO_COLOR is honoured (https://no-color.org), and so is a stdout
# that is not a terminal, which is the usual case under `curl | bash` in CI.
if [[ -z "${NO_COLOR:-}" ]] && [[ -t 1 ]]; then
  bold()  { printf '\033[1m%s\033[0m' "$1"; }
  green() { printf '\033[32m%s\033[0m' "$1"; }
  red()   { printf '\033[31m%s\033[0m' "$1"; }
else
  bold()  { printf '%s' "$1"; }
  green() { printf '%s' "$1"; }
  red()   { printf '%s' "$1"; }
fi

info()  { echo "  $(green "✓") $1"; }
step()  { echo "  $(bold "→") $1"; }
note()  { echo "    $1"; }
error() { echo "  $(red "✗") $1" >&2; exit 1; }

# resolve_sha256 assigns SHA256_CMD rather than printing it, and main calls it
# before the first download. Printing it would put this failure inside a command
# substitution, where error's exit ends the subshell and nothing else: the caller
# would carry on with an empty command, compute an empty digest, and report a
# checksum mismatch that never happened. Assigning to a global keeps the failure
# where the reader is — at top level, under set -e — and keeps it early, before
# any bytes have been fetched to verify.
resolve_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    SHA256_CMD=(sha256sum)
  elif command -v shasum >/dev/null 2>&1; then
    SHA256_CMD=(shasum -a 256)
  else
    error "No SHA-256 tool here, so the download could not be verified and nothing was installed: install sha256sum or shasum, then run this again."
  fi
}

path_contains_dir() {
  [[ ":$PATH:" == *":$1:"* ]]
}

# default_bin_dir prefers a directory already on PATH, so the install ends with a
# working command rather than an instruction to edit a shell profile.
default_bin_dir() {
  local platform="$1"

  if path_contains_dir "$HOME/bin"; then
    echo "$HOME/bin"
    return 0
  fi
  if path_contains_dir "$HOME/.local/bin"; then
    echo "$HOME/.local/bin"
    return 0
  fi
  if [[ "$platform" == windows_* ]]; then
    echo "$HOME/bin"
  else
    echo "$HOME/.local/bin"
  fi
}

# detect_platform names the archive, so it may only answer with combinations
# scripts/dist.sh actually builds. Anything else fails here with the reason,
# rather than 404ing on a download and looking like a network problem.
detect_platform() {
  local os arch
  os=$(uname -s | tr '[:upper:]' '[:lower:]')
  case "$os" in
    darwin) os="darwin" ;;
    linux) os="linux" ;;
    mingw*|msys*|cygwin*) os="windows" ;;
    *) error "krowk has no build for $os. Linux, macOS and Windows are what the release carries; from source: cargo install --locked --git https://github.com/${REPO} --features harness krowk" ;;
  esac

  arch=$(uname -m)
  case "$arch" in
    x86_64|amd64) arch="amd64" ;;
    aarch64|arm64) arch="arm64" ;;
    *) error "krowk has no build for $arch. amd64 and arm64 are what the release carries; from source: cargo install --locked --git https://github.com/${REPO} --features harness krowk" ;;
  esac

  # Windows ARM is deliberately not built. Say so, rather than offering a
  # download that was never uploaded.
  if [[ "$os" == "windows" && "$arch" == "arm64" ]]; then
    error "No Windows ARM build is published. Install inside WSL2, or build from source: cargo install --locked --git https://github.com/${REPO} --features harness krowk"
  fi

  echo "${os}_${arch}"
}

# detect_curl_fallback looks for the one curl failure that is not a real failure:
# Windows' Schannel backend cannot always reach a CRL, and refuses the download
# over a certificate it has no complaint about otherwise.
detect_curl_fallback() {
  local version_output help_output

  version_output=$(curl --version 2>/dev/null || true)
  if [[ "$version_output" != *[Ss]channel* ]]; then
    return 0
  fi

  help_output=$(curl --help all 2>/dev/null || true)
  if [[ "$help_output" == *"--ssl-revoke-best-effort"* ]]; then
    CURL_SCHANNEL_FALLBACK_FLAG="--ssl-revoke-best-effort"
  elif [[ "$help_output" == *"--ssl-no-revoke"* ]]; then
    CURL_SCHANNEL_FALLBACK_FLAG="--ssl-no-revoke"
  fi
}

# curl_run keeps curl's own words. Every failure this script reports is a failure
# somebody has to act on, and "failed to download" without the reason sends them
# to the wrong place.
#
# The reason goes to a file, not to a variable. Every caller here runs curl_run
# inside `$(…)` to capture what came back, and a variable assigned in that
# subshell dies with it — so a global would be empty in exactly the callers that
# want to quote it. The file outlives the subshell; curl_reason reads it back.
# The same goes for having said the Schannel line once: a marker file, because
# the counter would reset with every subshell and repeat the line each time.
# curl's status is taken on the same line as curl, with `|| status=$?`. Reading
# it after an `if` would read the `if` instead: a compound command whose
# condition was false and which has no else branch succeeds, so $? is 0 there
# however curl exited, and every failed download would be reported as a working
# one.
curl_run() {
  local status=0

  curl --show-error "$@" 2>"$CURL_ERROR_FILE" || status=$?
  if ((status == 0)); then
    : >"$CURL_ERROR_FILE"
    return 0
  fi

  if [[ -n "$CURL_SCHANNEL_FALLBACK_FLAG" ]] &&
    grep -q 'CRYPT_E_NO_REVOCATION_CHECK' "$CURL_ERROR_FILE"; then
    if [[ ! -e "$CURL_FALLBACK_NOTED_FILE" ]]; then
      step "Windows cannot check certificate revocation here; retrying with ${CURL_SCHANNEL_FALLBACK_FLAG}" >&2
      : >"$CURL_FALLBACK_NOTED_FILE"
    fi
    status=0
    curl --show-error "$CURL_SCHANNEL_FALLBACK_FLAG" "$@" 2>"$CURL_ERROR_FILE" || status=$?
    if ((status == 0)); then
      : >"$CURL_ERROR_FILE"
      return 0
    fi
  fi

  return "$status"
}

# curl_reason is what curl last complained about, on one line so it can be read
# inside a sentence, or nothing at all when the last call succeeded.
curl_reason() {
  [[ -n "$CURL_ERROR_FILE" && -s "$CURL_ERROR_FILE" ]] || return 0
  tr '\n' ' ' <"$CURL_ERROR_FILE" | sed 's/  */ /g; s/ *$//'
}

is_semver() {
  [[ $1 =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]
}

# latest_version follows the releases/latest redirect and reads the tag off the
# URL it lands on. That is a plain redirect rather than an API call, so it is not
# rate limited and needs no token; the API is the fallback for when GitHub
# changes the redirect.
latest_version() {
  local url version api_json

  if url=$(curl_run -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest"); then
    version="${url##*/}"
    version="${version#v}"
    if is_semver "$version"; then
      echo "$version"
      return 0
    fi
  fi

  if api_json=$(curl_run -fsSL -H 'Accept: application/vnd.github+json' \
    -H 'User-Agent: krowk-installer' \
    "https://api.github.com/repos/${REPO}/releases/latest"); then
    if [[ $api_json =~ \"tag_name\"[[:space:]]*:[[:space:]]*\"v?([^\"]+)\" ]]; then
      version="${BASH_REMATCH[1]}"
      if is_semver "$version"; then
        echo "$version"
        return 0
      fi
    fi
  fi

  local why
  why=$(curl_reason)
  error "Could not find the latest release of ${REPO}. ${why:+curl said: ${why}. }Name one with KROWK_VERSION, or install from source: cargo install --locked --git https://github.com/${REPO} --features harness krowk"
}

release_base_url() {
  if [[ -n "$BASE_URL_OVERRIDE" ]]; then
    echo "${BASE_URL_OVERRIDE%/}"
  else
    echo "https://github.com/${REPO}/releases/download/v$1"
  fi
}

# verify_checksum is not best-effort. These bytes are about to be made executable
# and put on PATH, so a checksums.txt that will not download, does not name the
# archive, or names a different digest all end the install — there is no path
# through this function that installs an unverified binary.
verify_checksum() {
  local tmp_dir="$1" archive="$2"
  local expected actual

  step "Verifying the download"

  # scripts/dist.sh writes `<digest>  <name>`; a binary-mode digest writes `*<name>`.
  expected=$(checksum_for "$tmp_dir" "$archive")
  if [[ -z "$expected" ]]; then
    error "checksums.txt does not mention ${archive}, so there is nothing to check it against. Report this at https://github.com/${REPO}/issues"
  fi

  actual=$(cd "$tmp_dir" && "${SHA256_CMD[@]}" "$archive" | awk '{print $1}')

  if [[ "$expected" != "$actual" ]]; then
    error "${archive} is not the file the release signed for (expected ${expected}, got ${actual}). Nothing is installed. Retry, and if it happens again report it at https://github.com/${REPO}/issues"
  fi
  info "Checksum matches"
}

# fetch_checksums downloads the release's checksums.txt into tmp_dir, before
# any archive: it is also the release's list of what it carries, which is how
# a release from before the lean build is told apart.
fetch_checksums() {
  local base_url="$1" tmp_dir="$2" why
  if ! curl_run -fsSL "${base_url}/checksums.txt" -o "${tmp_dir}/checksums.txt"; then
    why=$(curl_reason)
    error "checksums.txt would not download from ${base_url}${why:+ (${why})}. Nothing is installed: an archive nobody can check is not one to run."
  fi
}

checksum_for() {
  awk -v f="$2" '$2 == f || $2 == ("*" f) {print $1; exit}' "$1/checksums.txt"
}

# truthy says whether a CI variable is set to something that means yes: CI
# systems set CI=true or CI=1, and nobody sets CI=false to mean "this is CI".
truthy() {
  case "$(printf '%s' "${1:-}" | tr '[:upper:]' '[:lower:]')" in
    "" | 0 | false | no | off) return 1 ;;
    *) return 0 ;;
  esac
}

# ci_name names the CI system this runs under, or prints nothing.
ci_name() {
  local var
  for var in GITHUB_ACTIONS GITLAB_CI BUILDKITE CIRCLECI TRAVIS JENKINS_URL TF_BUILD TEAMCITY_VERSION CODEBUILD_BUILD_ID BITBUCKET_BUILD_NUMBER DRONE; do
    if [[ -n "${!var:-}" ]]; then
      echo "$var"
      return
    fi
  done
  if truthy "${CI:-}"; then
    echo "CI=${CI}"
  fi
}

# container_name names the container marker this machine carries, or prints
# nothing. Only markers a runtime leaves on purpose: guessing from cgroup paths
# misreads a systemd desktop as a container.
container_name() {
  # BuildKit, which runs every `docker build` now, leaves no /.dockerenv
  # during a RUN step, so a Dockerfile `RUN curl … | bash` is found here: no
  # controlling terminal means no person, and a script or an image build
  # wants the lean build. Toolbox and distrobox are containers too, and say
  # so; their users pass --full.
  if ! { : <"$TTY_DEVICE"; } 2>/dev/null; then
    echo "no terminal"
  elif [[ -e "${FS_ROOT}/.dockerenv" ]]; then
    echo "/.dockerenv"
  elif [[ -e "${FS_ROOT}/run/.containerenv" ]]; then
    echo "/run/.containerenv"
  elif [[ -n "${container:-}" ]]; then
    echo "container=${container}"
  elif [[ -n "${KUBERNETES_SERVICE_HOST:-}" ]]; then
    echo "KUBERNETES_SERVICE_HOST"
  fi
}

# choose_build settles BUILD and BUILD_REASON: an explicit ask first (the
# argument, then KROWK_LEAN), then what this machine is.
choose_build() {
  local asked="$1" found
  if [[ -n "$asked" ]]; then
    BUILD="$asked"
    BUILD_REASON="--${asked} was passed"
    return
  fi
  case "${KROWK_LEAN:-}" in
    "") ;;
    1 | true | yes)
      BUILD=lean
      BUILD_REASON="KROWK_LEAN=${KROWK_LEAN}"
      return
      ;;
    0 | false | no)
      BUILD=full
      BUILD_REASON="KROWK_LEAN=${KROWK_LEAN}"
      return
      ;;
    *) error "KROWK_LEAN=${KROWK_LEAN} is neither 1 (the lean build) nor 0 (the full build)" ;;
  esac
  found=$(ci_name)
  if [[ -n "$found" ]]; then
    BUILD=lean
    BUILD_REASON="CI detected (${found})"
    return
  fi
  found=$(container_name)
  if [[ "$found" == "no terminal" ]]; then
    BUILD=lean
    BUILD_REASON="no terminal, so no person (a Dockerfile RUN, a script)"
    return
  elif [[ -n "$found" ]]; then
    BUILD=lean
    BUILD_REASON="a container detected (${found})"
    return
  fi
  BUILD=full
  BUILD_REASON=""
}

# archive_for is the name scripts/dist.sh gives the archive: krowk_… for the
# full build, krowk-lean_… for the lean one.
archive_for() {
  local version="$1" platform="$2" build="$3" ext="tar.gz" name="krowk"
  [[ "$platform" == windows_* ]] && ext="zip"
  [[ "$build" == lean ]] && name="krowk-lean"
  echo "${name}_${version}_${platform}.${ext}"
}

# download_binaries fetches one archive and installs both binaries out of it.
download_binaries() {
  local version="$1" platform="$2" tmp_dir="$3"
  local archive base_url why suffix="" ext="tar.gz"

  if [[ "$platform" == windows_* ]]; then
    ext="zip"
    suffix=".exe"
  fi

  base_url=$(release_base_url "$version")
  fetch_checksums "$base_url" "$tmp_dir"
  archive=$(archive_for "$version" "$platform" "$BUILD")
  # A release cut before the lean build existed has only the one archive.
  # Asking it for krowk-lean_… would 404 every CI install of a pinned old
  # version, so that release's only build is installed, and said so.
  if [[ "$BUILD" == lean && -z "$(checksum_for "$tmp_dir" "$archive")" ]]; then
    local full
    full=$(archive_for "$version" "$platform" full)
    if [[ -n "$(checksum_for "$tmp_dir" "$full")" ]]; then
      BUILD=full
      BUILD_REASON="krowk ${version} predates the lean build, so its one build is installed"
      archive="$full"
    fi
  fi

  step "Downloading krowk ${version} (${BUILD} build) for ${platform//_/ }, from ${REPO}"
  if [[ "$BUILD" == lean ]]; then
    note "The lean build: ${BUILD_REASON}. Pass --full, or set KROWK_LEAN=0, for krowk's own agent."
  elif [[ -n "$BUILD_REASON" ]]; then
    note "The full build: ${BUILD_REASON}."
  fi
  if ! curl_run -fsSL "${base_url}/${archive}" -o "${tmp_dir}/${archive}"; then
    why=$(curl_reason)
    error "Could not download ${base_url}/${archive}${why:+ (${why})}. Check that ${version} is a released version: https://github.com/${REPO}/releases"
  fi

  verify_checksum "$tmp_dir" "$archive"

  if [[ "$ext" == "zip" ]]; then
    command -v unzip >/dev/null 2>&1 || error "unzip is needed to open ${archive} and is not installed"
    unzip -q "${tmp_dir}/${archive}" -d "$tmp_dir"
  else
    tar -xzf "${tmp_dir}/${archive}" -C "$tmp_dir"
  fi

  mkdir -p "$BIN_DIR"
  local name
  for name in "${BINARIES[@]}"; do
    if [[ ! -f "${tmp_dir}/${name}${suffix}" ]]; then
      error "${archive} does not contain ${name}${suffix}. Report this at https://github.com/${REPO}/issues"
    fi
    # Move then chmod, so the file is never briefly executable under a name
    # something else might pick up.
    mv -f "${tmp_dir}/${name}${suffix}" "${BIN_DIR}/${name}${suffix}"
    chmod +x "${BIN_DIR}/${name}${suffix}"
    info "Installed ${BIN_DIR}/${name}${suffix}"
  done
}

# setup_path only writes to a shell profile when it has to. If the chosen
# directory is already on PATH — which default_bin_dir tries hard to arrange —
# this script leaves the user's dotfiles alone.
setup_path() {
  if path_contains_dir "$BIN_DIR"; then
    return 0
  fi

  local shell_rc
  case "${SHELL:-}" in
    */zsh)  shell_rc="$HOME/.zshrc" ;;
    */bash) shell_rc="$HOME/.bashrc" ;;
    *)      shell_rc="$HOME/.profile" ;;
  esac

  # The profile's directory is $HOME, which normally exists — but an installer
  # that aborts here has already put working binaries on disk, and failing after
  # the work is done is the worst place to fail.
  mkdir -p "$(dirname "$shell_rc")"

  # PATH itself has already answered "is this directory reachable?" above. All
  # the profile can answer is the narrower question of whether this installer has
  # written this line before, so that is the only thing looked for: the whole
  # line, exactly. Searching the file for the directory name instead would count
  # a comment, an alias, or a longer path that merely starts the same way, and
  # then skip the export while PATH stays broken.
  local export_line="export PATH=\"$BIN_DIR:\$PATH\""

  if [[ -f "$shell_rc" ]] && grep -qxF "$export_line" "$shell_rc" 2>/dev/null; then
    info "$BIN_DIR is already exported in $shell_rc"
    note "This shell has not read that yet: source $shell_rc"
    return 0
  fi

  {
    echo ""
    echo "# Added by the krowk installer"
    echo "$export_line"
  } >>"$shell_rc"
  info "Added $BIN_DIR to PATH in $shell_rc"
  note "This shell has not read that yet: source $shell_rc"
}

# verify_install runs the thing that was just installed. A binary that landed but
# will not execute is the failure worth catching here — the wrong architecture,
# or Windows refusing an unsigned executable.
verify_install() {
  local platform="$1" suffix="" installed err_file
  [[ "$platform" == windows_* ]] && suffix=".exe"

  err_file=$(mktemp "${TMPDIR:-/tmp}/krowk-verify.XXXXXX")
  if installed=$("${BIN_DIR}/krowk${suffix}" --version 2>"$err_file"); then
    rm -f "$err_file"
    info "krowk ${installed} works"
    # Asked of the binary itself rather than read off the build asked for: a
    # release from before the agent, or the fallback for one, has none.
    if "${BIN_DIR}/krowk${suffix}" -p --help >/dev/null 2>&1; then
      HAS_AGENT=yes
    fi
    return 0
  fi

  local why
  why=$(<"$err_file")
  rm -f "$err_file"

  local detail="krowk was installed to ${BIN_DIR} but will not run"
  [[ -n "$why" ]] && detail="${detail}: ${why}"
  if [[ "$platform" == windows_* ]]; then
    detail="$detail
    Windows may have blocked it: Smart App Control runs code-signed binaries only.
    Installing inside WSL2 avoids that."
  fi
  error "$detail"
}

# skills_dir is where an agent looks for skills on this machine, or empty when
# nothing here uses them. CLAUDE_CONFIG_DIR wins, since a user who moved their
# config has said where it lives.
skills_dir() {
  local config="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
  if [[ -d "${config}/skills" ]]; then
    echo "${config}/skills"
  fi
}

# The ownership marker and the version stamp krowk writes beside every skill it
# manages. This script is their only writer: an upgrade re-runs it, and a
# directory it claimed once is one it recognises and refreshes after.
MANAGED_MARKER=".managed-by-krowk-cli"
INSTALLED_VERSION_FILE=".installed-version"
MANAGED_MARKER_CONTENT="This directory is managed by krowk. Manual edits will be overwritten on upgrade."

# write_managed_file writes one file krowk owns, and never through a symlink.
# The bytes go to a temporary file in the destination's own directory and are
# then renamed onto the final name: rename(2) replaces whatever is at that name
# rather than following it, so a link planted in a managed file's name is
# destroyed instead of being written through. Nothing is ever redirected or
# chmod-ed onto a final path — a `>` follows a symlink, and `chmod` follows it
# too.
#
# The temp file is a sibling on purpose: a rename is only atomic within one
# filesystem, and $TMPDIR is regularly on another one. Every failure path
# removes it — a stranded .krowk-XXXXXX left in the skill directory would make
# the next run's adoption check refuse a directory krowk itself littered.
#
# A directory at the destination is refused rather than replaced, and refused
# before anything is written: `mv` onto a directory moves the file *into* it and
# reports success, which would leave the managed file at a path nobody named.
# The -d test follows links, which is what is wanted here — a link to a
# directory is a directory as far as mv is concerned.
#
# Reads from stdin so a caller can pipe a downloaded file through it without
# the content ever passing through a variable.
write_managed_file() {
  local path="$1" dir tmp

  if [[ -L "$path" ]]; then
    note "${path} is a symlink, which points somewhere this installer never looked; leaving it alone."
    return 1
  fi
  if [[ -d "$path" ]]; then
    note "${path} is a directory, not a file krowk wrote; leaving it alone."
    return 1
  fi
  if [[ -e "$path" && ! -f "$path" ]]; then
    # A FIFO, a socket, a device: not a file this installer wrote, and not one
    # a rename should quietly replace.
    note "${path} is not a regular file krowk wrote; leaving it alone."
    return 1
  fi

  dir=$(dirname "$path")
  tmp=$(mktemp "${dir}/.krowk-XXXXXX") || return 1

  cat >"$tmp" || { rm -f "$tmp"; return 1; }
  chmod 0644 "$tmp" || { rm -f "$tmp"; return 1; }
  mv -f "$tmp" "$path" || { rm -f "$tmp"; return 1; }

  # What the rename was supposed to achieve, asked of the filesystem rather
  # than assumed from an exit status: the temp file is gone because it became
  # the destination, and the destination is a regular file and not a link.
  if [[ -e "$tmp" || -L "$path" || ! -f "$path" ]]; then
    rm -f "$tmp"
    # A directory appearing at the destination between the test above and the
    # rename is the one way mv can succeed and leave the file inside it. Only
    # this call's file is cleared away — anything else in there is not this
    # call's to remove — so the next run does not refuse the whole directory
    # over krowk's own litter.
    if [[ -d "$path" && ! -L "$path" ]]; then
      rm -f "${path}/$(basename "$tmp")"
    fi
    note "${path} is not what this installer just wrote; leaving it alone."
    return 1
  fi
}

# claim_skill_dir is the gate every skill write goes through: it creates a missing directory,
# adopts an empty one, accepts one that already carries krowk's marker, and
# refuses anything else. A populated directory without the marker is somebody's
# own skill, and an installer that overwrote it would destroy work nobody asked
# it to touch — so it says why and leaves it exactly as it found it.
#
# It also adopts one shape a stricter gate would refuse: a directory holding
# nothing but a regular SKILL.md, which is the only file a pre-marker krowk
# installer ever wrote and the one it overwrote on every single run. Adopting it
# now therefore takes nothing from anybody, while anything else in the directory
# means somebody put it there and the refusal stands. This is a one-time handoff
# and it lives here, in the installer, rather than weakening the gate the rest
# of krowk writes through.
#
# The tests are on the directory itself, never through it: -L before -d, because
# a symlink here points at a directory this script never looked at, and writing
# through it would land somewhere nothing reasoned about. A directory that
# cannot be listed is refused rather than assumed empty — "I could not look" is
# not "there is nothing there". A directory somebody else owns is refused
# whatever its mode says, because its owner can empty, re-mode or replace it
# whenever they like and a marker in it would be vouching for nothing. And the
# marker has to say what krowk's markers say: this is the destructive side, and
# a name is cheap to forge.
claim_skill_dir() {
  local dir="$1" entries name marker marker_bytes

  # Two passes, like the Go gate: the first may find nothing there and try to
  # create it, and if something else won that race the second asks what
  # actually landed instead of assuming krowk made it. There is no third.
  local pass
  for pass in 1 2; do
    if [[ -L "$dir" ]]; then
      note "${dir} is a symlink, which points somewhere this installer never looked; leaving it alone."
      return 1
    fi

    if [[ ! -e "$dir" ]]; then
      # mkdir -p on the parents, plain mkdir on the directory itself: mkdir -p
      # accepts an existing directory silently, which would turn "somebody
      # else created this" into "krowk made this" — the one thing this gate
      # exists to tell apart.
      if ! mkdir -p "$(dirname "$dir")"; then
        note "Cannot create $(dirname "$dir"), so no skill was written."
        return 1
      fi
      # -m 0755 rather than the umask's idea of a directory mode, which
      # mkdir -m ignores in both directions: a permissive umask would
      # otherwise leave this world-writable, with krowk's marker vouching for
      # a directory any local user can rewrite the skill in, and a restrictive
      # one would leave it at a mode the binary half does not create. The Go
      # gate chmods to the same 0755 after its own mkdir for that reason.
      if mkdir -m 0755 "$dir" 2>/dev/null; then
        return 0
      fi
      # mkdir failed for one of two quite different reasons. If something is
      # there now, somebody else created it first and the next pass asks what
      # landed. If nothing is there, the directory simply could not be made —
      # a read-only filesystem, a full disk — and saying it "keeps changing"
      # would send the reader looking for a race that never happened.
      if [[ ! -e "$dir" ]]; then
        note "Cannot create ${dir}, so no skill was written."
        return 1
      fi
      [[ "$pass" == 1 ]] || break
      continue
    fi

    if [[ ! -d "$dir" ]]; then
      note "${dir} exists and is not a directory; leaving it alone."
      return 1
    fi

    if [[ ! -O "$dir" ]]; then
      note "${dir} belongs to another user, so krowk is not the one managing it; leaving it alone."
      return 1
    fi

    if ! entries=$(ls -A -- "$dir" 2>/dev/null); then
      note "${dir} cannot be read, so what is in it is unknown; leaving it alone."
      return 1
    fi

    if [[ -L "${dir}/${MANAGED_MARKER}" ]]; then
      # A link in the marker's name proves nothing: its target was never
      # inspected, and shape is not ownership.
      note "${dir} carries a symlink where krowk's marker should be; leaving it alone."
      return 1
    elif [[ -f "${dir}/${MANAGED_MARKER}" ]]; then
      # Before the marker is read, let alone believed: a directory anybody may
      # write to is a directory anybody may plant a marker in, and reading it
      # first would authorise the claim on evidence the reader could have
      # written. Only the group and other write bits go — how readable the
      # user wants their own directory is not this gate's business — and it is
      # best-effort, because the mode is a hardening measure rather than the
      # claim itself.
      chmod go-w "$dir" 2>/dev/null || true

      # Bounded, like every read on the Go side, and compared with the
      # surrounding whitespace stripped: an editor that added a trailing
      # newline has not changed who wrote the marker.
      # The size is measured on the file, not on the string: `$(...)` strips
      # trailing newlines, so a sentence followed by two thousand of them
      # would otherwise measure as short. One byte past the bound is read, so
      # "a sentence" is told apart from "something much larger wearing a
      # marker's name", and the larger thing is refused rather than compared
      # on its first 512 bytes — which is what the Go half does.
      marker_bytes=$(head -c 513 -- "${dir}/${MANAGED_MARKER}" | wc -c)
      if [[ "$marker_bytes" -gt 512 ]]; then
        note "${dir} carries a marker krowk did not write; leaving it alone."
        note "Move it aside and re-run this installer to have krowk manage it."
        return 1
      fi
      marker=$(head -c 512 -- "${dir}/${MANAGED_MARKER}")
      # Trimmed as a whole string, front and back, which is what Go's
      # TrimSpace does — a line-oriented strip would leave a leading newline
      # in place and call two identical markers different.
      marker="${marker#"${marker%%[![:space:]]*}"}"
      marker="${marker%"${marker##*[![:space:]]}"}"
      if [[ "$marker" != "${MANAGED_MARKER_CONTENT}" ]]; then
        note "${dir} carries a marker krowk did not write; leaving it alone."
        note "Move it aside and re-run this installer to have krowk manage it."
        return 1
      fi
      return 0
    elif [[ -n "$entries" ]]; then
      # No marker and not empty: either the one file a pre-marker installer
      # wrote, which is safe to adopt, or somebody's work, which is not.
      while IFS= read -r name; do
        [[ -n "$name" ]] || continue
        if [[ "$name" != "SKILL.md" || -L "${dir}/${name}" || ! -f "${dir}/${name}" ]]; then
          note "${dir} was not written by krowk; leaving it alone."
          note "Move it aside and re-run this installer to have krowk manage it."
          return 1
        fi
      done <<<"$entries"
      # Adopted, so it is krowk's from here — and a directory somebody created
      # under a permissive umask may be world-writable, which would let any
      # local user rewrite the skill with krowk's marker vouching for it.
      chmod go-w "$dir" 2>/dev/null || true
    else
      # Empty and unmarked: adopted for the same reason, and closed to the
      # world for the same reason.
      chmod go-w "$dir" 2>/dev/null || true
    fi

    return 0
  done

  note "${dir} keeps changing underneath this installer; leaving it alone."
  return 1
}

# install_skill is best-effort on purpose. krowk works without it; the skill only
# teaches an agent which command to reach for. So a machine with no agent config
# gets a sentence saying where the skill lives, not an error and not a directory
# created speculatively under someone's home.
install_skill() {
  local version="$1" dir url tmp

  if [[ "${KROWK_SKIP_SKILL:-}" == "1" ]]; then
    step "Skipping the agent skill (KROWK_SKIP_SKILL=1)"
    return 0
  fi

  dir=$(skills_dir)
  if [[ -z "$dir" ]]; then
    note "No agent skills directory here, so none was written."
    note "For Claude Code: mkdir -p ~/.claude/skills and re-run, or copy"
    note "https://github.com/${REPO}/blob/main/skills/krowk/SKILL.md yourself."
    return 0
  fi

  if [[ -n "$BASE_URL_OVERRIDE" ]]; then
    url="${BASE_URL_OVERRIDE%/}/SKILL.md"
  else
    # From the tag that was installed, so the skill describes the binary that is
    # on this machine rather than whatever main says today.
    url="https://raw.githubusercontent.com/${REPO}/v${version}/skills/krowk/SKILL.md"
  fi

  tmp=$(mktemp "${TMPDIR:-/tmp}/krowk-skill.XXXXXX")
  if ! curl_run -fsSL "$url" -o "$tmp"; then
    rm -f "$tmp"
    note "Could not fetch the agent skill from ${url} — krowk itself is installed and working."
    return 0
  fi

  if ! claim_skill_dir "${dir}/krowk"; then
    rm -f "$tmp"
    return 0
  fi

  # The marker goes down first, then the skill, then the version stamp. A run
  # interrupted anywhere in there leaves a directory that is still recognisably
  # krowk's, so the next run refreshes it instead of refusing it; the stamp is
  # last because a version claimed before the file it describes was written
  # would be a lie about what is on disk.
  #
  # Every one of these is best-effort, like the rest of install_skill: the
  # binaries are already in place and working, and a skill that could not be
  # written is a sentence to print, not a reason to fail an install.
  if ! printf '%s\n' "$MANAGED_MARKER_CONTENT" | write_managed_file "${dir}/krowk/${MANAGED_MARKER}"; then
    rm -f "$tmp"
    note "Could not mark ${dir}/krowk as krowk's, so no skill was written — krowk itself is installed and working."
    return 0
  fi
  if ! write_managed_file "${dir}/krowk/SKILL.md" <"$tmp"; then
    rm -f "$tmp"
    note "Could not write ${dir}/krowk/SKILL.md — krowk itself is installed and working."
    return 0
  fi
  rm -f "$tmp"
  if ! printf '%s\n' "$version" | write_managed_file "${dir}/krowk/${INSTALLED_VERSION_FILE}"; then
    note "The agent skill was written, but its version could not be stamped."
    return 0
  fi
  info "Agent skill written to ${dir}/krowk/SKILL.md"
}

next_steps() {
  echo ""
  echo "  Next:"
  if [[ "$HAS_AGENT" == yes ]]; then
    echo "    $(bold "krowk")                         Open krowk's agent in this terminal"
  fi
  echo "    $(bold "krowk push screenshot.png")     Upload without a key — the link is live, and lasts a day"
  echo "    $(bold "krowk login --token …")    Add a key, and uploads keep, group under runs and stay yours"
  echo "    $(bold "krowk help")                    Everything else — add --json for the surface as data"
  echo ""
}

usage() {
  echo "usage: install.sh [--lean | --full]    (piped: curl -fsSL https://krowk.com/install | bash -s -- --lean)"
}

main() {
  local asked=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --lean) asked=lean ;;
      --full) asked=full ;;
      -h | --help)
        usage
        return 0
        ;;
      *)
        usage >&2
        error "$1 is not an option of the installer"
        ;;
    esac
    shift
  done

  echo ""
  echo "  $(bold "krowk") — permalinks for agent output"
  echo ""

  command -v curl >/dev/null 2>&1 || error "curl is needed and is not installed"
  # Before the network, not after it: a machine with no way to check a checksum
  # should hear that instead of downloading an archive it cannot verify.
  resolve_sha256

  local platform version tmp_dir
  choose_build "$asked"
  platform=$(detect_platform)
  detect_curl_fallback

  # One directory for everything this run writes, removed on the way out. curl's
  # complaints live here too, because curl_run is called from inside command
  # substitutions and a file is the only place a reason survives one.
  tmp_dir=$(mktemp -d)
  # shellcheck disable=SC2064  # expand tmp_dir now: the trap must name this run's directory.
  trap "rm -rf '${tmp_dir}'" EXIT
  CURL_ERROR_FILE="${tmp_dir}/curl.err"
  CURL_FALLBACK_NOTED_FILE="${tmp_dir}/curl.noted"
  : >"$CURL_ERROR_FILE"

  if [[ -z "$BIN_DIR" ]]; then
    BIN_DIR=$(default_bin_dir "$platform")
  fi

  if [[ -n "$VERSION" ]]; then
    version="${VERSION#v}"
    is_semver "$version" || error "KROWK_VERSION=${VERSION} is not a version. It looks like 0.1.0, or 0.1.0-rc.1."
  elif [[ -n "$BASE_URL_OVERRIDE" ]]; then
    error "KROWK_INSTALL_BASE_URL has no release to ask for the latest version. Set KROWK_VERSION too."
  else
    version=$(latest_version)
  fi

  download_binaries "$version" "$platform" "$tmp_dir"
  setup_path
  verify_install "$platform"
  install_skill "$version"
  next_steps
}

# Sourcing this file — which scripts/install_test.sh does, to reach the helpers
# without installing anything — must not run the installer. The if-form is
# required: `[[ … ]] && main` returns 1 when sourced and trips the sourcing
# shell's set -e. So is `:-$0`: bash reading from stdin, which is exactly what
# `curl | bash` is, leaves BASH_SOURCE unset and set -u would abort on it.
if [[ "${BASH_SOURCE[0]:-$0}" == "$0" ]]; then
  main "$@"
fi
