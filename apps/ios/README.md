# krowk for iPhone

The native iPhone app: UIKit, Swift 6 with complete strict concurrency,
iOS 26 and up, iPhone only for now, and no third-party Swift packages
(R-IOS-13). It is outside the cargo workspace; `make check` never builds it.

```
apps/ios/
  project.yml        XcodeGen spec: generates Krowk.xcodeproj (macOS)
  Package.swift      xtool manifest over the same sources (Linux)
  xtool.yml          xtool's app settings (Linux)
  xtool-Info.plist   the Info.plist keys xtool needs as a file (Linux)
  App/               the app target: AppDelegate, SceneDelegate, Assets.xcassets
  KrowkKit/          local package: views and models, and their tests
  KrowkCore/         local package: the Rust core's XCFramework and bindings (make ios-core)
  UITests/           XCUITest target: LaunchTests
```

Both manifests are thin and point at the same sources. Add a file to `App/`
or a package and both pick it up. A new Info.plist key goes in both
`project.yml` (as `INFOPLIST_KEY_*`) and `xtool-Info.plist`; CI checks that
every key `xtool-Info.plist` sets is in Xcode's Info.plist, with the same
value.

## On a Mac: Xcode, the simulator and UI tests

Needs Xcode with the iOS 26 SDK, Rust with the `aarch64-apple-ios` and
`aarch64-apple-ios-sim` targets, and XcodeGen (`brew install xcodegen`).
CI used XcodeGen 2.46.0 (Homebrew). From the repository root:

```
make ios-core
xcodegen generate --spec apps/ios/project.yml
xcodebuild test -project apps/ios/Krowk.xcodeproj -scheme Krowk-iOS \
  -destination 'platform=iOS Simulator,id=<udid>'
```

`xcrun simctl list devices available` lists the simulators and their udids.
The scheme `Krowk-iOS` runs the UI test `LaunchTests` and KrowkKit's unit
tests. The simulator slice is arm64 only, so this needs an Apple silicon Mac.

No machine of ours runs macOS, so for now this path runs on GitHub's
`macos-26` runner: the `app` job in `.github/workflows/ios-core.yml` runs
the three commands and saves a simulator screenshot as an artifact. Ticket
72 owns the app's full CI.

## On Linux: xtool

With the [omarchy-apple-dev](https://github.com/joshuaswarren/omarchy-apple-dev)
toolchain and its `darwin` Swift SDK (an iOS SDK from Xcode), from the
repository root:

```
. ~/.config/omarchy-apple-dev/env.sh
make ios-core
cd apps/ios && xtool dev build
```

That writes `apps/ios/xtool/KrowkApp.app`, an arm64 iPhone build. From the
same directory, omarchy-apple-dev's `device-run.sh` installs it on a paired
iPhone, also over a tailnet, and `ship.sh` is how ticket 74 will sign and
upload TestFlight builds (the app icon is still empty). Neither has been run
for this app yet. There is no simulator, XcodeGen or XCUITest on
Linux. KrowkKit's tests run on the Mac path.

xtool's executable and module are both `KrowkApp`; Xcode's executable is
`Krowk`, with the module `KrowkApp`. `Package.swift` turns off the
`-all_load` that xtool's toolset adds, which would otherwise link Rust's
`compiler_builtins` from `libkrowk_ffi.a` twice.

## Why the Xcode project is generated

`Krowk.xcodeproj` is generated from `project.yml` and git ignores it. A
checked-in `.pbxproj` is a merge conflict for every two branches that add a
file. Edit `project.yml`, never the generated project, and regenerate.

## The module name

The app's module is `KrowkApp`, not `Krowk` or `krowk`. The Rust bindings'
module is `Krowk`, and on a case-insensitive disk (a Mac's default) an app
module of either name clashes with it. `project.yml` sets
`PRODUCT_MODULE_NAME: KrowkApp`; xtool's product has that name.

## The bundle id

`com.krowk.app` is a proposal. The person confirms it in ticket 109, before
ticket 74 registers it in App Store Connect. It is in `project.yml` and
`xtool.yml`. Neither sets `ITSAppUsesNonExemptEncryption`: krowk does its
own end-to-end encryption, and the export compliance answer is the person's,
in ticket 74.
