#!/usr/bin/env bash
# R-IOS-14, R-CLIENT-1: `make ios-core` — the iPhone app's Rust core,
# krowk-ffi, as a static, arm64-only XCFramework (device and simulator)
# built with the release profile's size settings, wrapped in the local
# Swift package apps/ios/KrowkCore. Never part of `make check` (R-REPO-1).
#
# It builds on a Mac with Xcode, or on Linux with the omarchy-apple-dev
# toolchain (github.com/joshuaswarren/omarchy-apple-dev): the Swift
# toolchain's clang, llvm-ar and llvm-objdump on PATH (its env.sh puts them
# there) and Apple's iOS SDKs, named by KROWK_IOS_SDK and KROWK_IOS_SIM_SDK
# or found in xtool's `darwin` Swift SDK. Xcode writes
# the XCFramework on a Mac; on Linux this script writes the same layout
# and Info.plist itself. KROWK_IOS_ASSEMBLE=1 does that on a Mac too,
# which is how CI holds the Linux layout to Xcode (.github/workflows/ios-core.yml).
#
# What it makes, under apps/ios/KrowkCore/build/ (gitignored), replacing
# whatever an earlier run left:
#
#   KrowkFFI.xcframework/   ios-arm64 and ios-arm64-simulator, each a
#                           static libkrowk_ffi.a with KrowkFFI.h and
#                           module.modulemap
#   Sources/Krowk/Krowk.swift
set -euo pipefail
export LC_ALL=C
# An SDK path given relative to where the script was run from.
absolute() {
  case $1 in
    /* | "") echo "$1" ;;
    *) echo "$PWD/$1" ;;
  esac
}
ios_sdk=$(absolute "${KROWK_IOS_SDK:-}")
sim_sdk=$(absolute "${KROWK_IOS_SIM_SDK:-}")
cd "$(dirname "$0")/.."
root=$PWD
out=$root/apps/ios/KrowkCore/build
# The app's minimum (Package.swift's platforms). Rust's own objects keep
# rustc's default minimum, which is lower: a library may ask for less than
# the app it links into, never more. The C and assembly ring compiles get
# 26.0 through CFLAGS_<target>, which only the iOS target's cc-rs reads.
# Never IPHONEOS_DEPLOYMENT_TARGET: an environment-wide one reaches the
# host's build scripts and proc macros too, and Apple's linker then builds
# them for iOS.
min=26.0

refuse() {
  echo "ios-core: $1" >&2
  exit 1
}
need_sdk="needs Xcode on a Mac, or Linux with the omarchy-apple-dev toolchain and an iOS SDK (KROWK_IOS_SDK and KROWK_IOS_SIM_SDK, or xtool's darwin Swift SDK)"

# xtool's darwin Swift SDK bundle, the first of SwiftPM's two places.
darwin_bundle() {
  local base
  for base in "${XDG_CONFIG_HOME:-$HOME/.config}/swiftpm/swift-sdks" "$HOME/.swiftpm/swift-sdks"; do
    if [ -d "$base/darwin.artifactbundle" ]; then
      echo "$base/darwin.artifactbundle"
      return 0
    fi
  done
}
# <platform>.sdk in that bundle, or else its newest <platform><version>.sdk;
# nothing when there is neither.
bundled_sdk() {
  local found
  [ -n "$bundle" ] || return 0
  found=$({ find -H "$bundle" -maxdepth 8 -name "$1.sdk" 2>/dev/null || true; } | sort | sed -n 1p)
  if [ -z "$found" ]; then
    found=$({ find -H "$bundle" -maxdepth 8 -name "$1[0-9]*.sdk" 2>/dev/null || true; } | sort -V | tail -n 1)
  fi
  echo "$found"
}

case $(uname -s) in
  Darwin)
    # /usr/bin/xcodebuild is there with only the Command Line Tools too,
    # and fails when run; so do the SDK lookups without the iOS platform.
    if ! xcodebuild -version >/dev/null 2>&1 \
      || ! xcrun --sdk iphoneos --show-sdk-path >/dev/null 2>&1 \
      || ! xcrun --sdk iphonesimulator --show-sdk-path >/dev/null 2>&1; then
      refuse "$need_sdk"
    fi
    host_lib=libkrowk_ffi.dylib
    objdump=$(xcrun --find objdump) || refuse "Xcode has no objdump, to check the slices"
    # cc-rs finds each SDK through xcrun.
    sdk_aarch64_apple_ios=
    sdk_aarch64_apple_ios_sim=
    ;;
  Linux)
    bundle=$(darwin_bundle)
    sdk_aarch64_apple_ios=${ios_sdk:-$(bundled_sdk iPhoneOS)}
    sdk_aarch64_apple_ios_sim=${sim_sdk:-$(bundled_sdk iPhoneSimulator)}
    if [ -z "$sdk_aarch64_apple_ios" ] || [ -z "$sdk_aarch64_apple_ios_sim" ]; then refuse "$need_sdk"; fi
    # Each SDK the one its target needs, not the other swapped in.
    for pair in "iphoneos:$sdk_aarch64_apple_ios" "iphonesimulator:$sdk_aarch64_apple_ios_sim"; do
      sdk=${pair#*:}
      grep -Eq "\"CanonicalName\"[[:space:]]*:[[:space:]]*\"${pair%%:*}[0-9]" "$sdk/SDKSettings.json" 2>/dev/null \
        || refuse "$sdk is not an ${pair%%:*} SDK (its SDKSettings.json says otherwise, or is missing): $need_sdk"
    done
    for tool in clang llvm-ar llvm-objdump; do
      command -v "$tool" >/dev/null || refuse "no $tool on PATH; with omarchy-apple-dev, run: . ~/.config/omarchy-apple-dev/env.sh"
    done
    objdump=llvm-objdump
    host_lib=libkrowk_ffi.so
    ;;
  *)
    refuse "$need_sdk"
    ;;
esac

targets=(aarch64-apple-ios aarch64-apple-ios-sim)
rustup target add "${targets[@]}"

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

# Where cargo puts its builds: CARGO_TARGET_DIR, a configured target-dir,
# or target/.
target_dir=$(cargo metadata --format-version 1 --no-deps --locked | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
[ -n "$target_dir" ] || refuse "cargo metadata named no target directory"

# The bindings, in library mode from the host build: the interface is read
# out of the host's cdylib, never the iOS libraries.
cargo build -p krowk-ffi --locked --features bindgen --lib --bin uniffi-bindgen
mkdir "$stage/gen" "$stage/headers"
"$target_dir/debug/uniffi-bindgen" generate --library "$target_dir/debug/$host_lib" \
  --language swift --no-format --out-dir "$stage/gen"
# Only these two in the headers directory, the modulemap under the one name
# Xcode looks for; a KrowkFFI.modulemap beside it is "redefinition of module".
cp "$stage/gen/KrowkFFI.h" "$stage/headers/KrowkFFI.h"
cp "$stage/gen/KrowkFFI.modulemap" "$stage/headers/module.modulemap"

# One static library per target, release profile (opt-level "s", LTO, one
# codegen unit, abort on panic; its `strip` does not apply to a static
# library, so stripping is the app's link's). The crate stays lib + cdylib;
# the crate type is overridden here. Each target's variables reach only
# its own cargo: SDKROOT names one SDK, so it cannot be set for both.
for t in "${targets[@]}"; do
  tv=${t//-/_}
  case $t in
    aarch64-apple-ios) cflags="-miphoneos-version-min=$min" ;;
    aarch64-apple-ios-sim) cflags="-mios-simulator-version-min=$min" ;;
  esac
  vars=()
  sdk_var=sdk_$tv
  sdk=${!sdk_var}
  if [ -n "$sdk" ]; then
    # Linux: clang cross-compiles ring's C, and cc-rs passes it SDKROOT
    # as -isysroot. Read only for Apple targets, so the host's build
    # scripts never see it.
    vars+=("SDKROOT=$sdk" "CC_$tv=clang" "AR_$tv=llvm-ar")
  fi
  vars+=("CFLAGS_$tv=$cflags")
  env "${vars[@]}" cargo rustc -p krowk-ffi --lib --release --locked --target "$t" --crate-type staticlib
done
lib() { echo "$target_dir/$1/release/libkrowk_ffi.a"; }

# The XCFramework, arm64 only: device and simulator differ by platform,
# which is what lets two arm64 libraries share one.
fw=$stage/KrowkFFI.xcframework
if [ "$(uname -s)" = Darwin ] && [ "${KROWK_IOS_ASSEMBLE:-}" != 1 ]; then
  xcodebuild -create-xcframework \
    -library "$(lib aarch64-apple-ios)" -headers "$stage/headers" \
    -library "$(lib aarch64-apple-ios-sim)" -headers "$stage/headers" \
    -output "$fw"
else
  # What `xcodebuild -create-xcframework` writes for two static libraries
  # with headers: a directory per slice holding the library and Headers/,
  # and an Info.plist naming them.
  for slice in ios-arm64:aarch64-apple-ios ios-arm64-simulator:aarch64-apple-ios-sim; do
    id=${slice%%:*}
    mkdir -p "$fw/$id"
    cp "$(lib "${slice#*:}")" "$fw/$id/libkrowk_ffi.a"
    cp -R "$stage/headers" "$fw/$id/Headers"
  done
  library() {
    cat <<EOF
		<dict>
			<key>BinaryPath</key>
			<string>libkrowk_ffi.a</string>
			<key>HeadersPath</key>
			<string>Headers</string>
			<key>LibraryIdentifier</key>
			<string>$1</string>
			<key>LibraryPath</key>
			<string>libkrowk_ffi.a</string>
			<key>SupportedArchitectures</key>
			<array>
				<string>arm64</string>
			</array>
			<key>SupportedPlatform</key>
			<string>ios</string>
EOF
    if [ -n "${2:-}" ]; then
      printf '\t\t\t<key>SupportedPlatformVariant</key>\n\t\t\t<string>%s</string>\n' "$2"
    fi
    printf '\t\t</dict>\n'
  }
  {
    cat <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AvailableLibraries</key>
	<array>
EOF
    library ios-arm64
    library ios-arm64-simulator simulator
    cat <<'EOF'
	</array>
	<key>CFBundlePackageType</key>
	<string>XFWK</string>
	<key>XCFrameworkFormatVersion</key>
	<string>1.0</string>
</dict>
</plist>
EOF
  } > "$fw/Info.plist"
fi

# Exactly two slices, each arm64 only and built for its own platform
# (rustc marks a device object with LC_VERSION_MIN_IPHONEOS, clang with
# LC_BUILD_VERSION's ios).
for slice in ios-arm64:ios ios-arm64-simulator:iossimulator; do
  id=${slice%%:*}
  a=$fw/$id/libkrowk_ffi.a
  [ -f "$a" ] || refuse "the XCFramework has no $id/libkrowk_ffi.a"
  headers=$("$objdump" --macho --private-headers "$a")
  archs=$(printf '%s\n' "$headers" | awk '/^ *magic /{getline; print $2}' | sort -u | tr '\n' ' ')
  [ "$archs" = "ARM64 " ] || refuse "$id/libkrowk_ffi.a is not arm64 only: $archs"
  # One platform mark per member, so a member with none, or with another
  # OS's LC_VERSION_MIN_*, fails too.
  marks=$(printf '%s\n' "$headers" | awk '
    /cmd LC_VERSION_MIN_IPHONEOS$/ { print "ios"; next }
    /cmd LC_VERSION_MIN_/ { print "other"; next }
    /^ *platform / { print $2 }')
  members=$(printf '%s\n' "$headers" | { grep -c '^Mach header' || true; })
  [ "$(printf '%s\n' "$marks" | wc -l | tr -d ' ')" = "$members" ] \
    || refuse "$id/libkrowk_ffi.a has members without exactly one platform"
  platforms=$(printf '%s\n' "$marks" | sort -u | tr '\n' ' ')
  [ "$platforms" = "${slice#*:} " ] || refuse "$id/libkrowk_ffi.a is built for $platforms, not ${slice#*:}"
done
slices=$(find "$fw" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')
[ "$slices" = 2 ] || refuse "the XCFramework has $slices slices, not 2"

# Replace the last run's outputs whole, so nothing stale survives.
mkdir -p "$out"
rm -rf "$out/KrowkFFI.xcframework" "$out/Sources"
mv "$fw" "$out/KrowkFFI.xcframework"
mkdir -p "$out/Sources/Krowk"
cp "$stage/gen/Krowk.swift" "$out/Sources/Krowk/Krowk.swift"

# A line each, for the size budget a later ticket sets.
bytes() { find "$@" -type f -exec cat {} + | wc -c | tr -d ' '; }
for t in "${targets[@]}"; do
  echo "ios-core: $t libkrowk_ffi.a $(bytes "$(lib "$t")") bytes"
done
echo "ios-core: KrowkFFI.xcframework $(bytes "$out/KrowkFFI.xcframework") bytes"
echo "ios-core: wrote ${out#"$root"/}"
