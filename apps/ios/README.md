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
  KrowkKit/          local package: views and models (KrowkKit), the Keychain key
                     storage (KrowkKeys), sign-in over the bindings
                     (KrowkAccount), and KrowkKit's tests
  KrowkCore/         local package: the Rust core's XCFramework and bindings (make ios-core)
  AppTests/          unit tests hosted in the app: KeychainKeyStorageTests, SignInTests
  UITests/           XCUITest target: LaunchTests, SignInUITests
  scripts/           with-fleet.sh: runs a command against the fake fleet
```

Both manifests are thin and point at the same sources. Add a file to `App/`
or a package and both pick it up. A new Info.plist key goes in both
`project.yml` (as `INFOPLIST_KEY_*`) and `xtool-Info.plist`; CI checks that
every key `xtool-Info.plist` sets is in Xcode's Info.plist, with the same
value.

## On a Mac: Xcode, the simulator and UI tests

Needs Xcode with the iOS 26 SDK, Rust with the `aarch64-apple-ios` and
`aarch64-apple-ios-sim` targets, and XcodeGen 2.46.0. CI uses exactly
XcodeGen 2.46.0, the release's `xcodegen.zip` checked against its sha256
(`.github/workflows/ios.yml`, which fails unless this README names the
version it pins); `brew install xcodegen` gives the newest. From the repository
root:

```
make ios-core
xcodegen generate --spec apps/ios/project.yml
apps/ios/scripts/with-fleet.sh xcodebuild test -project apps/ios/Krowk.xcodeproj \
  -scheme Krowk-iOS -destination 'platform=iOS Simulator,id=<udid>'
```

`xcrun simctl list devices available` lists the simulators and their udids.
The scheme `Krowk-iOS` runs the UI tests `LaunchTests` and `SignInUITests`,
the app-hosted `KeychainKeyStorageTests` and `SignInTests` (target
`Krowk-iOSTests`), KrowkKit's unit tests and KrowkCore's `BindingsTests`.
The hosted tests are hosted because the Keychain refuses a SwiftPM test
bundle with no host app (`-34018`, a missing entitlement). They clear the
app's Keychain items before and after each test, so run them on a
simulator, never on a phone whose krowk you use: its device identity would
go. The simulator slice is arm64 only, so this needs an Apple silicon Mac.

### The fake fleet

The sign-in tests run against the fake fleet (R-IOS-15): the stand-in
registry, the relay and a host daemon on 127.0.0.1, which the simulator
shares with the Mac. `apps/ios/scripts/with-fleet.sh <command>` builds it
with cargo (and the debug `krowk --features harness` it pairs with, unless
`KROWK_BIN` names one), starts it, exports what it printed as
`TEST_RUNNER_KROWK_API_URL`, `TEST_RUNNER_KROWK_RELAY_URL` and
`TEST_RUNNER_KROWK_PAIRING_CODE`, runs the command, and stops the fleet.
xcodebuild hands each `TEST_RUNNER_<NAME>` to the tests as `<NAME>`;
without `KROWK_API_URL` the sign-in tests fail and say to use the script.
A UI test hands it on to the app in its launch environment: a DEBUG build
reads `KROWK_API_URL` (with `KROWK_FIRST_LAUNCH=1`, which makes the
launch a fresh install's, and `KROWK_APPEARANCE=dark|light` for
screenshots, which CI's simulator won't take from its own setting); a
release build ignores them all and talks to `https://api.krowk.com/v1`.

No machine of ours runs macOS, so this path runs on GitHub's `macos-26`
runner, in `.github/workflows/ios.yml`, for every pull request and push to
main that touches `apps/ios`, the crates the app links (krowk-ffi,
krowk-client, krowk-api), the fake fleet (krowk-devregistry and the
harness's fleet), the workspace's manifests, cargo config and toolchain
pin, `scripts/ios_core.sh` or the workflow itself. A CLI-only change does
not start it. Its `bindings` job builds the framework once per distinct
Rust input (`make ios-core`, both as Xcode and as Linux lay it out,
compared) and caches it; the `app` job builds the fake fleet, generates
the project, checks its build settings, runs the scheme's tests on an
iPhone simulator through `with-fleet.sh`, and keeps the UI tests'
screenshots, light and dark, as the artifact `krowk-ios-screenshots`, a
fresh launch's as `krowk-ios-screenshot` (and the `.xcresult` when a test
fails). `gh workflow run ios.yml --ref <branch>` runs it by hand. If
`app` fails to restore the framework (the cache was evicted after `key`
looked), re-run all jobs, not just the failed one.

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

