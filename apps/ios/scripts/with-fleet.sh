#!/usr/bin/env bash
# Runs a command (the iOS tests) against the fake fleet (R-IOS-15): builds
# and starts it with cargo from the repository root, keeping a pipe to its
# stdin, exports what it printed for the test process, runs the command,
# then closes the fleet's stdin, which stops it. Exits with the command's
# status.
#
#   apps/ios/scripts/with-fleet.sh xcodebuild test -project apps/ios/Krowk.xcodeproj \
#     -scheme Krowk-iOS -destination 'platform=iOS Simulator,id=<udid>'
#
# xcodebuild hands every TEST_RUNNER_<NAME> variable to the test process as
# <NAME>, so the tests read KROWK_API_URL, KROWK_RELAY_URL,
# KROWK_PAIRING_CODE, KROWK_PAIR_URL and KROWK_SESSIONS_URL: a POST to the
# pair URL gives a fresh code (the old one's pairing ended), since a code
# works once and the simulator can't write to the fleet's stdin; one to the
# sessions URL hosts a session of the test's own, which a POST to
# <url>/<id>/release lets go (its lease lapses) and <url>/<id>/host hosts
# again (the host coming back, R-HAND-4), and a POST to
# <url>/<id>/turn runs a turn in (its body the prompt; "Please stream a
# long answer" streams one at 200 tokens a second, far slower on a busy
# runner), and <url>/<id>/turn-ended answers `ended` once that turn has
# ended at the host, or `running` if it still runs 5 s later. Host A
# announces itself with one trusted repository and the mock model ready
# (R-HOST-3), and starts a session a phone asks for there (R-HOST-4); a
# POST to KROWK_HOSTS_URL/offline lets its announcement lapse, replying
# once it is listed offline, KROWK_HOSTS_URL/hold holds its refreshes so a
# start waits, and KROWK_HOSTS_URL/online brings it back. The fleet
# runs with --demo: its session is titled "A demo session", has a turn
# with a tool call and its result, and holds an approval. It runs with
# --direct-lan too: each session's host offers its LAN listener on
# 127.0.0.1, which the phone races and moves onto (R-NET-2), and a POST
# to <url>/<id>/direct/close closes that listener, the LAN path lost. The fleet
# binds 127.0.0.1 only; the simulator shares the Mac's loopback. Its log
# is appended to $KROWK_FLEET_LOG (default target/fake-fleet.log), and its
# last lines are shown if it fails to start.
#
# A second fleet, with no demo, runs beside it for the tests that remove
# their phone from host A's devices (ticket 117): removing a device rotates
# host A's user key, and the first fleet's hosts would stop on it. Its URLs
# go to the tests as KROWK_REMOVAL_API_URL, KROWK_REMOVAL_RELAY_URL,
# KROWK_REMOVAL_PAIR_URL and KROWK_REMOVAL_DEVICES_URL: a POST to
# <devices url>/remove, its body a device's name, runs `krowk devices
# remove` as that fleet's host A.
#
# The fleet answers pairings with a debug krowk built with the harness
# feature: KROWK_BIN names one, else this builds it.
set -euo pipefail

root=$(cd "$(dirname "$0")/../../.." && pwd)
cd "$root"
[ "$#" -gt 0 ] || { echo "usage: $0 <command> [args…]" >&2; exit 2; }

target=${CARGO_TARGET_DIR:-$root/target}
log=${KROWK_FLEET_LOG:-$target/fake-fleet.log}
mkdir -p "$(dirname "$log")"
if [ -z "${KROWK_BIN:-}" ]; then
  cargo build -q -p krowk --features harness
  export KROWK_BIN=$target/debug/krowk
fi
# Built first, so reading its lines below waits for the fleet, not cargo.
cargo build -q -p krowk-harness --example fake_fleet

fifo=$(mktemp -d "${TMPDIR:-/tmp}/krowk-fleet.XXXXXX")
mkfifo "$fifo/stdin" "$fifo/stdout" "$fifo/stdin2" "$fifo/stdout2"
fleet='' fleet2=''

