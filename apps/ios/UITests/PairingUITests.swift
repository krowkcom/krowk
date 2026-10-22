// R-E2E-3: pairing the iPhone end to end, on the simulator against the
// fake fleet: sign in, then type the code the fleet's `krowk devices add`
// shows. A wrong code says a code works once; a fresh one names host A
// ("Pairing with host-a"), host A says yes by itself, and the app reaches
// "This iPhone is on your devices". Relaunched, it skips onboarding. The
// simulator has no camera, so the scan button is absent. Screenshots of
// each screen, light and dark, are kept in the results.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its API URL and
// its fresh-code URL to this runner as KROWK_API_URL and KROWK_PAIR_URL.
import XCTest

final class PairingUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    /// The test ends paired: one more fresh launch clears the keys it left
    /// in the simulator's Keychain.
    @MainActor
    private func forgetTheKeys() {
        let app = XCUIApplication()
        app.launchEnvironment["KROWK_FIRST_LAUNCH"] = "1"
        app.launchEnvironment["KROWK_API_URL"] = "http://127.0.0.1:9/v1"
        app.launch()
        _ = app.staticTexts["sign-in-title"].waitForExistence(timeout: 10)
        app.terminate()
    }

    @MainActor
    func testPairByTypingTheCode() async throws {
        addTeardownBlock { @MainActor in
            self.forgetTheKeys()
            XCUIDevice.shared.appearance = .light
        }
        for appearance in Screenshots.appearances {
            XCUIDevice.shared.appearance = appearance
            try await pair(appearance)
        }
    }

    @MainActor
    private func pair(_ appearance: XCUIDevice.Appearance) async throws {
        let api = try Self.env("KROWK_API_URL")
        let app = XCUIApplication()
        app.launchEnvironment["KROWK_FIRST_LAUNCH"] = "1"
        app.launchEnvironment["KROWK_API_URL"] = api
        app.launchEnvironment["KROWK_APPEARANCE"] = Screenshots.name(of: appearance)
        // Host A refuses a name that reads like one already on its list.
        app.launchEnvironment["KROWK_DEVICE_NAME"] = "UI iPhone \(UUID().uuidString.prefix(8))"
        app.launch()

        // Sign in; the test approves the code as the page's button would.
        let signIn = app.buttons["sign-in"]
        XCTAssertTrue(signIn.waitForExistence(timeout: 10), "no Sign in button")
        signIn.tap()
        allowTheSignInPage()
        let code = try XCTUnwrap(signInCode(in: app), "no sign-in code shown")
        let site = api.hasSuffix("/v1") ? String(api.dropLast(3)) : api
        try await Self.post("\(site)/_approve/cli/authorizations/\(code)/approval")

        // The pairing screen, under "Signed in to <workspace>".
        let title = app.staticTexts["pairing-title"]
        if !title.waitForExistence(timeout: 30) { Screenshots.tree(of: app) }
        XCTAssertTrue(title.exists, "no pairing screen 30 s after approving")
        XCTAssertTrue(app.staticTexts["signed-in"].exists)
        // No camera in the simulator: DataScannerViewController.isSupported
        // is false, and there is no scan button.
        XCTAssertFalse(app.buttons["pairing-scan"].exists, "a scan button where the scanner is unsupported")
        Screenshots.keep("pairing", in: app)

        // A wrong code: the pairing ends, a code works once.
        let first = try await Self.freshCode()
        type((first.first == "2" ? "3" : "2") + first.dropFirst(), in: app)
        let failed = app.staticTexts["pairing-failed"]
        if !failed.waitForExistence(timeout: 60) { Screenshots.tree(of: app) }
        XCTAssertTrue(failed.exists, "no failure 60 s after a wrong code")
        XCTAssertTrue(failed.label.hasPrefix("The pairing failed, and a code works once — on 'host-a' run"), failed.label)
        XCTAssertTrue(failed.label.hasSuffix("Nothing was kept."), failed.label)
        Screenshots.keep("pairing-failed", in: app)

        // A fresh code: "Pairing with host-a", then on the devices.
        type(try await Self.freshCode(), in: app)
        let with = app.staticTexts["pairing-with"]
        let done = app.staticTexts["pairing-done"]
        // Host A answers by itself, so this screen may be brief.
        if with.waitForExistence(timeout: 30) {
            XCTAssertEqual(with.label, "Pairing with host-a")
            Screenshots.keep("pairing-with", in: app)
        }
        if !done.waitForExistence(timeout: 90) { Screenshots.tree(of: app) }
        XCTAssertTrue(done.exists, "not paired 90 s after the code")
        XCTAssertEqual(done.label, "This iPhone is on your devices.")
        Screenshots.keep("paired", in: app)
        app.buttons["pairing-continue"].tap()
        XCTAssertTrue(app.staticTexts["protocol-version"].waitForExistence(timeout: 10), "Continue did not go home")
        app.terminate()

        // Relaunched, the paired phone skips onboarding.
        let again = XCUIApplication()
        again.launchEnvironment["KROWK_API_URL"] = api
        again.launch()
        XCTAssertTrue(again.staticTexts["protocol-version"].waitForExistence(timeout: 10), "a relaunch did not open home")
        XCTAssertFalse(again.staticTexts["sign-in-title"].exists)
        XCTAssertFalse(again.staticTexts["pairing-title"].exists)
        again.terminate()
    }

    /// Types `code` into the field: a whole code starts at once.
    @MainActor
    private func type(_ code: String, in app: XCUIApplication) {
        let field = app.textFields["pairing-code"]
        XCTAssertTrue(field.waitForExistence(timeout: 10), "no code field")
        field.tap()
        field.typeText(code)
    }

    /// The code the sign-in shows, from "Check the page shows the code K7QF-9M3X."
    @MainActor
    private func signInCode(in app: XCUIApplication) -> String? {
        let label = app.staticTexts["sign-in-code"]
        guard label.waitForExistence(timeout: 20) else { return nil }
        return label.label.split(separator: " ").last.map { String($0.dropLast()) }
    }

    /// iOS asks before an app opens a sign-in page that shares Safari's
    /// cookies; the person says Continue.
    @MainActor
    private func allowTheSignInPage() {
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let proceed = springboard.buttons["Continue"]
        if proceed.waitForExistence(timeout: 10) { proceed.tap() }
    }

    private static func env(_ name: String) throws -> String {
        try XCTUnwrap(
            ProcessInfo.processInfo.environment[name].flatMap { $0.isEmpty ? nil : $0 },
            "\(name) is unset: run the tests through apps/ios/scripts/with-fleet.sh")
    }

    /// A fresh code from the fleet's `devices add`: a code works once.
    private static func freshCode() async throws -> String {
        let (status, body) = try await request(try env("KROWK_PAIR_URL"))
        XCTAssertEqual(status, 200, body)
        return body
    }

    private static func post(_ url: String) async throws {
        let (status, body) = try await request(url)
        XCTAssertEqual(status, 200, "POST \(url): \(body)")
    }

    private static func request(_ url: String) async throws -> (Int, String) {
        var request = URLRequest(url: try XCTUnwrap(URL(string: url)))
        request.httpMethod = "POST"
        request.timeoutInterval = 60
        let (body, response) = try await URLSession.shared.data(for: request)
        return ((response as? HTTPURLResponse)?.statusCode ?? 0, String(decoding: body, as: UTF8.self))
    }
}
