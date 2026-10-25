// R-IOS-2, R-IOS-7, R-HAND-4 (ticket 132) with no fleet: a connection
// started before its screen is held by `LiveSessions` until the screen
// takes it, given once, and stopped when another session's screen comes
// or none does; the list's read starts before its screen, which then reads
// no second time; a cold launch whose read fails keeps the kept rows, last
// seen and never live, and says so; and a transcript whose host is away —
// none on the session, or none known with the relay out of reach — shows
// the offline notice, stays readable and queues a prompt, until live. The
// hosted ConnectEarlyTests run the same against the fleet.
import Foundation
import Testing
import UIKit
@testable import KrowkKit

@MainActor
@Suite("R-IOS-2: connect early")
struct ConnectEarlyLifecycleTests {
    @Test("R-IOS-2: an early connection is followed at once and hears the lifecycle; its screen takes it once; the same session is never started twice")
    func testR_IOS_2_anEarlyConnectionIsTakenByItsScreenOnce() {
        let lifecycle = LiveSessions()
        let session = StubSession()
        #expect(lifecycle.startEarly(LiveSessions.Early(id: "a", session: session)))
        #expect(session.followed.withLock { $0 } == 1, "not started")
        #expect(lifecycle.follows("a"))
        #expect(!lifecycle.follows("b"))
        // Again (the foreground after the launch): kept, nothing new.
        let again = StubSession()
        #expect(!lifecycle.startEarly(LiveSessions.Early(id: "a", session: again)))
        #expect(again.followed.withLock { $0 } == 0)
        #expect(lifecycle.early?.session === session)

        let taken = lifecycle.takeEarly("a")
        #expect(taken?.session === session)
        #expect(lifecycle.takeEarly("a") == nil, "given twice")
        #expect(session.stopped.withLock { $0 } == 0)
        // Its screen holds it from here: still followed, so not started again.
        let screen = TranscriptViewController(row: SessionRow(id: "a", title: "A", detail: "", live: false, accessibilityLabel: "A"), session: session, lifecycle: lifecycle)
        #expect(lifecycle.follows("a"))
        #expect(!lifecycle.startEarly(LiveSessions.Early(id: "a", session: again)))
        _ = screen
    }

    @Test("R-IOS-2: another session's screen stops the early connection; so does a screen that never comes, and the phone gone from the person's devices")
    func testR_IOS_2_anEarlyConnectionNoScreenTakesIsStopped() async throws {
        let lifecycle = LiveSessions(earlyFor: .milliseconds(50))
        let first = StubSession()
        lifecycle.startEarly(LiveSessions.Early(id: "a", session: first))
        #expect(lifecycle.takeEarly("b") == nil)
        #expect(first.stopped.withLock { $0 } == 1)
        #expect(!lifecycle.follows("a"))

        let unseen = StubSession()
        lifecycle.startEarly(LiveSessions.Early(id: "a", session: unseen))
        try await waitFor { unseen.stopped.withLock { $0 } == 1 }
        #expect(lifecycle.early == nil)
        #expect(!lifecycle.follows("a"))

        // Ended before its screen came (the session gone): let go, and a
        // later start makes a new one.
        let lifecycleEnded = LiveSessions(earlyFor: .seconds(60))
        let ended = StubSession()
        lifecycleEnded.startEarly(LiveSessions.Early(id: "a", session: ended))
        ended.say(.ended("no such session"))
        try await waitFor { lifecycleEnded.early == nil }
        #expect(!lifecycleEnded.follows("a"))
        #expect(lifecycleEnded.startEarly(LiveSessions.Early(id: "a", session: StubSession())))

        let gone = StubSession()
        let shared = LiveSessions(earlyFor: .seconds(60))
        shared.startEarly(LiveSessions.Early(id: "a", session: gone))
        shared.stopAll()
        #expect(shared.early == nil)
        #expect(gone.stopped.withLock { $0 } == 1)
    }
}

@MainActor
@Suite("R-IOS-2, R-IOS-7: the list read before its screen")
struct EarlyListTests {
    let clock = TestClock()

    func listing(_ ids: [String], hostedBy: String? = nil) -> SessionListing {
        SessionListing(sessions: ids.map { Stub(id: $0, updatedMs: clock.ms - 12 * 60_000, hostedBy: hostedBy) }, unreadable: 0)
    }

    @Test("R-IOS-2: the read starts before the list's screen exists, after the kept listing; the screen appearing reads no second time")
    func testR_IOS_2_theListReadsBeforeItsScreen() async throws {
        let source = StubSource(.success(listing(["a", "b"], hostedBy: "host")), gated: true)
        defer { source.release() }
        let kept = listing(["a"])
        let clock = clock
        let model = SessionListModel(source: source, kept: { kept }, clock: { clock.now })
        model.readEarly()
        model.readEarly()
        try await waitFor { source.calls.count == 1 }
        #expect(model.listing?.sessions.map(\.id) == ["a"], "the kept listing first")

        let screen = SessionListViewController(model: model)
        screen.loadViewIfNeeded()
        screen.beginAppearanceTransition(true, animated: false)
        screen.endAppearanceTransition()
        defer {
            screen.beginAppearanceTransition(false, animated: false)
            screen.endAppearanceTransition()
        }
        source.release()
        try await waitFor { model.listedAt != nil }
        try await waitFor { screen.screen().rows.map(\.live) == [true, true] }
        #expect(source.calls.count == 1, "read again as the screen appeared")
    }