## Where the keys are kept

`KrowkKeys.KeychainKeyStorage` is the bindings' `KeyStorage` (R-IOS-8): the
app makes one when its first scene connects and hands it to `Account.open`
and `SignIn.wait`. Every slot Rust writes (`device.json`, `signing.json`,
`credentials.json`, …) is a generic-password Keychain item under the service
`com.krowk.keys`, `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`, not
synchronizable, its bytes sealed (ECDH, HKDF-SHA256, AES-GCM with the slot's
name as authenticated data) to a non-exportable Secure Enclave P-256 key
kept as the item `wrapping-key`. No key is written to a file. Swift only
moves the bytes; krowk-client owns what they mean.

- **No biometry on key reads.** The enclave key's access control is
  `.privateKeyUsage` only: every registry call and the relay join read keys.
  Face ID guards approvals (ticket 91).
- **The simulator** has no Secure Enclave. Under
  `#if targetEnvironment(simulator)` only, the wrapping key is a software
  P-256 key in the same item, so the tests cover everything but the enclave.
  A device build has no fallback: with no Secure Enclave the storage throws
  and the app says it can't keep keys safely.
- **Reinstalls and restores.** Keychain items outlive the app. The first
  launch (no file `Library/Application Support/krowk.installed`) deletes
  every `com.krowk.keys` item before anything reads one. The marker is a
  file excluded from backups, not a `UserDefaults` key, which a backup
  would carry: after a restore the old items are sealed to an enclave key
  that may be gone, so a restore counts as a first launch too.
- **When it can't open.** A phone with no Secure Enclave gets `Refused`
  with the code `no_secure_enclave`, and the app says it can't keep keys
  safely. Anything else (the phone locked, a Keychain error) says the keys
  couldn't be opened and is tried again when the app comes forward.
- **Plaintext.** Swift keeps no copy of its own: the bytes to seal arrive
  in UniFFI's `Data` and the opened bytes leave in CryptoKit's, which
  UniFFI copies and frees. Neither can be wiped from Swift (Canon
  `crypto.md` → Zeroization). Logs name a slot and an `OSStatus`, never
  bytes.
- **Signing.** No `kSecAttrAccessGroup` and no `keychain-access-groups`
  entitlement: the items are in the app's default group, its application
  identifier, which both Xcode's and xtool's signing (a free Apple ID's
  development profile too) give the app. Changing the team or the bundle
  id changes that group, and the app no longer sees its earlier items. The
  Notification Service Extension that would share them is M2.

The Secure Enclave itself only runs on a real iPhone (ticket 107).

## Signing in

A phone with no krowk key opens on onboarding's "Sign in to krowk"
(R-IOS-9), a SwiftUI screen over `KrowkKit.SignInModel`. Its one button
calls krowk-ffi's `SignIn.start` with the API URL and the device's name
(`UIDevice.current.name`, control characters dropped, cut to 64), off the
main actor, and opens the approval page in an `ASWebAuthenticationSession`
that is not ephemeral, so the browser's registry login is used. Only https
opens, or http when the API itself is http, as the terminal's
`browsable_url`. The page never redirects back: `SignIn.wait`, Rust's poll
with the terminal's timing, finishes the sign-in, writes the key into the
Keychain item `credentials.json` through `KeychainKeyStorage`, and hands
Swift the workspace only, never the key; the sheet is then closed. A sheet
closed early leaves the poll running, with "Open the page again" and
"Cancel". A denial, a lapse after 15 minutes, or a Keychain that won't
keep the key ends in one sentence and "Start again". At launch,
`KrowkAccount.Launch` reads the storage through `Account.open`: no key is
onboarding, a key without pairing is "Signed in to <workspace>" (until
ticket 84), a paired phone never sees onboarding.

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
