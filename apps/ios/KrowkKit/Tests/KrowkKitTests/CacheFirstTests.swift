// R-IOS-1, R-IOS-3 (ticket 131): cache first, with no fleet. The list's
// first snapshot is the kept listing, read before any network read and
// never live; a transcript draws its kept newest page before what the
// session brings, which merges under it by event id, and pages older ones
// in above as the person scrolls up; the cache keeps a read's place with
// its events. The fleet's sessions through the real cache and registry are
// AppTests' CacheFirstTests.
import Foundation
import Synchronization
import Testing
import UIKit
@testable import KrowkCache
@testable import KrowkKit

/// What the phone kept of a session, as the test sets it: a page in
/// memory, a newest page held until `release`, older pages by `older`.
final class StubKept: KeptTranscript, @unchecked Sendable {
    private let lock = Mutex<(released: Bool, reads: Int)>((false, 0))
    let inMemory: KeptPage?
    let newestPage: KeptPage?
    let older: [Int: KeptPage]

    init(ready: KeptPage? = nil, newest: KeptPage? = nil, older: [Int: KeptPage] = [:], released: Bool = true) {
        inMemory = ready
        newestPage = newest
        self.older = older
        lock.withLock { $0.released = released }
    }

    func release() { lock.withLock { $0.released = true } }
    var reads: Int { lock.withLock { $0.reads } }

    @MainActor func ready() -> KeptPage? { inMemory }

    func newest() async -> KeptPage? {
        lock.withLock { $0.reads += 1 }
        while !lock.withLock({ $0.released }) { try? await Task.sleep(for: .milliseconds(5)) }
        return newestPage
    }

    func page(before older: Int) async -> KeptPage? {
        lock.withLock { $0.reads += 1 }
        return self.older[older]
    }
}

@MainActor
@Suite("R-IOS-1, R-IOS-3: cache first, the transcript model")
struct KeptTranscriptModelTests {
    static let s = TranscriptScreenTests.session
    static func prompt(_ n: Int) -> TranscriptEvent {
        TranscriptScreenTests.event("e\(n)", .userText(item: "u\(n)", text: "prompt \(n)", images: []))
    }
    static func answer(_ n: Int) -> TranscriptEvent {
        TranscriptScreenTests.event("a\(n)", .assistantText(item: "r\(n)", text: "answer \(n)"))
    }

    @Test("R-IOS-1: the kept page drawn first, then the session's read and catch-up holding some of the same events: each drawn once, in order")
    func testR_IOS_1_keptAndReadEventsMergeByID() {
        let model = TranscriptModel(sessionID: Self.s)
        model.drawKept([Self.prompt(1), Self.answer(1), Self.prompt(2)])
        #expect(model.ids == ["item:u1", "item:r1", "item:u2"])
        model.apply(frame: [.attached([Self.prompt(2), Self.answer(2)]), .caughtUp([Self.answer(2), Self.prompt(3)]), .logged([Self.prompt(3), Self.answer(3)])])
        #expect(model.ids == ["item:u1", "item:r1", "item:u2", "item:r2", "item:u3", "item:r3"])
        #expect(Set(model.ids).count == model.ids.count)
        #expect(model.attached)
    }

    @Test("R-IOS-3: an older kept page goes above every row, in order; one that came after the session's read goes above it too")
    func testR_IOS_3_anOlderPageGoesOnTop() {
        let model = TranscriptModel(sessionID: Self.s)
        model.drawKept([Self.prompt(3), Self.answer(3)])
        model.drawKept([Self.prompt(1), Self.answer(1), Self.prompt(2), Self.answer(2), Self.prompt(3)])
        #expect(model.ids == ["item:u1", "item:r1", "item:u2", "item:r2", "item:u3", "item:r3"])

        let late = TranscriptModel(sessionID: Self.s)
        late.apply(frame: [.attached([Self.prompt(2), Self.answer(2)])])
        late.drawKept([Self.prompt(1), Self.answer(1), Self.prompt(2)])
        #expect(late.ids == ["item:u1", "item:r1", "item:u2", "item:r2"])
    }

    @Test("R-IOS-3: an older page's turn never sets one running, and its end refuses no bubble; a result drawn before its call takes it in")
    func testR_IOS_3_anOlderPageIsHistory() {
        let model = TranscriptModel(sessionID: Self.s)
        model.drawKept([TranscriptScreenTests.event("x4", .toolResult(item: "t2", callID: "c1", output: "done", isError: false))])
        let token = model.issue(.prompt("typed"))
        model.drawKept([
            TranscriptScreenTests.event("x1", .turnStarted(turn: "old")),
            TranscriptScreenTests.event("x2", .toolCall(item: "t1", callID: "c1", name: "read", input: "{}")),
            TranscriptScreenTests.event("x3", .turnCompleted(turn: "old", end: .completed)),
        ])
        #expect(!model.isTurnRunning)
        #expect(model.item(TranscriptModel.bubbleID(token))?.refused == false)
        #expect(model.ids == ["item:t1", "turn:old", TranscriptModel.bubbleID(token)], "the result taken into its call")
        #expect(model.item("item:t1")?.output == "done")
    }
}

