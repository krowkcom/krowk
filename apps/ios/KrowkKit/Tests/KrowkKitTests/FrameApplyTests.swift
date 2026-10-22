// R-LAG-7, R-PERF-4: a running turn followed live on the transcript
// screen. One batch per display frame: 200 deltas in one frame are one
// transaction, appended to their own cell's text storage in place, with no
// snapshot applied; the completed item's text is what's left drawn; a
// control character a delta boundary splits is still stripped; live lines
// and the catch-up that repeats them draw each event once; the list
// follows the end while at it, stops when scrolled up and offers "↓ New";
// the display link pauses when idle and wakes on new updates. The fleet's
// streaming turn is AppTests' LiveFollowTests; the per-frame cost baseline
// is FrameApplyBaselineTests.
import Synchronization
import Testing
import UIKit
@testable import KrowkKit

@MainActor
@Suite("R-LAG-7, R-PERF-4: one batch per display frame, only the streaming cell re-laid out")
struct FrameApplyTests {
    static let session = TranscriptScreenTests.session
    let row = SessionRow(id: FrameApplyTests.session, title: "A demo session", detail: "app", live: true, accessibilityLabel: "A demo session")

    static func event(_ id: String, _ change: TranscriptChange?, parent: String? = nil, session: String = FrameApplyTests.session) -> TranscriptEvent {
        TranscriptEvent(id: id, parentID: parent, sessionID: session, change: change)
    }

    static func started(_ item: String, _ kind: LiveItemKind, turn: String = "t2", session: String = FrameApplyTests.session) -> TranscriptUpdate {
        .started(session: session, turn: turn, item: item, kind: kind)
    }

    static func delta(_ item: String, _ text: String, session: String = FrameApplyTests.session) -> TranscriptUpdate {
        .delta(session: session, item: item, text: text)
    }

    func shown(_ screen: TranscriptViewController) -> UIWindow {
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        window.layoutIfNeeded()
        return window
    }

    /// A screen with the first turn drawn, then answer `a2` started with
    /// `first` as its text: its cell, on screen.
    func streaming(_ first: String = "Hello", rows: [TranscriptEvent] = TranscriptScreenTests.turn) async throws -> (StubSession, TranscriptViewController, UIWindow, TranscriptTextCell) {
        let session = StubSession()
        let screen = TranscriptViewController(row: row, session: session)
        let window = shown(screen)
        session.bring(rows, attach: true)
        screen.frame()
        try await waitFor { screen.drewRows }
        session.bring([Self.started("a2", .assistant), Self.delta("a2", first)])
        screen.frame()
        try await waitFor { screen.dataSource.snapshot().itemIdentifiers.last == "item:a2" }
        window.layoutIfNeeded()
        let cell = try #require(visibleCell("item:a2", in: screen, window: window) as? TranscriptTextCell)
        return (session, screen, window, cell)
    }

    func visibleCell(_ id: String, in screen: TranscriptViewController, window: UIWindow) -> UICollectionViewCell? {
        guard let path = screen.dataSource.indexPath(for: id) else { return nil }
        if screen.collectionView.cellForItem(at: path) == nil {
            screen.collectionView.scrollToItem(at: path, at: .bottom, animated: false)
            window.layoutIfNeeded()
        }
        return screen.collectionView.cellForItem(at: path)
    }

