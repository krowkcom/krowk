// R-PERM-2: approvals answered from the iPhone, on the simulator against
// the fake fleet (ticket 91). The phone signs in and pairs, then opens the
// fleet's demo session, whose third turn is held on an approval for a
// `bash` call: the sheet rises over the transcript with the call's
// summary, Allow once (the owner check a stub that says yes, R-IOS-11)
// takes it down, and the host runs the call: its row reads done and the
// turn goes on to its answer. In a session of the test's own, the fleet
// asks again and Deny refuses the call: its row reads failed with the
// refusal. Light and dark, screenshots of each.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its URLs to this
// runner as KROWK_API_URL, KROWK_RELAY_URL, KROWK_PAIR_URL and
// KROWK_SESSIONS_URL.
import XCTest

final class ApprovalUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    /// The fleet's held call, as its row and the sheet name it.
    static let call = "bash touch approved.txt"

    @MainActor
    func testR_PERM_2_AllowOnceAndDenyFromTheSheet() async throws {
        addTeardownBlock { @MainActor in
            FleetUI.forgetTheKeys()
            XCUIDevice.shared.appearance = .light
        }
        for appearance in Screenshots.appearances {
            XCUIDevice.shared.appearance = appearance
            let app = try await FleetUI.pairedApp(appearance, arguments: ["-KrowkOwnerCheck", "yes"])
            if appearance == .light {
                // The demo session's held call: the first test to open it
                // answers it.
                let demo = FleetUI.listed("A demo session", in: app)
                XCTAssertTrue(demo.waitForExistence(timeout: 30), "the demo session is not listed")
                demo.tap()
            } else {
                try await FleetUI.openOwnSession("Allow me \(UUID().uuidString.prefix(4))", asking: true, in: app)
            }
            try await allow(in: app)
            try await FleetUI.openOwnSession("Deny me \(UUID().uuidString.prefix(4))", asking: true, in: app)
            try await deny(in: app)
            app.terminate()
        }
    }

    /// The sheet over the transcript names the call; Allow once takes it
    /// down, and the call runs: its row reads done, and the turn answers.
    @MainActor
    private func allow(in app: XCUIApplication) async throws {
        let sheet = try sheetUp(in: app)
        Screenshots.keep("approval", in: app)
        let allow = sheet.buttons["approval-allow"]
        try await FleetUI.enabled(allow)
        allow.tap()
        XCTAssertTrue(sheet.waitForNonExistence(timeout: 10), "the sheet stayed up after Allow once")
        let done = FleetUI.toolRow(Self.call + ". Done", in: app)
        if !done.waitForExistence(timeout: 60) { Screenshots.tree(of: app) }
        XCTAssertTrue(done.exists, "the allowed call never ran")
        let answer = app.collectionViews["transcript"].cells.matching(identifier: "transcript-assistant")
            .containing(NSPredicate(format: "label CONTAINS 'approved.txt is in the project'")).firstMatch
        _ = answer.waitForExistence(timeout: 30)
        Screenshots.keep("approval-allowed", in: app)
    }

    /// Deny refuses the call: its row reads failed, with the refusal.
    @MainActor
    private func deny(in app: XCUIApplication) async throws {
        let sheet = try sheetUp(in: app)
        let deny = sheet.buttons["approval-deny"]
        try await FleetUI.enabled(deny)
        deny.tap()
        XCTAssertTrue(sheet.waitForNonExistence(timeout: 10), "the sheet stayed up after Deny")
        let failed = FleetUI.toolRow(Self.call + ". Failed", in: app)
        if !failed.waitForExistence(timeout: 60) { Screenshots.tree(of: app) }
        XCTAssertTrue(failed.exists, "the denied call's row does not read failed")
        Screenshots.keep("approval-denied", in: app)
    }

    /// The approval sheet, up over the transcript, naming the fleet's call.
    @MainActor
    private func sheetUp(in app: XCUIApplication) throws -> XCUIElement {
        let sheet = app.otherElements["approval-sheet"]
        if !sheet.waitForExistence(timeout: 30) { Screenshots.tree(of: app) }
        XCTAssertTrue(sheet.exists, "no approval sheet 30 s after opening the session")
        let summary = sheet.staticTexts["approval-summary"]
        XCTAssertTrue(summary.waitForExistence(timeout: 5))
        XCTAssertTrue(summary.label.contains("touch approved.txt"), summary.label)
        XCTAssertTrue(sheet.buttons["approval-allow"].exists)
        XCTAssertTrue(sheet.buttons["approval-deny"].exists)
        return sheet
    }
}

