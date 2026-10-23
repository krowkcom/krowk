// R-PERM-2, R-LAG-8, R-IOS-5: the approval sheet over the transcript
// screen (ticket 91), on a stub session, in a window of the host app's
// scene, where a sheet rises (a SwiftPM test's window has no scene, and
// UIKit never finishes presenting there). A request raises the sheet and
// marks its call; an answer goes as the session's command for the
// request's own session, the row reads "allowed — sending…" at once and
// "allowed" on the ack; an ack's error brings the request back with the
// host's sentence; answered on another device, it goes with a toast; put
// away, it waits behind the hand; settled while rising, it goes once
// risen. The sheet alone and the queue are KrowkKit's ApprovalSheetTests
// and ApprovalQueueTests; through the fleet, UITests' ApprovalUITests.
import KrowkKit
import QuartzCore
import Synchronization
import Testing
import UIKit

/// An owner check that always says yes.
private struct AlwaysYes: OwnerCheck {
    @MainActor func confirm(reason: String) async -> Bool { true }
}

/// A session that brings what a test hands it and keeps what it was sent.
final class ScreenStubSession: LiveSession {
    private let told = Mutex<(@MainActor @Sendable (RelayStatus) -> Void)?>(nil)
    private let waker = Mutex<(@MainActor @Sendable () -> Void)?>(nil)
    private let pending = Mutex<[TranscriptUpdate]>([])
    struct Sent: Equatable, Sendable {
        let command: LiveCommand
        let token: UInt64
    }
    let commands = Mutex<[Sent]>([])

    func follow(status: @escaping @MainActor @Sendable (RelayStatus) -> Void) {
        told.withLock { $0 = status }
    }

    func stop() {}

    func onUpdates(_ wake: @escaping @MainActor @Sendable () -> Void) {
        waker.withLock { $0 = wake }
    }

    @MainActor func takeFrame() -> [TranscriptUpdate] {
        pending.withLock { pending in
            defer { pending = [] }
            return pending
        }
    }

    func send(_ command: LiveCommand, token: UInt64) {
        commands.withLock { $0.append(Sent(command: command, token: token)) }
    }

    /// `updates` at the next frame.
    func bring(_ updates: [TranscriptUpdate]) {
        pending.withLock { $0.append(contentsOf: updates) }
        if let wake = waker.withLock({ $0 }) { Task { @MainActor in wake() } }
    }

    func say(_ status: RelayStatus) {
        guard let told = told.withLock({ $0 }) else { return }
        Task { @MainActor in told(status) }
    }
}

@MainActor
@Suite("R-PERM-2, R-LAG-8: the approval sheet over the transcript", .serialized)
struct ApprovalScreenTests {
    /// One frame's apply now, as the display link would.
    static func frame(_ screen: TranscriptViewController) {
        screen.driver?.tick(at: CACurrentMediaTime())
    }

    static let session = "s1"
    let row = SessionRow(id: ApprovalScreenTests.session, title: "A demo session", detail: "app", live: true, accessibilityLabel: "A demo session")

    static func event(_ id: String, _ change: TranscriptChange?) -> TranscriptEvent {
        TranscriptEvent(id: id, sessionID: session, change: change)
    }

