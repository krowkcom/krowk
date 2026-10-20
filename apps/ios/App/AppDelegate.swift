// The iPhone app's entry point (R-CLIENT-2, R-IOS-13): the UIKit lifecycle,
// one window scene. Built by Xcode from project.yml, and on Linux by xtool
// from ../Package.swift; the same file for both.
import UIKit

@main
final class AppDelegate: UIResponder, UIApplicationDelegate {
    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
    ) -> Bool {
        true
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
