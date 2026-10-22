// R-IOS-7, R-IOS-3, R-HAND-4: the session list over a stubbed listing:
// the row formatter, the rebuild throttle, the model's refreshes and
// staleness, and the screen (rows keyed by id, the unreadable footer, the
// empty sentence). No fleet: the hosted SessionListTests cover the real
// listing.
import Foundation
import Synchronization
import Testing
import UIKit
@testable import KrowkKit

struct Stub: SessionRecord {
    var id: String
    var title: String = ""
    var cwd: String = "/home/me/krowk"
    var updatedMs: UInt64 = 0
    var hostedBy: String?
}

/// A listing that answers what it's set to, noting where it ran; held
/// until `release` when `gated`.
final class StubSource: SessionSource {
    private let answer: Mutex<Result<SessionListing, SessionListFailure>>
    private let ranOnMain = Mutex<[Bool]>([])
    private let gate: DispatchSemaphore?

    init(_ answer: Result<SessionListing, SessionListFailure>, gated: Bool = false) {
        self.answer = Mutex(answer)
        gate = gated ? DispatchSemaphore(value: 0) : nil
    }

    func set(_ new: Result<SessionListing, SessionListFailure>) {
        answer.withLock { $0 = new }
    }

    func release() {
        gate?.signal()
    }

    var calls: [Bool] { ranOnMain.withLock { $0 } }

    func list() throws(SessionListFailure) -> SessionListing {
        ranOnMain.withLock { $0.append(Thread.isMainThread) }
        gate?.wait()
        return try answer.withLock { $0 }.get()
    }
}

/// A clock the test moves.
final class TestClock: Sendable {
    private let time = Mutex(Date(timeIntervalSince1970: 1_790_000_000))
    var now: Date { time.withLock { $0 } }
    func advance(_ seconds: TimeInterval) { time.withLock { $0 += seconds } }
    var ms: UInt64 { UInt64(now.timeIntervalSince1970 * 1000) }
}

@MainActor
@Suite("R-IOS-7: the session row formatter")
struct SessionRowFormatterTests {
    let formatter = SessionRowFormatter(locale: Locale(identifier: "en_US"))
    let now = Date(timeIntervalSince1970: 1_790_000_000)
    var nowMs: UInt64 { UInt64(now.timeIntervalSince1970 * 1000) }

    @Test("R-IOS-7: an empty title shows the cwd's last component")
    func emptyTitleIsTheCwdBasename() {
        let row = formatter.row(Stub(id: "a", title: "", cwd: "/home/me/krowk/", updatedMs: nowMs), holderName: nil, liveTrusted: true, now: now)
        #expect(row.title == "krowk")
        #expect(formatter.row(Stub(id: "b", title: "Fix the build"), holderName: nil, liveTrusted: true, now: now).title == "Fix the build")
        #expect(formatter.row(Stub(id: "c", title: " ", cwd: ""), holderName: nil, liveTrusted: true, now: now).title == "Untitled session")
    }

    @Test("R-IOS-7: updatedMs 12 minutes ago with no holder reads \"last seen 12m ago\"")
    func twelveMinutesAgo() {
        let row = formatter.row(Stub(id: "a", updatedMs: nowMs - 12 * 60_000), holderName: nil, liveTrusted: true, now: now)
        #expect(row.detail == "krowk · last seen 12m ago")
        #expect(!row.live)
        // VoiceOver hears whole words.
        #expect(row.accessibilityLabel == "krowk, last seen 12 minutes ago", "the place is said once")
        #expect(formatter.row(Stub(id: "a", title: "Fix it", updatedMs: nowMs - 12 * 60_000), holderName: nil, liveTrusted: true, now: now).accessibilityLabel == "Fix it, krowk, last seen 12 minutes ago")
        #expect(formatter.row(Stub(id: "a", updatedMs: nowMs - 20_000), holderName: nil, liveTrusted: true, now: now).detail == "krowk · last seen just now")
        // A host's clock ahead of this phone's is "just now", not "in 2m".
        #expect(formatter.row(Stub(id: "a", updatedMs: nowMs + 120_000), holderName: nil, liveTrusted: true, now: now).detail == "krowk · last seen just now")
    }

