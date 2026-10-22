#!/usr/bin/env bash
# R-IOS-10: the iPhone app has no web view and loads no remote image. Agent
# output is hostile (a prompt injection can put links and image markdown
# in it), so nothing in apps/ios may render web content or fetch an image
# by URL: no WebKit, no WKWebView, no SFSafariViewController, no
# AsyncImage, no UIImage or Data read from a URL. The Swift sources are
# searched (generated bindings and build output aside); given the built
# app's executable, its linked libraries are checked for WebKit too.
#
#   apps/ios/scripts/no-web.sh [path/to/Krowk.app/Krowk]
#
# Exits 1 naming what it found.
set -euo pipefail

ios=$(cd "$(dirname "$0")/.." && pwd)
pattern='import[[:space:]]+WebKit|WKWebView|WKNavigation|SFSafariViewController|import[[:space:]]+SafariServices|AsyncImage|UIImage\(contentsOf|UIImage\(data:[[:space:]]*(try\??[[:space:]]+)?Data\(contentsOf|Data\(contentsOf:[[:space:]]*URL\(string|NSData\(contentsOf'
found=$(grep -rnE --include='*.swift' "$pattern" "$ios" \
  --exclude-dir=build --exclude-dir=.build --exclude-dir=xtool --exclude-dir=DerivedData || true)
if [ -n "$found" ]; then
  echo "R-IOS-10: apps/ios renders web content or loads an image by URL:" >&2
  echo "$found" >&2
  exit 1
fi

if [ "$#" -gt 0 ]; then
  bin=$1
  if command -v otool >/dev/null; then
    libs=$(otool -L "$bin")
  else
    libs=$(llvm-objdump --macho --dylibs-used "$bin")
  fi
  if grep -E 'WebKit|SafariServices' <<<"$libs"; then
    echo "R-IOS-10: $bin links WebKit or SafariServices" >&2
    exit 1
  fi
  echo "R-IOS-10: $(basename "$bin") links no WebKit"
fi
echo "R-IOS-10: no web view and no remote image in apps/ios"
