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
#    stand-in for a tapped alert, KROWK_NOTIFICATION_USERINFO (ticket 106).
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
stub=0 auth=0 checked=0 apns=0 tap=0
while IFS= read -r -d '' file; do
  used=$(libs "$file") || continue
  [ -n "$used" ] || continue
  checked=$((checked + 1))
  if grep -aqE 'StubOwnerCheck|-KrowkOwnerCheck' "$file"; then stub=1; echo "the owner check's stub is in $file"; fi
  if grep -aq KROWK_FAKE_APNS_TOKEN "$file"; then apns=1; echo "the stand-in APNs token is in $file"; fi
  if grep -aq KROWK_NOTIFICATION_USERINFO "$file"; then tap=1; echo "the stand-in tapped alert is in $file"; fi
  if grep -qE '/LocalAuthentication\.framework/' <<<"$used"; then auth=1; fi
done < <(find "$app" -type f \( -perm -u+x -o -name '*.dylib' \) -print0)
[ "$checked" -gt 0 ] || { echo "approvals.sh: no Mach-O file in $app" >&2; exit 2; }
[ "$auth" = 1 ] || { echo "R-IOS-11: $(basename "$app") does not link LocalAuthentication" >&2; exit 1; }
case $kind in
  release)
    [ "$stub" = 0 ] || { echo "R-IOS-11: a Release build has the owner check's stub" >&2; exit 1; }
    [ "$apns" = 0 ] || { echo "R-IOS-12: a Release build reads KROWK_FAKE_APNS_TOKEN" >&2; exit 1; }
    [ "$tap" = 0 ] || { echo "R-IOS-12: a Release build reads KROWK_NOTIFICATION_USERINFO" >&2; exit 1; }
    echo "R-IOS-11: the Release build has only the LAContext owner check" ;;
  debug)
    [ "$stub" = 1 ] || { echo "approvals.sh: a Debug build has no stub, so this check finds nothing" >&2; exit 1; }
    [ "$apns" = 1 ] || { echo "approvals.sh: a Debug build has no stand-in APNs token, so this check finds nothing" >&2; exit 1; }
    [ "$tap" = 1 ] || { echo "approvals.sh: a Debug build has no stand-in tapped alert, so this check finds nothing" >&2; exit 1; }
    echo "R-IOS-11: the Debug build has the UI tests' stub, as it may" ;;
  *) echo "approvals.sh: release or debug, not $kind" >&2; exit 2 ;;
esac
