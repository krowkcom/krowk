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
# <NAME>, so the tests read KROWK_API_URL, KROWK_RELAY_URL and
# KROWK_PAIRING_CODE. The fleet binds 127.0.0.1 only; the simulator shares
# the Mac's loopback. Its log goes to $KROWK_FLEET_LOG (default
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
cargo run -q -p krowk-harness --example fake_fleet -- --pair <"$fifo/stdin" >"$fifo/stdout" 2>"$log" &
fleet=$!
# Holding the write end open keeps the fleet running; closing it stops it.
exec 3>"$fifo/stdin"
exec 4<"$fifo/stdout"

stop() {
  exec 3>&- 2>/dev/null || true
  # A clean stop takes a moment; one that hangs is killed.
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    kill -0 "$fleet" 2>/dev/null || break
    sleep 1
  done
  kill "$fleet" 2>/dev/null || true
  wait "$fleet" 2>/dev/null || true
  exec 4<&- 2>/dev/null || true
  rm -rf "$fifo"
}
trap stop EXIT

api='' relay='' pair='' ready=''
# Five minutes: the first start builds and seeds the stand-in registry.
while IFS= read -r -t 300 line <&4; do
  case $line in
    api=*) api=${line#api=} ;;
    relay=*) relay=${line#relay=} ;;
    pair=*) pair=${line#pair=} ;;
    ready) ready=1; break ;;
  esac
done
if [ -z "$ready" ] || [ -z "$api" ]; then
  echo "with-fleet: the fake fleet did not start; the end of $log:" >&2
  tail -n 40 "$log" >&2 || true
  exit 1
fi
echo "with-fleet: api=$api relay=$relay pair=$pair" >&2

export TEST_RUNNER_KROWK_API_URL=$api
export TEST_RUNNER_KROWK_RELAY_URL=$relay
export TEST_RUNNER_KROWK_PAIRING_CODE=$pair

status=0
"$@" || status=$?
exit "$status"