    @Test("R-IOS-7: a holder means live, named from the verified list, never by its raw id")
    func holderIsLive() {
        let id = "0123456789abcdef0123456789abcdef"
        let held = Stub(id: "a", updatedMs: nowMs - 12 * 60_000, hostedBy: id)
        let named = formatter.row(held, holderName: "host-a", liveTrusted: true, now: now)
        #expect(named.live)
        #expect(named.detail == "krowk · ● live on host-a")
        #expect(named.accessibilityLabel == "krowk, live on host-a")
        let unnamed = formatter.row(held, holderName: nil, liveTrusted: true, now: now)
        #expect(unnamed.detail == "krowk · ● live")
        #expect(!unnamed.detail.contains(id))
        #expect(formatter.row(held, holderName: "  ", liveTrusted: true, now: now).detail == "krowk · ● live")
    }

    @Test("R-IOS-7: a live mark from a listing that isn't the latest is not shown")
    func untrustedIsNotLive() {
        let held = Stub(id: "a", updatedMs: nowMs - 12 * 60_000, hostedBy: "d")
        let row = formatter.row(held, holderName: "host-a", liveTrusted: false, now: now)
        #expect(!row.live)
        #expect(row.detail == "krowk · last seen 12m ago")
    }
}

/// A clock the throttle's rebuilds are scheduled on, moved by the test:
/// the throttle's timing without a slow simulator's.
@MainActor
final class VirtualTime {
    private(set) var now: Duration = .zero
    private var due: [(at: Duration, fire: @MainActor () -> Void)] = []

    func schedule(_ delay: Duration, _ fire: @escaping @MainActor () -> Void) {
        due.append((now + delay, fire))
    }

    /// Moves the clock on, running what falls due, earliest first.
    func advance(by step: Duration) {
        let end = now + step
        while let next = due.indices.filter({ due[$0].at <= end }).min(by: { due[$0].at < due[$1].at }) {
            let job = due.remove(at: next)
            now = job.at
            job.fire()
        }
        now = end
    }
}

@MainActor
@Suite("R-IOS-7: rebuilds are throttled to 10 a second")
struct SnapshotThrottleTests {
    @Test("R-IOS-7: 50 triggers in 300 ms give at most 4 applies")
    func fiftyTriggers() {
        let time = VirtualTime()
        var reads = 0
        var applies = 0
        let throttle = SnapshotThrottle<Int>(schedule: { time.schedule($0, $1) }, read: {
            reads += 1
            return reads
        }, apply: { _, _ in applies += 1 })
        for _ in 0..<50 {
            throttle.trigger()
            time.advance(by: .milliseconds(6))
        }
        #expect(time.now == .milliseconds(300))
        time.advance(by: .milliseconds(200))
        #expect(applies >= 1)
        #expect(applies <= 4, "\(applies) applies")
        #expect(reads == applies, "every rebuild read a changed list")
    }

    @Test("R-IOS-7: an unchanged list applies none")
    func unchangedAppliesNone() {
        let time = VirtualTime()
        var applies = 0
        let throttle = SnapshotThrottle<[String]>(schedule: { time.schedule($0, $1) }, read: { ["a", "b"] }, apply: { _, _ in applies += 1 })
        throttle.rebuild()
        #expect(applies == 1)
        for _ in 0..<50 {
            throttle.trigger()
            time.advance(by: .milliseconds(6))
        }
        time.advance(by: .milliseconds(200))
        #expect(applies == 1)
    }

