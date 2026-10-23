import Krowk
import KrowkAccount
import KrowkKeys
import KrowkKit
import UIKit
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

    /// Whether the window shows a screen of the app's, not why it can't.
    private var opened = false

    /// Session screens left with commands still out (ticket 90): kept
    /// until the host answers them, for as long as the app runs.
    private let parkedScreens = ParkedScreens()

    private static let log = Logger(subsystem: "com.krowk.app", category: "sign-in")

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
    /// connected) is tried again when the app comes forward.
    func sceneWillEnterForeground(_ scene: UIScene) {
        if !opened { showRoot() }
    }

    private static let hostingUnitTests = ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil

    private func showRoot() {
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
                Self.keys = try KeychainKeyStorage()
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
        case .signedOut: return signIn(api: api, keys: keys)
        case .signedIn(let workspace): return pairing(api: api, keys: keys, workspace: workspace)
        case .paired: return sessions(api: api, keys: keys)
        }
    }

    /// "Sessions" (R-IOS-3, R-IOS-7): the paired phone's first screen, the
    /// person's synced sessions listed off the main thread.
    private func sessions(api: String, keys: KeychainKeyStorage) -> UIViewController {
        let log = Logger(subsystem: "com.krowk.app", category: "sessions")
        let source = RegistrySessions(api: api, storage: keys)
        let model = SessionListModel(
            source: source,
            // A failure's kind only: never a session's id, title or cwd.
            log: { log.error("\($0, privacy: .public)") }
        )
        // A tapped session opens on its transcript, followed live through
        // the relay while its screen is open, held to the head its listing
        // named. One left with commands still out (a prompt queued for a
        // sleeping laptop) is kept, still followed, and opens as it was.
        let parked = parkedScreens
        // Allowing a call asks for Face ID first (R-IOS-11).
        let owner = OwnerChecks.make()
        return SessionListViewController(model: model) { [weak model] row in
            if let kept = parked.take(row.id) { return kept }
            let known = (model?.listing?.sessions.first { $0.id == row.id } as? SessionSummary)?.head
            // Its approvals rise in a sheet over the transcript (R-PERM-2).
            let approvals = ApprovalQueue(sessionID: row.id, ownerCheck: owner)
            let live = RelayConnection(opening: { try source.openSession(id: row.id, known: known) }, approvals: approvals)
            let screen = TranscriptViewController(row: row, session: live, approvals: approvals)
            screen.parking = parked
            // What an earlier screen of it had refused, with "Edit and resend".
            screen.restore(parked.takeRefusals(row.id))
            return screen
        }
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
            }
        )
        return PairingScreens.pairing(model, workspace: workspace)
    }

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
    }
}

private extension KrowkError {
    var detail: String {
        switch self {
        case .InvalidInput(let detail), .Internal(let detail), .Refused(_, let detail), .Network(let detail),
             .Decode(let detail): detail
        }
    }
}