    static func call(_ id: String, turn: String = "turn2") -> ApprovalPrompt {
        ApprovalPrompt(sessionID: session, turnID: turn, requestID: id, tool: "bash", input: #"{"command":"touch approved.txt"}"#, summary: "bash `touch approved.txt`", reason: "bash isn't allowed by a rule")
    }

    /// The screen live, its turn under way with a bash call waiting.
    func opened() async throws -> (ScreenStubSession, ApprovalQueue, TranscriptViewController, UIWindow) {
        let session = ScreenStubSession()
        let queue = ApprovalQueue(sessionID: Self.session, ownerCheck: AlwaysYes())
        let screen = TranscriptViewController(row: row, session: session, approvals: queue)
        let scene = try #require(UIApplication.shared.connectedScenes.first as? UIWindowScene, "no window scene in the host app")
        let window = UIWindow(windowScene: scene)
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        window.layoutIfNeeded()
        session.bring([.attached([
            Self.event("e1", .userText(item: "u1", text: "Please read the README", images: [])),
            Self.event("e2", .turnCompleted(turn: "turn1", end: .completed)),
            Self.event("f1", .turnStarted(turn: "turn2")),
            Self.event("f2", .userText(item: "u2", text: "Please make the file approved.txt", images: [])),
            Self.event("f3", .toolCall(item: "t9", callID: "toolu_01Touch", name: "bash", input: #"{"command":"touch approved.txt"}"#)),
        ])])
        Self.frame(screen)
        session.say(.live)
        try await waitFor { screen.drewRows && screen.statusBar.status == .live && queue.connected }
        return (session, queue, screen, window)
    }

    @Test("R-PERM-2, R-LAG-8: a request raises the sheet and marks its call; Allow once sends the approve for the request's session at once, the row reads allowed — sending…, then allowed on the ack")
    func allowOverTheTranscript() async throws {
        let (session, queue, screen, window) = try await opened()
        defer { window.isHidden = true }
        queue.requested(Self.call("r1", turn: "turn2"))
        let sheet = try await risen(screen)
        #expect(screen.model.item("item:t9")?.verdict == "waiting for approval")
        try await waitFor { sheet.armed }
        sheet.allowButton.sendActions(for: .primaryActionTriggered)
        try await waitFor { !session.commands.withLock { $0 }.isEmpty }
        let sent = try #require(session.commands.withLock { $0 }.first)
        #expect(sent.command == .approve(ApprovalReply(sessionID: "s1", requestID: "r1", verdict: .allow)))
        #expect(screen.approvalSheet == nil, "the sheet goes at once")
        #expect(screen.model.item("item:t9")?.verdict == "allowed — sending…")
        #expect(screen.model.commandsOut == 1, "a screen left now is kept until the ack")

        session.bring([.commandMinted(token: sent.token, id: "dev-1"), .commandSent(id: "dev-1", queued: false), .commandAcked(id: "dev-1", error: nil)])
        Self.frame(screen)
        #expect(screen.model.item("item:t9")?.verdict == "allowed")
        #expect(queue.held.isEmpty)
        #expect(screen.model.commandsOut == 0)
    }

    @Test("R-LAG-8: an ack's error brings the request back over the transcript with the host's sentence")
    func ackErrorBringsItBack() async throws {
        let (session, queue, screen, window) = try await opened()
        defer { window.isHidden = true }
        queue.requested(Self.call("r1", turn: "turn2"))
        let sheet = try await risen(screen)
        try await waitFor { sheet.armed }
        sheet.denyButton.sendActions(for: .primaryActionTriggered)
        try await waitFor { !session.commands.withLock { $0 }.isEmpty }
        let sent = try #require(session.commands.withLock { $0 }.first)
        #expect(screen.model.item("item:t9")?.verdict == "denied — sending…")
        let refusal = "a viewer of session s1 may prompt, steer, interrupt or answer an approval of that session, or interrupt or answer an approval of one of its own subagents, and nothing else"
        session.bring([.commandMinted(token: sent.token, id: "dev-1"), .commandAcked(id: "dev-1", error: refusal)])
        Self.frame(screen)
        let again = try await risen(screen)
        #expect(again.noteLabel.text == "Your answer wasn't taken: \(refusal)")
        #expect(screen.model.item("item:t9")?.verdict == "waiting for approval")
    }

    @Test("R-PERM-2: approval.resolved from another device takes the sheet down with a toast; the row says how it went")
    func answeredElsewhere() async throws {
        let (_, queue, screen, window) = try await opened()
        defer { window.isHidden = true }
        queue.requested(Self.call("r1", turn: "turn2"))
        #expect(screen.approvalSheet != nil)
        queue.resolved(requestID: "r1", resolution: .allowed)
        #expect(screen.approvalSheet == nil)
        #expect(screen.toast.text?.contains("Answered on another device: allowed") == true)
        #expect(screen.model.item("item:t9")?.verdict == "allowed on another device")
    }

    @Test("R-PERM-2: put away, the sheet waits behind the hand in the navigation bar; a new request raises it again")
    func putAway() async throws {
        let (_, queue, screen, window) = try await opened()
        defer { window.isHidden = true }
        queue.requested(Self.call("r1", turn: "turn2"))
        let sheet = try await risen(screen)
        // As a swipe down does: the sheet gone, then its delegate told.
        sheet.dismiss(animated: false)
        try await waitFor { screen.presentedViewController == nil }
        screen.presentationControllerDidDismiss(try #require(sheet.presentationController))
        #expect(screen.approvalSheet == nil)
        #expect(screen.navigationItem.rightBarButtonItem?.accessibilityLabel == "1 approval waiting")
        queue.requested(Self.call("r2", turn: "turn2"))
        _ = try await risen(screen)
    }

    @Test("R-PERM-2: a request settled while its sheet is still rising takes the sheet down once it has risen")
    func settledWhileRising() async throws {
        let (_, queue, screen, window) = try await opened()
        defer { window.isHidden = true }
        queue.requested(Self.call("r1", turn: "turn2"))
        #expect(screen.approvalSheet != nil)
        queue.resolved(requestID: "r1", resolution: .denied)
        #expect(screen.approvalSheet == nil)
        try await waitFor { screen.presentedViewController == nil }
    }

    @Test("R-PERM-2: the turn's end takes the sheet down and clears the call's line")
    func turnEnd() async throws {
        let (_, queue, screen, window) = try await opened()
        defer { window.isHidden = true }
        queue.requested(Self.call("r1", turn: "turn2"))
        #expect(screen.approvalSheet != nil)
        queue.turnEnded(sessionID: "s1", turnID: "turn2")
        #expect(screen.approvalSheet == nil)
        #expect(screen.model.item("item:t9")?.verdict == "")
        #expect(screen.navigationItem.rightBarButtonItem == nil)
    }

    /// The screen's sheet, once it has finished rising.
    private func risen(_ screen: TranscriptViewController) async throws -> ApprovalSheetController {
        try await waitFor { screen.approvalSheet.map { screen.presentedViewController === $0 && !$0.isBeingPresented } ?? false }
        let sheet = try #require(screen.approvalSheet)
        sheet.loadViewIfNeeded()
        return sheet
    }

    private func waitFor(_ done: () -> Bool) async throws {
        for _ in 0..<500 where !done() {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(done())
    }
}
