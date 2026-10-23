// The iPhone app's entry point (R-CLIENT-2, R-IOS-13): the UIKit lifecycle,
// one window scene. Built by Xcode from project.yml, and on Linux by xtool
// from ../Package.swift; the same file for both.
import KrowkKit
import UIKit

@main
final class AppDelegate: UIResponder, UIApplicationDelegate {
    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
    ) -> Bool {
        true
    }

    /// The APNs token, new or the same again: the push registration sends
    /// it to the registry when it changed (R-IOS-12). None before pairing.
    func application(_ application: UIApplication, didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data) {
        guard let push = SceneDelegate.push else { return }
        Task { await push.didRegister(deviceToken: deviceToken) }
    }

    func application(_ application: UIApplication, didFailToRegisterForRemoteNotificationsWithError error: any Error) {
        SceneDelegate.push?.didFailToRegister(error)
    }

    // The generated Info.plist declares a scene manifest without a
    // configuration, so the delegate class is named here, in code, rather
    // than as a module-qualified string in a plist.
    func application(
        _ application: UIApplication,
        configurationForConnecting connectingSceneSession: UISceneSession,
        options: UIScene.ConnectionOptions
    ) -> UISceneConfiguration {
        let configuration = UISceneConfiguration(name: "Default", sessionRole: connectingSceneSession.role)
        configuration.delegateClass = SceneDelegate.self
        return configuration
    }
}