    @Test("R-LAG-7, R-PERF-4: 200 deltas in one frame are one apply transaction, one append to the same cell's storage, and no snapshot apply; the completed item's text is what's drawn, in that same cell")
    func burstInOneFrame() async throws {
        let (session, screen, window, cell) = try await streaming()
        defer { window.isHidden = true }
        let driver = try #require(screen.driver)
        let identity = ObjectIdentifier(cell)
        #expect(cell.text.drawnText == "Hello")
        #expect(cell.accessibilityIdentifier == "transcript-assistant")
        let (snapshots, transactions, modelTransactions, appends) = (screen.snapshotApplies, driver.transactions, screen.model.transactions, cell.appends)

        let words = (0..<200).map { " w\($0)" }
        session.bring(words.map { Self.delta("a2", $0) })
        screen.frame()
        // In the frame itself: one transaction, one append, the cell drawn.
        #expect(driver.transactions == transactions + 1)
        #expect(screen.model.transactions == modelTransactions + 1)
        #expect(cell.appends == appends + 1)
        let whole = "Hello" + words.joined()
        #expect(cell.text.drawnText == whole)
        #expect(cell.streamed?.bytes == whole.utf8.count)
        // And after: the observation's own redraw adds nothing, and no
        // snapshot was applied for text.
        try await Task.sleep(for: .milliseconds(50))
        window.layoutIfNeeded()
        #expect(screen.snapshotApplies == snapshots, "a snapshot applied for deltas")
        #expect(cell.appends == appends + 1)
        #expect(cell.text.drawnText == whole)

        // Its logged completion, then the turn's end: the separator is a
        // new row (one snapshot), the answer's cell the same object.
        session.bring([.logged([
            Self.event("e10", .assistantText(item: "a2", text: whole), parent: "e6"),
            Self.event("e11", .turnCompleted(turn: "t2", end: .completed), parent: "e10"),
        ])])
        screen.frame()
        try await waitFor { screen.dataSource.snapshot().itemIdentifiers.last == "turn:t2" }
        window.layoutIfNeeded()
        let state = try #require(screen.model.item("item:a2"))
        #expect(state.done)
        #expect(state.text == whole)
        let after = try #require(visibleCell("item:a2", in: screen, window: window))
        #expect(ObjectIdentifier(after) == identity, "the answer moved to another cell")
        #expect(cell.text.drawnText == whole)
        #expect(cell.streamed == nil, "drawn whole once complete")
        #expect(screen.snapshotApplies == snapshots + 1)
        let end = try #require(visibleCell("turn:t2", in: screen, window: window) as? TranscriptMarkCell)
        #expect(end.label.text == "Turn completed")
        #expect(!screen.model.isStreaming)
    }

    @Test("R-LAG-7: a long answer streams into blocks, and a frame edits only the last: the blocks above keep their views and text; once whole it's one view again")
    func onlyTheLastBlock() async throws {
        let (session, screen, window, cell) = try await streaming("Start\n")
        defer { window.isHidden = true }
        let line = String(repeating: "word ", count: 20) + "\n"
        for _ in 0..<40 {
            session.bring([Self.delta("a2", line)])
            screen.frame()
        }
        let blocks = cell.text.blocks
        #expect(blocks.count >= 2, "\(blocks.count) blocks for \(cell.text.drawnText.count) characters")
        #expect(cell.text.live.storage.length <= TranscriptStreamingText.blockLimit)
        let frozen = blocks.map { ($0.view, $0.view.text ?? "") }
        session.bring([Self.delta("a2", "and more")])
        screen.frame()
        for (block, (view, text)) in zip(cell.text.blocks, frozen) {
            #expect(block.view === view)
            #expect(block.view.text == text, "a block above was edited")
        }
        let whole = "Start\n" + String(repeating: line, count: 40) + "and more"
        #expect(cell.text.drawnText == whole)
        session.bring([.logged([Self.event("e10", .assistantText(item: "a2", text: whole), parent: "e6")])])
        screen.frame()
        try await waitFor { cell.streamed == nil }
        #expect(cell.text.blocks.isEmpty)
        #expect(cell.textView.view.text == whole)
    }

    @Test("R-LAG-7: a completion that differs from what streamed (deltas a gap lost) replaces the text; one that's equal leaves it be")
    func completionReplaces() {
        let model = TranscriptModel(sessionID: Self.session)
        model.apply(frame: [Self.started("a1", .assistant, turn: "t1"), Self.delta("a1", "Hel")])
        let state = model.item("item:a1")
        #expect(state?.text == "Hel")
        #expect(state?.done == false)
        #expect(model.isStreaming)
        model.apply(frame: [.logged([Self.event("e1", .assistantText(item: "a1", text: "Hello, world"))])])
        #expect(state?.text == "Hello, world")
        #expect(state?.done == true)
        #expect(model.ids == ["item:a1"])
        // A late delta for a whole item changes nothing.
        model.apply(frame: [Self.delta("a1", "!!")])
        #expect(state?.text == "Hello, world")
    }

