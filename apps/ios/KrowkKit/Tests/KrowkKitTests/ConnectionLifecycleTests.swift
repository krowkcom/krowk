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
        let screen = TranscriptViewController(row: SessionRow(id: "s", title: "t", detail: "", live: true, accessibilityLabel: "t"), session: session)
        LiveSessions.shared.willEnterForeground()
        #expect(session.counts.withLock { $0 } == LifecycleCounts(foreground: 1))
        #expect(!screen.isViewLoaded, "no view was needed")
        LiveSessions.shared.didBecomeActive()
        LiveSessions.shared.didEnterBackground()
        LiveSessions.shared.wake()
        #expect(session.counts.withLock { $0 } == LifecycleCounts(foreground: 1, background: 1, wake: 2))
        LiveSessions.shared.remove(session)
        LiveSessions.shared.willEnterForeground()
        #expect(session.counts.withLock { $0 }.foreground == 1)
        _ = screen
    }

    @Test("R-IOS-2: the registry holds its sessions weakly: one let go is never told again, nor kept")
    func weak() {
        weak var gone: CountingSession?
        do {
            let session = CountingSession()
            gone = session
            LiveSessions.shared.add(session)
        }
        #expect(gone == nil)
        LiveSessions.shared.willEnterForeground()
    }

    @Test("R-OFF-2: NWPathMonitor's first callback, the path as it is when it starts, is no change; every one after is")
    func firstPathIsNoChange() {
        var paths = PathChanges()
        let seen = [paths.changed(), paths.changed(), paths.changed()]
        #expect(seen == [false, true, true])
    }

    @Test("R-IOS-1: waiting on the backoff, the bar counts down to the next try in whole seconds, keeping why the last failed; never a spinner")
    func countdown() {
        let now = ContinuousClock.now
        #expect(RelayStatus.retrying(at: now + .milliseconds(3200), why: nil).text(at: now) == "reconnecting in 4s")
        #expect(RelayStatus.retrying(at: now + .seconds(1), why: nil).text(at: now) == "reconnecting in 1s")
        #expect(RelayStatus.retrying(at: now, why: nil).text(at: now) == "reconnecting")
        let why = "couldn't open the session: krowk's registry cannot be reached"
        #expect(RelayStatus.retrying(at: now + .seconds(2), why: why).text(at: now) == "\(why) — trying again in 2s")
        #expect(RelayStatus.retrying(at: now - .seconds(1), why: why).text(at: now) == "\(why) — trying again now")

        var t = RelayStatusTracker()
        t.host(true)
        t.dropped()
        let at = now + .seconds(5)
        t.retry(at: at)
        #expect(t.status == .retrying(at: at, why: nil))
        t.note("not on the relay ws://127.0.0.1:1: refused")
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
