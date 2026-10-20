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

    private static let log = Logger(subsystem: "com.krowk.app", category: "sign-in")

    func scene(
        _ scene: UIScene,
        willConnectTo session: UISceneSession,
        options connectionOptions: UIScene.ConnectionOptions
    ) {
        guard let windowScene = scene as? UIWindowScene else { return }
        let window = UIWindow(windowScene: windowScene)
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

    /// The screen the keys call for (R-IOS-9): onboarding with no krowk
    /// key, "Signed in to <workspace>" with a key but no pairing (ticket 84
    /// replaces it), and home once paired, never onboarding.
    private func screen() throws -> UIViewController {
        // protocolVersion() crosses into the Rust core (krowk-ffi).
        let home = { HomeViewController(protocolVersion: protocolVersion()) }
        guard let keys = Self.keys else { return home() }
        let api = SignInRules.apiURL()
        switch try Launch.read(api: api, storage: keys) {
        case .signedOut: return signIn(api: api, keys: keys)
        case .signedIn(let workspace): return SignInScreens.signedIn(workspace)
        case .paired: return home()
        }
    }

    private func signIn(api: String, keys: KeychainKeyStorage) -> UIViewController {
        let log = Self.log
        let model = SignInModel(
            service: RegistrySignIn(api: api, storage: keys),
            presenter: WebAuthSessionPresenter(window: { [weak self] in self?.window }),
            deviceName: UIDevice.current.name,
            // A Keychain status or a network error; never the key, which
            // Swift doesn't hold.
            log: { log.error("\($0, privacy: .public)") },
            onSignedIn: { [weak self] workspace in self?.show(SignInScreens.signedIn(workspace)) }
        )
        return SignInScreens.signIn(model)
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