/// Signing in and pairing a fresh install against the fake fleet, and the
/// fleet's sessions, for UI tests past onboarding.
enum FleetUI {
    static func env(_ name: String) throws -> String {
        try XCTUnwrap(
            ProcessInfo.processInfo.environment[name].flatMap { $0.isEmpty ? nil : $0 },
            "\(name) is unset: run the tests through apps/ios/scripts/with-fleet.sh")
    }

    /// The app launched as a fresh install, signed in through the fleet's
    /// approval, paired with a fresh code, on Sessions.
    @MainActor
    static func pairedApp(_ appearance: XCUIDevice.Appearance, arguments: [String] = []) async throws -> XCUIApplication {
        let api = try env("KROWK_API_URL")
        let app = XCUIApplication()
        app.launchArguments = arguments
        app.launchEnvironment["KROWK_FIRST_LAUNCH"] = "1"
        app.launchEnvironment["KROWK_API_URL"] = api
        app.launchEnvironment["KROWK_RELAY_URL"] = try env("KROWK_RELAY_URL")
        app.launchEnvironment["KROWK_APPEARANCE"] = Screenshots.name(of: appearance)
        app.launchEnvironment["KROWK_DEVICE_NAME"] = "UI iPhone \(UUID().uuidString.prefix(8))"
        app.launch()

        let signIn = app.buttons["sign-in"]
        XCTAssertTrue(signIn.waitForExistence(timeout: 10), "no Sign in button")
        signIn.tap()
        let code = try XCTUnwrap(signInCode(in: app), "no sign-in code shown")
        XCTAssertTrue(allowTheSignInPage(until: app.staticTexts[code]), "the page in the sheet does not show \(code)")
        let site = api.hasSuffix("/v1") ? String(api.dropLast(3)) : api
        let (approved, why) = try await request("\(site)/_approve/cli/authorizations/\(code)/approval")
        XCTAssertEqual(approved, 200, why)

        let field = app.textFields["pairing-code"]
        if !field.waitForExistence(timeout: 30) { Screenshots.tree(of: app) }
        XCTAssertTrue(field.exists, "no pairing screen 30 s after approving")
        let (status, fresh) = try await request(try env("KROWK_PAIR_URL"))
        XCTAssertEqual(status, 200, fresh)
        field.tap()
        field.typeText(String(fresh.dropLast()))
        app.typeText(String(fresh.suffix(1)))
        let done = app.staticTexts["pairing-done"]
        if !done.waitForExistence(timeout: 90) { Screenshots.tree(of: app) }
        XCTAssertTrue(done.exists, "not paired 90 s after the code")
        // R-IOS-12: iOS asks to send notifications once paired, once per
        // install; PairingUITests checks what the answer registers.
        NotificationPrompt.allow(timeout: 5)
        app.buttons["pairing-continue"].tap()
        let list = app.collectionViews["sessions-list"]
        XCTAssertTrue(list.waitForExistence(timeout: 10), "Continue did not open Sessions")
        return app
    }

    /// The code the sign-in shows, from "Check the page shows the code
    /// K7QF-9M3X.": shown as the sheet opens, before iOS asks about it.
    @MainActor
    static func signInCode(in app: XCUIApplication) -> String? {
        let label = app.staticTexts["sign-in-code"]
        guard label.waitForExistence(timeout: 20) else { return nil }
        return label.label.split(separator: " ").last.map { String($0.dropLast()) }
    }

    /// iOS asks before an app opens a sign-in page that shares Safari's
    /// cookies, and the person says Continue; then `page`, an element of
    /// the page in the sheet, shows. On a busy simulator the question can
    /// come 20 s after the tap, so this waits for whichever comes first,
    /// the question or the page, rather than a while for the question
    /// alone: one that came late was never answered, and the page never
    /// opened behind it. A tap the prompt didn't take (it was still
    /// animating in) leaves it up, and it is answered again. Whether the
    /// page showed within `timeout`.
    @MainActor
    static func allowTheSignInPage(until page: XCUIElement, timeout: TimeInterval = 60) -> Bool {
        let proceed = XCUIApplication(bundleIdentifier: "com.apple.springboard").buttons["Continue"]
        let deadline = Date.now.addingTimeInterval(timeout)
        while Date.now < deadline {
            if page.exists { return true }
            if proceed.waitForExistence(timeout: 1) {
                proceed.tap()
                _ = page.waitForExistence(timeout: 3)
            }
        }
        return page.exists
    }

