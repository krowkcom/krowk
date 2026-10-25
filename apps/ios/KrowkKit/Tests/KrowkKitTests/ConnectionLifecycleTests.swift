// R-IOS-2, R-OFF-2, R-IOS-1 (ticket 86): the app's lifecycle reaches every
// session followed live — a screen's session from the moment the screen is
// made, before its view loads — and the bar counts down to the
// supervisor's next try rather than showing a spinner. The rules
// themselves (backoff, wake, background) are krowk-client's, tested on a
// fake clock in its ConnectionSupervisorTests.
import Synchronization
import Testing
import UIKit
@testable import KrowkKit

@Suite("R-IOS-2, R-OFF-2: the app's lifecycle reaches the sessions it follows")
@MainActor
struct ConnectionLifecycleTests {
    @Test("R-IOS-2: a session screen's session hears the foreground from the moment the screen is made, before its view loads; one stopped no longer does")
    func foregroundBeforeAnyView() {
        let session = CountingSession()
        let lifecycle = LiveSessions()
        let screen = TranscriptViewController(row: SessionRow(id: "s", title: "t", detail: "", live: true, accessibilityLabel: "t"), session: session, lifecycle: lifecycle)
        lifecycle.willEnterForeground()
        #expect(session.counts.withLock { $0 } == LifecycleCounts(foreground: 1))
        #expect(!screen.isViewLoaded, "no view was needed")
        lifecycle.didBecomeActive()
        lifecycle.didEnterBackground()
        #expect(lifecycle.inBackground)
        lifecycle.wake()
        #expect(session.counts.withLock { $0 } == LifecycleCounts(foreground: 1, background: 1, wake: 2))
        lifecycle.remove(session)
        lifecycle.willEnterForeground()
        #expect(!lifecycle.inBackground)
        #expect(session.counts.withLock { $0 }.foreground == 1)
        _ = screen
    }

    @Test("R-IOS-2: the registry holds its sessions weakly: one let go is never told again, nor kept")
    func weak() {
        let lifecycle = LiveSessions()
        weak var gone: CountingSession?
        do {
            let session = CountingSession()
            gone = session
            lifecycle.add(session)
        }
        #expect(gone == nil)
        lifecycle.willEnterForeground()
    }

    @Test("Ticket 86: in the background a screen applies what comes at once (its display link doesn't fire there), so nothing piles up in the mailbox before iOS suspends the app; in the foreground it waits for the link")
    func appliedInTheBackground() async throws {
        let session = StubSession()
        let lifecycle = LiveSessions()
        let screen = TranscriptViewController(row: SessionRow(id: TranscriptScreenTests.session, title: "t", detail: "", live: true, accessibilityLabel: "t"), session: session, lifecycle: lifecycle)
        // Loaded, never shown: no display link, so only the background
        // path can apply anything.
        screen.loadViewIfNeeded()
        lifecycle.didEnterBackground()
        session.bring(TranscriptScreenTests.turn, attach: true)
        try await waitFor { screen.model.attached }
        let taken = session.taken.withLock { $0 }
        #expect(taken >= 1)

        // Something waiting as the app goes to the background (it came
        // after the last frame, and woke nothing more): applied as it goes.
        lifecycle.willEnterForeground()
        session.bring([TranscriptScreenTests.event("e6", .userText(item: "u1b", text: "Before", images: []))])
        try await Task.sleep(for: .milliseconds(100))
        #expect(session.taken.withLock { $0 } == taken, "no frame in the foreground without the link")
        lifecycle.didEnterBackground()
        #expect(session.taken.withLock { $0 } == taken + 1, "drained as the app went to the background")
        lifecycle.willEnterForeground()
    }

    private func waitFor(_ done: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while !done() && ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(done())
    }

    @Test("R-OFF-2: NWPathMonitor's first callback, the path as it is when it starts, is no change; every one after is")
    func firstPathIsNoChange() {
        var paths = PathChanges()
        let seen = [paths.changed(), paths.changed(), paths.changed()]
        #expect(seen == [false, true, true])
    }

