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
        for appearance in Screenshots.appearances {
            XCUIDevice.shared.appearance = appearance
            let app = XCUIApplication()
            // A fresh install, whatever an earlier test left in the Keychain;
            // and never krowk's registry, should anything call it.
            app.launchEnvironment["KROWK_FIRST_LAUNCH"] = "1"
            app.launchEnvironment["KROWK_API_URL"] = "http://127.0.0.1:9/v1"
            app.launchEnvironment["KROWK_APPEARANCE"] = Screenshots.name(of: appearance)
            app.launch()

            XCTAssertTrue(app.staticTexts["sign-in-title"].waitForExistence(timeout: 10), "no sign-in title after 10 s")
            XCTAssertEqual(app.staticTexts["sign-in-title"].label, "Sign in to krowk")
            XCTAssertTrue(app.buttons["sign-in"].exists, "no Sign in button")
            XCTAssertEqual(app.buttons.matching(identifier: "sign-in").count, 1)
            Screenshots.keep("sign-in", in: app)
            app.terminate()
        }
        XCUIDevice.shared.appearance = .light
    }
}

/// Screenshots kept in the test results, which CI exports as an artifact.
/// A running app does not reliably redraw when the simulator's appearance
/// changes, so each test runs once per appearance, set before launch.
enum Screenshots {
    static let appearances: [XCUIDevice.Appearance] = [.light, .dark]

    static func name(of appearance: XCUIDevice.Appearance) -> String {
        appearance == .dark ? "dark" : "light"
    }

    @MainActor
    static func keep(_ name: String, in app: XCUIApplication) {
        let label = name(of: XCUIDevice.shared.appearance)
        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.name = "krowk-\(name)-\(label)"
        attachment.lifetime = .keepAlways
        XCTContext.runActivity(named: "screenshot \(name) (\(label))") { $0.add(attachment) }
    }

    /// The element tree, kept when a test can't find what it looks for.
    @MainActor
    static func tree(of app: XCUIApplication) {
        // Printed too, so the CI log shows it without the result bundle.
        print(app.debugDescription)
        let attachment = XCTAttachment(string: app.debugDescription)
        attachment.name = "element-tree"
        attachment.lifetime = .keepAlways
        XCTContext.runActivity(named: "element tree") { $0.add(attachment) }
    }
}