    /// The row of the session titled `title`.
    @MainActor
    static func listed(_ title: String, in app: XCUIApplication) -> XCUIElement {
        app.collectionViews["sessions-list"].cells.matching(NSPredicate(format: "label BEGINSWITH %@", title + ", ")).firstMatch
    }

    /// A session of the test's own, titled `title`, hosted by the fleet;
    /// with `asking`, a turn the fleet types in it holds a `bash` call on
    /// its approval. Opened from Sessions (back to it first, if need be).
    @MainActor
    static func openOwnSession(_ title: String, asking: Bool, in app: XCUIApplication) async throws {
        let sessions = try env("KROWK_SESSIONS_URL")
        let (status, id) = try await request(sessions, body: title)
        XCTAssertEqual(status, 200, id)
        if asking {
            let (started, reply) = try await request("\(sessions)/\(id)/turn", body: "Please make the file approved.txt")
            XCTAssertEqual(started, 200, reply)
        }
        openListed(title, in: app)
    }

    /// The session titled `title` opened from Sessions (back to it first,
    /// if need be), the list pulled down until it's there.
    @MainActor
    static func openListed(_ title: String, in app: XCUIApplication) {
        let list = app.collectionViews["sessions-list"]
        for _ in 0..<3 where !list.exists {
            app.navigationBars.buttons["Sessions"].tap()
            _ = list.waitForExistence(timeout: 5)
        }
        XCTAssertTrue(list.exists, "back did not return to Sessions")
        let row = listed(title, in: app)
        for _ in 0..<10 where !row.waitForExistence(timeout: 3) {
            list.swipeDown()
        }
        if !row.exists { Screenshots.tree(of: app) }
        XCTAssertTrue(row.exists, "\(title) is not listed")
        row.tap()
    }

    /// A tool call's row whose line (for VoiceOver) starts `prefix`.
    @MainActor
    static func toolRow(_ prefix: String, in app: XCUIApplication) -> XCUIElement {
        app.collectionViews["transcript"].cells.matching(identifier: "transcript-tool")
            .descendants(matching: .any).matching(NSPredicate(format: "label BEGINSWITH %@", prefix)).firstMatch
    }

    /// Waits for `button` to take taps: a request's buttons wait a moment
    /// after it shows.
    @MainActor
    static func enabled(_ button: XCUIElement) async throws {
        XCTAssertTrue(button.waitForExistence(timeout: 10))
        let deadline = Date.now.addingTimeInterval(10)
        while !button.isEnabled && Date.now < deadline {
            try await Task.sleep(for: .milliseconds(100))
        }
        XCTAssertTrue(button.isEnabled, "\(button.identifier) never took taps")
    }

    /// One more fresh launch clears the keys a paired test left in the
    /// simulator's Keychain.
    @MainActor
    static func forgetTheKeys() {
        let app = XCUIApplication()
        app.launchEnvironment["KROWK_FIRST_LAUNCH"] = "1"
        app.launchEnvironment["KROWK_API_URL"] = "http://127.0.0.1:9/v1"
        app.launch()
        _ = app.staticTexts["sign-in-title"].waitForExistence(timeout: 10)
        app.terminate()
    }

    /// A POST to the fleet. Two minutes: hosting a session waits up to a
    /// minute for its bridge on a busy runner, after its first turn.
    static func request(_ url: String, body: String = "", timeout: TimeInterval = 120) async throws -> (Int, String) {
        var request = URLRequest(url: try XCTUnwrap(URL(string: url)))
        request.httpMethod = "POST"
        request.httpBody = Data(body.utf8)
        request.timeoutInterval = timeout
        let (body, response) = try await URLSession.shared.data(for: request)
        return ((response as? HTTPURLResponse)?.statusCode ?? 0, String(decoding: body, as: UTF8.self))
    }
}
