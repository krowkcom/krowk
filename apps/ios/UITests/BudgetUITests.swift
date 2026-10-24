// R-PERF-7: the app's memory while it scrolls a 10,000-item session, a CI
// gate (ticket 127), and its launch, a CI trend. The Debug build opens
// TranscriptFixture's 10,000 rows with KROWK_FIXTURE=tenk, held in memory,
// no network; its "Scroll through" takes the list from top to bottom, a
// screen a display frame, five times under XCTMemoryMetric. CI reads the
// peak physical memory from the result bundle and fails over
// `memoryScrollMB.max` in apps/ios/budgets.json (scripts/budgets.sh
// memory). The launch is XCTApplicationLaunchMetric's, written to
// ios-perf.json and the step summary, failing nothing: a simulator's
// launch is no phone's (the phone's own signposts, ticket 128).
import XCTest

final class BudgetUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    @MainActor
    func testR_PERF_7_scrollingTenThousandItemsStaysUnderTheMemoryCeiling() {
        let app = XCUIApplication()
        app.launchEnvironment["KROWK_FIXTURE"] = "tenk"
        app.launchEnvironment["KROWK_APPEARANCE"] = "light"
        app.launch()
        let scroll = app.buttons["fixture-scroll"]
        if !scroll.waitForExistence(timeout: 60) { Screenshots.tree(of: app) }
        XCTAssertTrue(scroll.exists, "the 10,000-item fixture never opened")

        let options = XCTMeasureOptions()
        options.iterationCount = 5
        var passes = 0
        measure(metrics: [XCTMemoryMetric(application: app)], options: options) {
            passes += 1
            scroll.tap()
            // Top to bottom: the button reads the pass once it's done.
            let done = NSPredicate(format: "label == %@", "Scrolled \(passes)")
            let scrolled = XCTNSPredicateExpectation(predicate: done, object: scroll)
            XCTAssertEqual(XCTWaiter().wait(for: [scrolled], timeout: 300), .completed, "pass \(passes) never reached the bottom: \(scroll.label)")
        }
        Screenshots.keep("fixture-tenk-bottom", in: app)
        app.terminate()
    }

    @MainActor
    func testLaunch() {
        measure(metrics: [XCTApplicationLaunchMetric()]) {
            let app = XCUIApplication()
            app.launchEnvironment["KROWK_FIRST_LAUNCH"] = "1"
            app.launchEnvironment["KROWK_API_URL"] = "http://127.0.0.1:9/v1"
            app.launch()
        }
    }
}
