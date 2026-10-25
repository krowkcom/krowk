// R-IOS-7, R-IOS-2, R-HAND-4 (ticket 132): airplane mode, as the
// simulator can stand it in. The simulator can't turn the network off, so
// a relaunch points the registry and the relay at a port nothing listens
// on (KROWK_API_URL and KROWK_RELAY_URL = http://127.0.0.1:9). Paired
// against the fake fleet, the app opens a session of the test's own and a
// turn lands in it; terminated with it open, then relaunched offline, the
// list shows the session "last seen", never live, its footer saying the
// list couldn't be read; opened, the session shows the cached turn under
// the offline notice, the bar says the relay can't be reached, and
// nothing on screen is a spinner. Screenshots are kept in the results.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its URLs to this
// runner.
import XCTest

final class OfflineUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    @MainActor
    func testR_IOS_7_inAirplaneModeTheLastSessionOpensFromTheCacheMarkedLastSeen() async throws {
        addTeardownBlock { @MainActor in FleetUI.forgetTheKeys() }
        let app = try await FleetUI.pairedApp(.light)
        // A session of the test's own: the fleet runs turns only in those.
        let title = "Cached \(UUID().uuidString.prefix(4))"
        let sessions = try FleetUI.env("KROWK_SESSIONS_URL")
        let (made, id) = try await FleetUI.request(sessions, body: title)
        XCTAssertEqual(made, 200, id)
        FleetUI.openListed(title, in: app)

        // A turn lands: typed at the host, drawn on the phone.
        let prompt = "Say hello from the phone"
        let (started, reply) = try await FleetUI.request("\(sessions)/\(id)/turn", body: prompt)
        XCTAssertEqual(started, 200, reply)
        let transcript = app.collectionViews["transcript"]
        let asked = transcript.descendants(matching: .any).matching(NSPredicate(format: "label == %@ OR value == %@", prompt, prompt)).firstMatch
        let answered = transcript.descendants(matching: .any)
            .matching(NSPredicate(format: "label CONTAINS %@ OR value CONTAINS %@", "Hello! I can read this project", "Hello! I can read this project")).firstMatch
        if !answered.waitForExistence(timeout: 120) { Screenshots.tree(of: app) }
        XCTAssertTrue(answered.exists, "the turn's answer never showed")
        XCTAssertTrue(asked.exists, "the turn's prompt never showed")
        // To the background, where the cache writes what it held back,
        // then terminated with the session open: it is the one last open.
        XCUIDevice.shared.press(.home)
        _ = app.wait(for: .runningBackgroundSuspended, timeout: 30)
        app.terminate()

        // Offline: nothing listens on the registry or the relay.
        let offline = XCUIApplication()
        offline.launchEnvironment["KROWK_API_URL"] = "http://127.0.0.1:9/v1"
        offline.launchEnvironment["KROWK_RELAY_URL"] = "http://127.0.0.1:9"
        offline.launchEnvironment["KROWK_APPEARANCE"] = "light"
        offline.launch()

        // The list: the session last seen, never live, and the footer says
        // the list couldn't be read.
        let row = FleetUI.listed(title, in: offline)
        if !row.waitForExistence(timeout: 30) { Screenshots.tree(of: offline) }
        XCTAssertTrue(row.exists, "the session was not drawn from the cache")
        XCTAssertTrue(row.label.contains("last seen"), row.label)
        XCTAssertFalse(row.label.contains("live"), row.label)
        // Under the last row: the fleet's other tests' sessions are listed
        // too, so it may be below the screen.
        let list = offline.collectionViews["sessions-list"]
        let footer = list.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", "couldn't be reached")).firstMatch
        for _ in 0..<15 where !footer.waitForExistence(timeout: 4) {
            list.swipeUp()
        }
        if !footer.exists { Screenshots.tree(of: offline) }
        XCTAssertTrue(footer.exists, "the footer never said the list couldn't be read")
        XCTAssertTrue(footer.label.contains("no session shows as live"), footer.label)
        XCTAssertFalse(offline.activityIndicators["sessions-loading"].exists, "a spinner over the cached list")
        Screenshots.keep("offline-sessions", in: offline)

        // Back up to the row, then opened: the cached turn under the offline notice, the bar saying
        // the relay can't be reached, and no spinner.
        for _ in 0..<15 where !row.isHittable {
            list.swipeDown()
        }
        XCTAssertFalse(row.label.contains("live"), "live with the registry out of reach: \(row.label)")
        row.tap()
        let cached = offline.collectionViews["transcript"].descendants(matching: .any)
            .matching(NSPredicate(format: "label == %@ OR value == %@", prompt, prompt)).firstMatch
        if !cached.waitForExistence(timeout: 30) { Screenshots.tree(of: offline) }
        XCTAssertTrue(cached.exists, "the cached turn was not drawn")
        let notice = offline.descendants(matching: .any)["route-notice"]
        if !notice.waitForExistence(timeout: 30) { Screenshots.tree(of: offline) }
        XCTAssertTrue(notice.exists, "no offline notice")
        XCTAssertTrue(notice.label.contains("The host is offline (last seen"), notice.label)
        let bar = offline.descendants(matching: .any)["relay-status"]
        let unreachable = XCTNSPredicateExpectation(predicate: NSPredicate(format: "label BEGINSWITH %@", "can't reach the relay"), object: bar)
        let barSaid = await XCTWaiter().fulfillment(of: [unreachable], timeout: 60)
        if barSaid != .completed { Screenshots.tree(of: offline) }
        XCTAssertEqual(barSaid, .completed, "the bar never said the relay can't be reached: \(bar.label)")
        XCTAssertFalse(offline.activityIndicators["transcript-loading"].exists, "a spinner over the cached transcript")
        // Readable: it scrolls.
        offline.collectionViews["transcript"].swipeDown()
        XCTAssertTrue(cached.exists)
        Screenshots.keep("offline-session", in: offline)
        offline.terminate()
    }
}