# Waits up to ten seconds for a fleet to stop by itself (its stdin closed),
# then kills it with every process it started (its own process group).
end_fleet() {
  local pid=$1
  [ -n "$pid" ] || return 0
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    kill -0 "$pid" 2>/dev/null || break
    sleep 1
  done
  kill -- "-$pid" 2>/dev/null || kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

stop() {
  { exec 3>&-; } 2>/dev/null || true
  { exec 5>&-; } 2>/dev/null || true
  end_fleet "$fleet"
  end_fleet "$fleet2"
  { exec 4<&-; } 2>/dev/null || true
  { exec 6<&-; } 2>/dev/null || true
  rm -rf "$fifo"
}
trap stop EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Appended, so a retried run keeps every attempt's log.
echo "with-fleet: starting the fake fleets, $(date)" >>"$log"
# Each in its own process group (job control), so stop() can end all of it.
# The second, with no demo, is the removal tests' own: removing a phone
# rotates host A's user key, which would stop the first fleet's hosts.
set -m
cargo run -q -p krowk-harness --example fake_fleet -- --demo --direct-lan <"$fifo/stdin" >"$fifo/stdout" 2>>"$log" &
fleet=$!
cargo run -q -p krowk-harness --example fake_fleet -- --pair <"$fifo/stdin2" >"$fifo/stdout2" 2>>"$log" &
fleet2=$!
set +m
# Holding the write ends open keeps the fleets running; closing them stops them.
exec 3>"$fifo/stdin"
exec 4<"$fifo/stdout"
exec 5>"$fifo/stdin2"
exec 6<"$fifo/stdout2"

# Reads a fleet's lines from fd $1 up to `ready` into fleet_<key> variables
# with the prefix $2. Five minutes: the first start builds and seeds the
# stand-in registry.
read_fleet() {
  local fd=$1 prefix=$2 line ready=''
  while IFS= read -r -t 300 line <&"$fd"; do
    case $line in
      ready) ready=1; break ;;
      *=*) printf -v "${prefix}_${line%%=*}" '%s' "${line#*=}" ;;
    esac
  done
  [ -n "$ready" ]
}

fleet_api='' fleet_relay='' fleet_pair='' fleet_pair_url='' fleet_sessions_url='' fleet_devices_url='' fleet_hosts_url=''
removal_api='' removal_relay='' removal_pair_url='' removal_devices_url=''
if ! read_fleet 4 fleet || [ -z "$fleet_api" ] || ! read_fleet 6 removal || [ -z "$removal_api" ]; then
  echo "with-fleet: a fake fleet did not start; the end of $log:" >&2
  tail -n 40 "$log" >&2 || true
  exit 1
fi
echo "with-fleet: api=$fleet_api relay=$fleet_relay pair=$fleet_pair pair_url=$fleet_pair_url sessions_url=$fleet_sessions_url hosts_url=$fleet_hosts_url" >&2
echo "with-fleet: removal api=$removal_api relay=$removal_relay pair_url=$removal_pair_url devices_url=$removal_devices_url" >&2

export TEST_RUNNER_KROWK_API_URL=$fleet_api
export TEST_RUNNER_KROWK_RELAY_URL=$fleet_relay
export TEST_RUNNER_KROWK_PAIRING_CODE=$fleet_pair
export TEST_RUNNER_KROWK_PAIR_URL=$fleet_pair_url
export TEST_RUNNER_KROWK_SESSIONS_URL=$fleet_sessions_url
export TEST_RUNNER_KROWK_HOSTS_URL=$fleet_hosts_url
export TEST_RUNNER_KROWK_REMOVAL_API_URL=$removal_api
export TEST_RUNNER_KROWK_REMOVAL_RELAY_URL=$removal_relay
export TEST_RUNNER_KROWK_REMOVAL_PAIR_URL=$removal_pair_url
export TEST_RUNNER_KROWK_REMOVAL_DEVICES_URL=$removal_devices_url

status=0
# Without the fleets' pipes: a helper the command leaves running must not
# hold a fleet's stdin open.
"$@" 3>&- 4<&- 5>&- 6<&- || status=$?
exit "$status"