    @Test("R-IOS-7: on the real clock, a trigger is applied about 100 ms later")
    func realClock() async throws {
        var applies = 0
        var value = 0
        let throttle = SnapshotThrottle<Int>(read: { value }, apply: { _, _ in applies += 1 })
        throttle.rebuild()
        value = 1
        throttle.trigger()
        throttle.trigger()
        #expect(applies == 1)
        let deadline = ContinuousClock.now + .seconds(5)
        while applies < 2, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(20))
        }
        #expect(applies == 2)
    }
}

@MainActor
@Suite("R-IOS-7, R-IOS-3: the session list's model and screen, stubbed")
struct SessionListModelTests {
    let clock = TestClock()

    func model(_ source: StubSource) -> SessionListModel {
        let clock = clock
        return SessionListModel(source: source, clock: { clock.now })
    }

    @Test("R-IOS-3: the listing never runs on the main thread")
    func neverOnMain() async {
        let source = StubSource(.success(SessionListing(sessions: [], unreadable: 0)))
        let model = model(source)
        await model.refresh()
        await model.refresh()
        #expect(source.calls == [false, false])
    }

    @Test("R-IOS-7: the unreadable count is a footer, and no row")
    func unreadableIsAFooter() async {
        let source = StubSource(.success(SessionListing(sessions: [Stub(id: "a", title: "One")], unreadable: 2)))
        let list = SessionListViewController(model: model(source))
        list.loadViewIfNeeded()
        await list.model.refresh()
        list.throttle.rebuild()
        #expect(list.dataSource.snapshot().itemIdentifiers == ["a"])
        #expect(list.footerText == "2 sessions can't be opened on this iPhone.")
        #expect(list.emptyLabel.isHidden)

        source.set(.success(SessionListing(sessions: [], unreadable: 1)))
        await list.model.refresh()
        list.throttle.rebuild()
        #expect(list.dataSource.snapshot().numberOfItems == 0)
        #expect(list.footerText == "1 session can't be opened on this iPhone.")
        #expect(list.emptyLabel.isHidden, "the footer says why the list is empty")
    }

    @Test("R-IOS-3: no session at all says how to start one")
    func emptySaysHow() async {
        let list = SessionListViewController(model: model(StubSource(.success(SessionListing(sessions: [], unreadable: 0)))))
        list.loadViewIfNeeded()
        await list.model.refresh()
        list.throttle.rebuild()
        #expect(!list.emptyLabel.isHidden)
        #expect(list.emptyLabel.text == "No synced sessions yet. Start one on your laptop with krowk; it syncs by itself once sync is on.")
        #expect(list.footerText.isEmpty)
    }

