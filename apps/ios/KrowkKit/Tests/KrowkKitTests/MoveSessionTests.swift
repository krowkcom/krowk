// R-HAND-4, R-LAG-8: moving a session to another host over a stubbed host
// source: Move to… lists the online hosts but the holder, each with its
// directories; a move reads `moving to <host>…` at once, then `moved to
// <host>` with the new holder, or the host's sentence with the session
// where it was; a cancelled wait says nothing. No fleet: the hosted
// MoveSessionTests and the CLI's r_hand_4 test cover the real hosts.
import Foundation
import Synchronization
import Testing
import UIKit
@testable import KrowkKit

/// Hosts whose take answers `outcome` once released, noting each take and
/// whether it ran off the main thread.
final class StubMover: HostSource {
    private let outcome: Mutex<MoveOutcome>
    private let takes = Mutex<[(MoveRequest, Bool)]>([])
    private let gate = DispatchSemaphore(value: 0)
    let cancels = Mutex(0)

    init(_ outcome: MoveOutcome) {
        self.outcome = Mutex(outcome)
    }

    func release() { gate.signal() }
    var taken: [(MoveRequest, Bool)] { takes.withLock { $0 } }

    func hosts() throws -> HostListing { HostListing(hosts: [], unreadable: 0) }
    func start(_ request: StartRequest) -> StartOutcome { .refused(code: "x", message: "x") }
    func newCommandID() -> String { UUID().uuidString.lowercased() }

    func take(_ request: MoveRequest) -> MoveOutcome {
        takes.withLock { $0.append((request, Thread.isMainThread)) }
        gate.wait()
        return outcome.withLock { $0 }
    }

    func cancelTake() {
        cancels.withLock { $0 += 1 }
        gate.signal()
    }
}

@MainActor
@Suite("R-HAND-4: moving a session to another host")
struct MoveSessionTests {
    let request = MoveRequest(id: "c1", device: "b1", hostName: "laptop", sessionID: "s1", cwd: "/home/ann/code/app")

    /// Waits for `condition`, a turn of the main actor at a time, up to 5 s.
    func until(_ condition: () -> Bool) async {
        let deadline = Date.now.addingTimeInterval(5)
        while !condition(), Date.now < deadline {
            try? await Task.sleep(for: .milliseconds(10))
        }
    }

    @Test("R-HAND-4: Move to… lists online hosts but the holder, each with its directories, home as ~")
    func sections() {
        let hosts: [any AnnouncedHost] = [
            StubHost(device: "a1", name: "desktop"),
            StubHost(device: "b1", name: "laptop", directories: ["/home/ann/code/app", "/home/ann/code/clone"]),
            StubHost(device: "c1", name: "old", online: false),
        ]
        let built = MovePickerViewController.sections(hosts: hosts, listed: true, failed: false, holder: "a1")
        #expect(built.hosts.map { $0.name } == ["laptop"])
        #expect(built.sections.map { $0.header } == ["laptop"])
        let titles = built.sections[0].choices.map { $0.title }
        #expect(titles == ["~/code/app", "~/code/clone"])
        let values = built.sections[0].choices.map { $0.value }
        #expect(values == ["/home/ann/code/app", "/home/ann/code/clone"])
        #expect(built.sections[0].footer?.contains("clone of this session's repository") == true)

        let alone = MovePickerViewController.sections(hosts: [StubHost(device: "a1")], listed: true, failed: false, holder: "a1")
        #expect(alone.hosts.isEmpty)
        #expect(alone.sections.first?.footer?.hasPrefix("No other host is online") == true)
        let unread = MovePickerViewController.sections(hosts: hosts, listed: false, failed: true, holder: "a1")
        #expect(unread.hosts.isEmpty)
    }

    @Test("R-HAND-4, R-LAG-8: moving to <host>… at once, off the main thread, then moved to <host> and the holder is that host")
    func taken() async {
        let source = StubMover(.taken)
        let move = SessionMove(source: source, holder: "a1")
        #expect(move.move(request))
        #expect(move.barLine == "moving to laptop…")
        #expect(!move.move(request), "one move at a time")
        await until { !source.taken.isEmpty }
        #expect(source.taken.first?.1 == false, "the take runs off the main thread")
        source.release()
        await until { !move.isMoving }
        #expect(move.state == .moved(host: "laptop"))
        #expect(move.barLine == "moved to laptop")
        #expect(move.holder == "b1")
        #expect(!move.move(request), "no move to the holder")
    }

    @Test("R-HAND-4: a refusal is the host's sentence, the bar says nothing more, and the session stays")
    func refused() async {
        let source = StubMover(.refused(code: "take_refused", message: "anthropic isn't ready on laptop — the session stays where it is"))
        let move = SessionMove(source: source, holder: "a1")
        move.move(request)
        source.release()
        await until { !move.isMoving }
        #expect(move.refusal == "anthropic isn't ready on laptop — the session stays where it is")
        #expect(move.barLine == nil)
        #expect(move.holder == "a1")
        move.dismissRefusal()
        #expect(move.state == nil)
    }

    @Test("R-HAND-4: the screen going ends the wait, and says nothing of the move")
    func cancelled() async {
        let source = StubMover(.refused(code: "cancelled", message: "stopped waiting"))
        let move = SessionMove(source: source, holder: "a1")
        move.move(request)
        await until { !source.taken.isEmpty }
        move.stopWaiting()
        #expect(source.cancels.withLock { $0 } == 1)
        await until { !move.isMoving }
        #expect(move.state == nil)
        #expect(move.holder == "a1")
    }
}
