#!/usr/bin/env bash
# R-PERF-7: the iPhone app's budgets, held in CI (ticket 127). The
# ceilings are apps/ios/budgets.json's; a measurement over one fails.
# Raising a ceiling is a commit that changes its `max`, with the new
# measurement and its date in `why`, as crates/krowk-bench/budgets.toml
# is for the CLI.
#
#   budgets.sh size <Krowk.app>        a Release app: its executable
#                                      (binaryBytes) and the whole bundle
#                                      (appKilobytes, du -sk)
#   budgets.sh memory <Tests.xcresult> the peak physical memory while
#                                      BudgetUITests scrolls 10,000 items
#                                      (memoryScrollMB)
#   budgets.sh perf <test.log> <Tests.xcresult>
#                                      the trend, failing nothing: the
#                                      launch, the frame-apply baseline,
#                                      host-to-screen latency, a
#                                      notification's cold launch and the
#                                      longest frame gap in a burst
#
# The main-thread stall gate is StallBudgetTests' own, against `stall`.
# With IOS_PERF set, each adds what it measured to that JSON file (CI's
# ios-perf.json artifact); `perf` prints the step summary's lines.
set -euo pipefail

ios=$(cd "$(dirname "$0")/.." && pwd)
budgets=$ios/budgets.json

# Adds `$1` (a jq object) to IOS_PERF, if set.
record() {
  [ -n "${IOS_PERF:-}" ] || return 0
  [ -s "$IOS_PERF" ] || echo '{}' >"$IOS_PERF"
  jq --argjson add "$1" '. + $add' "$IOS_PERF" >"$IOS_PERF.new" && mv "$IOS_PERF.new" "$IOS_PERF"
}

# Fails when `$2` (a number) is over budget `$1`'s max, saying so either way.
hold() {
  local id=$1 measured=$2 unit=$3 max
  max=$(jq -er --arg id "$id" '.[$id].max | numbers' "$budgets") \
    || { echo "::error::R-PERF-7: apps/ios/budgets.json has no numeric max for $id"; return 1; }
  if jq -en --argjson m "$measured" --argjson max "$max" '$m > $max' >/dev/null; then
    echo "::error::R-PERF-7: $id is $measured $unit, over its ceiling of $max $unit (apps/ios/budgets.json)"
    return 1
  fi
  echo "R-PERF-7: $id is $measured $unit, within its ceiling of $max $unit"
}

case ${1:-} in
  size)
    app=${2:?budgets.sh size <Krowk.app>}
    [ -f "$app/Krowk" ] || { echo "budgets.sh: no executable at $app/Krowk" >&2; exit 2; }
    binary=$(wc -c <"$app/Krowk" | tr -d ' ')
    kilobytes=$(du -sk "$app" | cut -f1)
    record "{\"binaryBytes\": $binary, \"appKilobytes\": $kilobytes}"
    status=0
    hold binaryBytes "$binary" bytes || status=1
    hold appKilobytes "$kilobytes" KB || status=1
    exit $status
    ;;
  memory)
    result=${2:?budgets.sh memory <Tests.xcresult>}
    metrics=$(xcrun xcresulttool get test-results metrics --path "$result")
    # The peak physical memory XCTMemoryMetric took in each of the scroll
    # test's iterations, in kB: the largest, in MB.
    mb=$(jq -r '
      [.[] | select((.testIdentifier // .testIdentifierURL // "" | tostring) | test("scrollingTenThousandItems"))
        | .. | objects | select(has("measurements") and ((.identifier // "") + (.displayName // "") | test("physical_peak|Peak Physical"; "i")))
        | (.unitOfMeasurement // "kB") as $unit | .measurements[] | (if type == "object" then .value // .doubleValue else . end)
        | if $unit == "kB" then . / 1024 elif $unit == "MB" then . else error("unit \($unit)") end]
      | if length == 0 then empty else max * 10 | round / 10 end' <<<"$metrics")
    if [ -z "$mb" ]; then
      echo "::error::R-PERF-7: no peak physical memory for BudgetUITests' scroll in $result"
      jq -c '.[] | {testIdentifier, metrics: [.. | objects | select(has("measurements")) | {identifier, displayName, unitOfMeasurement}]}' <<<"$metrics" | head -20 >&2 || true
      exit 1
    fi
    record "{\"memoryScrollMB\": $mb}"
    hold memoryScrollMB "$mb" MB
    ;;
  perf)
    log=${2:?budgets.sh perf <test.log> <Tests.xcresult>} result=${3:?budgets.sh perf <test.log> <Tests.xcresult>}
    launch=null
    if [ -d "$result" ]; then
      # XCTApplicationLaunchMetric's durations, in s: their median, in ms.
      launch=$(xcrun xcresulttool get test-results metrics --path "$result" | jq '
        [.[] | select((.testIdentifier // .testIdentifierURL // "" | tostring) | test("BudgetUITests.*testLaunch"))
          | .. | objects | select(has("measurements") and ((.identifier // "") + (.displayName // "") | test("AppLaunch|Launch"; "i")))
          | .measurements[] | (if type == "object" then .value // .doubleValue else . end)] | sort | if length == 0 then null else .[length / 2 | floor] * 1000 | round end' || echo null)
    fi
    lines() { { [ -f "$log" ] && grep -oE "$1" "$log" || true; } | sort -u | jq -R . | jq -s .; }
    apply=$(lines 'krowk\.frame\.apply baseline.*|Test Case .*testFrameApplyBaseline.*measured.*')
    latency=$(lines 'krowk host→screen latency.*')
    cold=$(lines 'krowk notification cold launch.*')
    # In run order, not sorted: a gate run again reports its last.
    stall=$({ [ -f "$log" ] && grep -oE 'krowk main-thread stall:.*' "$log" || true; } | jq -R . | jq -s .)
    stallMs=$(jq -r '.[-1] // "" | capture("frames (?<ms>[0-9.]+) ms") | .ms // empty' <<<"$stall" || true)
    record "$(jq -n --argjson launch "$launch" --argjson apply "$apply" --argjson latency "$latency" --argjson cold "$cold" \
      --argjson stall "$stall" --arg stallMs "${stallMs:-}" \
      '{launchMs: $launch, frameApply: $apply, hostToScreen: $latency, notificationColdLaunch: $cold,
        stall: $stall, stallMs: (if $stallMs == "" then null else ($stallMs | tonumber) end)}')"
    echo "### iPhone app budgets and trend (R-PERF-7; the simulator, not a phone)"
    echo '```'
    if [ -n "${IOS_PERF:-}" ] && [ -s "$IOS_PERF" ]; then
      jq -r '"binary: \(.binaryBytes // "—") bytes; app: \(.appKilobytes // "—") KB; memory scrolling 10k items: \(.memoryScrollMB // "—") MB (gates)",
        "launch: \(.launchMs // "—") ms; longest frame gap in a burst: \(.stallMs // "—") ms (trend)"' "$IOS_PERF"
    fi
    jq -nr --argjson a "$apply" --argjson b "$latency" --argjson c "$cold" --argjson d "$stall" '$a + $b + $c + $d | .[]'
    echo '```'
    ;;
  *)
    sed -n '2,25p' "$0" >&2
    exit 2
    ;;
esac
