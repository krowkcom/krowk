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
  Krowk-Info.plist   keys Xcode has no INFOPLIST_KEY_* setting for, merged into its Info.plist
  App/               the app target: AppDelegate, SceneDelegate, Assets.xcassets
  KrowkKit/          local package: views and models (KrowkKit), the Keychain key
                     storage (KrowkKeys), sign-in, pairing, the session
                     list, the relay connection and the transcript's
                     events over the bindings (KrowkAccount), and
                     KrowkKit's tests
  KrowkCore/         local package: the Rust core's XCFramework and bindings (make ios-core)
  AppTests/          unit tests hosted in the app: KeychainKeyStorageTests, SignInTests,
                     PairingTests, SessionListTests, RelayConnectionTests,
                     TranscriptModelTests, TranscriptTests, LiveFollowTests,
                     and the TestFleet fixture
  UITests/           XCUITest target: LaunchTests, SignInUITests, PairingUITests
  scripts/           with-fleet.sh: runs a command against the fake fleet;
                     no-web.sh: no web view, no remote image (R-IOS-10)
```

Both manifests are thin and point at the same sources. Add a file to `App/`
or a package and both pick it up. A new Info.plist key goes in both
`project.yml` (as `INFOPLIST_KEY_*`) and `xtool-Info.plist` (a key with no `INFOPLIST_KEY_*` setting, such as
`CADisableMinimumFrameDurationOnPhone`, which lets the transcript draw at
120 Hz on ProMotion, goes in `Krowk-Info.plist`, which Xcode merges); CI
checks that every key `xtool-Info.plist` sets is in Xcode's Info.plist,
with the same value.

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
The scheme `Krowk-iOS` runs the UI tests `LaunchTests`, `SignInUITests` and
`PairingUITests`, the app-hosted `KeychainKeyStorageTests`, `SignInTests`,
`PairingTests`, `SessionListTests`, `RelayConnectionTests`,
`TranscriptModelTests`, `TranscriptTests` and `LiveFollowTests` (target
`Krowk-iOSTests`), KrowkKit's unit tests (`FrameApplyBaselineTests`
prints the per-frame apply baseline, which CI writes to its run's
summary) and KrowkCore's `BindingsTests`.
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
`TEST_RUNNER_KROWK_API_URL`, `TEST_RUNNER_KROWK_RELAY_URL`,
`TEST_RUNNER_KROWK_PAIRING_CODE`, `TEST_RUNNER_KROWK_PAIR_URL` and
`TEST_RUNNER_KROWK_SESSIONS_URL`, runs the command, and stops the fleet. It
runs with `--demo`, so its session is titled "A demo session" and holds an
approval. A code works once, so a test that joins
POSTs to `KROWK_PAIR_URL` for a fresh one: the fleet ends its running
`krowk devices add` (host A's, the real CLI, which says yes by itself) and
answers the next one's code. A test that needs a lease to lapse POSTs to
`KROWK_SESSIONS_URL` (its body the title): host A hosts a new session of
the test's own and answers with its id; a POST to `<url>/<id>/release`
stops that session's host and answers once its lease is gone, leaving the
fleet's own session hosted for every other test. A POST to
`<url>/<id>/turn` types a prompt (its body) at host A in that session and
answers `started`: "Please stream a long answer" has the model make a
`read` call, then stream 120 numbered lines at 200 tokens a second, the
turn `LiveFollowTests` and the UI test follow live; "Please stream a very
long answer" streams 300 lines, which `LiveFollowTests` holds to keeping
up.
xcodebuild hands each `TEST_RUNNER_<NAME>` to the tests as `<NAME>`;
without `KROWK_API_URL` the sign-in tests fail and say to use the script.
A UI test hands it on to the app in its launch environment: a DEBUG build
reads `KROWK_API_URL` (with `KROWK_FIRST_LAUNCH=1`, which makes the
launch a fresh install's, `KROWK_DEVICE_NAME`, a name of the test's own,
since host A refuses one that reads like a device already on its list, and
`KROWK_APPEARANCE=dark|light` for
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
iPhone, also over a tailnet (not run for this app yet), and `ship.sh` signs
and uploads a TestFlight build (Releasing to TestFlight, below). There is no
simulator, XcodeGen or XCUITest on Linux. KrowkKit's tests run on the Mac
path.

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
(`UIDevice.current.name`, the generic "iPhone" without a special
entitlement; control and format characters dropped, cut to 64 bytes), off
the main actor, and opens the approval page in an
`ASWebAuthenticationSession` that is not ephemeral, so the browser's
registry login is used. Only https opens, or http when the API itself is
http, as the terminal's `browsable_url`; Swift also refuses a page with a
user or password before its host, and an http page anywhere but the API's
own host and port. The page never redirects back: `SignIn.wait`, Rust's poll
with the terminal's timing, finishes the sign-in, writes the key into the
Keychain item `credentials.json` through `KeychainKeyStorage`, and hands
Swift the workspace only, never the key; the sheet is then closed. A sheet
closed early leaves the poll running, with "Open the page again" and
"Cancel". A denial, a lapse after 15 minutes, or a Keychain that won't
keep the key ends in one sentence and "Start again". At launch,
`KrowkAccount.Launch` reads the storage through `Account.open`: no key is
onboarding, a key without the user keys is pairing, a paired phone never
sees onboarding.

## Pairing

A signed-in phone holds no user key, so it can't open a synced session
until it joins the person's devices (R-E2E-3). The pairing screen,
SwiftUI over `KrowkKit.PairingModel`, says: on your laptop run `krowk
devices add`, then scan its QR code or type the code.

- **Typed.** The field upper-cases as typed; krowk-ffi's
  `parsePairingCode` (krowk-client's parser: case, spaces and dashes
  ignored) decides whether it is a code, never Swift. As soon as the field
  holds a whole code the pairing starts and the field is cleared; "Pair"
  on anything else says it isn't a code, before any network call.
- **Scanned.** "Scan the QR code" opens VisionKit's
  `DataScannerViewController`, QR codes only, after the camera permission
  (a refusal says so and offers Settings). Each payload goes through
  `parsePairingInvite`, krowk-client's strict parser: anything that isn't
  krowk's invite is ignored and the scanner keeps looking. `joinInvite`
  checks the account, then the open pairing, before the code is spent:
  `invite_other_account` says "This code is for another krowk account.",
  whatever pairing is open; `invite_other_pairing`, "This code is from a
  different device's pairing". The screen names the laptop only when the
  invite's device is the open pairing's. A camera that is restricted, or
  won't start, says to type the code. The button is absent where
  `DataScannerViewController.isSupported` is false: the simulator, so the
  tests drive the payload → join path through `PairingModel.scanned`.
- **Joining.** Off the main actor: `Account.openPairing` names the device
  showing the code ("Pairing with <name>"; nothing is sent to the pairing),
  then `joinPairing`/`joinInvite` runs krowk-client's joining side, the
  terminal's `krowk sync join`: SPAKE2, the payload check, the user keys and
  the device list kept through `KeychainKeyStorage`, the ack, and the claim
  polled every 500 ms for up to 60 s. The new device record carries the
  phone's name and `iOS <version>`. The laptop asks whether to add the
  phone; once it says yes, "This iPhone is on your devices", and Continue
  opens Sessions. From then on a launch
  skips onboarding. The phone is a viewer: it takes no lease and never
  hosts (R-HAND-4).
- **A code works once.** Nothing retries a join: a wrong or spent code
  ends the pairing on both sides, keeps nothing, and says "The pairing
  failed, and a code works once — on '<laptop>' run `krowk devices add`
  again for a new code. Nothing was kept." No open pairing says to run
  `krowk devices add` first. A failure before the code was sent (the
  registry unreachable, a Keychain that won't open) says the code wasn't
  used. No code is ever logged.
- **Cancelling.** Cancel, or the screen going away, ends the join. Once its
  first message is out it ends the pairing on the registry too, so the
  laptop isn't left waiting; before that the code is unspent and the
  laptop's pairing stays open for it. Once the laptop's sealed reply is
  in, it lists the phone, so the join finishes instead and the screen
  follows the keys. A phone the person removed from their devices is told
  so; resetting it is not built yet.

The hosted tests' `TestFleet` (an actor) signs a phone in through the
fleet and joins it once per test process, over a `KeychainKeyStorage` of its
own service (`com.krowk.keys.testfleet.…`, purged as the process exits,
and what a killed process left, as the next one starts):
`try await TestFleet.shared.paired()` is a paired account for the tests
that need one.

## Sessions

A paired phone opens on "Sessions" (R-IOS-3, R-IOS-7), a UIKit
`UICollectionView` with a plain list layout and a diffable data source
keyed by session id, over `KrowkKit.SessionListModel` (`@MainActor`,
`@Observable`):

- **The listing.** `KrowkAccount.RegistrySessions` calls krowk-ffi's
  `Account.listSessions` on a thread of its own (it holds
  `dispatchPrecondition(condition: .notOnQueue(.main))`): one registry call
  per page of 100 and one per session, each record checked against the
  pinned device list and its sealed index unsealed. No transcript is read.
  Rows read the generated `SessionSummary` through the `SessionRecord`
  protocol, with no Swift copy of the index (R-PROTO-2).
- **A row.** The title, or the cwd's last component when it has none;
  then the cwd's last component and `● live on <name>` (the holder's name
  from the verified device list, `Account.devices`; nothing more without
  one, never a raw id) or `last seen 12m ago` from the index's `updatedMs`
  (`RelativeDateTimeFormatter`, `.abbreviated`, ticked each minute while
  on screen). Most recently written first; a duplicate id is dropped.
- **Live is the lease.** The registry names a holder only while its lease
  is unexpired, which a host renews every 20 s on a 60 s TTL. So the list
  is read every 20 s while on screen, on every return to the foreground,
  and on pull-to-refresh. A refresh that fails keeps the rows but shows
  none live, and says so; a listing older than 60 s (the app was away)
  shows none live until the next read lands.
- **Unreadable sessions** (sealed under a user key the phone doesn't hold,
  or signed by a device not on its list) are counted in the footer, "2
  sessions can't be opened on this iPhone.", never shown.
- **Throttled.** Rebuilds of the screen are coalesced to at most 10 a
  second, and one that changes nothing is not applied. The first listing
  is applied without animation, later ones in 150 ms that never hold up a
  touch.
- A tap opens the session's transcript (below), followed live through the
  relay while it's open. The list never offers to take or
  host a session (R-HAND-4).

## Following a session live

`KrowkAccount.RelayConnection` keeps one WebSocket to the relay open for a
session and feeds every message through krowk-ffi's `Viewer` (R-CLIENT-1).
Swift owns the socket and the timers, Rust decides everything else: the
join and its signature (`X-Krowk-Ticket` header, the relay's challenge,
the signed `join`), the 10 s heartbeat (`Ping`, a text `ping`), a link
silent for 30 s being dead, the acks inside the relay's window, and when
to join again and from which cursor (1 s after a drop or a refusal; on a
host's presence notice). Swift never builds or parses those messages.

- **The socket** is Network.framework only: an `NWConnection` to the join
  URL with `NWProtocolWebSocket` (pings auto-answered, messages up to the
  relay's 4 MiB batch and its header) over `NWParameters.tls` for `wss`,
  the system's trust and nothing weakened, or `.tcp` for the fake fleet's
  `ws://` on 127.0.0.1 (DEBUG only, and only to this machine: any other
  `ws://` stops the viewer rather than send a ticket in the clear).
  Network.framework's WebSocket sends a `Host` header without the port,
  so a relay must know its origin rather than read it from `Host`: the
  hosted one does, `krowk relay serve` does with `--origin` (which it
  requires off loopback), and the fake fleet's relay is started with the
  origin it prints. Without that a join on a non-default port fails
  `bad_signature`, the refusal naming the origin the relay checked. Network.framework isn't subject to App Transport
  Security, and loopback needs no local network permission, so the
  Info.plist gains nothing. Each binary message goes to `received`, a
  text or pong one only says the link lives, a ping is answered by
  Network.framework and not fed in, and the next is read once the last
  was fed: one at a time, so a fast relay waits in the socket, not in
  memory. A close frame, a receive with nothing in it, and a `waiting`,
  failed or cancelled connection are a close. Opening the session is
  tried once: if it fails the bar says `stopped: <why>` until the screen
  is opened again (ticket 86's lifecycle retries).
- **Threading.** `RelayConnection` is an actor whose executor is its own
  serial queue (`krowk.relay`), the one each `NWConnection` starts on and
  the timers fire on; callbacks step in with `assumeIsolated`, and the
  receive path holds `dispatchPrecondition(condition: .notOnQueue(.main))`.
  The calls that block (`Account.openSession` and `Viewer.open`, the
  relay ticket, `readChunks`) run on a thread of their own, never on the
  queue or the concurrency runtime's threads, and are fed back on the
  queue, so a heartbeat never waits behind them (R-LAG-9).
- **Updates** for the screen go into a lock-protected mailbox
  (`OSAllocatedUnfairLock<[ViewerUpdate]>`), which the session screen
  drains once per display frame (`CADisplayLink`) into its transcript
  (`takeFrame(into:)`); live deltas are ticket 89's.
- **The status** is a bar under the navigation bar, never a spinner over
  what's on screen (R-IOS-1): `connecting`; `live` once a host welcomed
  the phone; `host offline` when joined with no host, read-only, and the
  viewer waits for the relay's presence notice rather than redialing
  (R-HAND-4); `reconnecting` after a drop; `can't reach the relay: <the
  viewer's note>`; `stopped: <why>` if the viewer ends.
- **The relay.** Rust picks it from the registry (`wss://relay.krowk.com`
  for krowk's, env `production` only in a release build). A DEBUG build
  takes `KROWK_RELAY_URL` from its launch environment, the fake fleet's,
  passed to `Account.openSession` as its override.
- Nothing is logged: the relay ticket goes from the registry call straight
  to the `Viewer`, which puts it in the `Dial` it returns.
- Backoff, wake and suspend, network changes and connecting on return to
  the foreground are ticket 86's.

## A session's transcript

Tapping a session pushes `KrowkKit.TranscriptViewController`: its title
(the index's), the relay bar, and the session's turns top to bottom,
scrolled to the end (ticket 88).

- **What's drawn.** Your prompts (`userText`) as right-aligned bubbles,
  an image as its `[Image #N]` text only; answers (`assistantText`) full
  width; reasoning as one "Thought" line a tap expands, and no row when
  the provider withheld it (empty); a tool call as `<name> <summary>`
  (its input's `command`, `file_path`/`path`, `pattern` or `url`, else
  the JSON's first 80 characters) with `✓` or `✗` and its result's first
  line beneath, a tap expanding the output, monospaced, at most 64 KiB
  and then "… N more bytes"; a turn's end as a thin separator with its
  status (a failure with its error's message); `model.switched` as "Now
  on <model>". Everything else draws nothing, and so does an event of
  another session on the same log (a subagent's).
- **The data** is the viewer's: `Attached` (the session read at rest,
  from its checkpoint, by `Viewer.open` off the main thread), and the
  logged events of `Line` and `CaughtUp`, all generated `LogEvent`s that
  `KrowkAccount` places with an exhaustive switch over `LogBody` and
  `Item` (R-PROTO-2). The log at rest holds only completed items.
- **The model.** `TranscriptModel` (`@MainActor`, `@Observable`) holds
  the row ids in order and one `@Observable ItemState` per row, keyed by
  `itemId` (`turnId` for a turn's end). An event applied twice (same id)
  is a no-op, and a result pairs with its call by `callId`, so the
  diffable data source never sees an id twice (asserted in DEBUG).
- **The view.** A compositional list (estimated heights) with a diffable
  data source, `selfSizingInvalidation = .enabledIncludingConstraints`,
  and cells that observe their own `ItemState` with
  `withObservationTracking`, so one item's change redraws one cell; a
  snapshot only orders rows, applied without animation. Text is TextKit 2
  (`UITextView(usingTextLayoutManager: true)`, non-editable,
  non-scrolling), its `textStorage` kept for ticket 89 to append to. A
  tool's output is laid out only when expanded.
- **Hostile output** (R-IOS-10), in `SafeText`, which every cell draws
  through: C0 controls but `\n` and `\t`, DEL, C1 and the bidi overrides
  and isolates (U+202A–U+202E, U+2066–U+2069) are stripped; markdown is
  not rendered (M1), so an image's markdown is text and nothing is
  fetched; a link is only an `http(s)://` URL written with its scheme to
  an ASCII host with no user or password before it (`NSDataDetector`),
  its host shown after it, and a tap opens a sheet naming the host and
  the whole URL whose "Open" alone calls
  `UIApplication.shared.open`; a long press shows no preview. There is no
  web view in the app: `scripts/no-web.sh` fails on WebKit, a web view,
  Safari's view, `AsyncImage` or an image read from a URL in the sources,
  and on WebKit, SafariServices or LinkPresentation linked by any Mach-O
  file in the built app (ios.yml runs it).
- **No spinner over content** (R-IOS-1): an activity indicator shows only
  in the empty body while the session is read; once a row is drawn it
  never shows again. A session read with nothing to draw says so, and one
  that stopped before it was read points at the bar.

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

`com.krowk.app` is a proposal. The person confirms it in ticket 109 and
registers it in App Store Connect (Pending for the person, below). It is in
`project.yml` and `xtool.yml`. Neither sets `ITSAppUsesNonExemptEncryption`:
krowk does its own end-to-end encryption, and the export compliance answer is
the person's (Export compliance, below).

## Releasing to TestFlight

A tag is the whole trigger (R-IOS-16):

```
git tag -a ios-v0.1.0 -m "iOS 0.1.0" && git push origin ios-v0.1.0
gh run watch        # pick the TestFlight run for ios-v0.1.0
```

`.github/workflows/testflight.yml` answers tags `ios-v<major>.<minor>.<patch>`
only, never the CLI's `v*`, so the app and the CLI release independently
(R-REPO-1). It can also be run by hand, from main or an `ios-v*` tag:
`gh workflow run testflight.yml --ref main -f version=0.1.0`.

- **Versions.** The version (`MARKETING_VERSION`, `CFBundleShortVersionString`)
  is the tag minus `ios-v`, or the dispatch's `version`; anything but
  `N.N.N` fails before building. The build number (`CURRENT_PROJECT_VERSION`,
  `CFBundleVersion`) is `<run number>.<attempt>`, so every run and every
  re-run uploads a new, higher build (as long as the workflow keeps its
  file name: renaming it starts its run numbers again). `project.yml`'s `0.1.0` and `1` are
  placeholders for local builds. The cargo workspace's `0.0.0` plays no part.
- **Signing.** Cloud-managed and automatic: `xcodebuild archive` and
  `-exportArchive` sign with `-allowProvisioningUpdates` and the person's
  App Store Connect API key, and Apple's cloud-managed distribution
  certificate signs the upload. No certificate, provisioning profile or
  keychain is in the repo or its secrets. The `.p8` is written to the
  runner's temp folder for the job only and removed in a step that always
  runs, together with any signing identity and profile the job added to the
  runner (a hosted runner is thrown away after the job anyway). Archiving
  with automatic signing makes a development profile, which Apple only
  issues to a team with at least one registered device (the iPhone of
  ticket 107 will do). Each run may leave an "Apple Development … Created
  via API" certificate in the developer account; revoke old ones there if
  they pile up.
- **Secrets.** `APPLE_TEAM_ID`, `ASC_KEY_ID`, `ASC_ISSUER_ID` and `ASC_KEY`
  (the `.p8` file's contents), in the GitHub environment `testflight`, which
  only the `upload` job names. Its first step fails, naming the missing ones,
  until all four are there; the next checks their shapes. Protect the
  environment before any secret goes in (Pending for the person, item 2,
  has the commands):
  - Deployment branches and tags: selected only, `main` as a **branch** rule
    and `ios-v*` as a **tag** rule. Then no workflow run from any other
    branch, a pull request's included, can read the secrets. (`ios-v*` as a
    branch rule would admit a branch of that name instead.)
  - Required reviewers: you. Each upload waits for your approval; look at
    the tagged commit's `testflight.yml` before approving. This is
    what stops someone with write access who pushes an `ios-v*` tag on a
    commit whose own workflow does what it likes with the secrets.
  - A tag ruleset on `ios-v*` (Settings → Rules → Rulesets → New tag
    ruleset) with Restrict creations, Restrict updates and Restrict
    deletions, only you on its bypass list, so nobody else can make or move
    such a tag.
- **Pull requests.** One that changes the workflow, `ExportOptions.plist`,
  `project.yml`, `scripts/release-preflight.sh` or
  `.github/actions/ios-project` runs the job `dry-run` instead: the preflight
  checks with stand-in values and the same Release archive, unsigned. It
  names no secret and no environment, and `upload` is skipped for a pull
  request. A fork's pull request gets no secrets at all. A pull request
  from a branch of this repository runs its own copy of the workflow, so
  there it is the environment's deployment rule above, not the workflow,
  that keeps the secrets out of reach.
- **The API key's role.** Ticket 110 says App Manager. Cloud-managed
  signing may need more: if the first run fails with "Cloud signing
  permission error", make the key Admin (or give it access to cloud-managed
  distribution certificates) and set `ASC_KEY`/`ASC_KEY_ID` again.
- **The icon.** App Store Connect refuses a build with no app icon, and
  `App/Assets.xcassets/AppIcon.appiconset` is still empty, so `upload` stops
  after archiving, saying so, until it has a 1024×1024 PNG without alpha.
- **Not reproducible.** The CLI's releases are rebuilt on a second runner
  and their checksums and SBOMs signed with cosign (`release.yml`). None of
  that applies to the app in M1: a TestFlight build is neither reproducible
  nor carries signed provenance, beyond the commit the run's summary names.
- `ExportOptions.plist` is checked in with `teamID` as a placeholder; the
  workflow sets it on a copy. `destination` `upload` makes the export the
  upload.

### On Linux, with omarchy-apple-dev's ship.sh

The alternative, by hand on this Linux box: omarchy-apple-dev's `ship.sh`
builds the xtool project in Release, compiles the icon, stamps the `DT*`
keys, signs with rcodesign and uploads with the same kind of API key. Its
distribution certificate's private key stays in
`~/.config/omarchy-apple-dev/distribution/`, on this machine only. Without
`ASC_KEY_ID` it signs with a local TEST identity and validates offline,
which needs no Apple account:

```
. ~/.config/omarchy-apple-dev/env.sh
make ios-core
cd apps/ios
BUILD_NUMBER=1 ~/src/omarchy-apple-dev/ship.sh </dev/null          # offline check
ASC_KEY_PATH=/path/to/AuthKey_XXXXXXXXXX.p8 ASC_ISSUER_ID=<issuer-uuid> ASC_KEY_ID=XXXXXXXXXX \
  BUILD_NUMBER=<n> ~/src/omarchy-apple-dev/ship.sh --upload </dev/null
```

The version is `CFBundleShortVersionString` in `xtool-Info.plist`; set it to
the release's in your working copy only, not committed (`ios.yml` holds that
file's keys equal to `project.yml`'s `0.1.0`). Always pass `BUILD_NUMBER`,
and keep to one path per version: App Store Connect wants each build number
of a version higher than the last. `ship.sh`'s default, the UTC time as
`yyyymmddHHMM`, is far above CI's `<run number>.<attempt>`, so CI's next
upload of that version would be refused. Above CI's last build for that
version (the run summary names it), e.g. `<last run + 1>.1`, works until
CI runs that number. `ASC_ENTITLEMENTS_PATH` is
not needed yet; push (ticket 105) will request `aps-environment` through it.
Like the workflow, it stops at the empty icon (actool: "AppIcon declares
size 1024x1024 but no source file matched").

## Export compliance (pending — the person decides)

App Store Connect asks every build whether it uses non-exempt encryption
(`ITSAppUsesNonExemptEncryption`). krowk's app does more than call HTTPS:
its Rust core, krowk-client, encrypts the person's own sessions end to end
(HPKE, XChaCha20-Poly1305, Ed25519 signatures, SPAKE2 pairing), so that the
relay and the registry only ever carry ciphertext between the person's own
devices. "Uses only exempt encryption (HTTPS)" therefore can't be assumed.

The likely answer is "yes, non-exempt encryption, qualifying for the
mass-market exemption under category 5A992 (5D992 for software) / an
encryption registration self-classification", which may need an annual
self-classification report to the US Bureau of Industry and Security (the
EAR's rules on that report have changed over the years; check the current
ones). That is the person's call (or their counsel's), not the build's,
and nothing here states it as a conclusion. Apple's guide: "Complying with
encryption export regulations" in its developer documentation.

Until it is decided, `project.yml` and `xtool-Info.plist` leave the key
unset (both jobs print what the archive says), and each uploaded
build waits in App Store Connect as "Missing Compliance" until someone
answers there, which is safe. Once decided:

1. Set `INFOPLIST_KEY_ITSAppUsesNonExemptEncryption` (`YES` or `NO`) on the
   `Krowk-iOS` target in `project.yml`, and `ITSAppUsesNonExemptEncryption`
   in `xtool-Info.plist`, with the same value (`ios.yml` checks every key
   `xtool-Info.plist` sets against Xcode's, not the other way round).
2. If Apple issues an export compliance code, set
   `INFOPLIST_KEY_ITSEncryptionExportComplianceCode` in `project.yml` and
   `ITSEncryptionExportComplianceCode` in `xtool-Info.plist` to it.

## Pending for the person

Nothing here uploads until these are done (tickets 109, 110, 111, and the
icon).

1. **The bundle id and the app record** (ticket 109). Confirm `com.krowk.app`,
   register it in the Apple Developer portal (Certificates, Identifiers &
   Profiles → Identifiers → +, App IDs, with Push Notifications), and create
   the app in App Store Connect: Apps → + → New App, platform iOS, name
   "krowk", that bundle id, SKU `krowk-ios`.
2. **The API key and the secrets** (ticket 110). App Store Connect → Users
   and Access → Integrations → App Store Connect API → Team Keys → +, role
   App Manager (or Admin; see "The API key's role" above). Download the
   `.p8` (once only), note its Key ID and the
   Issuer ID. Protect the environment first, then add the secrets without
   echoing them into shell history (`gh secret set` prompts for each value;
   Releasing to TestFlight → Secrets says why):
   ```
   R=repos/krowkcom/krowk/environments/testflight
   me=$(gh api user -q .id)
   gh api -X PUT $R --input - <<< '{"deployment_branch_policy": {"protected_branches": false,
     "custom_branch_policies": true}, "reviewers": [{"type": "User", "id": '"$me"'}]}'
   gh api -X POST $R/deployment-branch-policies -f name=main -f type=branch
   gh api -X POST $R/deployment-branch-policies -f name='ios-v*' -f type=tag
   gh secret set APPLE_TEAM_ID --env testflight -R krowkcom/krowk   # prompts for the value
   gh secret set ASC_KEY_ID    --env testflight -R krowkcom/krowk
   gh secret set ASC_ISSUER_ID --env testflight -R krowkcom/krowk
   gh secret set ASC_KEY       --env testflight -R krowkcom/krowk < AuthKey_XXXXXXXXXX.p8
   gh secret list --env testflight -R krowkcom/krowk                # the four names
   ```
   Then the tag ruleset on `ios-v*` (Releasing to TestFlight → Secrets).
   The team must also have a registered device (Signing, above).
3. **The export-compliance decision** (ticket 111): above.
4. **An app icon.** A 1024×1024 PNG without alpha in
   `App/Assets.xcassets/AppIcon.appiconset`, named in its `Contents.json`.
5. **The first real run:** `git tag -a ios-v0.1.0 -m "iOS 0.1.0" && git push
   origin ios-v0.1.0`, then `gh run watch`. Ticket 107 verifies the build on
   a phone.