    @Test("R-IOS-3: the first paint never waits for the listing")
    func firstPaintDoesNotWait() async throws {
        let source = StubSource(.success(SessionListing(sessions: [Stub(id: "a")], unreadable: 0)), gated: true)
        let list = SessionListViewController(model: model(source))
        list.loadViewIfNeeded()
        list.beginAppearanceTransition(true, animated: false)
        list.endAppearanceTransition()
        // The view is up and loading while the listing is held.
        #expect(list.screen().status == .loading)
        #expect(list.spinner.isAnimating)
        source.release()
        let deadline = ContinuousClock.now + .seconds(5)
        while list.model.listing == nil, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(20))
        }
        #expect(list.model.listing != nil)
        list.beginAppearanceTransition(false, animated: false)
        list.endAppearanceTransition()
    }

    @Test("R-IOS-7: rows are one per id, most recently written first")
    func dedupedAndOrdered() async {
        let source = StubSource(.success(SessionListing(sessions: [
            Stub(id: "old", updatedMs: 1_000),
            Stub(id: "new", updatedMs: 3_000),
            Stub(id: "old", title: "a duplicate", updatedMs: 9_000),
            Stub(id: "mid", updatedMs: 2_000),
        ], unreadable: 0)))
        let list = SessionListViewController(model: model(source))
        list.loadViewIfNeeded()
        await list.model.refresh()
        list.throttle.rebuild()
        #expect(list.dataSource.snapshot().itemIdentifiers == ["new", "mid", "old"])
    }

    @Test("R-IOS-7: a refresh that fails keeps the rows but shows none live, and says so")
    func failedRefreshIsNotLive() async {
        let source = StubSource(.success(SessionListing(sessions: [Stub(id: "a", updatedMs: clock.ms, hostedBy: "d")], unreadable: 0, deviceNames: ["d": "host-a"])))
        let list = SessionListViewController(model: model(source))
        await list.model.refresh()
        #expect(list.screen().rows.map(\.live) == [true])
        #expect(list.screen().rows.first?.detail == "krowk · ● live on host-a")

        source.set(.failure(.network(detail: "offline")))
        await list.model.refresh()
        let screen = list.screen()
        #expect(screen.rows.map(\.id) == ["a"])
        #expect(screen.rows.map(\.live) == [false])
        #expect(screen.stale?.hasPrefix("krowk's registry couldn't be reached.") == true, "\(String(describing: screen.stale))")

        source.set(.success(SessionListing(sessions: [Stub(id: "a", updatedMs: clock.ms, hostedBy: "d")], unreadable: 0)))
        await list.model.refresh()
        #expect(list.screen().rows.map(\.live) == [true])
        #expect(list.screen().stale == nil)
    }

    @Test("R-IOS-3: appearing, then the launch's first foreground, reads the list once")
    func appearingTwiceReadsOnce() async throws {
        let source = StubSource(.success(SessionListing(sessions: [], unreadable: 0)))
        let model = model(source)
        model.appeared()
        model.appeared()
        let deadline = ContinuousClock.now + .seconds(5)
        while model.listing == nil, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(20))
        }
        try await Task.sleep(for: .milliseconds(200))
        #expect(source.calls.count == 1)
        model.disappeared()
    }

    @Test("R-IOS-7: a listing older than a lease's TTL shows none live until it is read again")
    func oldListingIsNotLive() async {
        let source = StubSource(.success(SessionListing(sessions: [Stub(id: "a", updatedMs: clock.ms, hostedBy: "d")], unreadable: 0)))
        let model = model(source)
        await model.refresh()
        #expect(model.liveTrusted)
        // The app comes back after a minute away: the clock ticks on
        // appearing, before the read it starts has ended.
        clock.advance(61)
        source.set(.success(SessionListing(sessions: [Stub(id: "a", updatedMs: clock.ms - 61_000)], unreadable: 0)))
        model.appeared()
        #expect(!model.liveTrusted)
        // The read appearing started, awaited so it doesn't outlive the test.
        await model.refresh()
        model.disappeared()
    }

    @Test("R-IOS-3: a first read that fails says so and to pull down")
    func firstFailure() async {
        let list = SessionListViewController(model: model(StubSource(.failure(.network(detail: "offline")))))
        list.loadViewIfNeeded()
        await list.model.refresh()
        list.throttle.rebuild()
        #expect(!list.emptyLabel.isHidden)
        #expect(list.emptyLabel.text == "krowk's registry couldn't be reached. Pull down to try again.")
    }

    @Test("R-HAND-4: the list offers no way to take or host a session: a row only opens it")
    func noHosting() {
        let list = SessionListViewController(model: model(StubSource(.success(SessionListing(sessions: [], unreadable: 0)))))
        list.loadViewIfNeeded()
        #expect(!list.responds(to: #selector(UICollectionViewDelegate.collectionView(_:contextMenuConfigurationForItemsAt:point:))))
        #expect(list.title == "Sessions")
        #expect(list.navigationItem.rightBarButtonItems?.isEmpty ?? true)
        #expect(list.collectionView.refreshControl != nil, "pull to refresh")
    }

    @Test("R-IOS-7: the log is told a failure's kind, never the core's words")
    func logsTheKindOnly() async {
        var said: [String] = []
        let model = SessionListModel(source: StubSource(.failure(.other(detail: "session 0199 'secret title' unreadable"))), log: { said.append($0) })
        await model.refresh()
        #expect(said == ["listing sessions failed: other"])
    }
}