    @Test("R-IOS-10: a control or bidi character on either side of a delta boundary is stripped, as in the whole text; the link is tappable once the item is whole")
    func splitControlStripped() async throws {
        let (session, screen, window, cell) = try await streaming("See https://exa")
        defer { window.isHidden = true }
        let pieces = ["mple.com/x now\u{1B}", "[31m red\u{9B}", "\u{202E}txt\r", "\nend\u{0}"]
        for piece in pieces {
            session.bring([Self.delta("a2", piece)])
            screen.frame()
        }
        let whole = "See https://exa" + pieces.joined()
        let drawn = cell.text.drawnText
        #expect(!drawn.unicodeScalars.contains { SafeText.stripped($0) }, "\(drawn.unicodeScalars.map { String($0.value, radix: 16) })")
        #expect(drawn == SafeText.clean(whole))
        #expect(cell.appends >= pieces.count)

        session.bring([.logged([Self.event("e10", .assistantText(item: "a2", text: whole), parent: "e6")])])
        screen.frame()
        try await waitFor { cell.streamed == nil }
        let text = cell.textView.view.attributedText ?? NSAttributedString()
        var links: [URL] = []
        text.enumerateAttribute(.link, in: NSRange(location: 0, length: text.length)) { value, _, _ in
            if let url = value as? URL { links.append(url) }
        }
        #expect(links.map(\.absoluteString) == ["https://example.com/x"])
        #expect(!cell.text.drawnText.unicodeScalars.contains { SafeText.stripped($0) })
    }