@MainActor
@Suite("R-IOS-1, R-IOS-3: cache first, the session list")
struct KeptListingTests {
    let clock = TestClock()

    func listing(_ ids: [String], hostedBy: String? = nil) -> SessionListing {
        SessionListing(sessions: ids.enumerated().map { Stub(id: $1, updatedMs: clock.ms - UInt64($0) * 60_000, hostedBy: hostedBy) }, unreadable: 0)
    }

    @Test("R-IOS-1: the kept listing is the model's before any read starts, and never live; the read then replaces it")
    func testR_IOS_1_theKeptListingComesFirstAndIsNeverLive() async {
        let source = StubSource(.success(listing(["a", "b", "c"], hostedBy: "host")))
        let kept = listing(["b", "a"], hostedBy: "host")
        let clock = clock
        let model = SessionListModel(source: source, kept: { kept }, clock: { clock.now })
        #expect(!model.primed)
        await model.prime()
        #expect(model.primed)
        #expect(source.calls.isEmpty, "no network read before the kept listing")
        #expect(model.listing?.sessions.map(\.id) == ["b", "a"])
        #expect(!model.liveTrusted, "a kept listing names no lease")
        await model.refresh()
        #expect(model.listing?.sessions.map(\.id) == ["a", "b", "c"])
        #expect(model.liveTrusted)
    }

    @Test("R-IOS-1: a notification's route waits for a read, not the kept listing")
    func testR_IOS_1_settledIsARead() async {
        let source = StubSource(.success(listing(["a", "new"])))
        let kept = listing(["a"])
        let model = SessionListModel(source: source, kept: { kept })
        await model.prime()
        await model.settled()
        #expect(source.calls.count == 1)
        #expect(model.listing?.sessions.map(\.id) == ["a", "new"])
    }

    @Test("R-IOS-1: the list's first snapshot is the kept rows, last seen, with no spinner; the read's rows follow, live")
    func testR_IOS_1_theFirstSnapshotIsTheKeptRows() async throws {
        let source = StubSource(.success(listing(["a", "b"], hostedBy: "host")), gated: true)
        defer { source.release() }
        let kept = listing(["a"], hostedBy: "host")
        let clock = clock
        let model = SessionListModel(source: source, kept: { kept }, clock: { clock.now })
        let screen = SessionListViewController(model: model)
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        defer { window.isHidden = true }
        try await waitFor { screen.dataSource.snapshot().itemIdentifiers == ["a"] }
        #expect(!screen.spinner.isAnimating)
        #expect(screen.screen().rows.allSatisfy { !$0.live && $0.detail.contains("last seen") })
        source.release()
        try await waitFor { screen.dataSource.snapshot().itemIdentifiers == ["a", "b"] }
        #expect(screen.screen().rows.allSatisfy { $0.live })
    }

    private func waitFor(_ done: () -> Bool) async throws {
        for _ in 0..<300 where !done() {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(done())
    }
}

@MainActor
@Suite("R-IOS-1, R-IOS-3: cache first, the transcript screen")
struct KeptTranscriptScreenTests {
    let row = SessionRow(id: TranscriptScreenTests.session, title: "A demo session", detail: "app", live: false, accessibilityLabel: "A demo session")
    typealias M = KeptTranscriptModelTests

    func shown(_ screen: TranscriptViewController) -> UIWindow {
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        window.layoutIfNeeded()
        return window
    }

    @Test("R-IOS-1, R-IOS-3: a page prefetched into memory is drawn as the screen loads: no spinner, ever, and nothing read")
    func testR_IOS_3_aPrefetchedPageDrawsAtOnce() {
        let kept = StubKept(ready: KeptPage(events: [M.prompt(1), M.answer(1)], older: nil))
        let screen = TranscriptViewController(row: row, session: StubSession(), kept: kept)
        let window = shown(screen)
        defer { window.isHidden = true }
        #expect(screen.drewRows)
        #expect(screen.dataSource.snapshot().itemIdentifiers == ["item:u1", "item:r1"])
        #expect(!screen.spinner.isAnimating)
        #expect(screen.emptyLabel.isHidden)
        #expect(kept.reads == 0)
    }

