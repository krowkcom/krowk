// R-IOS-9: signing in from the iPhone end to end, on the simulator against
// the fake fleet: "Sign in" opens the stand-in's approval page in the
// ASWebAuthenticationSession sheet, closing the sheet leaves the sign-in
// waiting, the page opens again, and approving it there closes the sheet by
// itself and lands on "Signed in to <workspace>". Screenshots of each
// screen, light and dark, are kept in the results.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its API URL to this
// runner as KROWK_API_URL, and the test hands it on to the app.
import XCTest

final class SignInUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    /// The test ends signed in to the fleet: one more fresh launch clears
    /// the key it left in the simulator's Keychain.
    @MainActor
    private func forgetTheKey() {
        let app = XCUIApplication()
        app.launchEnvironment["KROWK_FIRST_LAUNCH"] = "1"
        app.launchEnvironment["KROWK_API_URL"] = "http://127.0.0.1:9/v1"
        app.launch()
        _ = app.staticTexts["sign-in-title"].waitForExistence(timeout: 10)
        app.terminate()
    }

    @MainActor
    func testSignInThroughTheBrowser() throws {
        // A teardown block, not a defer: a failed assertion stops the test
        // with an exception, which skips a defer.
        addTeardownBlock { @MainActor in
            self.forgetTheKey()
            XCUIDevice.shared.appearance = .light
        }
        for appearance in Screenshots.appearances {
            XCUIDevice.shared.appearance = appearance
            try signIn(appearance)
        }
    }

    @MainActor
    private func signIn(_ appearance: XCUIDevice.Appearance) throws {
        let api = try XCTUnwrap(
            ProcessInfo.processInfo.environment["KROWK_API_URL"].flatMap { $0.isEmpty ? nil : $0 },
            "KROWK_API_URL is unset: run the tests through apps/ios/scripts/with-fleet.sh")
        let app = XCUIApplication()
        app.launchEnvironment["KROWK_FIRST_LAUNCH"] = "1"
        app.launchEnvironment["KROWK_API_URL"] = api
        app.launchEnvironment["KROWK_APPEARANCE"] = Screenshots.name(of: appearance)
        app.launch()

        let signIn = app.buttons["sign-in"]
        XCTAssertTrue(signIn.waitForExistence(timeout: 10), "no Sign in button")
        signIn.tap()

        // The sheet, with the stand-in's page naming the code.
        let code = try XCTUnwrap(FleetUI.signInCode(in: app), "no sign-in code shown")
        XCTAssertTrue(FleetUI.allowTheSignInPage(until: app.staticTexts[code]), "the page in the sheet does not show \(code)")
        Screenshots.keep("approval-page", in: app)

        // Closing the sheet doesn't stop the sign-in. Its close button,
        // not the screen's own Cancel behind it.
        let close = app.buttons.matching(NSPredicate(
            format: "NOT (identifier BEGINSWITH 'sign-in') AND label IN {'Close', 'Cancel', 'Done'}")).firstMatch
        if !close.waitForExistence(timeout: 5) { Screenshots.tree(of: app) }
        XCTAssertTrue(close.exists, "no close button on the sheet")
        close.tap()
        let reopen = app.buttons["sign-in-reopen"]
        if !reopen.waitForExistence(timeout: 10) { Screenshots.tree(of: app) }
        XCTAssertTrue(reopen.exists, "no Open the page again after the sheet closed")
        XCTAssertTrue(app.staticTexts["sign-in-waiting"].exists)
        XCTAssertTrue(app.buttons["sign-in-cancel"].exists)
        Screenshots.keep("waiting", in: app)

        // The page again, approved there: the sheet closes by itself.
        reopen.tap()
        // The page's button, in the sheet's web content.
        let approve = app.buttons["Approve"].firstMatch
        if !FleetUI.allowTheSignInPage(until: approve) { Screenshots.tree(of: app) }
        XCTAssertTrue(approve.exists, "no Approve button on the page")
        approve.tap()
        let signedIn = app.staticTexts["signed-in"]
        XCTAssertTrue(signedIn.waitForExistence(timeout: 30), "not signed in 30 s after approving")
        XCTAssertTrue(signedIn.label.hasPrefix("Signed in to "), signedIn.label)
        XCTAssertFalse(app.buttons["Approve"].exists, "the sheet is still open")
        Screenshots.keep("signed-in", in: app)
        app.terminate()
    }
}
