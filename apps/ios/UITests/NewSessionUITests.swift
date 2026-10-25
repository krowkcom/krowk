// R-HOST-4, R-LAG-8, R-HOST-3: a session started from the phone, through
// its screens, against the fake fleet. On Sessions, `+` opens "New
// session"; the person picks host-a (online, its krowk version), the
// fleet's repository, the mock model's instance, types a prompt and taps
// Start. The sheet closes at once and the row reads `starting on host-a…`
// (R-LAG-8), held there while the fleet holds host A's refreshes
// (KROWK_HOSTS_URL/hold); once the host answers, the session's transcript
// opens and shows the model's answer.
import XCTest

final class NewSessionUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    @MainActor
    func testR_HOST_4_R_LAG_8_pickAHostADirectoryAndAModelThenStart() async throws {
        addTeardownBlock { @MainActor in FleetUI.forgetTheKeys() }
        let app = try await FleetUI.pairedApp(.light)

        let add = app.navigationBars.buttons["new-session-open"]
        if !add.waitForExistence(timeout: 30) { Screenshots.tree(of: app) }
        XCTAssertTrue(add.exists, "no + on Sessions")
        add.tap()

        // The host step reads the hosts every 10 s; host-a announces itself
        // every 2 s, so it is listed online on the first or second read.
        let host = app.tables["new-session-hosts"].cells["host-host-a"]
        if !host.waitForExistence(timeout: 60) { Screenshots.tree(of: app) }
        XCTAssertTrue(host.exists, "host-a is not listed")
        let online = XCTNSPredicateExpectation(predicate: NSPredicate(format: "label CONTAINS %@", "krowk "), object: host)
        XCTAssertEqual(XCTWaiter().wait(for: [online], timeout: 30), .completed, "host-a is not online: \(host.label)")
        Screenshots.keep("new-session-hosts", in: app)
        host.tap()

        let directory = app.tables["new-session-directories"].cells["directory-0"]
        XCTAssertTrue(directory.waitForExistence(timeout: 10), "no directory")
        XCTAssertTrue(directory.label.contains("project"), "\(directory.label)")
        directory.tap()

        let model = app.tables["new-session-models"].cells.matching(NSPredicate(format: "identifier BEGINSWITH %@", "model-anthropic/")).firstMatch
        if !model.waitForExistence(timeout: 10) { Screenshots.tree(of: app) }
        XCTAssertTrue(model.exists, "no ready model")
        Screenshots.keep("new-session-models", in: app)
        model.tap()

        let prompt = app.textViews["new-session-prompt"]
        XCTAssertTrue(prompt.waitForExistence(timeout: 10))
        let focused = { (prompt.value(forKey: "hasKeyboardFocus") as? Bool) == true }
        for _ in 0..<3 where !focused() {
            prompt.tap()
            _ = XCTWaiter().wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "hasKeyboardFocus == true"), object: prompt)], timeout: 5)
        }
        prompt.typeText("Say hello from the phone")
        let start = app.navigationBars.buttons["new-session-start"]
        XCTAssertTrue(start.isEnabled, "Start doesn't take taps")
        // Host A's refreshes held, so the start waits for the test to look
        // at its row rather than race the host's answer.
        let hosts = try FleetUI.env("KROWK_HOSTS_URL")
        // Never left held for the tests after this one, however this ends.
        addTeardownBlock { _ = try? await FleetUI.request("\(hosts)/online") }
        let (held, said) = try await FleetUI.request("\(hosts)/hold")
        XCTAssertEqual(held, 200, said)
        start.tap()

        // R-LAG-8: the sheet closes at once and the row says so.
        let starting = app.collectionViews["sessions-list"].cells.matching(NSPredicate(format: "label BEGINSWITH %@", "starting on host-a")).firstMatch
        if !starting.waitForExistence(timeout: 10) { Screenshots.tree(of: app) }
        let shown = starting.exists
        Screenshots.keep("new-session-starting", in: app)
        let (online, reply) = try await FleetUI.request("\(hosts)/online")
        XCTAssertEqual(online, 200, reply)
        XCTAssertTrue(shown, "no starting on host-a… row")

        // Started: the session opens, and its first turn's answer shows.
        let answer = app.collectionViews["transcript"].descendants(matching: .any)
            .matching(NSPredicate(format: "label CONTAINS %@ OR value CONTAINS %@", "Hello! I can read this project", "Hello! I can read this project")).firstMatch
        if !answer.waitForExistence(timeout: 90) { Screenshots.tree(of: app) }
        XCTAssertTrue(answer.exists, "the started session's transcript never showed the answer")
        Screenshots.keep("new-session-started", in: app)
        app.terminate()
    }
}
