// A tapped notification opens its session, or the approval it waits on
// (ticket 106, R-PERM-2, R-IOS-12): iOS's notification delegate, set in
// application(_:didFinishLaunchingWithOptions:) before it returns, so the
// tap that launched a killed app is delivered too.
import Foundation
import KrowkKit
import UserNotifications

/// `UNUserNotificationCenter`'s delegate. The payload is read off the main
/// actor into a `NotificationRoute` (a UUID and a kind, or nothing), and
/// only that crosses to the router: no part of `userInfo` goes further.
final class NotificationTaps: NSObject, UNUserNotificationCenterDelegate, Sendable {
    static let shared = NotificationTaps()

    /// Where the routes go: held until the window's root is set up, then
    /// to the paired phone's session list (SceneDelegate tells it which).
    @MainActor static let router = NotificationRouter()

    private override init() {}

    /// The person tapped an alert: its session opens. Dismissing one opens
    /// nothing.
    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        guard response.actionIdentifier == UNNotificationDefaultActionIdentifier else { return }
        let route = NotificationRoute(userInfo: response.notification.request.content.userInfo)
        await MainActor.run { Self.router.handle(route) }
    }

    /// An alert while the app is in the foreground: none for the session
    /// on screen and live, a banner with sound for any other.
    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        let route = NotificationRoute(userInfo: notification.request.content.userInfo)
        return await MainActor.run { NotificationRoute.presentation(of: route, shown: Self.router.shownSession) }
    }

    #if DEBUG
    /// The UI tests' stand-in for a tapped alert, which a simulator can't
    /// be made to tap reliably: `KROWK_NOTIFICATION_USERINFO`, the alert's
    /// `userInfo` as JSON, from the launch environment of a Debug build
    /// only (scripts/approvals.sh checks a Release one never reads it),
    /// taken through the same parsing and router as a real tap, as if it
    /// had launched the app.
    @MainActor static func tapFromLaunchEnvironment() {
        guard let json = ProcessInfo.processInfo.environment["KROWK_NOTIFICATION_USERINFO"],
              let userInfo = (try? JSONSerialization.jsonObject(with: Data(json.utf8))) as? [String: Any]
        else { return }
        router.handle(NotificationRoute(userInfo: userInfo))
    }
    #endif
}
