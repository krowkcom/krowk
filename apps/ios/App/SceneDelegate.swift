import Krowk
import KrowkAccount
import KrowkCache
import KrowkKeys
import KrowkKit
import UIKit
import UserNotifications
import os

/// One window with one navigation controller: the M1 shell. iPad's
/// UISplitViewController comes later, not in M1.
final class SceneDelegate: UIResponder, UIWindowSceneDelegate {
    var window: UIWindow?

    /// The app's one key storage (R-IOS-8), made when the first scene
    /// connects, before anything reads a key: the sign-in, pairing and
    /// session calls are opened with it. Nil until it could be made, and
    /// while the app hosts unit tests: nil means no keys, never a reason
    /// to keep them anywhere else.
    private(set) static var keys: KeychainKeyStorage?

    /// The on-device cache of the person's sessions (ticket 129), written
    /// as they sync: each listing's index, each open session's logged
    /// events. Forgotten on reset, sign-out and the first launch after an
    /// install; a forgotten one refuses every call, and a fresh one takes
    /// its place.
    private static var cache = SessionCache()

    /// Whether the window shows a screen of the app's, not why it can't.
    private var opened = false

    /// Session screens left with commands still out (ticket 90): kept
    /// until the host answers them, for as long as the app runs.
    private let parkedScreens = ParkedScreens()

    private static let log = Logger(subsystem: "com.krowk.app", category: "sign-in")

    /// The paired phone's push registration (R-IOS-12): made once the
    /// phone is on the person's devices, nil before. The app delegate
    /// hands it iOS's token.
    private(set) static var push: PushRegistration?

    /// While the paired app is in the foreground, whether the phone is
    /// still on the person's devices (ticket 117): checked each minute and
    /// on every return, besides every read of the session list. Nil
    /// before pairing, and once it found the phone gone.
    private var watch: StandingWatch?

    func scene(
        _ scene: UIScene,
        willConnectTo session: UISceneSession,
        options connectionOptions: UIScene.ConnectionOptions
    ) {
        guard let windowScene = scene as? UIWindowScene else { return }
        let window = UIWindow(windowScene: windowScene)
        #if DEBUG
        // Screenshots in both appearances: the CI simulator does not
        // reliably follow its own appearance setting.
        switch ProcessInfo.processInfo.environment["KROWK_APPEARANCE"] {
        case "dark": window.overrideUserInterfaceStyle = .dark
        case "light": window.overrideUserInterfaceStyle = .light
        default: break
        }
        #endif
        self.window = window
        showRoot()
        window.makeKeyAndVisible()
    }

    /// A failure that may pass (the phone was locked when the scene
    /// connected) is tried again when the app comes forward. First, before
    /// any view appears, every session followed live checks its connection
    /// (R-IOS-2, ticket 86).
    func sceneWillEnterForeground(_ scene: UIScene) {
        // A removal while the app was away is heard now, not a minute on;
        // a session joining again meanwhile asks for itself (a refused
        // ticket is checked).
        // Its budget first: from here to caught up (ticket 128).
        Budgets.shared.cameForward()
        watch?.start()
        // The unlock may have come while the app was suspended, unheard.
        if UIApplication.shared.isProtectedDataAvailable { Self.phoneUnlocked() }
        LiveSessions.shared.willEnterForeground()
        if !opened { showRoot() }
        // Each time the app comes forward, a cold launch too (iOS calls
        // this after `willConnectTo`), the token is kept current and a
        // permission turned off in Settings clears it (R-IOS-12).
        if let push = Self.push { Task { await push.onForeground() } }
    }

    /// Active again (after the foreground, or a system sheet or call):
    /// a lost connection is joined, a live one probed.
    func sceneDidBecomeActive(_ scene: UIScene) {
        LiveSessions.shared.didBecomeActive()
    }

    /// In the background a live connection is kept while iOS lets it be,
    /// and a lost one waits for the foreground: push covers a finished
    /// turn and an approval meanwhile.
    func sceneDidEnterBackground(_ scene: UIScene) {
        Budgets.shared.wentToBackground()
        LiveSessions.shared.didEnterBackground()
        watch?.stop()
    }

