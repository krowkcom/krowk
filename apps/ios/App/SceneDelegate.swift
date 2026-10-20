import Krowk
import KrowkKit
import UIKit

/// One window with one navigation controller: the M1 shell. iPad's
/// UISplitViewController comes later, not in M1.
final class SceneDelegate: UIResponder, UIWindowSceneDelegate {
    var window: UIWindow?

    func scene(
        _ scene: UIScene,
        willConnectTo session: UISceneSession,
        options connectionOptions: UIScene.ConnectionOptions
    ) {
        guard let windowScene = scene as? UIWindowScene else { return }
        // protocolVersion() crosses into the Rust core (krowk-ffi).
        let navigation = UINavigationController(rootViewController: HomeViewController(protocolVersion: protocolVersion()))
        navigation.navigationBar.prefersLargeTitles = true
        let window = UIWindow(windowScene: windowScene)
        window.rootViewController = navigation
        window.makeKeyAndVisible()
        self.window = window
    }
}
