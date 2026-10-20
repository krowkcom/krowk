import Krowk
import KrowkKeys
import KrowkKit
import UIKit

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
        if Self.keys == nil { showRoot() }
    }

    private static let hostingUnitTests = ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil

    private func showRoot() {
        let root: UIViewController
        do throws(KrowkError) {
            // Hosting Krowk-iOSTests, the app leaves the Keychain to the
            // tests, which clear and remake it.
            if Self.keys == nil && !Self.hostingUnitTests { Self.keys = try KeychainKeyStorage() }
            // protocolVersion() crosses into the Rust core (krowk-ffi).
            root = HomeViewController(protocolVersion: protocolVersion())
        } catch .Refused(code: KeychainKeyStorage.noSecureEnclave, detail: let detail) {
            root = KeysUnavailableViewController(cause: .noSecureEnclave, reason: detail)
        } catch {
            root = KeysUnavailableViewController(cause: .failed, reason: error.detail)
        }
        let navigation = UINavigationController(rootViewController: root)
        navigation.navigationBar.prefersLargeTitles = true
        window?.rootViewController = navigation
    }
}

private extension KrowkError {
    var detail: String {
        switch self {
        case .InvalidInput(let detail), .Internal(let detail), .Refused(_, let detail), .Network(let detail): detail
        }
    }
}
