// R-IOS-1, R-IOS-10: a session's transcript, as the screen draws it from
// its model: rows in order, each kind in its cell, a tool's result under
// its call, a tool's output laid out only once expanded and at most
// 64 KiB of it; an activity indicator only in the empty body, never once
// rows are drawn. The model's rules over the generated events are
// AppTests' TranscriptModelTests; the demo session through the fleet is
// TranscriptTests.
import Testing
import UIKit
@testable import KrowkKit

@MainActor
@Suite("R-IOS-1, R-IOS-10: the transcript screen")
struct TranscriptScreenTests {
    static let session = "s1"
    let row = SessionRow(id: TranscriptScreenTests.session, title: "A demo session", detail: "app", live: true, accessibilityLabel: "A demo session")

    static func event(_ id: String, _ change: TranscriptChange?, session: String = TranscriptScreenTests.session) -> TranscriptEvent {
        TranscriptEvent(id: id, sessionID: session, change: change)
    }

    /// A first turn: a prompt, reasoning, a tool call and its result, an
    /// answer, the turn's end.
    static let turn: [TranscriptEvent] = [
        event("e0", nil),
        event("e1", .userText(item: "u1", text: "Please read the README", images: [])),
        event("e2", .reasoning(item: "r1", text: "The person wants the README read.")),
        event("e3", .toolCall(item: "t1", callID: "c1", name: "read", input: #"{"path":"README.md"}"#)),
        event("e4", .toolResult(item: "t2", callID: "c1", output: "# The demo project\nA project.", isError: false)),
        event("e5", .assistantText(item: "a1", text: "The README says this is the demo project.")),
        event("e6", .turnCompleted(turn: "turn1", end: .completed)),
    ]

    /// The screen in a window, the size of an iPhone's.
    func shown(_ screen: TranscriptViewController) -> UIWindow {
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        window.layoutIfNeeded()
        return window
    }

    @Test("R-IOS-1: while the session is read the empty body shows the title and an indicator; once rows are drawn no indicator is visible, ever")
    func noSpinnerOverRows() async throws {
        let session = StubSession()
        let screen = TranscriptViewController(row: row, session: session)
        let window = shown(screen)
        defer { window.isHidden = true }
        #expect(screen.title == "A demo session")
        #expect(screen.spinner.isAnimating && !screen.spinner.isHidden, "an indicator while nothing is read")
        #expect(screen.spinner.superview === screen.collectionView.backgroundView)

        session.bring(Self.turn, attach: true)
        screen.frame()
        try await waitFor { screen.drewRows }
        #expect(!screen.spinner.isAnimating)
        #expect(screen.spinner.isHidden)
        #expect(!Self.visibleIndicators(in: screen.view), "an indicator over drawn rows")

        // Whatever comes next — more rows, the connection ending — none
        // shows again.
        session.bring([Self.event("e7", .userText(item: "u2", text: "And then?", images: []))])
        screen.frame()
        session.say(.ended("gone"))
        try await waitFor { screen.statusBar.status == .ended("gone") }
        try await waitFor { screen.dataSource.snapshot().numberOfItems == 6 }
        #expect(!Self.visibleIndicators(in: screen.view))
        #expect(screen.emptyLabel.isHidden)
    }

    @Test("R-IOS-1: a session read with nothing to draw says so, and one that stopped before it was read says the bar says why; no indicator stays")
    func emptyAndStopped() async throws {
        let empty = StubSession()
        let screen = TranscriptViewController(row: row, session: empty)
        let window = shown(screen)
        defer { window.isHidden = true }
        empty.bring([Self.event("e0", nil)], attach: true)
        screen.frame()
        try await waitFor { !screen.emptyLabel.isHidden }
        #expect(screen.emptyLabel.text == "Nothing in this session yet.")
        #expect(!screen.spinner.isAnimating)

        let stopped = StubSession()
        let other = TranscriptViewController(row: row, session: stopped)
        let second = shown(other)
        defer { second.isHidden = true }
        stopped.say(.ended("this phone can't open the session"))
        try await waitFor { !other.emptyLabel.isHidden }
        #expect(other.emptyLabel.text == "This session couldn't be read. The bar above says why.")
        #expect(!other.spinner.isAnimating)
    }

    @Test("The rows, in order: the prompt in a bubble, a reasoning line, the tool call with its result beneath it, the answer, the turn's end")
    func rowsInOrder() async throws {
        let session = StubSession()
        let screen = TranscriptViewController(row: row, session: session)
        let window = shown(screen)
        defer { window.isHidden = true }
        session.bring(Self.turn, attach: true)
        screen.frame()
        try await waitFor { screen.drewRows }
        window.layoutIfNeeded()

        #expect(screen.dataSource.snapshot().itemIdentifiers == ["item:u1", "item:r1", "item:t1", "item:a1", "turn:turn1"])
        let cells = (0..<5).compactMap { screen.collectionView.cellForItem(at: IndexPath(item: $0, section: 0)) }
        #expect(cells.map(\.accessibilityIdentifier) == ["transcript-user", "transcript-reasoning", "transcript-tool", "transcript-assistant", "transcript-turn-end"])

        let user = try #require(cells[0] as? TranscriptTextCell)
        #expect(user.textView.view.text == "Please read the README")
        #expect(user.textView.view.textLayoutManager != nil, "TextKit 2")
        let bubble = user.textView.view.convert(user.textView.view.bounds, to: screen.collectionView)
        #expect(bubble.maxX > screen.collectionView.bounds.width - 40, "right-aligned: \(bubble)")
        #expect(bubble.minX >= 48, "inset from the left: \(bubble)")

        let thought = try #require(cells[1] as? TranscriptToolCell)
        #expect(thought.title.text == "Thought")
        #expect(thought.body.view.isHidden, "collapsed until tapped")

        let tool = try #require(cells[2] as? TranscriptToolCell)
        #expect(tool.title.text == "read README.md")
        #expect(tool.result.text == "✓ # The demo project")
        #expect(tool.body.view.isHidden)
        #expect(tool.body.view.text.isEmpty, "a collapsed output isn't laid out")
        #expect(tool.header.accessibilityValue == "collapsed")

        let answer = try #require(cells[3] as? TranscriptTextCell)
        #expect(answer.textView.view.text == "The README says this is the demo project.")
        let end = try #require(cells[4] as? TranscriptMarkCell)
        #expect(end.label.text == "Turn completed")
    }

    @Test("A tap expands a tool call to its full output, monospaced, cut at 64 KiB with how many more bytes; another collapses it")
    func expandTool() async throws {
        let session = StubSession()
        let screen = TranscriptViewController(row: row, session: session)
        let window = shown(screen)
        defer { window.isHidden = true }
        let output = "first line\n" + String(repeating: "x", count: 70_000)
        session.bring([
            Self.event("e1", .toolCall(item: "t1", callID: "c1", name: "bash", input: #"{"command":"cat big.log"}"#)),
            Self.event("e2", .toolResult(item: "t2", callID: "c1", output: output, isError: true)),
        ], attach: true)
        screen.frame()
        try await waitFor { screen.drewRows }
        window.layoutIfNeeded()
        let path = IndexPath(item: 0, section: 0)
        let cell = try #require(screen.collectionView.cellForItem(at: path) as? TranscriptToolCell)
        #expect(cell.title.text == "bash cat big.log")
        #expect(cell.result.text == "✗ first line")
        let collapsed = cell.frame.height

        screen.collectionView(screen.collectionView, didSelectItemAt: path)
        try await waitFor { !cell.body.view.isHidden }
        let shown = cell.body.view.text ?? ""
        #expect(shown.hasPrefix("first line\nxxx"))
        #expect(shown.hasSuffix("\n" + SafeText.more(output.utf8.count - SafeText.outputLimit)))
        #expect(cell.header.accessibilityValue == "expanded")
        try await waitFor {
            window.layoutIfNeeded()
            return cell.frame.height > collapsed
        }

        screen.collectionView(screen.collectionView, didSelectItemAt: path)
        try await waitFor { cell.body.view.isHidden }
        #expect(cell.body.view.text.isEmpty)
    }

    @Test("R-IOS-10: hostile text in a cell is drawn cleaned, its link tappable with its host beside it, an image's markdown as text")
    func hostileCell() async throws {
        let session = StubSession()
        let screen = TranscriptViewController(row: row, session: session)
        let window = shown(screen)
        defer { window.isHidden = true }
        session.bring([
            Self.event("e1", .assistantText(item: "a1", text: "Open \u{202E}[docs](https://evil.example/x)\u{0} now ![p](https://evil.example/p.png)")),
            Self.event("e2", .turnCompleted(turn: "turn1", end: .failed("bad\u{202E} gateway"))),
        ], attach: true)
        screen.frame()
        try await waitFor { screen.drewRows }
        window.layoutIfNeeded()
        let cell = try #require(screen.collectionView.cellForItem(at: IndexPath(item: 0, section: 0)) as? TranscriptTextCell)
        let text = cell.textView.view.text ?? ""
        #expect(!text.unicodeScalars.contains { SafeText.stripped($0) })
        #expect(text.contains("(evil.example)"))
        #expect(text.contains("![p](https://evil.example/p.png"))
        let mark = try #require(screen.collectionView.cellForItem(at: IndexPath(item: 1, section: 0)) as? TranscriptMarkCell)
        #expect(mark.label.text == "Turn failed: bad gateway")
    }

    @Test("A long prompt's bubble wraps inside the screen, its text not clipped; a short one hugs its text")
    func bubbles() async throws {
        let session = StubSession()
        let screen = TranscriptViewController(row: row, session: session)
        let window = shown(screen)
        defer { window.isHidden = true }
        let long = String(repeating: "A prompt that goes on. ", count: 25)
        session.bring([
            Self.event("e1", .userText(item: "u1", text: "Hi", images: [])),
            Self.event("e2", .userText(item: "u2", text: long, images: [])),
        ], attach: true)
        screen.frame()
        try await waitFor { screen.drewRows }
        window.layoutIfNeeded()
        let width = screen.collectionView.bounds.width
        let short = try #require(screen.collectionView.cellForItem(at: IndexPath(item: 0, section: 0)) as? TranscriptTextCell)
        let shortFrame = short.textView.view.convert(short.textView.view.bounds, to: screen.collectionView)
        #expect(shortFrame.width < width / 3, "a short prompt's bubble hugs it: \(shortFrame)")
        let longCell = try #require(screen.collectionView.cellForItem(at: IndexPath(item: 1, section: 0)) as? TranscriptTextCell)
        let view = longCell.textView.view
        let frame = view.convert(view.bounds, to: screen.collectionView)
        #expect(frame.minX >= 48 && frame.maxX <= width, "\(frame) in \(width)")
        let needed = view.sizeThatFits(CGSize(width: view.bounds.width, height: .greatestFiniteMagnitude)).height
        #expect(view.bounds.height >= needed - 1, "clipped: \(view.bounds.height) of \(needed)")
        #expect(longCell.bounds.height >= view.bounds.height)
    }

    @Test("A row's change redraws its cell, shown again or not: one observation per item")
    func redrawAfterReshow() async throws {
        let session = StubSession()
        let screen = TranscriptViewController(row: row, session: session)
        let window = shown(screen)
        defer { window.isHidden = true }
        session.bring(Self.turn, attach: true)
        screen.frame()
        try await waitFor { screen.drewRows }
        window.layoutIfNeeded()
        // Configured again (as scrolling back would), then changed.
        var snapshot = screen.dataSource.snapshot()
        snapshot.reconfigureItems(["item:t1"])
        await screen.dataSource.apply(snapshot, animatingDifferences: false)
        window.layoutIfNeeded()
        let path = try #require(screen.dataSource.indexPath(for: "item:t1"))
        let cell = try #require(screen.collectionView.cellForItem(at: path) as? TranscriptToolCell)
        screen.model.item("item:t1")?.expanded = true
        try await waitFor { !cell.body.view.isHidden }
        #expect(cell.body.view.text.hasPrefix("# The demo project"))
        #expect(cell.header.accessibilityValue == "expanded")
    }

    /// Whether any activity indicator in `view` is animating and visible.
    static func visibleIndicators(in view: UIView) -> Bool {
        if let spinner = view as? UIActivityIndicatorView, spinner.isAnimating, !spinner.isHidden { return true }
        return view.subviews.contains { visibleIndicators(in: $0) }
            || ((view as? UICollectionView)?.backgroundView.map { visibleIndicators(in: $0) } ?? false)
    }

    private func waitFor(_ done: () -> Bool) async throws {
        for _ in 0..<300 where !done() {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(done())
    }
}