    private static let hostingUnitTests = ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil

    private func showRoot() {
        #if DEBUG
        // The memory budget's fixture (ticket 127): no keys, no network.
        if let fixture = Fixture.screen() {
            opened = true
            return show(fixture)
        }
        #endif
        let root: UIViewController
        do {
            // Hosting Krowk-iOSTests, the app leaves the Keychain to the
            // tests, which clear and remake it.
            if Self.keys == nil && !Self.hostingUnitTests {
                #if DEBUG
                // The UI tests start each launch as a fresh install, which
                // purges what an earlier test left in the Keychain.
                if ProcessInfo.processInfo.environment["KROWK_FIRST_LAUNCH"] == "1" {
                    try? FileManager.default.removeItem(at: KeychainKeyStorage.installMarker)
                }
                #endif
                // The first launch after an install starts with no cache.
                Self.keys = try KeychainKeyStorage(purging: SessionCache.files())
            }
            root = try screen()
            opened = true
        } catch KrowkError.Refused(code: KeychainKeyStorage.noSecureEnclave, detail: let detail) {
            root = KeysUnavailableViewController(cause: .noSecureEnclave, reason: detail)
        } catch let error as KrowkError {
            root = KeysUnavailableViewController(cause: .failed, reason: error.detail)
        } catch {
            root = KeysUnavailableViewController(cause: .failed, reason: String(describing: error))
        }
        show(root)
    }

    /// The screen the keys call for (R-IOS-9, R-E2E-3): onboarding with no
    /// krowk key, pairing with a key but no user keys, and the session list
    /// once paired, never onboarding.
    private func screen() throws -> UIViewController {
        // protocolVersion() crosses into the Rust core (krowk-ffi).
        let home = { HomeViewController(protocolVersion: protocolVersion()) }
        guard let keys = Self.keys else { return home() }
        let api = SignInRules.apiURL()
        switch try Launch.read(api: api, storage: keys) {
        case .signedOut:
            // Whatever was registered is forgotten: signed in again, the
            // phone registers afresh.
            Self.push = nil
            PushRegistration.signedOut()
            // So is the cache: decrypted content outlasts no account.
            Task { await Self.forgetCache() }
            return signIn(api: api, keys: keys)
        case .signedIn(let workspace): return pairing(api: api, keys: keys, workspace: workspace)
        case .paired:
            // Kept current from the foreground that follows; never asks.
            _ = Self.push(api: api, keys: keys)
            return sessions(api: api, keys: keys)
        }
    }

