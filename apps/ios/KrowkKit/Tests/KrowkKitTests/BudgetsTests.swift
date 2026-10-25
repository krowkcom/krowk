// R-PERF-7, R-IOS-PERF, R-LAG-8 (ticket 128): the phone measures its own
// budgets — launch to the cached list, foreground to caught up, tap to the
// optimistic bubble, keystroke to glyph — as signpost intervals and one log
// line each. Here on a fake clock and a fake display link: every interval
// begun is ended once, a keystroke in ten is measured, and the line reads
// `budget <name> <ms> ms`. BudgetUITests shows the intervals fire in the
// real app.
import OSLog
import Testing
@testable import KrowkKit

@Suite("R-PERF-7, R-IOS-PERF: the phone's own budgets, as signposts and log lines")
@MainActor
struct BudgetsTests {
    @Test("R-IOS-PERF: launch.cachedList counts from the process's start, ends at the first listing, once per process")
    func launch() {
        let rig = Rig(processAge: 0.25)
        rig.budgets.launchBegan()
        rig.advance(0.1)
        rig.budgets.listShown()
        rig.budgets.listShown()
        rig.budgets.launchBegan()
        rig.budgets.listShown()
        #expect(rig.recorded.count == 1)
        #expect(rig.recorded.first?.0 == .launchCachedList)
        #expect(abs((rig.recorded.first?.1 ?? 0) - 350) < 1, "the time before main counts")
        rig.expectBalanced()
    }

    @Test("R-IOS-PERF: a launch whose first screen isn't Sessions measures nothing, then or later")
    func launchAbandoned() {
        let rig = Rig()
        rig.budgets.launchBegan()
        rig.budgets.launchAbandoned()
        rig.budgets.listShown()
        rig.budgets.launchBegan()
        rig.budgets.listShown()
        #expect(rig.recorded.isEmpty)
        #expect(rig.budgets.begun[.launchCachedList] == 1)
        rig.expectBalanced()
    }

    @Test("R-IOS-PERF: foreground.caughtUp ends at the open session's bar live again, after 10 s away or more")
    func foreground() {
        let rig = Rig()
        // A launch's first foreground: no time away, nothing measured.
        rig.budgets.cameForward()
        rig.budgets.sessionStatus(live: false)
        rig.budgets.sessionStatus(live: true)
        #expect(rig.recorded.isEmpty)

        // Away 9 s: a wake.
        rig.budgets.wentToBackground()
        rig.advance(9)
        rig.budgets.cameForward()
        rig.budgets.sessionStatus(live: false)
        rig.budgets.sessionStatus(live: true)
        rig.budgets.listRead()
        #expect(rig.recorded.isEmpty)

        // Away a minute: the bar still reading live is the old connection's,
        // not caught up; live after reconnecting is.
        rig.budgets.wentToBackground()
        rig.advance(60)
        rig.budgets.cameForward()
        rig.budgets.sessionStatus(live: true)
        rig.advance(0.2)
        rig.budgets.sessionStatus(live: false)
        rig.advance(0.1)
        rig.budgets.sessionStatus(live: true)
        rig.budgets.sessionStatus(live: true)
        #expect(rig.recorded.map(\.0) == [.foregroundCaughtUp])
        #expect(abs((rig.recorded.first?.1 ?? 0) - 300) < 1)
        rig.expectBalanced()
    }

    @Test("R-IOS-PERF: with no session open, the list's first read landing ends foreground.caughtUp; back in the background first, it's given up")
    func foregroundOnTheList() {
        let rig = Rig()
        rig.budgets.wentToBackground()
        rig.advance(10)
        rig.budgets.cameForward()
        rig.advance(0.05)
        rig.budgets.listRead()
        rig.budgets.listRead()
        #expect(rig.recorded.map(\.0) == [.foregroundCaughtUp])

        rig.budgets.wentToBackground()
        rig.advance(30)
        rig.budgets.cameForward()
        rig.budgets.wentToBackground()
        rig.budgets.listRead()
        #expect(rig.recorded.count == 1)
        #expect(rig.budgets.begun[.foregroundCaughtUp] == 2)
        rig.expectBalanced()
    }

