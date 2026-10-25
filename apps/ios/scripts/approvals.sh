#!/usr/bin/env bash
# R-IOS-11: what the phone may answer an approval with, and who must be
# holding it.
#
# 1. The sources never name allowProject (in any spelling) outside a
#    comment: a viewer allows a call once or for the session, and the host
#    refuses a rule for the project from one, so the app has no path that
#    could send it. The generated bindings (KrowkCore/build) and build
#    output aside.
# 2. Given a built app: a Release build compiles only the LAContext owner
#    check. The UI tests' stub and the launch argument that picks it are
#    under #if DEBUG, so neither may be in any Mach-O file of a Release
#    app, which must link LocalAuthentication. A Debug app must have both
#    (the check finds what it looks for). The same for the UI tests'
#    stand-in APNs token, KROWK_FAKE_APNS_TOKEN (R-IOS-12), and their
#    stand-in for a tapped alert, KROWK_NOTIFICATION_USERINFO (ticket 106),
#    and the budgets' seams (ticket 127, R-PERF-7): the deliberate stall,
#    KROWK_TEST_STALL_MS, the 10,000-item fixture, KROWK_FIXTURE, and the
#    stall test's breakdown of the main thread's time, FrameCosts (ticket
#    140), found by the name of the piece a whole redraw records. The
#    phone's own budgets (ticket 128, R-IOS-PERF) are the other way about:
#    their signposts must be in every build, Release included.
#
#   apps/ios/scripts/approvals.sh [path/to/Krowk.app release|debug]
#
# Exits 1 naming what it found.
set -euo pipefail

ios=$(cd "$(dirname "$0")/.." && pwd)
found=$(grep -rniE --include='*.swift' 'allow_?project' "$ios" \
  --exclude-dir=build --exclude-dir=.build --exclude-dir=xtool --exclude-dir=DerivedData \
  | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' || true)
if [ -n "$found" ]; then
  echo "R-IOS-11: apps/ios names allowProject outside a comment:" >&2
  echo "$found" >&2
  exit 1
fi
echo "R-IOS-11: no allowProject in apps/ios's sources"

[ "$#" -gt 0 ] || exit 0
app=$1 kind=${2:-release}
[ -d "$app" ] || { echo "approvals.sh: $app is not an app bundle" >&2; exit 2; }
libs() {
  if command -v otool >/dev/null; then otool -L "$1" 2>/dev/null; else llvm-objdump --macho --dylibs-used "$1" 2>/dev/null; fi
}
stub=0 auth=0 checked=0 apns=0 tap=0 stall=0 fixture=0 fixtureID=0 scroller=0 costs=0 budgets=0
while IFS= read -r -d '' file; do
  used=$(libs "$file") || continue
  [ -n "$used" ] || continue
  checked=$((checked + 1))
  if grep -aqE 'StubOwnerCheck|-KrowkOwnerCheck' "$file"; then stub=1; echo "the owner check's stub is in $file"; fi
  if grep -aq KROWK_FAKE_APNS_TOKEN "$file"; then apns=1; echo "the stand-in APNs token is in $file"; fi
  if grep -aq KROWK_NOTIFICATION_USERINFO "$file"; then tap=1; echo "the stand-in tapped alert is in $file"; fi
  if grep -aq KROWK_TEST_STALL_MS "$file"; then stall=1; echo "the deliberate stall is in $file"; fi
  if grep -aq 'text drawn whole' "$file"; then costs=1; echo "the stall test's FrameCosts is in $file"; fi
  # KROWK_FIXTURE is short enough for an optimised build to keep inline,
  # not as text: the fixture's session id and its scroller's class name
  # (KrowkKit's and the app's) are what a leak would leave.
  if grep -aqE 'KROWK_FIXTURE|0192f0a0-0000-7000-8000-00000000127f|FixtureScroller' "$file"; then fixture=1; echo "the 10,000-item fixture is in $file"; fi
  if grep -aq 0192f0a0-0000-7000-8000-00000000127f "$file"; then fixtureID=1; fi
  if grep -aq FixtureScroller "$file"; then scroller=1; fi
  # Two of the four intervals' names: the others are short enough to be
  # kept inline.
  if grep -aq launch.cachedList "$file" && grep -aq foreground.caughtUp "$file"; then budgets=1; fi
  if grep -qE '/LocalAuthentication\.framework/' <<<"$used"; then auth=1; fi
done < <(find "$app" -type f \( -perm -u+x -o -name '*.dylib' \) -print0)
[ "$checked" -gt 0 ] || { echo "approvals.sh: no Mach-O file in $app" >&2; exit 2; }
[ "$auth" = 1 ] || { echo "R-IOS-11: $(basename "$app") does not link LocalAuthentication" >&2; exit 1; }
[ "$budgets" = 1 ] || { echo "R-IOS-PERF: $(basename "$app") ($kind) does not measure its budgets (no launch.cachedList and foreground.caughtUp signposts)" >&2; exit 1; }
echo "R-IOS-PERF: the $kind build measures its budgets"
case $kind in
  release)
    [ "$stub" = 0 ] || { echo "R-IOS-11: a Release build has the owner check's stub" >&2; exit 1; }
    [ "$apns" = 0 ] || { echo "R-IOS-12: a Release build reads KROWK_FAKE_APNS_TOKEN" >&2; exit 1; }
    [ "$tap" = 0 ] || { echo "R-IOS-12: a Release build reads KROWK_NOTIFICATION_USERINFO" >&2; exit 1; }
    [ "$stall" = 0 ] || { echo "R-PERF-7: a Release build reads KROWK_TEST_STALL_MS" >&2; exit 1; }
    [ "$fixture" = 0 ] || { echo "R-PERF-7: a Release build reads KROWK_FIXTURE" >&2; exit 1; }
    [ "$costs" = 0 ] || { echo "R-PERF-7: a Release build has the stall test's FrameCosts" >&2; exit 1; }
    echo "R-IOS-11: the Release build has only the LAContext owner check"
    echo "R-PERF-7: the Release build has neither the deliberate stall, the 10,000-item fixture nor FrameCosts" ;;
  debug)
    [ "$stub" = 1 ] || { echo "approvals.sh: a Debug build has no stub, so this check finds nothing" >&2; exit 1; }
    [ "$apns" = 1 ] || { echo "approvals.sh: a Debug build has no stand-in APNs token, so this check finds nothing" >&2; exit 1; }
    [ "$tap" = 1 ] || { echo "approvals.sh: a Debug build has no stand-in tapped alert, so this check finds nothing" >&2; exit 1; }
    [ "$stall" = 1 ] || { echo "approvals.sh: a Debug build has no deliberate stall, so this check finds nothing" >&2; exit 1; }
    [ "$costs" = 1 ] || { echo "approvals.sh: a Debug build has no FrameCosts, so this check finds nothing" >&2; exit 1; }
    [ "$fixtureID" = 1 ] && [ "$scroller" = 1 ] || { echo "approvals.sh: a Debug build has no 10,000-item fixture, so this check finds nothing" >&2; exit 1; }
    echo "R-IOS-11: the Debug build has the UI tests' stub, as it may" ;;
  *) echo "approvals.sh: release or debug, not $kind" >&2; exit 2 ;;
esac
