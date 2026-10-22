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
# long answer" streams one at 200 tokens a second). The fleet runs with
# --demo: its session is titled "A demo session", has a turn with a tool
# call and its result, and holds an approval. The fleet binds 127.0.0.1
# only; the simulator shares
# the Mac's loopback. Its log is appended to $KROWK_FLEET_LOG (default
# target/fake-fleet.log), and its last lines are shown if it fails to start.
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
mkfifo "$fifo/stdin" "$fifo/stdout"
fleet=''

stop() {
  { exec 3>&-; } 2>/dev/null || true
  if [ -n "$fleet" ]; then
    # A clean stop takes a moment; one that hangs is killed, with every
    # process it started (its own process group).
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      kill -0 "$fleet" 2>/dev/null || break
      sleep 1
    done
    kill -- "-$fleet" 2>/dev/null || kill "$fleet" 2>/dev/null || true
    wait "$fleet" 2>/dev/null || true
  fi
  { exec 4<&-; } 2>/dev/null || true
  rm -rf "$fifo"
}
trap stop EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Appended, so a retried run keeps every attempt's log.
echo "with-fleet: starting the fake fleet, $(date)" >>"$log"
# Its own process group (job control), so stop() can end all of it.
set -m
cargo run -q -p krowk-harness --example fake_fleet -- --demo <"$fifo/stdin" >"$fifo/stdout" 2>>"$log" &
fleet=$!
set +m
# Holding the write end open keeps the fleet running; closing it stops it.
exec 3>"$fifo/stdin"
exec 4<"$fifo/stdout"

api='' relay='' pair='' pair_url='' sessions_url='' ready=''
# Five minutes: the first start builds and seeds the stand-in registry.
while IFS= read -r -t 300 line <&4; do
  case $line in
    api=*) api=${line#api=} ;;
    relay=*) relay=${line#relay=} ;;
    pair=*) pair=${line#pair=} ;;
    pair_url=*) pair_url=${line#pair_url=} ;;
    sessions_url=*) sessions_url=${line#sessions_url=} ;;
    ready) ready=1; break ;;
  esac
done
if [ -z "$ready" ] || [ -z "$api" ]; then
  echo "with-fleet: the fake fleet did not start; the end of $log:" >&2
  tail -n 40 "$log" >&2 || true
  exit 1
fi
echo "with-fleet: api=$api relay=$relay pair=$pair pair_url=$pair_url sessions_url=$sessions_url" >&2

export TEST_RUNNER_KROWK_API_URL=$api
export TEST_RUNNER_KROWK_RELAY_URL=$relay
export TEST_RUNNER_KROWK_PAIRING_CODE=$pair
export TEST_RUNNER_KROWK_PAIR_URL=$pair_url
export TEST_RUNNER_KROWK_SESSIONS_URL=$sessions_url

status=0
# Without the fleet's pipes: a helper the command leaves running must not
# hold the fleet's stdin open.
"$@" 3>&- 4<&- || status=$?
exit "$status"
