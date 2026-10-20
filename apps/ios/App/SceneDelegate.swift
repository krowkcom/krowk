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
    /// session calls are opened with it. Nil when it could not be made.
    private(set) static var keys: KeychainKeyStorage?

    func scene(
        _ scene: UIScene,
        willConnectTo session: UISceneSession,
        options connectionOptions: UIScene.ConnectionOptions
    ) {
        guard let windowScene = scene as? UIWindowScene else { return }
        let root: UIViewController
        do throws(KrowkError) {
            if Self.keys == nil { Self.keys = try KeychainKeyStorage() }
            // protocolVersion() crosses into the Rust core (krowk-ffi).
            root = HomeViewController(protocolVersion: protocolVersion())
        } catch {
            root = KeysUnavailableViewController(reason: error.detail)
        }
        let navigation = UINavigationController(rootViewController: root)
        navigation.navigationBar.prefersLargeTitles = true
        let window = UIWindow(windowScene: windowScene)
        window.rootViewController = navigation
        window.makeKeyAndVisible()
        self.window = window
    }
}

private extension KrowkError {
    var detail: String {
        switch self {
        case .InvalidInput(let detail), .Internal(let detail), .Refused(_, let detail), .Network(let detail): detail
        }
    }
}
