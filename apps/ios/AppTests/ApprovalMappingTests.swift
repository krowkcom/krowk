// R-PERM-2, R-IOS-11, R-PROTO-2: approvals over the generated types
// (ticket 91). The session's `approval.requested` lines, live or re-sent
// in a welcome, reach the screen's ApprovalQueue (ticket 89's
// ApprovalSink) once each; `approval.resolved` and a logged turn's end
// take them away. The answer is the generated `Command.approve` for the
// request's own session — a subagent's for its request — with its
// answers, never JSON written here, and only ever allow, allowSession or
// deny. The app's owner check is LAContext's unless a DEBUG build's UI
// test picks the stub; a Release build has no stub at all (ios.yml runs
// apps/ios/scripts/approvals.sh on one).
import Krowk
import KrowkAccount
@testable import KrowkApp
import KrowkKit
import Testing

/// An owner check that always says yes.
private struct Yes: OwnerCheck {
    @MainActor func confirm(reason: String) async -> Bool { true }
}

@MainActor
@Suite("R-PERM-2, R-IOS-11: approvals over the generated types")
struct ApprovalMappingTests {
    static let session = TranscriptModelTests.session
    static let child = TranscriptModelTests.child

    static func request(_ id: String, session: String = ApprovalMappingTests.session, turn: String = "t1") -> ApprovalRequest {
        ApprovalRequest(
            sessionId: session, turnId: turn, requestId: id, tool: "bash", input: #"{"command":"touch approved.txt"}"#,
            summary: "bash `touch approved.txt`", reason: "asks", remember: ["Bash(touch:*)"],
            questions: [Question(id: "q", header: "H", question: "Which?", options: [QuestionOption(label: "a", description: "first")], multiSelect: true, secret: false)])
    }

    @Test("R-PROTO-2: a generated ApprovalRequest is the sheet's prompt, field for field, its own session kept")
    func prompt() {
        let prompt = ApprovalPrompt(Self.request("r1", session: Self.child))
        #expect(prompt == ApprovalPrompt(
            sessionID: Self.child, turnID: "t1", requestID: "r1", tool: "bash", input: #"{"command":"touch approved.txt"}"#,
            summary: "bash `touch approved.txt`", reason: "asks", remember: ["Bash(touch:*)"],
            questions: [ApprovalQuestion(id: "q", header: "H", question: "Which?", options: [ApprovalOption(label: "a", description: "first")], multiSelect: true)]))
    }

    @Test("R-PERM-2: the answer is the generated approve command for the request's own session (a subagent's), with its answers")
    func command() {
        let reply = ApprovalReply(sessionID: Self.child, requestID: "r1", verdict: .allow, answers: [QuestionReply(id: "q", picked: ["a"], text: "and b")])
        #expect(LiveCommand.approve(reply).command(session: Self.session) == .approve(
            sessionId: Self.child, requestId: "r1", decision: .allow,
            answers: [QuestionAnswer(id: "q", picked: ["a"], text: "and b")]))
        let denied = ApprovalReply(sessionID: Self.session, requestID: "r2", verdict: .deny)
        #expect(LiveCommand.approve(denied).command(session: Self.session) == .approve(sessionId: Self.session, requestId: "r2", decision: .deny, answers: []))
    }

    @Test("R-IOS-11: the phone's answers are only allow, allowSession and deny — never a rule for the project")
    func decisions() {
        let all: Set<ApprovalDecision> = [ApprovalVerdict.allow, .allowSession, .deny].reduce(into: []) { $0.insert($1.decision) }
        #expect(all == [.allow, .allowSession, .deny])
    }

    @Test("R-PERM-2: the same request from the welcome and live reaches the queue once; resolved, it goes; a logged turn's end drops what it asked")
    func throughTheSink() throws {
        let queue = ApprovalQueue(sessionID: Self.session, ownerCheck: Yes())
        let line = { (r: ApprovalRequest) in MailboxItem.viewer(.line(line: .live(.approvalRequested(r)))) }
        _ = TranscriptUpdate.from(posted: [.viewer(.host(present: true)), line(Self.request("r1")), line(Self.request("r2", turn: "t2"))], approvals: queue)
        _ = TranscriptUpdate.from(posted: [line(Self.request("r1"))], approvals: queue)
        #expect(queue.held.map(\.prompt.requestID) == ["r1", "r2"])

        _ = TranscriptUpdate.from(posted: [.viewer(.line(line: .live(.approvalResolved(sessionId: Self.session, turnId: "t1", requestId: "r1", decision: .allowSession))))], approvals: queue)
        #expect(queue.held.map(\.prompt.requestID) == ["r2"])

        let end = try decodeLogEvent(json: TranscriptModelTests.line("e90", #""type":"turn.completed","turnId":"t2","status":"interrupted","usage":\#(TranscriptModelTests.usage),"durationMs":1"#))
        _ = TranscriptUpdate.from(posted: [.viewer(.line(line: .log(end)))], approvals: queue)
        #expect(queue.held.isEmpty)
        // Re-sent after its turn ended (a stale welcome): never shown.
        _ = TranscriptUpdate.from(posted: [line(Self.request("r3", turn: "t2"))], approvals: queue)
        #expect(queue.held.isEmpty)
    }

    @Test("R-PERM-2 (ticket 145): the core's approvalGone, said at a rejoin for a request resolved while the phone was away, closes it on the sheet; an empty caughtUp draws nothing")
    func goneThroughTheSink() {
        let queue = ApprovalQueue(sessionID: Self.session, ownerCheck: Yes())
        let line = { (r: ApprovalRequest) in MailboxItem.viewer(.line(line: .live(.approvalRequested(r)))) }
        _ = TranscriptUpdate.from(posted: [.viewer(.host(present: true)), line(Self.request("r1")), line(Self.request("r2"))], approvals: queue)
        #expect(queue.held.map(\.prompt.requestID) == ["r1", "r2"])
        let shown = TranscriptUpdate.from(posted: [.viewer(.approvalGone(requestId: "r1")), .viewer(.caughtUp(events: []))], approvals: queue)
        #expect(queue.held.map(\.prompt.requestID) == ["r2"])
        #expect(shown.isEmpty)
    }

    @Test("R-IOS-11: the app's owner check is LAContext's; only a DEBUG build's launch argument picks the UI tests' stub")
    func ownerCheck() {
        #expect(OwnerChecks.make(arguments: []) is DeviceOwnerCheck)
        #expect(OwnerChecks.make(arguments: ["Krowk", "-KrowkOwnerCheck"]) is DeviceOwnerCheck, "no value: no stub")
        #if DEBUG
        #expect((OwnerChecks.make(arguments: ["Krowk", "-KrowkOwnerCheck", "yes"]) as? StubOwnerCheck)?.answer == true)
        #expect((OwnerChecks.make(arguments: ["Krowk", "-KrowkOwnerCheck", "no"]) as? StubOwnerCheck)?.answer == false)
        #endif
    }
}
