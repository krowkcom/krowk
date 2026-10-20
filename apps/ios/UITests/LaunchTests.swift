// R-CLIENT-2, R-IOS-13, R-IOS-9: the native app launches on an iPhone
// simulator and, with no krowk key in its Keychain (decided by the Rust
// core, through Account.open), opens on onboarding's "Sign in to krowk".
import XCTest

final class LaunchTests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    /// R-CLIENT-2, R-IOS-13, R-IOS-9: a fresh install opens on "Sign in to
    /// krowk" with one button, in light and dark.
    @MainActor
    func testAFreshInstallOpensOnSignIn() {
        let app = XCUIApplication()
        // A fresh install, whatever an earlier test left in the Keychain;
        // and never krowk's registry, should anything call it.
        app.launchEnvironment["KROWK_FIRST_LAUNCH"] = "1"
        app.launchEnvironment["KROWK_API_URL"] = "http://127.0.0.1:9/v1"
        app.launch()

        XCTAssertTrue(app.staticTexts["sign-in-title"].waitForExistence(timeout: 10), "no sign-in title after 10 s")
        XCTAssertEqual(app.staticTexts["sign-in-title"].label, "Sign in to krowk")
        XCTAssertTrue(app.buttons["sign-in"].exists, "no Sign in button")
        XCTAssertEqual(app.buttons.matching(identifier: "sign-in").count, 1)
        Screenshots.keep("sign-in", in: app)
    }
}

/// Screenshots kept in the test results, light and dark, which CI exports
/// as an artifact.
enum Screenshots {
    @MainActor
    static func keep(_ name: String, in app: XCUIApplication) {
        for (appearance, label) in [(XCUIDevice.Appearance.light, "light"), (.dark, "dark")] {
            XCUIDevice.shared.appearance = appearance
            // The appearance change is animated (under 200 ms).
            Thread.sleep(forTimeInterval: 0.5)
            let attachment = XCTAttachment(screenshot: app.screenshot())
            attachment.name = "krowk-\(name)-\(label)"
            attachment.lifetime = .keepAlways
            XCTContext.runActivity(named: "screenshot \(name) (\(label))") { $0.add(attachment) }
        }
        XCUIDevice.shared.appearance = .light
    }
}
