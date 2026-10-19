#!/usr/bin/env bash
# R-IOS-14, R-CLIENT-1: `make ios-core` — the iPhone app's Rust core,
# krowk-ffi, as a static, arm64-only XCFramework (device and simulator)
# built with the release profile's size settings, wrapped in the local
# Swift package apps/ios/KrowkCore. Never part of `make check` (R-REPO-1).
#
# It builds on a Mac with Xcode, or on Linux with the omarchy-apple-dev
# toolchain (github.com/joshuaswarren/omarchy-apple-dev): any clang (the
# Swift toolchain's) and Apple's iOS SDKs, named by KROWK_IOS_SDK and
# KROWK_IOS_SIM_SDK or found in xtool's `darwin` Swift SDK. Xcode writes
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
cd "$(dirname "$0")/.."
root=$PWD
out=$root/apps/ios/KrowkCore/build
target_dir=${CARGO_TARGET_DIR:-$root/target}
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

# The first <platform>.sdk inside xtool's darwin Swift SDK bundle,
# unversioned name first (iPhoneOS.sdk sorts before iPhoneOS26.0.sdk).
bundled_sdk() {
  local base bundle
  for base in "${XDG_CONFIG_HOME:-$HOME/.config}/swiftpm/swift-sdks" "$HOME/.swiftpm/swift-sdks"; do
    bundle=$base/darwin.artifactbundle
    [ -d "$bundle" ] || continue
    { find "$bundle" -maxdepth 8 \( -name "$1.sdk" -o -name "$1[0-9]*.sdk" \) 2>/dev/null || true; } | sort | sed -n 1p
    return 0
  done
}

case $(uname -s) in
  Darwin)
    if ! command -v xcodebuild >/dev/null || ! command -v xcrun >/dev/null; then refuse "$need_sdk"; fi
    host_lib=libkrowk_ffi.dylib
    lipo=lipo
    # cc-rs finds each SDK through xcrun.
    sdk_aarch64_apple_ios=
    sdk_aarch64_apple_ios_sim=
    ;;
  Linux)
    sdk_aarch64_apple_ios=${KROWK_IOS_SDK:-$(bundled_sdk iPhoneOS)}
    sdk_aarch64_apple_ios_sim=${KROWK_IOS_SIM_SDK:-$(bundled_sdk iPhoneSimulator)}
    if [ -z "$sdk_aarch64_apple_ios" ] || [ -z "$sdk_aarch64_apple_ios_sim" ]; then refuse "$need_sdk"; fi
    for sdk in "$sdk_aarch64_apple_ios" "$sdk_aarch64_apple_ios_sim"; do
      [ -d "$sdk/usr/include" ] || refuse "$sdk is not an iOS SDK (no usr/include): $need_sdk"
    done
    command -v clang >/dev/null || refuse "no clang on PATH; with omarchy-apple-dev, run: . ~/.config/omarchy-apple-dev/env.sh"
    lipo=$(command -v llvm-lipo) || refuse "no llvm-lipo on PATH, to check the slices' architectures"
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
# codegen unit, abort on panic, stripped). The crate stays lib + cdylib;
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
    # Linux: clang cross-compiles ring's C against the SDK's headers.
    vars+=("SDKROOT=$sdk" "CC_$tv=clang")
    cflags="$cflags -isysroot $sdk"
    if command -v llvm-ar >/dev/null; then vars+=("AR_$tv=llvm-ar"); fi
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

# Exactly two slices, each a single-architecture arm64 library.
for id in ios-arm64 ios-arm64-simulator; do
  a=$fw/$id/libkrowk_ffi.a
  [ -f "$a" ] || refuse "the XCFramework has no $id/libkrowk_ffi.a"
  "$lipo" -info "$a" | grep -q 'architecture: arm64$' || refuse "$id/libkrowk_ffi.a is not arm64 only: $("$lipo" -info "$a")"
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