    /// "Sessions" (R-IOS-3, R-IOS-7): the paired phone's first screen, the
    /// person's synced sessions listed off the main thread. Cache first
    /// (R-IOS-1, ticket 131): the rows the cache kept are its first
    /// snapshot, "last seen" and never live, and a session opens on what
    /// the cache kept of it, then reads on from where its last read
    /// stopped.
    private func sessions(api: String, keys: KeychainKeyStorage) -> UIViewController {
        let log = Logger(subsystem: "com.krowk.app", category: "sessions")
        // Every listing checks the phone is still on the person's devices,
        // and so does a refused open; either finding it gone tells the
        // watch, which shows why, once.
        // This list's own watch, never a later pairing's: a check this
        // account makes late reports to the watch it was made with.
        let ref = WatchRef()
        let cache = Self.cache
        let source = RegistrySessions(api: api, storage: keys, cache: cache, onGone: { gone in
            Task { @MainActor in ref.watch?.report(gone) }
        })
        let watch = StandingWatch(check: { source.standing() }) { [weak self] gone in
            self?.deviceGone(gone, api: api, keys: keys, source: source)
        }
        ref.watch = watch
        self.watch?.stop()
        self.watch = watch
        watch.start()
        let model = SessionListModel(
            source: source,
            // Off the main actor, on the cache's queue; before any read.
            kept: { await cache.listing() },
            // A failure's kind only: never a session's id, title or cwd.
            log: { log.error("\($0, privacy: .public)") }
        )
        // Newest pages read ahead from the cache, never the network.
        let shelf = KeptTranscripts(cache: cache)
        // A tapped session opens on its transcript, followed live through
        // the relay while its screen is open, held to the head its listing
        // named. One left with commands still out (a prompt queued for a
        // sleeping laptop) is kept, still followed, and opens as it was.
        let parked = parkedScreens
        // Allowing a call asks for Face ID first (R-IOS-11).
        let owner = OwnerChecks.make()
        // A session's transcript, followed live through the relay while its
        // screen is open, opened by `opening`, drawn first from what the
        // cache kept of it, if anything (ticket 131).
        let transcript = { @MainActor (row: SessionRow, kept: (any KeptTranscript)?, opening: @escaping @Sendable () throws -> SessionOpening) -> UIViewController in
            // Its approvals rise in a sheet over the transcript (R-PERM-2).
            let approvals = ApprovalQueue(sessionID: row.id, ownerCheck: owner)
            // Its logged events are kept as they come (ticket 129).
            let live = RelayConnection(
                resuming: opening, approvals: approvals,
                standing: { source.standing() }, cache: TranscriptCacheWriter(session: row.id, cache: cache))
            let screen = TranscriptViewController(row: row, session: live, approvals: approvals, kept: kept)
            screen.parking = parked
            // What an earlier screen of it had refused, with "Edit and resend".
            screen.restore(parked.takeRefusals(row.id))
            return screen
        }
        // `+` starts a session on one of the person's hosts (R-HOST-4); the
        // started one opens live as soon as the registry lists it.
        let newSessions = NewSessions(source: source, starting: StartingSessions(source: source)) { session, request in
            // Its screen parked with commands out, if it was left so.
            if let kept = parked.take(session) { return kept }
            let place = SessionRowFormatter.basename(request.cwd)
            let row = SessionRow(id: session, title: place.isEmpty ? "New session" : place, detail: "", live: true, accessibilityLabel: place)
            // New: the cache holds nothing of it yet.
            return transcript(row, nil) { SessionOpening(session: try source.openStartedSession(id: session)) }
        }
        return SessionListViewController(model: model, newSessions: newSessions) { [weak model] row in
            let listed = model?.listing?.sessions ?? []
            // Its neighbours in the list, from the cache: a switch to one
            // draws from memory (R-IOS-3).
            shelf.prefetch(around: row.id, in: listed.map(\.id))
            if let kept = parked.take(row.id) { return kept }
            // The listing's head, fresh or kept; the cache's place may hold
            // it to a later one. Read on from where the cache's last read
            // stopped, followed from the newest event it holds.
            let known = listed.first { $0.id == row.id }.flatMap(listedHead)
            return transcript(row, shelf.transcript(row.id)) { try source.resume(id: row.id, known: known) }
        }
    }