    @Test("R-LAG-7: live lines, then a catch-up bringing the same events, draw each event once")
    func overlapDrawsOnce() async throws {
        let session = StubSession()
        let screen = TranscriptViewController(row: row, session: session)
        let window = shown(screen)
        defer { window.isHidden = true }
        session.bring(Array(TranscriptScreenTests.turn.prefix(2)), attach: true)
        screen.frame()
        try await waitFor { screen.drewRows }
        let later: [TranscriptEvent] = [
            Self.event("e2", .reasoning(item: "r1", text: "Thinking."), parent: "e1"),
            Self.event("e3", .toolCall(item: "t1", callID: "c1", name: "read", input: #"{"path":"README.md"}"#), parent: "e2"),
            Self.event("e4", .toolResult(item: "t2", callID: "c1", output: "# The demo", isError: false), parent: "e3"),
            Self.event("e5", .assistantText(item: "a1", text: "Done."), parent: "e4"),
            Self.event("e6", .turnCompleted(turn: "turn1", end: .completed), parent: "e5"),
        ]
        // The answer streamed, then everything logged live.
        session.bring([Self.started("a1", .assistant, turn: "turn1"), Self.delta("a1", "Do"), Self.delta("a1", "ne.")])
        screen.frame()
        session.bring([.logged(later)])
        screen.frame()
        try await waitFor { screen.dataSource.snapshot().itemIdentifiers.last == "turn:turn1" }
        let rows = screen.dataSource.snapshot().itemIdentifiers
        #expect(rows == ["item:u1", "item:a1", "item:r1", "item:t1", "turn:turn1"])
        let snapshots = screen.snapshotApplies

        // The host's catch-up repeats them all (and the attach's): nothing
        // new, nothing twice, no snapshot.
        session.bring([.caughtUp(Array(TranscriptScreenTests.turn.prefix(2)) + later)])
        screen.frame()
        try await Task.sleep(for: .milliseconds(50))
        #expect(screen.dataSource.snapshot().itemIdentifiers == rows)
        #expect(Set(screen.model.ids).count == screen.model.ids.count)
        #expect(screen.snapshotApplies == snapshots)
        #expect(screen.model.item("item:a1")?.text == "Done.")
        #expect(screen.model.item("item:t1")?.output == "# The demo")
    }

    @Test("A tool call shows as it starts (running…) and settles with its result; a notice is a one-line note; another session's lines are skipped; a tool input delta draws nothing")
    func toolsAndNotices() async throws {
        let (session, screen, window, _) = try await streaming()
        defer { window.isHidden = true }
        session.bring([
            Self.started("c9", .toolCall(callID: "call-9", name: "bash")),
            .notice(session: Self.session, text: "Retrying\u{1B} in 2 s"),
            Self.started("x1", .assistant, session: "another"),
            Self.delta("x1", "not ours", session: "another"),
            .notice(session: "another", text: "not ours"),
        ])
        screen.frame()
        try await waitFor { screen.dataSource.snapshot().itemIdentifiers.suffix(2) == ["item:c9", "notice:1"] }
        window.layoutIfNeeded()
        #expect(screen.model.item("item:x1") == nil)
        let tool = try #require(visibleCell("item:c9", in: screen, window: window) as? TranscriptToolCell)
        #expect(tool.title.text == "bash")
        #expect(tool.result.text == "running…")
        #expect(tool.header.accessibilityLabel == "bash. Running")
        let note = try #require(visibleCell("notice:1", in: screen, window: window) as? TranscriptMarkCell)
        #expect(note.accessibilityIdentifier == "transcript-notice")
        #expect(note.label.text == "Retrying in 2 s")

        session.bring([.logged([Self.event("e20", .toolCall(item: "c9", callID: "call-9", name: "bash", input: #"{"command":"ls -la"}"#))])])
        screen.frame()
        try await waitFor { tool.title.text == "bash ls -la" }
        #expect(tool.result.text == "running…")
        session.bring([.logged([Self.event("e21", .toolResult(item: "c10", callID: "call-9", output: "total 0", isError: false))])])
        screen.frame()
        try await waitFor { tool.result.text == "✓ total 0" }
        #expect(screen.model.ids.filter { $0 == "item:c9" || $0 == "item:c10" } == ["item:c9"], "the result is under its call")
    }

    @Test("R-LAG-7: a completion a gap's catch-up brings after the turn's end came live still sets the item's text, and a call's input")
    func lateCompletion() {
        let model = TranscriptModel(sessionID: Self.session)
        model.apply(frame: [
            Self.started("a1", .assistant, turn: "t1"), Self.delta("a1", "Hel"),
            Self.started("c1", .toolCall(callID: "k1", name: "bash"), turn: "t1"),
            .logged([Self.event("e9", .turnCompleted(turn: "t1", end: .completed), parent: "e8")]),
        ])
        #expect(model.item("item:a1")?.done == true)
        model.apply(frame: [.caughtUp([
            Self.event("e7", .toolCall(item: "c1", callID: "k1", name: "bash", input: #"{"command":"ls"}"#), parent: "e6"),
            Self.event("e8", .assistantText(item: "a1", text: "Hello, whole"), parent: "e7"),
        ])])
        #expect(model.item("item:a1")?.text == "Hello, whole")
        #expect(model.item("item:c1")?.toolLine == "bash ls")
        #expect(model.item("item:c1")?.running == false)
        #expect(model.ids == ["item:a1", "item:c1", "turn:t1"])
    }

    @Test("R-LAG-7: VoiceOver moving to a row above the last stops the following; on the last row it goes on")
    func voiceOverReadsBack() async throws {
        var rows: [TranscriptEvent] = []
        for n in 0..<10 {
            rows.append(Self.event("p\(n)", .userText(item: "u\(n)", text: "Prompt \(n)", images: [])))
        }
        let (_, screen, window, cell) = try await streaming("Streaming", rows: rows)
        defer { window.isHidden = true }
        #expect(screen.following)
        screen.focused(cell.textView.view)
        #expect(screen.following, "the last row: still following")
        let path = IndexPath(item: 0, section: 0)
        screen.collectionView.scrollToItem(at: path, at: .top, animated: false)
        window.layoutIfNeeded()
        let first = try #require(screen.collectionView.cellForItem(at: path))
        NotificationCenter.default.post(name: UIAccessibility.elementFocusedNotification, object: nil, userInfo: [UIAccessibility.focusedElementUserInfoKey: first.contentView])
        try await waitFor { !screen.following }
    }

    @Test("R-LAG-7: an event after a streamed item's late completion goes under that item, not under the row the stream left last")
    func lateCompletionAnchors() {
        let model = TranscriptModel(sessionID: Self.session)
        model.attach([Self.event("e1", .userText(item: "u1", text: "Hi", images: []))])
        model.apply(frame: [
            Self.started("a1", .assistant, turn: "t1"), Self.delta("a1", "Hel"),
            .logged([Self.event("e9", .turnCompleted(turn: "t1", end: .completed), parent: "e4")]),
        ])
        #expect(model.ids == ["item:u1", "item:a1", "turn:t1"])
        model.apply(frame: [.caughtUp([
            Self.event("e2", .assistantText(item: "a1", text: "Hello"), parent: "e1"),
            Self.event("e3", .toolCall(item: "c1", callID: "k", name: "read", input: "{}"), parent: "e2"),
        ])])
        #expect(model.ids == ["item:u1", "item:a1", "item:c1", "turn:t1"])
        #expect(model.item("item:a1")?.text == "Hello")
    }

    @Test("A host reusing an item id across kinds never draws a row twice")
    func reusedItemID() {
        let model = TranscriptModel(sessionID: Self.session)
        model.apply(frame: [Self.started("x", .assistant, turn: "t1")])
        model.apply(frame: [.logged([Self.event("e1", .toolCall(item: "x", callID: "k", name: "bash", input: "{}"))])])
        model.apply(frame: [Self.delta("x", "text")])
        #expect(model.ids == ["item:x"])
        model.apply(frame: [Self.started("y", .assistant, turn: "t1"), .logged([Self.event("e2", .reasoning(item: "y", text: "r"))])])
        model.apply(frame: [.logged([Self.event("e3", .assistantText(item: "y", text: "a"))])])
        #expect(Set(model.ids).count == model.ids.count)
    }

    @Test("A notice repeated straight after itself is drawn once")
    func noticeOnce() {
        let model = TranscriptModel(sessionID: Self.session)
        model.apply(frame: [.notice(session: Self.session, text: "Retrying"), .notice(session: Self.session, text: "Retrying")])
        model.apply(frame: [.notice(session: Self.session, text: "Retrying")])
        #expect(model.ids == ["notice:1"])
        model.apply(frame: [.notice(session: Self.session, text: "Rate limited"), .notice(session: Self.session, text: "Retrying")])
        #expect(model.ids == ["notice:1", "notice:2", "notice:3"])
    }

    @Test("R-LAG-7: an expanded \"Thought\" that streams grows by appends too, and a collapsed one draws none of its text")
    func streamingThought() async throws {
        let (session, screen, window, _) = try await streaming()
        defer { window.isHidden = true }
        session.bring([Self.started("r2", .reasoning), Self.delta("r2", "Thinking")])
        screen.frame()
        try await waitFor { screen.dataSource.snapshot().itemIdentifiers.last == "item:r2" }
        window.layoutIfNeeded()
        let cell = try #require(visibleCell("item:r2", in: screen, window: window) as? TranscriptToolCell)
        #expect(cell.output.view.isHidden)
        #expect(cell.output.streamed == nil, "collapsed, its text isn't drawn")
        screen.toggle("item:r2")
        try await waitFor { !cell.output.view.isHidden }
        #expect(cell.output.drawnText == "Thinking")
        let appends = cell.output.appends
        session.bring((0..<50).map { Self.delta("r2", " step\($0)") })
        screen.frame()
        #expect(cell.output.appends == appends + 1)
        #expect(cell.output.drawnText == "Thinking" + (0..<50).map { " step\($0)" }.joined())
        #expect(!screen.following, "opened to read: not following")
    }

    @Test("R-LAG-7: taken out of its navigation stack from beneath another screen, the screen lets its display link and session go")
    func leavesFromBeneath() async throws {
        let session = StubSession()
        let screen = TranscriptViewController(row: row, session: session)
        let nav = UINavigationController(rootViewController: UIViewController())
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = nav
        window.makeKeyAndVisible()
        defer { window.isHidden = true }
        nav.pushViewController(screen, animated: false)
        window.layoutIfNeeded()
        try await waitFor { screen.driver?.ticker != nil }
        nav.pushViewController(UIViewController(), animated: false)
        window.layoutIfNeeded()
        #expect(screen.driver?.ticker != nil, "covered, it still drains")
        nav.popToRootViewController(animated: false)
        window.layoutIfNeeded()
        try await waitFor { screen.driver?.ticker == nil }
        #expect(session.stopped.withLock { $0 } == 1)
    }

    @Test("R-LAG-7: a turn's end settles what of it was still under way: the streaming item is whole, a tool without its result no longer running")
    func turnEndSettles() {
        let model = TranscriptModel(sessionID: Self.session)
        model.apply(frame: [Self.started("a1", .assistant, turn: "t1"), Self.delta("a1", "Half"), Self.started("c1", .toolCall(callID: "k", name: "bash"), turn: "t1"), Self.started("a9", .assistant, turn: "t9"), Self.delta("a9", "Other")])
        model.apply(frame: [.logged([Self.event("e1", .turnCompleted(turn: "t1", end: .interrupted))])])
        #expect(model.item("item:a1")?.done == true)
        #expect(model.item("item:a1")?.text == "Half")
        #expect(model.item("item:c1")?.running == false)
        #expect(model.item("item:a9")?.done == false, "another turn's item streams on")
        #expect(model.isStreaming)
    }

    @Test("R-LAG-7: an answer with no visible text yet has no row; a streamed item's text is held to a bound however long it streams")
    func emptyAndBounded() {
        let model = TranscriptModel(sessionID: Self.session)
        model.apply(frame: [Self.started("a1", .assistant, turn: "t1"), Self.delta("a1", "  \n")])
        #expect(model.ids.isEmpty)
        model.apply(frame: [Self.delta("a1", "Now")])
        #expect(model.ids == ["item:a1"])
        let chunk = String(repeating: "x", count: 64 * 1024)
        for _ in 0..<8 { model.apply(frame: [Self.delta("a1", chunk)]) }
        #expect(model.item("item:a1")?.text.utf8.count == TranscriptModel.streamLimit)
        // Never shown, and completed empty: still no row.
        model.apply(frame: [Self.started("a2", .assistant, turn: "t1"), .logged([Self.event("e2", .assistantText(item: "a2", text: ""))])])
        #expect(model.ids == ["item:a1"])
    }

    @Test("R-LAG-7: at the end, the list follows what streams; scrolled up it stays put and \"↓ New\" shows; a tap returns to the end and follows again")
    func followsTheEnd() async throws {
        var rows: [TranscriptEvent] = []
        for n in 0..<30 {
            rows.append(Self.event("p\(n)", .userText(item: "u\(n)", text: "Prompt \(n)", images: [])))
            rows.append(Self.event("q\(n)", .assistantText(item: "v\(n)", text: "Answer \(n), a line or two long so the list is taller than the screen.")))
        }
        let (session, screen, window, _) = try await streaming("Line 0", rows: rows)
        defer { window.isHidden = true }
        let list = screen.collectionView!
        window.layoutIfNeeded()
        #expect(screen.following)
        #expect(screen.isAtBottom, "opened at the end")
        #expect(screen.newPill.isHidden)

        for n in 1...20 {
            session.bring([Self.delta("a2", "\nLine \(n)")])
            screen.frame()
            window.layoutIfNeeded()
            window.layoutIfNeeded()
            #expect(screen.isAtBottom, "not pinned after line \(n): \(list.contentOffset.y) of \(list.contentSize.height)")
        }

        // Scrolled up by the person: not following.
        list.setContentOffset(CGPoint(x: 0, y: 0), animated: false)
        screen.scrollViewDidEndDragging(list, willDecelerate: false)
        #expect(!screen.following)
        let offset = list.contentOffset.y
        session.bring([Self.delta("a2", "\nLine 21")])
        screen.frame()
        window.layoutIfNeeded()
        #expect(abs(list.contentOffset.y - offset) < 1, "moved while scrolled up")
        #expect(!screen.newPill.isHidden, "no ↓ New")
        #expect(screen.newPill.accessibilityIdentifier == "transcript-new")

        // ↓ New: the end, following again.
        screen.newPill.sendActions(for: .primaryActionTriggered)
        window.layoutIfNeeded()
        try await waitFor {
            window.layoutIfNeeded()
            return screen.isAtBottom
        }
        #expect(screen.following)
        #expect(screen.newPill.isHidden)
        session.bring([Self.delta("a2", "\nLine 22")])
        screen.frame()
        window.layoutIfNeeded()
        #expect(screen.isAtBottom)

        // A scroll alone, no finger's (the list's own), isn't undone by
        // the layout pass it causes: what's laid out didn't change.
        let bottom = list.contentOffset.y
        list.setContentOffset(CGPoint(x: 0, y: bottom - 30), animated: false)
        window.layoutIfNeeded()
        #expect(abs(list.contentOffset.y - (bottom - 30)) < 1, "snapped back to the end: \(list.contentOffset.y)")
        #expect(screen.following, "the list's own scroll isn't the person's")
        // VoiceOver's three-finger scroll up: not following.
        _ = list.accessibilityScroll(.up)
        window.layoutIfNeeded()
        #expect(!screen.following, "a VoiceOver scroll up still followed")
        let held2 = list.contentOffset.y
        session.bring([Self.delta("a2", "\nLine 22b")])
        screen.frame()
        window.layoutIfNeeded()
        #expect(abs(list.contentOffset.y - held2) < 1, "moved after a VoiceOver scroll up")
        screen.newPill.sendActions(for: .primaryActionTriggered)
        try await waitFor {
            window.layoutIfNeeded()
            return screen.isAtBottom
        }

        // A finger on the list at the end: what streams doesn't move it
        // under the finger; let go, the end is shown again.
        let touching = try #require(list as? TranscriptCollectionView)
        touching.touching = true
        let held = list.contentOffset.y
        session.bring([Self.delta("a2", "\nLine 23\nLine 24")])
        screen.frame()
        window.layoutIfNeeded()
        window.layoutIfNeeded()
        #expect(abs(list.contentOffset.y - held) < 1, "moved under the finger")
        touching.touching = nil
        session.bring([Self.delta("a2", "\nLine 25")])
        screen.frame()
        window.layoutIfNeeded()
        window.layoutIfNeeded()
        #expect(screen.isAtBottom, "not back at the end once let go")

        // Scrolled back down to the end by hand: following too.
        list.setContentOffset(.zero, animated: false)
        screen.scrollViewDidEndDragging(list, willDecelerate: false)
        #expect(!screen.following)
        list.setContentOffset(CGPoint(x: 0, y: list.contentSize.height + list.adjustedContentInset.bottom - list.bounds.height), animated: false)
        screen.scrollViewDidEndDragging(list, willDecelerate: false)
        #expect(screen.following)
    }

    @Test("R-LAG-7: the display link pauses after 1 s with nothing come and nothing under way (5 s while an item is), and the session's next update wakes it")
    func pausesWhenIdle() async throws {
        let session = StubSession()
        let model = TranscriptModel(sessionID: Self.session)
        let driver = FrameDriver(session: session, model: model)
        let ticker = FakeTicker()
        driver.ticker = ticker
        session.onUpdates { driver.wake() }

        driver.tick(at: 10)
        driver.tick(at: 10.5)
        #expect(!ticker.isPaused)
        driver.tick(at: 11)
        #expect(ticker.isPaused, "idle 1 s")
        #expect(driver.transactions == 0, "empty frames apply nothing")

        session.bring([.logged([Self.event("e1", .userText(item: "u1", text: "Hi", images: []))])])
        try await waitFor { !ticker.isPaused }
        driver.tick(at: 20)
        #expect(driver.transactions == 1)
        #expect(model.ids == ["item:u1"])

        // An item under way: frames go on through a quiet second.
        session.bring([Self.started("a1", .assistant, turn: "t1"), Self.delta("a1", "Hel")])
        // Its wake comes first, as on the main queue.
        try await Task.sleep(for: .milliseconds(20))
        driver.tick(at: 21)
        driver.tick(at: 22)
        driver.tick(at: 23.5)
        #expect(!ticker.isPaused, "paused while an item streams")
        driver.tick(at: 27)
        #expect(ticker.isPaused, "quiet 5 s")
        session.bring([Self.delta("a1", "lo")])
        try await waitFor { !ticker.isPaused }
        driver.tick(at: 40)
        #expect(model.item("item:a1")?.text == "Hello")
        #expect(driver.frames == 9)
        #expect(driver.transactions == 3)
    }

    @Test("R-LAG-7: the screen's display link asks for up to 120 Hz (ProMotion), at least 30")
    func frameRate() async throws {
        let session = StubSession()
        let screen = TranscriptViewController(row: row, session: session)
        let window = shown(screen)
        defer { window.isHidden = true }
        let ticker = try #require(screen.driver?.ticker as? DisplayLinkTicker)
        let range = ticker.link.preferredFrameRateRange
        #expect(range.minimum == 30 && range.maximum == 120 && range.preferred == 120)
    }

    private func waitFor(_ done: () -> Bool) async throws {
        for _ in 0..<300 where !done() {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(done())
    }
}

/// A display link that only says whether it is paused.
@MainActor
final class FakeTicker: FrameTicker {
    var isPaused = false
}