    @Test("R-IOS-7: a cold launch whose read fails keeps the kept rows, last seen and never live, and the footer says the list couldn't be read")
    func testR_IOS_7_aFailedColdReadKeepsTheKeptRowsLastSeen() async throws {
        let source = StubSource(.failure(.network(detail: "offline")))
        let kept = listing(["a", "b"], hostedBy: "host")
        let clock = clock
        let model = SessionListModel(source: source, kept: { kept }, clock: { clock.now })
        model.readEarly()
        try await waitFor { model.failure != nil }
        let screen = SessionListViewController(model: model)
        screen.loadViewIfNeeded()
        screen.throttle.rebuild()
        let shown = screen.screen()
        #expect(shown.status == .listed)
        #expect(shown.rows.map(\.id) == ["a", "b"])
        #expect(shown.rows.allSatisfy { !$0.live && $0.detail.contains("last seen 12m ago") }, "\(shown.rows.map(\.detail))")
        #expect(screen.footerText.hasPrefix("krowk's registry couldn't be reached."), "\(screen.footerText)")
        #expect(screen.footerText.contains("no session shows as live"))
        #expect(!screen.spinner.isAnimating)
    }
}

@MainActor
@Suite("R-IOS-7, R-HAND-4: a transcript whose host is away")
struct HostAwayScreenTests {
    func row(live: Bool) -> SessionRow {
        SessionRow(id: TranscriptScreenTests.session, title: "A demo session", detail: "", live: live, accessibilityLabel: "A demo session", lastSeen: "last seen 12m ago")
    }

    func shown(_ screen: TranscriptViewController) -> UIWindow {
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        window.layoutIfNeeded()
        return window
    }

    static let unreachable = RelayStatus.retrying(at: .now + .seconds(30), why: "can't reach the relay: krowk's registry cannot be reached")

    @Test("R-IOS-7, R-HAND-4: a row with no host known and the relay out of reach: the offline notice under the bar with last seen, the rows readable with no spinner, a prompt queued; live, the notice goes")
    func testR_IOS_7_outOfReachWithNoHostKnownIsOffline() async throws {
        let session = StubSession()
        let screen = TranscriptViewController(row: row(live: false), session: session)
        let window = shown(screen)
        defer { window.isHidden = true }
        session.bring(TranscriptScreenTests.turn, attach: true)
        screen.frame()
        session.say(Self.unreachable)
        try await waitFor { screen.noticeShown == .hostOffline }
        #expect(screen.hostAway)
        #expect(screen.notice.text?.contains("The host is offline (last seen 12m ago)") == true, "\(screen.notice.text ?? "")")
        #expect(screen.statusBar.label.text?.hasPrefix("can't reach the relay") == true, "\(screen.statusBar.label.text ?? "")")
        #expect(screen.drewRows)
        #expect(!screen.spinner.isAnimating)
        #expect(screen.collectionView.isScrollEnabled)
        #expect(screen.composer.noteLabel.text?.hasPrefix("host offline") == true, "\(screen.composer.noteLabel.text ?? "")")
        screen.composer.text = "Run the tests when you're back"
        #expect(screen.sendTyped(), "no prompt queues with the host away")
        let bubble = try #require(screen.model.ids.last)
        #expect(screen.model.item(bubble)?.caption == "queued — sends when your laptop is back")

        session.say(.live)
        try await waitFor { screen.noticeShown == nil }
        #expect(!screen.hostAway)
        #expect(screen.model.item(bubble)?.caption == "sending…")
    }

    @Test("R-IOS-7: a row a fresh listing named live, with the relay out of reach, is not called offline: the bar says why; no host on the session is, whatever the row said")
    func testR_IOS_7_aLiveRowOutOfReachIsNotOffline() async throws {
        let session = StubSession()
        let screen = TranscriptViewController(row: row(live: true), session: session)
        let window = shown(screen)
        defer { window.isHidden = true }
        session.say(Self.unreachable)
        try await waitFor { screen.statusBar.status == Self.unreachable }
        #expect(screen.noticeShown == nil)
        #expect(!screen.hostAway)
        session.say(.hostOffline)
        try await waitFor { screen.noticeShown == .hostOffline }
        #expect(screen.hostAway)
    }

    @Test("R-IOS-7: the notice reads last seen from the list it was opened from, as of now")
    func testR_IOS_7_theNoticeReadsLastSeenNow() async throws {
        let session = StubSession()
        let screen = TranscriptViewController(row: row(live: false), session: session)
        screen.lastSeen = { "last seen 14m ago" }
        let window = shown(screen)
        defer { window.isHidden = true }
        session.say(.hostOffline)
        try await waitFor { screen.noticeShown == .hostOffline }
        #expect(screen.notice.text?.contains("(last seen 14m ago)") == true, "\(screen.notice.text ?? "")")
    }
}

@MainActor
private func waitFor(_ done: () -> Bool) async throws {
    for _ in 0..<300 where !done() {
        try await Task.sleep(for: .milliseconds(10))
    }
    #expect(done())
}
