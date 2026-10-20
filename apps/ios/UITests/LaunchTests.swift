// R-CLIENT-2, R-IOS-13: the native app launches on an iPhone simulator and
// shows the protocol version it read from the Rust core.
import XCTest

final class LaunchTests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    /// R-CLIENT-2, R-IOS-13: launch shows the title "krowk" and "Protocol 1".
    @MainActor
    func testLaunchShowsTheProtocolVersion() {
        let app = XCUIApplication()
        app.launch()

        let version = app.staticTexts["protocol-version"]
        XCTAssertTrue(version.waitForExistence(timeout: 5), "no protocol-version label after 5 s")
        XCTAssertEqual(version.label, "Protocol 1")
        XCTAssertTrue(app.navigationBars["krowk"].exists, "no navigation bar titled krowk")
    }
}
