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
//
// R-PERF-7, R-IOS-PERF, R-LAG-8 (ticket 128): the phone measures its own
// budgets as signpost intervals (KrowkKit's Budgets). Against the fake
// fleet, XCTOSSignpostMetric proves two fire in the real app:
// `launch.cachedList` over launches of a paired phone to Sessions, and
// `tap.optimistic` over prompts typed and sent in a session. Their numbers
// are a simulator's; a phone's are read from its log (README.md → Reading
// the budgets on a phone, ticket 54).
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
            let done = NSPredicate(format: "label == %@", "Scrolled \(passes)")
            // A tap the app missed (it came as the screen settled) is tried
            // again: the button reads "Scrolling…" once a pass starts.
            let started = NSPredicate(format: "label == %@ OR label == %@", "Scrolling…", "Scrolled \(passes)")
            for _ in 0..<3 where !started.evaluate(with: scroll) {
                scroll.tap()
                _ = XCTWaiter().wait(for: [XCTNSPredicateExpectation(predicate: started, object: scroll)], timeout: 15)
            }
            XCTAssertTrue(started.evaluate(with: scroll), "pass \(passes) never started: \(scroll.label)")
            // Top to bottom: the button reads the pass once it's done.
            let scrolled = XCTNSPredicateExpectation(predicate: done, object: scroll)
            XCTAssertEqual(XCTWaiter().wait(for: [scrolled], timeout: 400), .completed, "pass \(passes) never reached the bottom: \(scroll.label)")
        }
        Screenshots.keep("fixture-tenk-bottom", in: app)
        app.terminate()
    }

    /// Paired, each launch opens on Sessions, and `launch.cachedList`
    /// spans the process's start to the first listing applied.
    @MainActor
    func testR_PERF_7_R_IOS_PERF_aPairedLaunchSignpostsLaunchToTheCachedList() async throws {
        addTeardownBlock { @MainActor in FleetUI.forgetTheKeys() }
        let paired = try await FleetUI.pairedApp(.light)
        paired.terminate()
        let app = XCUIApplication()
        app.launchEnvironment["KROWK_API_URL"] = try FleetUI.env("KROWK_API_URL")
        app.launchEnvironment["KROWK_RELAY_URL"] = try FleetUI.env("KROWK_RELAY_URL")
        app.launchEnvironment["KROWK_APPEARANCE"] = "light"
        let demo = FleetUI.listed("A demo session", in: app)
        let options = XCTMeasureOptions()
        options.iterationCount = 3
        let metric = XCTOSSignpostMetric(subsystem: "com.krowk.app", category: "budgets", name: "launch.cachedList")
        measure(metrics: [metric], options: options) {
            app.launch()
            if !demo.waitForExistence(timeout: 60) { Screenshots.tree(of: app) }
            XCTAssertTrue(demo.exists, "a paired launch never listed the demo session")
            app.terminate()
        }
    }

    /// In a session of the test's own, each prompt typed and sent shows
    /// its bubble at once, and `tap.optimistic` spans Send's touch-up to
    /// the frame after it. Not the demo session: its held approval's sheet
    /// would cover the composer for whichever test opens it first.
    @MainActor
    func testR_LAG_8_R_IOS_PERF_sendingAPromptSignpostsTapToTheOptimisticBubble() async throws {
        addTeardownBlock { @MainActor in FleetUI.forgetTheKeys() }
        let app = try await FleetUI.pairedApp(.light)
        try await FleetUI.openOwnSession("Budget \(UUID().uuidString.prefix(4))", asking: false, in: app)
        let text = app.textViews["composer-text"]
        if !text.waitForExistence(timeout: 30) { Screenshots.tree(of: app) }
        XCTAssertTrue(text.exists, "no composer 30 s after opening the session")
        let send = app.buttons["composer-send"]
        let options = XCTMeasureOptions()
        options.iterationCount = 3
        options.invocationOptions = [.manuallyStart, .manuallyStop]
        let metric = XCTOSSignpostMetric(subsystem: "com.krowk.app", category: "budgets", name: "tap.optimistic")
        var sent = 0
        measure(metrics: [metric], options: options) {
            sent += 1
            let prompt = "Budget tap \(sent)"
            let focused = { (text.value(forKey: "hasKeyboardFocus") as? Bool) == true }
            for _ in 0..<3 where !focused() {
                text.tap()
                _ = XCTWaiter().wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "hasKeyboardFocus == true"), object: text)], timeout: 5)
            }
            text.typeText(prompt)
            // Send takes taps once the text is in and the link allows it
            // (a steer, while a turn runs, waits for the host).
            let enabled = XCTNSPredicateExpectation(predicate: NSPredicate(format: "enabled == true"), object: send)
            XCTAssertEqual(XCTWaiter().wait(for: [enabled], timeout: 60), .completed, "Send never took taps")
            let bubble = app.collectionViews["transcript"].descendants(matching: .any)
                .matching(NSPredicate(format: "label CONTAINS %@", prompt)).firstMatch
            startMeasuring()
            send.tap()
            if !bubble.waitForExistence(timeout: 10) { Screenshots.tree(of: app) }
            stopMeasuring()
            XCTAssertTrue(bubble.exists, "no bubble for \(prompt)")
        }
        Screenshots.keep("budget-tap", in: app)
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
