// R-IOS-12: iOS's "krowk Would Like to Send You Notifications", which the
// app shows once, right after pairing, and what the fleet's stand-in
// registry says is registered (`GET /_push_tokens`: devices and their
// environment, never a token).
import XCTest

enum NotificationPrompt {
    /// Answers the system's prompt with Allow, keeping a screenshot of the
    /// whole screen with it up. False when it never came within `timeout`:
    /// the simulator asks once per install, so a later pairing finds it
    /// answered.
    @MainActor
    @discardableResult
    static func allow(timeout: TimeInterval = 10) -> Bool {
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let alert = springboard.alerts.firstMatch
        guard alert.waitForExistence(timeout: timeout) else { return false }
        let allow = alert.buttons["Allow"]
        guard allow.exists else { return false }
        let label = Screenshots.name(of: XCUIDevice.shared.appearance)
        let attachment = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        attachment.name = "krowk-push-permission-\(label)"
        attachment.lifetime = .keepAlways
        XCTContext.runActivity(named: "screenshot push-permission (\(label))") { $0.add(attachment) }
        allow.tap()
        return true
    }

    /// The devices the stand-in holds a push token for, by id, and each
    /// one's environment.
    static func registered(site: String) async throws -> [String: String] {
        let url = try XCTUnwrap(URL(string: site + "/_push_tokens"))
        let (body, response) = try await URLSession.shared.data(from: url)
        XCTAssertEqual((response as? HTTPURLResponse)?.statusCode, 200)
        let json = try XCTUnwrap(try JSONSerialization.jsonObject(with: body) as? [String: Any])
        let tokens = try XCTUnwrap(json["push_tokens"] as? [[String: Any]])
        return Dictionary(tokens.compactMap { t in
            (t["device"] as? String).map { ($0, t["environment"] as? String ?? "") }
        }, uniquingKeysWith: { a, _ in a })
    }
}