    /// The phone is no longer on the person's devices (ticket 117): every
    /// live session stops, with nothing retried, and the app says why and
    /// offers one thing, to reset the phone. The reset clears the push
    /// token first (it takes the keys), then forgets everything the phone
    /// kept, the Secure Enclave wrapping key included, and opens sign-in
    /// as a fresh install would.
    private func deviceGone(_ gone: DeviceGone, api: String, keys: KeychainKeyStorage, source: RegistrySessions) {
        LiveSessions.shared.stopAll()
        parkedScreens.letGoAll()
        // Nothing re-registers for push meanwhile, a call already queued
        // included; the reset clears the registry's copy through an
        // account of its own.
        let retired = Self.push
        retired?.retire()
        Self.push = nil
        // An approval sheet over a transcript, say, goes with the screens.
        window?.rootViewController?.dismiss(animated: false)
        let log = Logger(subsystem: "com.krowk.app", category: "reset")
        log.error("this iPhone is no longer on the person's devices: \(gone.kind, privacy: .public)")
        let tokens = RegistryPushTokens(api: api, storage: keys)
        let reset = PhoneReset(
            // After a `set` the retired registration had under way, never
            // before it.
            clearPushToken: {
                await retired?.drained()
                try await tokens.clear()
            },
            forgetPush: {
                Self.push = nil
                PushRegistration.signedOut()
                UIApplication.shared.unregisterForRemoteNotifications()
                // An alert still on the lock screen can name a session.
                UNUserNotificationCenter.current().removeAllDeliveredNotifications()
                Task { try? await UNUserNotificationCenter.current().setBadgeCount(0) }
            },
            forgetKeys: { try keys.forgetAll() },
            forgetCache: { await Self.forgetCache() },
            // A failure's kind and the core's words; never a key.
            log: { log.error("\($0, privacy: .public)") }
        )
        let model = DeviceGoneModel(
            gone: gone, reset: reset,
            // The check alone, never `onGone`'s report: this screen is it.
            recheck: { source.checkQuietly() },
            onlyCopy: { source.onlyCopy() },
            onReset: { [weak self] in
                guard let self else { return }
                log.notice("this iPhone was reset")
                watch = nil
                Self.keys = nil
                opened = false
                showRoot()
            },
            onListed: { [weak self] in
                guard let self else { return }
                log.notice("checked again: this iPhone is on the person's devices")
                _ = Self.push(api: api, keys: keys)
                show(sessions(api: api, keys: keys))
            }
        )
        show(DeviceGoneScreens.gone(model))
    }

    private func signIn(api: String, keys: KeychainKeyStorage) -> UIViewController {
        let log = Self.log
        let model = SignInModel(
            service: RegistrySignIn(api: api, storage: keys),
            presenter: WebAuthSessionPresenter(window: { [weak self] in self?.window }),
            deviceName: Self.deviceName,
            // A Keychain status or a network error; never the key, which
            // the model never holds (only the Keychain adapter, to seal it).
            log: { log.error("\($0, privacy: .public)") },
            onSignedIn: { [weak self] workspace in
                guard let self else { return }
                self.show(self.pairing(api: api, keys: keys, workspace: workspace))
            }
        )
        return SignInScreens.signIn(model)
    }

    /// Pairing (R-E2E-3): join the person's devices with the code their
    /// laptop's `krowk devices add` shows, scanned or typed. The session
    /// list once paired.
    private func pairing(api: String, keys: KeychainKeyStorage, workspace: SignedInWorkspace) -> UIViewController {
        let log = Logger(subsystem: "com.krowk.app", category: "pairing")
        let name = SignInRules.deviceName(Self.deviceName)
        let model = PairingModel(
            service: RegistryPairing(api: api, storage: keys, name: name, os: "iOS \(UIDevice.current.systemVersion)"),
            camera: SystemCameraAccess(),
            scannerSupported: PairingScreens.scannerSupported,
            deviceName: name,
            // The core's words for a failure; never a code, which the
            // model never hands to the log.
            log: { log.error("\($0, privacy: .public)") },
            onPaired: { [weak self] in
                guard let self else { return }
                self.show(self.sessions(api: api, keys: keys))
            },
            // On the person's devices: the one time the app asks to send
            // notifications (R-IOS-12), never at launch.
            onJoined: {
                let push = Self.push(api: api, keys: keys)
                Task { await push.afterPairing() }
            }
        )
        return PairingScreens.pairing(model, workspace: workspace)
    }

    /// The phone is locking: the cache lets its file go until it is
    /// unlocked.
    static func phoneLocking() {
        let cache = cache
        Task { await cache.suspend() }
    }

    static func phoneUnlocked() {
        let cache = cache
        Task { await cache.resume() }
    }

