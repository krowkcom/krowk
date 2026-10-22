// R-PERM-2, R-IOS-11, R-IOS-10, R-LAG-8: the approval sheet (ticket 91)
// and the transcript screen it rises over, on a stub session. The sheet
// shows the tool, its summary cleaned of control and bidi characters, why
// it asks and (on a tap) its input; Allow for this session only with rules
// to remember; buttons that take no tap until the request has been up a
// moment; questions as option rows and a field (secure for a secret);
// ⌘↵ allows once through the same checks. Over the screen: a request
// raises the sheet, an answer goes as the session's command with the
// request's own session, the call's row reads "allowed — sending…" at
// once and "allowed" on the ack; an ack's error brings the request back
// with the host's sentence; answered on another device, it goes with a
// toast. The queue's rules are ApprovalQueueTests.
import Testing
import UIKit
@testable import KrowkKit

@MainActor
@Suite("R-PERM-2, R-IOS-11: the approval sheet")
struct ApprovalSheetTests {
    /// A sheet over `queue`, armed at once, in a window of its own.
    func sheet(_ queue: ApprovalQueue, armDelay: TimeInterval = 0) -> (ApprovalSheetController, UIWindow) {
        let sheet = ApprovalSheetController(queue: queue)
        sheet.armDelay = armDelay
        queue.onChange = { [weak sheet] in sheet?.refresh() }
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = sheet
        window.makeKeyAndVisible()
        sheet.loadViewIfNeeded()
        sheet.refresh()
        window.layoutIfNeeded()
        return (sheet, window)
    }

    @Test("R-PERM-2, R-IOS-10: a call shows its tool, its summary and reason cleaned, the input on a tap; Allow once and Deny, and no Allow for this session without rules to remember")
    func callSheet() {
        let h = ApprovalHarness()
        h.queue.requested(ApprovalPrompt(
            sessionID: "s1", turnID: "turn1", requestID: "r1", tool: "bash",
            input: #"{"command":"rm -rf build"}"#,
            summary: "bash `rm -rf build`\u{202E}txt.exe\u{1B}[2J", reason: "bash isn't\u{0007} allowed by a rule"))
        let (sheet, window) = sheet(h.queue)
        defer { window.isHidden = true }
        #expect(sheet.toolLabel.text == "bash")
        #expect(sheet.summaryLabel.text == "bash `rm -rf build`txt.exe[2J", "control and bidi characters stripped")
        #expect(sheet.reasonLabel.text == "Why it asks: bash isn't allowed by a rule")
        #expect(!sheet.allowButton.isHidden && !sheet.denyButton.isHidden)
        #expect(sheet.allowSessionButton.isHidden, "no rules to remember: no Allow for this session")
        #expect(sheet.answerButton.isHidden && sheet.declineButton.isHidden)
        #expect(sheet.positionLabel.isHidden)
        #expect(sheet.allowButton.configuration?.image != nil, "Face ID's glyph on an allow that asks for it")
        #expect(sheet.inputLabel.superview?.isHidden == true, "the input shows on a tap")
        sheet.inputButton.sendActions(for: .primaryActionTriggered)
        #expect(sheet.inputLabel.superview?.isHidden == false)
        #expect(sheet.inputLabel.text == "{\n  \"command\" : \"rm -rf build\"\n}")
    }

    @Test("R-IOS-11: Allow for this session shows only when the request remembers rules, and lists them")
    func allowSessionWithRemember() {
        let h = ApprovalHarness()
        h.queue.requested(ApprovalQueueTests.call("r1", remember: ["Bash(make:*)", "Bash(npm test)"]))
        h.queue.requested(ApprovalQueueTests.call("r2"))
        let (sheet, window) = sheet(h.queue)
        defer { window.isHidden = true }
        #expect(!sheet.allowSessionButton.isHidden)
        #expect(sheet.rememberLabel.text == "Allow for this session remembers: Bash(make:*), Bash(npm test)")
        #expect(sheet.positionLabel.text == "1 of 2")
    }

    @Test("R-IOS-10: a request's input shows at most 16 KiB, then how much more")
    func inputCapped() {
        let h = ApprovalHarness()
        let big = String(repeating: "x", count: 40 * 1024)
        h.queue.requested(ApprovalPrompt(sessionID: "s1", turnID: "t", requestID: "r1", tool: "write", input: big, summary: "write big.txt", reason: ""))
        let (sheet, window) = sheet(h.queue)
        defer { window.isHidden = true }
        let shown = sheet.inputLabel.text ?? ""
        #expect(shown.hasPrefix(String(repeating: "x", count: 16 * 1024)))
        #expect(shown.hasSuffix(SafeText.more(24 * 1024)))
    }