    @Test("R-IOS-1, R-NET-2: waiting on the backoff, the bar counts down to the next try in whole seconds, keeping why the last failed; never a spinner; live, it names the path")
    func countdown() {
        let now = ContinuousClock.now
        #expect(RelayStatus.retrying(at: now + .milliseconds(3200), why: nil).text(at: now) == "reconnecting in 4s")
        #expect(RelayStatus.retrying(at: now + .seconds(1), why: nil).text(at: now) == "reconnecting in 1s")
        #expect(RelayStatus.retrying(at: now, why: nil).text(at: now) == "reconnecting")
        let why = "couldn't open the session: krowk's registry cannot be reached"
        #expect(RelayStatus.retrying(at: now + .seconds(2), why: why).text(at: now) == "\(why) — trying again in 2s")
        #expect(RelayStatus.retrying(at: now - .seconds(1), why: why).text(at: now) == "\(why) — trying again now")

        let at = now + .seconds(5)
        var first = RelayStatusTracker()
        first.retry(at: at)
        #expect(first.status == .retrying(at: at, why: nil), "the first join failing counts down too")

        var t = RelayStatusTracker()
        t.host(true)
        t.dropped()
        t.retry(at: at)
        #expect(t.status == .retrying(at: at, why: nil))
        // As the viewer says them: the wait, then its reason (a later Show).
        t.note("not on the relay ws://127.0.0.1:1: refused")
        #expect(t.status == .retrying(at: at, why: "can't reach the relay: not on the relay ws://127.0.0.1:1: refused"), "the countdown kept")
        t.retry(at: at)
        #expect(t.status == .retrying(at: at, why: "can't reach the relay: not on the relay ws://127.0.0.1:1: refused"))
        let later = at + .seconds(5)
        t.retry(at: later)
        #expect(t.status == .retrying(at: later, why: "can't reach the relay: not on the relay ws://127.0.0.1:1: refused"), "the reason kept through the next wait")
        t.host(true)
        #expect(t.status == .live)
        t.retry(at: at)
        #expect(t.status == .live, "a wait only follows a drop or a failure")

        var opening = RelayStatusTracker()
        opening.openFailed("krowk's registry cannot be reached", retryAt: at)
        #expect(opening.status == .retrying(at: at, why: why))
        opening.opened()
        #expect(opening.status == .connecting, "opened: the old reason gone")
        opening.host(true)
        #expect(opening.status == .live)
        opening.ended("gone")
        opening.openFailed("x", retryAt: at)
        #expect(opening.status == .ended("gone"))

        let bar = RelayStatusBar()
        bar.show(.retrying(at: ContinuousClock.now + .seconds(10), why: nil))
        #expect(bar.label.text?.hasPrefix("reconnecting in ") == true, "\(bar.label.text ?? "")")
        #expect(bar.accessibilityLabel == bar.label.text)
        #expect(bar.dot.backgroundColor == .systemYellow)
        bar.show(.retrying(at: ContinuousClock.now + .seconds(10), why: why))
        #expect(bar.dot.backgroundColor == .systemRed)
        bar.show(.live)
        #expect(bar.label.text == "live")
        // R-NET-2: live, the bar names the path the session comes by.
        bar.show(path: "direct over LAN")
        #expect(bar.label.text == "live · direct over LAN")
        #expect(bar.accessibilityLabel == "live")
        #expect(bar.accessibilityValue == "direct over LAN")
        bar.show(.reconnecting)
        #expect(bar.label.text == "reconnecting")
        #expect(bar.accessibilityValue == nil)
        bar.show(path: "relay")
        bar.show(.live)
        #expect(bar.label.text == "live · relay")
    }
}

struct LifecycleCounts: Equatable, Sendable {
    var foreground = 0
    var background = 0
    var wake = 0
}

/// A session that counts the lifecycle it is told of.
final class CountingSession: LiveSession {
    let counts = Mutex(LifecycleCounts())
    func follow(status: @escaping @MainActor @Sendable (RelayStatus) -> Void) {}
    func stop() {}
    func onUpdates(_ wake: @escaping @MainActor @Sendable () -> Void) {}
    @MainActor func takeFrame() -> [TranscriptUpdate] { [] }
    func send(_ command: LiveCommand, token: UInt64) {}
    func foreground() { counts.withLock { $0.foreground += 1 } }
    func background() { counts.withLock { $0.background += 1 } }
    func wake() { counts.withLock { $0.wake += 1 } }
}
