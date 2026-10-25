// The iPhone app's entry point (R-CLIENT-2, R-IOS-13): the UIKit lifecycle,
// one window scene. Built by Xcode from project.yml, and on Linux by xtool
// from ../Package.swift; the same file for both.
import KrowkKit
import UIKit
import UserNotifications

@main
final class AppDelegate: UIResponder, UIApplicationDelegate {
    /// The launch's budget (ticket 128) starts as early as the app's code
    /// runs; its number counts from the kernel's start of the process.
    func application(
        _ application: UIApplication,
        willFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
    ) -> Bool {
        Budgets.shared.launchBegan()
        return true
    }

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
    ) -> Bool {
        // Before this returns: the tap on an alert that launched the app
        // is delivered to the delegate set by now (ticket 106).
        UNUserNotificationCenter.current().delegate = NotificationTaps.shared
        #if DEBUG
        NotificationTaps.tapFromLaunchEnvironment()
        #endif
        return true
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

    /// The phone is locking: the session cache's file goes unreadable
    /// (`.complete`), so its connection is closed rather than held over
    /// it, and nothing opens it until the phone is unlocked (ticket 129).
    func applicationProtectedDataWillBecomeUnavailable(_ application: UIApplication) {
        SceneDelegate.phoneLocking()
    }

    func applicationProtectedDataDidBecomeAvailable(_ application: UIApplication) {
        SceneDelegate.phoneUnlocked()
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