    @Test("R-LAG-8: tap.optimistic ends at the display frame after the bubble; a tap that showed none is given up")
    func tap() {
        let rig = Rig()
        rig.budgets.tapped()
        rig.advance(0.002)
        rig.budgets.bubbleShown()
        rig.budgets.tapSettled()
        #expect(rig.ticker.isPaused == false, "the next frame is asked for")
        rig.advance(0.006)
        rig.budgets.frame()
        #expect(rig.ticker.isPaused, "idle between measurements")
        #expect(rig.recorded.map(\.0) == [.tapOptimistic])
        #expect(abs((rig.recorded.first?.1 ?? 0) - 8) < 0.01)

        // Send refused (nothing typed): no bubble, no number.
        rig.budgets.tapped()
        rig.budgets.tapSettled()
        rig.budgets.frame()
        #expect(rig.recorded.count == 1)
        #expect(rig.budgets.begun[.tapOptimistic] == 2)
        rig.expectBalanced()
    }

    @Test("R-IOS-PERF: key.glyph measures one keystroke in ten, each to the next display frame")
    func keystrokes() {
        let rig = Rig()
        for _ in 0..<25 {
            rig.budgets.keystroke()
            rig.advance(0.004)
            rig.budgets.frame()
        }
        #expect(rig.recorded.map(\.0) == [.keyGlyph, .keyGlyph, .keyGlyph], "keystrokes 1, 11 and 21")
        #expect(rig.recorded.allSatisfy { abs($0.1 - 4) < 0.01 })

        // A burst inside one frame: still one in ten, and one open at a time.
        let burst = Rig()
        for _ in 0..<30 { burst.budgets.keystroke() }
        burst.budgets.frame()
        #expect(burst.recorded.count == 1)
        #expect(burst.budgets.begun[.keyGlyph] == 1)
        rig.expectBalanced()
        burst.expectBalanced()
    }

    @Test("R-PERF-7: the line reads `budget <name> <ms> ms`, under com.krowk.app's budgets category, at notice")
    func line() throws {
        let since = Date.now.addingTimeInterval(-1)
        Budgets.line(.tapOptimistic, 12.345)
        Budgets.line(.launchCachedList, 381)
        let store = try OSLogStore(scope: .currentProcessIdentifier)
        let lines = try store.getEntries(at: store.position(date: since))
            .compactMap { $0 as? OSLogEntryLog }
            .filter { $0.subsystem == "com.krowk.app" && $0.category == "budgets" }
        // Other suites' screens may log meanwhile: these two, by their text.
        for text in ["budget tap.optimistic 12.3 ms", "budget launch.cachedList 381.0 ms"] {
            let line = lines.first { $0.composedMessage == text }
            #expect(line != nil, "\(text) is not in the log: \(lines.map(\.composedMessage))")
            #expect(line?.level == .notice)
        }
    }
}

/// A Budgets on a clock the test moves, with a ticker that only counts.
@MainActor
private final class Rig {
    var now: TimeInterval = 1000
    var recorded: [(Budgets.Measure, Double)] = []
    let ticker = PausedTicker()
    var budgets: Budgets!

    init(processAge: TimeInterval = 0) {
        let wall = Date().timeIntervalSince1970
        budgets = Budgets(
            clock: { [unowned self] in now },
            processStart: { wall - processAge },
            record: { [unowned self] in recorded.append(($0, $1)) }
        )
        budgets.ticker = ticker
    }

    func advance(_ seconds: TimeInterval) {
        now += seconds
    }

    /// Every interval begun was ended once.
    func expectBalanced(sourceLocation: SourceLocation = #_sourceLocation) {
        for measure in Budgets.Measure.allCases {
            #expect(budgets.begun[measure, default: 0] == budgets.ended[measure, default: 0], "\(measure)", sourceLocation: sourceLocation)
        }
    }
}

@MainActor
private final class PausedTicker: FrameTicker {
    var isPaused = true
}
