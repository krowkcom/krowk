#!/usr/bin/env bash
# R-IOS-10: the iPhone app has no web view and loads no remote image. Agent
# output is hostile (a prompt injection can put links and image markdown
# in it), so nothing in apps/ios may render web content or fetch an image
# by URL: no WebKit, no WKWebView, no SFSafariViewController, no
# LinkPresentation (which fetches a page for its preview), no AsyncImage,
# no UIImage or Data read from a URL. The Swift sources are searched
# (generated bindings and build output aside); given the built app, every
# Mach-O file in it — the executable, a Debug build's `.debug.dylib`
# (Xcode's ENABLE_DEBUG_DYLIB leaves the executable a stub), any framework
# — is checked for linking WebKit, SafariServices or LinkPresentation.
#
#   apps/ios/scripts/no-web.sh [path/to/Krowk.app]
#
# Exits 1 naming what it found.
set -euo pipefail

ios=$(cd "$(dirname "$0")/.." && pwd)
pattern='import[[:space:]]+WebKit|WKWebView|WKNavigation|SFSafariViewController|import[[:space:]]+SafariServices|import[[:space:]]+LinkPresentation|LPMetadataProvider|LPLinkView|AsyncImage|UIImage\(contentsOf|UIImage\(data:[[:space:]]*(try\??[[:space:]]+)?Data\(contentsOf|Data\(contentsOf:[[:space:]]*URL\(string|NSData\(contentsOf'
found=$(grep -rnE --include='*.swift' "$pattern" "$ios" \
  --exclude-dir=build --exclude-dir=.build --exclude-dir=xtool --exclude-dir=DerivedData || true)
if [ -n "$found" ]; then
  echo "R-IOS-10: apps/ios renders web content or loads an image by URL:" >&2
  echo "$found" >&2
  exit 1
fi

if [ "$#" -gt 0 ]; then
  app=$1
  [ -d "$app" ] || { echo "no-web.sh: $app is not an app bundle" >&2; exit 2; }
  libs() {
    if command -v otool >/dev/null; then otool -L "$1" 2>/dev/null; else llvm-objdump --macho --dylibs-used "$1" 2>/dev/null; fi
  }
  checked=0
  while IFS= read -r -d '' file; do
    used=$(libs "$file") || continue
    [ -n "$used" ] || continue
    checked=$((checked + 1))
    if grep -E '/(WebKit|SafariServices|LinkPresentation)\.framework/' <<<"$used"; then
      echo "R-IOS-10: $file links WebKit, SafariServices or LinkPresentation" >&2
      exit 1
    fi
  done < <(find "$app" -type f \( -perm -u+x -o -name '*.dylib' \) -print0)
  [ "$checked" -gt 0 ] || { echo "no-web.sh: no Mach-O file in $app" >&2; exit 2; }
  echo "R-IOS-10: $checked Mach-O file(s) in $(basename "$app") link no WebKit"
fi
echo "R-IOS-10: no web view and no remote image in apps/ios"
