#!/usr/bin/env bash
# R-IOS-16: the checks .github/workflows/testflight.yml runs before it builds
# anything, kept here so its pull-request dry run can run them too.
#
#   release-preflight.sh secrets   the testflight environment's four secrets
#                                  are set and shaped as Apple issues them;
#                                  names every missing one, never prints a value
#   release-preflight.sh version   the version (MARKETING_VERSION) and build
#                                  number (CURRENT_PROJECT_VERSION) of this run,
#                                  as VERSION=… and BUILD_NUMBER=… lines for
#                                  $GITHUB_ENV
#   release-preflight.sh signing-state
#                                  the runner's code-signing identities and
#                                  provisioning profiles, one per line, sorted,
#                                  so the upload's last step removes only what
#                                  its signing added (macOS only)
#
# `version` reads GitHub's GITHUB_EVENT_NAME, GITHUB_REF, GITHUB_REF_NAME,
# GITHUB_RUN_NUMBER and GITHUB_RUN_ATTEMPT, and INPUT_VERSION, the dispatch's
# input. A tag ios-v1.2.3 is version 1.2.3; a dispatch takes its input, and
# runs only from main or an ios-v* tag (the environment's own rule, ticket
# 110, says the same). The build number is <run number>.<attempt>, so every
# attempt uploads a new, higher build. Not the CLI's v* tags (R-REPO-1).
set -euo pipefail

die() {
  echo "::error::$*" >&2
  exit 1
}

semver='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'

case "${1:-}" in
  secrets)
    missing=()
    for name in APPLE_TEAM_ID ASC_KEY_ID ASC_ISSUER_ID ASC_KEY; do
      [ -n "${!name:-}" ] || missing+=("$name")
    done
    [ "${#missing[@]}" = 0 ] \
      || die "the testflight environment is missing ${missing[*]}; apps/ios/README.md → Releasing to TestFlight says how to add them"
    # Shapes only, so a value pasted into the wrong secret fails here, and
    # nothing but these characters reaches xcodebuild's arguments.
    [[ $APPLE_TEAM_ID =~ ^[A-Z0-9]{10}$ ]] || die "APPLE_TEAM_ID is not a 10-character team id"
    [[ $ASC_KEY_ID =~ ^[A-Z0-9]{10}$ ]] || die "ASC_KEY_ID is not a 10-character key id"
    [[ $ASC_ISSUER_ID =~ ^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$ ]] \
      || die "ASC_ISSUER_ID is not an issuer id (a lowercase UUID)"
    # A pattern match, not grep: a here-string can land in a temp file.
    [[ $ASC_KEY == *'-----BEGIN PRIVATE KEY-----'* ]] \
      || die "ASC_KEY is not the contents of an AuthKey_<id>.p8 file"
    echo "APPLE_TEAM_ID, ASC_KEY_ID, ASC_ISSUER_ID and ASC_KEY are set"
    ;;
  version)
    case "${GITHUB_EVENT_NAME:-}" in
      push)
        case "${GITHUB_REF:-}" in
          refs/tags/ios-v*) version=${GITHUB_REF_NAME#ios-v} ;;
          *) die "a push runs only for an ios-v<major>.<minor>.<patch> tag, not ${GITHUB_REF:-nothing}" ;;
        esac
        ;;
      workflow_dispatch)
        version=${INPUT_VERSION:-}
        case "${GITHUB_REF:-}" in
          refs/heads/main) ;;
          refs/tags/ios-v*)
            [ "$version" = "${GITHUB_REF_NAME#ios-v}" ] \
              || die "the version $version is not the tag's, ${GITHUB_REF_NAME#ios-v}"
            ;;
          *) die "run it from main or an ios-v* tag, not ${GITHUB_REF:-nothing}" ;;
        esac
        ;;
      *) die "a TestFlight build comes from an ios-v* tag or a dispatch, not ${GITHUB_EVENT_NAME:-nothing}" ;;
    esac
    [[ $version =~ $semver ]] || die "the version '$version' is not <major>.<minor>.<patch>, e.g. 0.1.0"
    [[ ${GITHUB_RUN_NUMBER:-} =~ ^[1-9][0-9]*$ && ${GITHUB_RUN_ATTEMPT:-} =~ ^[1-9][0-9]*$ ]] \
      || die "GITHUB_RUN_NUMBER and GITHUB_RUN_ATTEMPT must be set"
    echo "VERSION=$version"
    echo "BUILD_NUMBER=$GITHUB_RUN_NUMBER.$GITHUB_RUN_ATTEMPT"
    ;;
  signing-state)
    # Fails, printing nothing usable, when the keychain can't be listed.
    # Not -v: an identity signing added that isn't valid is removed too.
    ids=$(security find-identity -p codesigning)
    {
      awk '/^ *[0-9]+\) [0-9A-F]{40} / {print "identity\t" $2}' <<<"$ids"
      for dir in ~/Library/MobileDevice ~/Library/Developer/Xcode/UserData; do
        [ ! -d "$dir" ] || find "$dir" -name '*.mobileprovision' -exec printf 'profile\t%s\n' {} +
      done
    } | LC_ALL=C sort -u
    ;;
  *)
    echo "usage: release-preflight.sh secrets|version|signing-state" >&2
    exit 2
    ;;
esac
