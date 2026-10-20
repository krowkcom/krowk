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
    override func tearDown() {
        let app = XCUIApplication()
        app.launchEnvironment["KROWK_FIRST_LAUNCH"] = "1"
        app.launchEnvironment["KROWK_API_URL"] = "http://127.0.0.1:9/v1"
        app.launch()
        _ = app.staticTexts["sign-in-title"].waitForExistence(timeout: 10)
        app.terminate()
    }

    @MainActor
    func testSignInThroughTheBrowser() throws {
        for appearance in Screenshots.appearances {
            XCUIDevice.shared.appearance = appearance
            try signIn(appearance)
        }
        XCUIDevice.shared.appearance = .light
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
        allowTheSignInPage()
        let code = try XCTUnwrap(shownCode(in: app))
        XCTAssertTrue(app.staticTexts[code].waitForExistence(timeout: 20), "the page in the sheet does not show \(code)")
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
        allowTheSignInPage()
        // The page's button, in the sheet's web content.
        let approve = app.buttons["Approve"].firstMatch
        if !approve.waitForExistence(timeout: 20) { Screenshots.tree(of: app) }
        XCTAssertTrue(approve.exists, "no Approve button on the page")
        approve.tap()
        let signedIn = app.staticTexts["signed-in"]
        XCTAssertTrue(signedIn.waitForExistence(timeout: 30), "not signed in 30 s after approving")
        XCTAssertTrue(signedIn.label.hasPrefix("Signed in to "), signedIn.label)
        XCTAssertFalse(app.buttons["Approve"].exists, "the sheet is still open")
        Screenshots.keep("signed-in", in: app)
        app.terminate()
    }

    /// The code the app shows, from "Check the page shows the code K7QF-9M3X."
    @MainActor
    private func shownCode(in app: XCUIApplication) -> String? {
        let label = app.staticTexts["sign-in-code"]
        guard label.waitForExistence(timeout: 20) else { return nil }
        return label.label.split(separator: " ").last.map { String($0.dropLast()) }
    }

    /// iOS asks before an app opens a sign-in page that shares Safari's
    /// cookies (the session is not ephemeral); the person says Continue.
    @MainActor
    private func allowTheSignInPage() {
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let proceed = springboard.buttons["Continue"]
        if proceed.waitForExistence(timeout: 10) { proceed.tap() }
    }
}