    /// Deletes the cache's file; once it's gone a fresh cache takes its
    /// place, and what still holds the old one (a session screen's
    /// writes) is refused. One that couldn't be deleted stays refused
    /// for the rest of the run, nothing reopening the file, and is tried
    /// again at the next launch, which finds the phone signed out.
    private static func forgetCache() async {
        let forgotten = cache
        do {
            try await forgotten.forgetAll()
        } catch {
            Logger(subsystem: "com.krowk.app", category: "reset").error("the cache was not deleted: \(error.description, privacy: .public)")
            return
        }
        if cache === forgotten { cache = SessionCache() }
    }

    /// The push registration, made on first use, signing through the
    /// account in `keys` (R-CLIENT-1).
    private static func push(api: String, keys: KeychainKeyStorage) -> PushRegistration {
        if let push { return push }
        let log = Logger(subsystem: "com.krowk.app", category: "push")
        let register: @MainActor () -> Void
        #if DEBUG
        // The UI tests' stand-in for APNs, which gives GitHub's simulators
        // no token: KROWK_FAKE_APNS_TOKEN (hex) is handed back as iOS would
        // hand a token to the app delegate. Only from the launch
        // environment, in a Debug build (Xcode's, or xtool's dev build);
        // never in a Release one (scripts/approvals.sh checks).
        if let token = ProcessInfo.processInfo.environment["KROWK_FAKE_APNS_TOKEN"].flatMap({ Self.bytes(hex: $0) }) {
            register = { Task { await Self.push?.didRegister(deviceToken: token) } }
        } else {
            register = { UIApplication.shared.registerForRemoteNotifications() }
        }
        #else
        register = { UIApplication.shared.registerForRemoteNotifications() }
        #endif
        let made = PushRegistration(
            notifications: SystemNotifications(),
            sink: RegistryPushTokens(api: api, storage: keys),
            environment: PushEnvironment.current,
            register: register,
            // A failure's kind and the core's words; never the token,
            // which PushRegistration never hands to the log.
            log: { log.error("\($0, privacy: .public)") }
        )
        push = made
        return made
    }

    #if DEBUG
    nonisolated private static func bytes(hex: String) -> Data? {
        guard hex.count % 2 == 0, !hex.isEmpty else { return nil }
        var data = Data()
        var rest = Substring(hex)
        while !rest.isEmpty {
            guard let byte = UInt8(rest.prefix(2), radix: 16) else { return nil }
            data.append(byte)
            rest = rest.dropFirst(2)
        }
        return data
    }
    #endif

    /// What the phone calls itself: `UIDevice`'s name, the generic "iPhone"
    /// without a special entitlement. A DEBUG build takes `KROWK_DEVICE_NAME`
    /// from its launch environment, so each UI test's phone has a name of
    /// its own: the other device refuses one that reads like a listed one.
    private static var deviceName: String {
        #if DEBUG
        if let name = ProcessInfo.processInfo.environment["KROWK_DEVICE_NAME"], !name.isEmpty { return name }
        #endif
        return UIDevice.current.name
    }

    private func show(_ root: UIViewController) {
        let navigation = UINavigationController(rootViewController: root)
        navigation.navigationBar.prefersLargeTitles = true
        window?.rootViewController = navigation
        // A launch measures to the cached list only when Sessions is the
        // first screen (ticket 128); any later one changes nothing.
        if !(root is SessionListViewController) { Budgets.shared.launchAbandoned() }
        // A tapped alert's session opens from the paired phone's list,
        // held until it's up; signing in or pairing, routes are dropped. A
        // key failure that may pass (locked) keeps one held for the next
        // try (ticket 106).
        switch root {
        case let list as SessionListViewController: NotificationTaps.router.paired(list)
        case is KeysUnavailableViewController: break
        default: NotificationTaps.router.unpaired()
        }
    }
}

/// The watch a session list reports to, held weakly: the one it was made
/// with.
@MainActor
private final class WatchRef {
    weak var watch: StandingWatch?
}

private extension KrowkError {
    var detail: String {
        switch self {
        case .InvalidInput(let detail), .Internal(let detail), .Refused(_, let detail), .Network(let detail),
             .Decode(let detail): detail
        }
    }
}