    @Test("R-PERM-2: a request's buttons take no tap until it has been up a moment, so a tap meant for the last one never answers it")
    func armed() async throws {
        let h = ApprovalHarness()
        h.queue.requested(ApprovalQueueTests.call("r1", tool: "read"))
        let (sheet, window) = sheet(h.queue, armDelay: 0.3)
        defer { window.isHidden = true }
        #expect(!sheet.armed)
        #expect(!sheet.allowButton.isEnabled)
        sheet.choose(.allow)
        try await Task.sleep(for: .milliseconds(50))
        #expect(h.sent.replies.isEmpty)
        await ApprovalQueueTests.until { sheet.armed }
        #expect(sheet.allowButton.isEnabled)
        sheet.allowButton.sendActions(for: .primaryActionTriggered)
        await ApprovalQueueTests.until { h.sent.replies.count == 1 }
        #expect(h.sent.replies.map(\.verdict) == [.allow])
    }

    @Test("R-PERM-2: with no live link the buttons are off, and the sheet says why")
    func offline() {
        let h = ApprovalHarness(connected: false)
        h.queue.requested(ApprovalQueueTests.call("r1"))
        let (sheet, window) = sheet(h.queue)
        defer { window.isHidden = true }
        #expect(!sheet.allowButton.isEnabled && !sheet.denyButton.isEnabled)
        #expect(sheet.noteLabel.text == "Answer once the link to your laptop is live.")
    }

