// R-PERM-2, R-IOS-12, R-IOS-7: a tapped notification opens its session on
// the simulator against the fake fleet (ticket 106). A simulator can't be
// made to tap a real APNs alert reliably, so the Debug build takes the
// alert's userInfo from KROWK_NOTIFICATION_USERINFO at launch and hands it
// to the same parsing and router a tap reaches (the app delegate, as a tap
// that launched a killed app); a Release build never reads it
// (scripts/approvals.sh). A session of the test's own holds a bash call on
// its approval; the app, killed, is launched by the approval's alert and
// opens on that session with the sheet up (the time it took is printed
// for CI's summary), and approving lets the held call run. The same alert
// again lands on "Nothing is waiting for you here any more", no sheet; and
// with the session's host gone, the turn's alert opens it as last seen.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its URLs to this
// runner as KROWK_API_URL, KROWK_RELAY_URL, KROWK_PAIR_URL and
// KROWK_SESSIONS_URL.
import XCTest

final class NotificationUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    @MainActor
    func testR_PERM_2_AnApprovalAlertOpensTheSheetAndAllowRunsTheCall() async throws {
        addTeardownBlock { @MainActor in FleetUI.forgetTheKeys() }
        XCUIDevice.shared.appearance = .light
        var app = try await FleetUI.pairedApp(.light, arguments: ["-KrowkOwnerCheck", "yes"])
        let sessions = try FleetUI.env("KROWK_SESSIONS_URL")
        let title = "Tapped \(UUID().uuidString.prefix(4))"
        let (status, id) = try await FleetUI.request(sessions, body: title)
        XCTAssertEqual(status, 200, id)
        XCTAssertNotNil(UUID(uuidString: id), "the fleet's session id is not a UUID: \(id)")
        let (started, reply) = try await FleetUI.request("\(sessions)/\(id)/turn", body: "Please make the file approved.txt")
        XCTAssertEqual(started, 200, reply)
        // The call is waiting at the host once a phone that opens the
        // session is asked about it: then the app is killed unanswered.
        FleetUI.openListed(title, in: app)
        XCTAssertTrue(app.otherElements["approval-sheet"].waitForExistence(timeout: 60), "the fleet's call never asked")
        app.terminate()

        // R-PERM-2: launched by the approval's alert, on the session with the sheet up.
        let approval = #"{"aps":{"alert":{"title":"krowk","body":"A session needs your approval"},"sound":"default","thread-id":"\#(id)"},"krowk":{"session":"\#(id)","kind":"approval"}}"#
        let launched = Date.now
        app = try Self.launch(tapping: approval)
        let sheet = app.otherElements["approval-sheet"]
        if !sheet.waitForExistence(timeout: 60) { Screenshots.tree(of: app) }
        XCTAssertTrue(sheet.exists, "no approval sheet 60 s after the alert launched the app")
        let took = Date.now.timeIntervalSince(launched)
        print(String(format: "krowk notification cold launch → approval sheet: %.2f s (simulator, launch included)", took))
        XCTAssertTrue(app.navigationBars[title].exists, "the sheet is not over \(title)'s transcript")
        XCTAssertTrue(sheet.staticTexts["approval-summary"].label.contains("touch approved.txt"))
        Screenshots.keep("notification-approval", in: app)
        let allow = sheet.buttons["approval-allow"]
        try await FleetUI.enabled(allow)
        allow.tap()
        XCTAssertTrue(sheet.waitForNonExistence(timeout: 10), "the sheet stayed up after Allow once")
        let done = FleetUI.toolRow(ApprovalUITests.call + ". Done", in: app)
        if !done.waitForExistence(timeout: 60) { Screenshots.tree(of: app) }
        XCTAssertTrue(done.exists, "the allowed call never ran")
        // One screen of the session over the list, never two.
        app.navigationBars.buttons["Sessions"].tap()
        XCTAssertTrue(app.collectionViews["sessions-list"].waitForExistence(timeout: 10), "back did not return to Sessions")

        // A stale alert: answered already, so the welcome brings nothing.
        app = try Self.launch(tapping: approval)
        let notice = app.staticTexts["route-notice"]
        if !notice.waitForExistence(timeout: 60) { Screenshots.tree(of: app) }
        XCTAssertTrue(notice.label.contains("Nothing is waiting for you here any more"), notice.label)
        XCTAssertFalse(app.otherElements["approval-sheet"].exists, "a sheet for an answered request")
        Screenshots.keep("notification-nothing-waiting", in: app)

        // R-IOS-7: the host gone, a turn's alert opens the session as last seen.
        app.terminate()
        let (released, why) = try await FleetUI.request("\(sessions)/\(id)/release")
        XCTAssertEqual(released, 200, why)
        app = try Self.launch(tapping: #"{"krowk":{"session":"\#(id)","kind":"done"}}"#)
        let offline = app.staticTexts["route-notice"]
        if !offline.waitForExistence(timeout: 60) { Screenshots.tree(of: app) }
        XCTAssertTrue(offline.label.contains("last seen"), offline.label)
        XCTAssertFalse(app.otherElements["approval-sheet"].exists)
        Screenshots.keep("notification-host-offline", in: app)

        // A payload naming no session of this phone's: the app just opens.
        app = try Self.launch(tapping: #"{"krowk":{"session":"\#(UUID().uuidString)","kind":"approval"}}"#)
        XCTAssertTrue(app.collectionViews["sessions-list"].waitForExistence(timeout: 10))
        XCTAssertFalse(app.navigationBars.buttons["Sessions"].waitForExistence(timeout: 10), "a session opened for an alert naming none of the phone's")
        app.terminate()
    }

    /// The paired app launched again (not a fresh install) as if by a tap
    /// on an alert whose userInfo is `json`.
    @MainActor
    static func launch(tapping json: String) throws -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-KrowkOwnerCheck", "yes"]
        app.launchEnvironment["KROWK_API_URL"] = try FleetUI.env("KROWK_API_URL")
        app.launchEnvironment["KROWK_RELAY_URL"] = try FleetUI.env("KROWK_RELAY_URL")
        app.launchEnvironment["KROWK_APPEARANCE"] = "light"
        app.launchEnvironment["KROWK_NOTIFICATION_USERINFO"] = json
        app.launch()
        return app
    }
}
