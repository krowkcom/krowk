// R-IOS-1, R-HAND-4: how a session's live connection stands, as the bar
// under the navigation bar says it, from what the viewer showed and did;
// and the session screen follows its session under that bar, a strip over
// nothing, never a spinner over what's on screen.
import Synchronization
import Testing
import UIKit
@testable import KrowkKit

@Suite("R-IOS-1, R-HAND-4: the relay status bar")
struct RelayStatusTests {
    @Test("R-IOS-1: connecting, then live once a host welcomes the phone; a drop reads reconnecting until the next join says where the host is")
    func liveAndReconnecting() {
        var t = RelayStatusTracker()
        #expect(t.status == .connecting)
        t.dropped()
        #expect(t.status == .connecting, "a first join that fails is still connecting")
        t.host(true)
        #expect(t.status == .live)
        t.dropped()
        #expect(t.status == .reconnecting)
        t.host(true)
        #expect(t.status == .live)
        #expect(RelayStatus.live.text == "live")
        #expect(RelayStatus.connecting.text == "connecting")
        #expect(RelayStatus.reconnecting.text == "reconnecting")
    }

    @Test("R-HAND-4: joined with no host reads host offline, and a host arriving makes it live")
    func hostOffline() {
        var t = RelayStatusTracker()
        t.host(false)
        #expect(t.status == .hostOffline)
        #expect(t.status.text == "host offline")
        t.dropped()
        #expect(t.status == .reconnecting)
        t.host(false)
        t.host(true)
        #expect(t.status == .live)
    }

    @Test("R-IOS-1: the viewer's note is shown verbatim, and stays through the retries it announced")
    func unreachable() {
        var t = RelayStatusTracker()
        let note = "not on the relay wss://relay.krowk.com: refused; trying again every second"
        t.note(note)
        #expect(t.status.text == "can't reach the relay: \(note)")
        t.dropped()
        #expect(t.status == .unreachable(note))
        t.host(true)
        #expect(t.status == .live)
    }

    @Test("R-IOS-1: an end is the last word: nothing after it changes the status")
    func ended() {
        var t = RelayStatusTracker()
        t.host(true)
        t.ended("the session's key is gone")
        #expect(t.status.text == "stopped: the session's key is gone")
        t.host(true)
        t.note("x")
        t.dropped()
        #expect(t.status == .ended("the session's key is gone"))
        #expect(RelayStatus.ended(nil).text == "stopped")
    }

    @MainActor
    @Test("R-IOS-1: the session screen shows the bar under the navigation bar, over nothing, follows its session, and stops it once gone")
    func screen() async throws {
        let session = StubSession()
        let row = SessionRow(id: "s", title: "A demo session", detail: "app · ● live on host-a", live: true, accessibilityLabel: "A demo session")
        let screen = SessionPlaceholderViewController(row: row, session: session)
        screen.loadViewIfNeeded()
        #expect(screen.statusBar.superview === screen.view)
        #expect(screen.label.superview === screen.view)
        #expect(screen.statusBar.label.text == "connecting")
        #expect(!screen.view.subviews.contains { $0 is UIActivityIndicatorView }, "no spinner over the content")
        #expect(session.followed.withLock { $0 } == 1)
        session.say(.live)
        try await waitFor { screen.statusBar.status == .live }
        #expect(screen.statusBar.label.text == "live")
        #expect(screen.statusBar.accessibilityLabel == "live")
        session.say(.hostOffline)
        try await waitFor { screen.statusBar.status == .hostOffline }
        #expect(screen.statusBar.label.text == "host offline")

        // Gone (popped and released): the session stops.
        let other = StubSession()
        weak var gone: SessionPlaceholderViewController?
        do {
            let leaving = SessionPlaceholderViewController(row: row, session: other)
            leaving.loadViewIfNeeded()
            gone = leaving
        }
        #expect(gone == nil)
        #expect(other.stopped.withLock { $0 } == 1)
    }

    @MainActor
    @Test("R-IOS-1: with no session given, the screen is as before: no bar")
    func noSession() {
        let row = SessionRow(id: "s", title: "t", detail: "d", live: false, accessibilityLabel: "t")
        let screen = SessionPlaceholderViewController(row: row)
        screen.loadViewIfNeeded()
        #expect(screen.statusBar.superview == nil)
    }

    @MainActor
    private func waitFor(_ done: () -> Bool) async throws {
        for _ in 0..<200 where !done() {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(done())
    }
}

/// A session that says what the test tells it to.
final class StubSession: LiveSession {
    let followed = Mutex(0)
    let stopped = Mutex(0)
    private let told = Mutex<(@MainActor @Sendable (RelayStatus) -> Void)?>(nil)

    func follow(status: @escaping @MainActor @Sendable (RelayStatus) -> Void) {
        followed.withLock { $0 += 1 }
        told.withLock { $0 = status }
    }

    func stop() {
        stopped.withLock { $0 += 1 }
    }

    func takeFrame() -> Int { 0 }

    func say(_ status: RelayStatus) {
        guard let told = told.withLock({ $0 }) else { return }
        Task { @MainActor in told(status) }
    }
}