    @Test("R-PERM-2: questions show their header, question and options (one, or several), a field to write in, secure for a secret; Answer waits for every question, then sends the picks")
    func questions() async {
        let h = ApprovalHarness()
        let asked = ApprovalQueueTests.asking("q1", tool: "Read")
        h.queue.requested(asked)
        let (sheet, window) = sheet(h.queue)
        defer { window.isHidden = true }
        #expect(sheet.titleLabel.text == "The agent asks")
        #expect(sheet.allowButton.isHidden && sheet.denyButton.isHidden)
        #expect(!sheet.answerButton.isHidden && !sheet.declineButton.isHidden)
        #expect(sheet.options["which"]?.count == 2)
        #expect(sheet.options["which"]?.first?.configuration?.subtitle == "the default")
        #expect(sheet.fields["token"]?.isSecureTextEntry == true)
        #expect(sheet.fields["which"]?.isSecureTextEntry == false)
        #expect(!sheet.answerButton.isEnabled)

        sheet.options["which"]?[1].sendActions(for: .primaryActionTriggered)
        sheet.options["checks"]?[0].sendActions(for: .primaryActionTriggered)
        sheet.options["checks"]?[2].sendActions(for: .primaryActionTriggered)
        #expect(!sheet.answerButton.isEnabled, "the token is still unanswered")
        sheet.write("t0k3n", for: "token")
        #expect(sheet.answerButton.isEnabled)
        #expect(sheet.options["checks"]?[2].accessibilityTraits.contains(.selected) == true)
        sheet.answerButton.sendActions(for: .primaryActionTriggered)
        await ApprovalQueueTests.until { h.sent.replies.count == 1 }
        #expect(h.sent.replies == [ApprovalReply(sessionID: "s1", requestID: "q1", verdict: .allow, answers: [
            QuestionReply(id: "which", picked: ["dev"], text: nil),
            QuestionReply(id: "checks", picked: ["lint", "build"], text: nil),
            QuestionReply(id: "token", picked: [], text: "t0k3n", secret: true),
        ])])
        #expect(h.owner.calls == 0, "Read is on the read-only list")
    }

    @Test("R-IOS-11: ⌘↵ allows once through the same owner check; a no sends nothing and says so")
    func keyCommand() async {
        let h = ApprovalHarness(owner: FakeOwnerCheck(false))
        h.queue.requested(ApprovalQueueTests.call("r1"))
        let (sheet, window) = sheet(h.queue)
        defer { window.isHidden = true }
        let allow = sheet.keyCommands?.first { $0.input == "\r" && $0.modifierFlags == .command }
        #expect(allow != nil)
        if let action = allow?.action { _ = sheet.perform(action) }
        await ApprovalQueueTests.until { h.owner.calls == 1 }
        await ApprovalQueueTests.until { sheet.noteLabel.text != nil }
        #expect(h.sent.replies.isEmpty)
        #expect(sheet.noteLabel.text == "Not confirmed: nothing was sent.")
    }

    // MARK: Over the transcript

    let row = SessionRow(id: TranscriptScreenTests.session, title: "A demo session", detail: "app", live: true, accessibilityLabel: "A demo session")

    /// The screen live, its turn under way with a bash call waiting.
    func opened(_ owner: FakeOwnerCheck = FakeOwnerCheck()) async throws -> (StubSession, ApprovalQueue, TranscriptViewController, UIWindow) {
        let session = StubSession()
        let queue = ApprovalQueue(sessionID: TranscriptScreenTests.session, ownerCheck: owner)
        let screen = TranscriptViewController(row: row, session: session, approvals: queue)
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        window.layoutIfNeeded()
        session.bring(TranscriptScreenTests.turn + [
            ComposerScreenTests.event("f1", .turnStarted(turn: "turn2")),
            ComposerScreenTests.event("f2", .userText(item: "u2", text: "Please make the file approved.txt", images: [])),
            ComposerScreenTests.event("f3", .toolCall(item: "t9", callID: "toolu_01Touch", name: "bash", input: #"{"command":"touch approved.txt"}"#)),
        ], attach: true)
        screen.frame()
        session.say(.live)
        try await waitFor { screen.drewRows && screen.statusBar.status == .live && queue.connected }
        return (session, queue, screen, window)
    }

    @Test("R-PERM-2, R-LAG-8: a request raises the sheet and marks its call; Allow once sends the approve for the request's session at once, the row reads allowed — sending…, then allowed on the ack")
    func allowOverTheTranscript() async throws {
        let (session, queue, screen, window) = try await opened()
        defer { window.isHidden = true }
        queue.requested(ApprovalQueueTests.call("r1", turn: "turn2"))
        let sheet = try #require(screen.approvalSheet)
        sheet.loadViewIfNeeded()
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
        screen.frame()
        #expect(screen.model.item("item:t9")?.verdict == "allowed")
        #expect(queue.held.isEmpty)
        #expect(screen.model.commandsOut == 0)
    }

    @Test("R-LAG-8: an ack's error brings the request back over the transcript with the host's sentence")
    func ackErrorBringsItBack() async throws {
        let (session, queue, screen, window) = try await opened()
        defer { window.isHidden = true }
        queue.requested(ApprovalQueueTests.call("r1", turn: "turn2"))
        let sheet = try #require(screen.approvalSheet)
        sheet.loadViewIfNeeded()
        try await waitFor { sheet.armed }
        sheet.denyButton.sendActions(for: .primaryActionTriggered)
        try await waitFor { !session.commands.withLock { $0 }.isEmpty }
        let sent = try #require(session.commands.withLock { $0 }.first)
        #expect(screen.model.item("item:t9")?.verdict == "denied — sending…")
        let refusal = "a viewer of session s1 may prompt, steer, interrupt or answer an approval of that session, or interrupt or answer an approval of one of its own subagents, and nothing else"
        session.bring([.commandMinted(token: sent.token, id: "dev-1"), .commandAcked(id: "dev-1", error: refusal)])
        screen.frame()
        try await waitFor { screen.approvalSheet != nil }
        let again = try #require(screen.approvalSheet)
        again.loadViewIfNeeded()
        #expect(again.noteLabel.text == "Your answer wasn't taken: \(refusal)")
        #expect(screen.model.item("item:t9")?.verdict == "waiting for approval")
    }

    @Test("R-PERM-2: approval.resolved from another device takes the sheet down with a toast; the row says how it went")
    func answeredElsewhere() async throws {
        let (_, queue, screen, window) = try await opened()
        defer { window.isHidden = true }
        queue.requested(ApprovalQueueTests.call("r1", turn: "turn2"))
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
        queue.requested(ApprovalQueueTests.call("r1", turn: "turn2"))
        let sheet = try #require(screen.approvalSheet)
        // As a swipe down does: the sheet gone, then its delegate told.
        sheet.dismiss(animated: false)
        try await waitFor { screen.presentedViewController == nil }
        screen.presentationControllerDidDismiss(try #require(sheet.presentationController))
        #expect(screen.approvalSheet == nil)
        #expect(screen.navigationItem.rightBarButtonItem?.accessibilityLabel == "1 approval waiting")
        queue.requested(ApprovalQueueTests.call("r2", turn: "turn2"))
        #expect(screen.approvalSheet != nil)
    }

    @Test("R-PERM-2: the turn's end takes the sheet down and clears the call's line")
    func turnEnd() async throws {
        let (_, queue, screen, window) = try await opened()
        defer { window.isHidden = true }
        queue.requested(ApprovalQueueTests.call("r1", turn: "turn2"))
        #expect(screen.approvalSheet != nil)
        queue.turnEnded(sessionID: "s1", turnID: "turn2")
        #expect(screen.approvalSheet == nil)
        #expect(screen.model.item("item:t9")?.verdict == "")
        #expect(screen.navigationItem.rightBarButtonItem == nil)
    }

    private func waitFor(_ done: () -> Bool) async throws {
        for _ in 0..<300 where !done() {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(done())
    }
}