    @Test("R-IOS-1: while the kept page is read no spinner shows and what the session brings waits; then it merges under the kept rows, each once")
    func testR_IOS_1_theSessionWaitsForTheKeptPage() async throws {
        let kept = StubKept(newest: KeptPage(events: [M.prompt(1), M.answer(1), M.prompt(2)], older: nil), released: false)
        let session = StubSession()
        let screen = TranscriptViewController(row: row, session: session, kept: kept)
        let window = shown(screen)
        defer { window.isHidden = true }
        #expect(screen.readingKept)
        #expect(!screen.spinner.isAnimating, "no spinner before the cache said it holds nothing")
        session.bring([M.prompt(2), M.answer(2)], attach: true)
        screen.frame()
        #expect(screen.model.ids.isEmpty, "the session's read waits for the kept page")
        kept.release()
        try await waitFor { screen.model.ids == ["item:u1", "item:r1", "item:u2", "item:r2"] }
        #expect(!screen.spinner.isAnimating)
    }

    @Test("R-IOS-1: with nothing kept, the spinner shows as before")
    func testR_IOS_1_nothingKeptShowsTheSpinner() async throws {
        let kept = StubKept(newest: nil)
        let screen = TranscriptViewController(row: row, session: StubSession(), kept: kept)
        let window = shown(screen)
        defer { window.isHidden = true }
        try await waitFor { !screen.readingKept }
        #expect(screen.spinner.isAnimating)
    }

    @Test("R-IOS-3: near the top an older kept page is read and drawn above, and the rows on screen stay put")
    func testR_IOS_3_scrollingUpPagesOlderEventsIn() async throws {
        let newest = (40..<80).flatMap { [M.prompt($0), M.answer($0)] }
        let older = (0..<40).flatMap { [M.prompt($0), M.answer($0)] }
        let kept = StubKept(ready: KeptPage(events: newest, older: 40), older: [40: KeptPage(events: older, older: nil)])
        let screen = TranscriptViewController(row: row, session: StubSession(), kept: kept)
        let window = shown(screen)
        defer { window.isHidden = true }
        #expect(screen.model.ids.count == 80)
        // Scrolled to the top: the older page comes in above.
        screen.collectionView.setContentOffset(CGPoint(x: 0, y: -screen.collectionView.adjustedContentInset.top), animated: false)
        try await waitFor { screen.model.ids.count == 160 }
        #expect(screen.model.ids.first == "item:u0")
        #expect(screen.model.ids[80] == "item:u40")
        #expect(kept.reads == 1)
    }

    private func waitFor(_ done: () -> Bool) async throws {
        for _ in 0..<300 where !done() {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(done())
    }
}

@Suite("R-IOS-1: the cache keeps a read's place with its events")
struct KeptPlaceTests {
    func cache() async throws -> SessionCache {
        try await SessionCache.open(at: URL.temporaryDirectory.appending(path: "krowk-cache-tests/\(UUID().uuidString)/krowk-cache.sqlite"))
    }

    @Test("R-IOS-1: a read's events and its place go in together; a read that started over replaces the session's events and cursor; eviction forgets the place")
    func testR_IOS_1_aPlaceIsKeptWithItsEvents() async throws {
        let cache = try await cache()
        defer { Task { try? await cache.forgetAll() } }
        try await cache.putIndex([SessionCacheTests.session("s", updatedMs: 2), SessionCacheTests.session("t", updatedMs: 1)], listedAtMs: 1)
        let place = Data(#"{"next":1}"#.utf8)
        try await cache.putRead(session: "s", SessionCacheTests.events(0..<3), place: place)
        #expect(try await cache.place(session: "s") == place)
        #expect(try await cache.events(session: "s", limit: 10).count == 3)
        // Listed again: the place stays.
        try await cache.putIndex([SessionCacheTests.session("s", updatedMs: 3), SessionCacheTests.session("t", updatedMs: 1)], listedAtMs: 2)
        #expect(try await cache.place(session: "s") == place)

        try await cache.putCursor(session: "s", json: "{}")
        let again = Data(#"{"next":9}"#.utf8)
        try await cache.putRead(session: "s", SessionCacheTests.events(7..<9), place: again, replacing: true)
        #expect(try await cache.events(session: "s", limit: 10).map(\.id) == ["s-e8", "s-e7"])
        #expect(try await cache.cursor(session: "s") == nil)
        #expect(try await cache.place(session: "s") == again)

        // A session the index doesn't hold keeps no place.
        try await cache.putRead(session: "gone", SessionCacheTests.events(0..<1, in: "gone"), place: place)
        #expect(try await cache.place(session: "gone") == nil)

        try await cache.evict(keep: 0)
        #expect(try await cache.place(session: "s") == nil, "a place over no events")
        #expect(try await cache.index().map(\.id) == ["s", "t"])
    }
}
