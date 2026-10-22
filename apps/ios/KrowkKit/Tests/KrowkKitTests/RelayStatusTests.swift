// R-IOS-1, R-HAND-4: how a session's live connection stands, as the bar
// under the navigation bar says it, from what the viewer showed and did;
// and the session screen (its transcript) follows its session under that
// bar, a strip above the transcript, never a spinner over what's on screen.
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

    @Test("R-IOS-1: a gap reads catching up… until its catch-up comes, a host welcoming meanwhile changes nothing, and a drop reads reconnecting")
    func catchingUp() {
        var t = RelayStatusTracker()
        t.gap()
        #expect(t.status == .connecting, "nothing to miss before a join")
        t.host(true)
        t.gap()
        #expect(t.status == .catchingUp)
        #expect(t.status.text == "catching up…")
        t.host(true)
        #expect(t.status == .catchingUp)
        t.caughtUp()
        #expect(t.status == .live)
        t.caughtUp()
        #expect(t.status == .live, "a catch-up with no gap (a welcome's) changes nothing")
        t.gap()
        t.dropped()
        #expect(t.status == .reconnecting)
        t.host(false)
        t.gap()
        #expect(t.status == .hostOffline, "no host to catch up from")
        t.host(true)
        t.gap()
        t.host(false)
        #expect(t.status == .hostOffline, "the host went while catching up: host offline")
    }

    @MainActor
    @Test("R-IOS-1: the session screen shows the bar under the navigation bar, above its transcript, follows its session, and stops it once gone")
    func screen() async throws {
        let session = StubSession()
        let row = SessionRow(id: "s", title: "A demo session", detail: "app · ● live on host-a", live: true, accessibilityLabel: "A demo session")
        let screen = TranscriptViewController(row: row, session: session)
        screen.loadViewIfNeeded()
        #expect(screen.statusBar.superview === screen.view)
        #expect(screen.collectionView.superview === screen.view)
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
        weak var gone: TranscriptViewController?
        autoreleasepool {
            let leaving = TranscriptViewController(row: row, session: other)
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
        let screen = TranscriptViewController(row: row)
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

/// A session that says what the test tells it to, and brings the updates
/// it is given at the next frame, waking the screen as a real one does.
final class StubSession: LiveSession {
    let followed = Mutex(0)
    let stopped = Mutex(0)
    private let told = Mutex<(@MainActor @Sendable (RelayStatus) -> Void)?>(nil)
    private let waker = Mutex<(@MainActor @Sendable () -> Void)?>(nil)
    /// What the next frame brings.
    private let pending = Mutex<[TranscriptUpdate]>([])
    /// Frames taken, and those that brought something.
    let taken = Mutex(0)

    func follow(status: @escaping @MainActor @Sendable (RelayStatus) -> Void) {
        followed.withLock { $0 += 1 }
        told.withLock { $0 = status }
    }

    func stop() {
        stopped.withLock { $0 += 1 }
    }

    func onUpdates(_ wake: @escaping @MainActor @Sendable () -> Void) {
        waker.withLock { $0 = wake }
    }

    @MainActor func takeFrame() -> [TranscriptUpdate] {
        taken.withLock { $0 += 1 }
        return pending.withLock { pending in
            defer { pending = [] }
            return pending
        }
    }

    /// `events` at the next frame, as the session read at rest when
    /// `attach`, else as logged lines.
    func bring(_ events: [TranscriptEvent], attach: Bool = false) {
        bring([attach ? .attached(events) : .logged(events)])
    }

    /// `updates` at the next frame; the screen is woken if none were
    /// waiting, on the main actor, as a real session does.
    func bring(_ updates: [TranscriptUpdate]) {
        let waiting = pending.withLock { pending in
            let waiting = !pending.isEmpty
            pending.append(contentsOf: updates)
            return waiting
        }
        guard !waiting, let wake = waker.withLock({ $0 }) else { return }
        Task { @MainActor in wake() }
    }

    /// The composer's commands, in the order sent, with their tokens.
    let commands = Mutex<[Sent]>([])
    struct Sent: Equatable, Sendable {
        let command: LiveCommand
        let token: UInt64
    }

    func send(_ command: LiveCommand, token: UInt64) {
        commands.withLock { $0.append(Sent(command: command, token: token)) }
    }

    func say(_ status: RelayStatus) {
        guard let told = told.withLock({ $0 }) else { return }
        Task { @MainActor in told(status) }
    }
}
