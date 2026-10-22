// R-LAG-8, R-HAND-4, R-IOS-5: the composer under the transcript, over a
// stub session (ticket 90). Send shows the bubble in the same run-loop
// turn, before anything reaches the session, and hands the session the
// command with the bubble's token; while a turn runs the button reads
// Steer and Stop shows, which reads "stopping…" at once; with no host Send
// still queues a prompt while Steer and Stop are disabled, saying host
// offline; a refused bubble's "Edit and resend" puts its text back; ⌘↵
// and ⌘. are key commands; typing is never held up; a screen left with
// commands out is parked, still followed, until they're answered. Through
// the fleet: AppTests' ComposerTests.
import Testing
import UIKit
@testable import KrowkKit

@MainActor
@Suite("R-LAG-8, R-HAND-4: the composer")
struct ComposerScreenTests {
    let row = SessionRow(id: TranscriptScreenTests.session, title: "A demo session", detail: "app", live: true, accessibilityLabel: "A demo session")

    static func event(_ id: String, _ change: TranscriptChange?, parent: String? = nil) -> TranscriptEvent {
        TranscriptEvent(id: id, parentID: parent, sessionID: TranscriptScreenTests.session, change: change)
    }

    /// A screen in a window, its first turn drawn and the bar live.
    func opened() async throws -> (StubSession, TranscriptViewController, UIWindow) {
        let session = StubSession()
        let screen = TranscriptViewController(row: row, session: session)
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        window.layoutIfNeeded()
        session.bring(TranscriptScreenTests.turn, attach: true)
        screen.frame()
        session.say(.live)
        try await waitFor { screen.drewRows && screen.statusBar.status == .live }
        return (session, screen, window)
    }

    @Test("R-LAG-8, R-IOS-5: Send shows the bubble in the same run-loop turn, sending…, at the end of the list, before the session has the command; the field empties and stays editable")
    func sendIsOptimistic() async throws {
        let (session, screen, window) = try await opened()
        defer { window.isHidden = true }
        #expect(screen.composer.sendButton.configuration?.title == "Send")
        #expect(!screen.composer.sendButton.isEnabled, "nothing typed")
        screen.composer.text = "Please fix the build"
        #expect(screen.composer.sendButton.isEnabled)

        screen.composer.sendButton.sendActions(for: .primaryActionTriggered)
        // No await between the tap and these: the same run-loop turn.
        let id = try #require(screen.model.ids.last)
        #expect(screen.model.item(id)?.kind == .pending)
        #expect(screen.model.item(id)?.text == "Please fix the build")
        #expect(screen.model.item(id)?.caption == "sending…")
        #expect(screen.dataSource.snapshot().itemIdentifiers.last == id, "drawn now, not on a later frame")
        let token = try #require(screen.model.token(ofBubble: id))
        #expect(session.commands.withLock { $0 } == [StubSession.Sent(command: .prompt("Please fix the build"), token: token)])
        #expect(screen.composer.text.isEmpty)
        #expect(screen.composer.textView.isEditable, "typing is never held up by a send")

        window.layoutIfNeeded()
        let path = try #require(screen.dataSource.indexPath(for: id))
        let cell = try #require(screen.collectionView.cellForItem(at: path) as? TranscriptPendingCell)
        #expect(cell.caption.text == "sending…")
        #expect(cell.resend.isHidden)

        // Settled by the ack (its id minted first), then replaced by the
        // logged item: one row, never both.
        session.bring([.commandMinted(token: token, id: "d-1"), .commandSent(id: "d-1", queued: false), .commandAcked(id: "d-1", error: nil)])
        screen.frame()
        #expect(screen.model.item(id)?.caption == "")
        session.bring([.logged([Self.event("e7", .userText(item: "u7", text: "Please fix the build", images: []), parent: "e6")])])
        screen.frame()
        #expect(screen.model.ids.last == "item:u7")
        #expect(screen.model.item(id) == nil)
        #expect(screen.model.ids.filter { $0.hasPrefix("pending:") }.isEmpty)
    }

    @Test("R-LAG-8: while a turn runs the button reads Steer and sends a steer; Stop shows, and reads stopping… at once until the turn ends interrupted")
    func steerAndStop() async throws {
        let (session, screen, window) = try await opened()
        defer { window.isHidden = true }
        #expect(screen.composer.stopButton.isHidden)
        session.bring([.logged([Self.event("e7", .turnStarted(turn: "t2"), parent: "e6")])])
        screen.frame()
        #expect(screen.composer.sendButton.configuration?.title == "Steer")
        #expect(!screen.composer.stopButton.isHidden)
        #expect(screen.composer.stopButton.isEnabled)

        screen.composer.text = "Use the other API"
        screen.sendTyped()
        let steer = try #require(session.commands.withLock { $0 }.last)
        #expect(steer.command == .steer("Use the other API"))

        screen.composer.stopButton.sendActions(for: .primaryActionTriggered)
        #expect(screen.composer.stopButton.configuration?.title == "stopping…")
        #expect(!screen.composer.stopButton.isEnabled, "one stop at a time")
        let stop = try #require(session.commands.withLock { $0 }.last)
        #expect(stop.command == .interrupt)
        // Typing still works while it stops.
        #expect(screen.composer.textView.isEditable)

        session.bring([.logged([Self.event("e8", .turnCompleted(turn: "t2", end: .interrupted), parent: "e7")])])
        screen.frame()
        #expect(screen.model.item("turn:t2")?.text == "Turn interrupted")
        #expect(screen.composer.stopButton.isHidden)
        #expect(screen.composer.sendButton.configuration?.title == "Send")
    }

    @Test("R-HAND-4: with no host Send still queues a prompt, its bubble saying it waits for the laptop; Steer and Stop are disabled, saying host offline")
    func hostOffline() async throws {
        let (session, screen, window) = try await opened()
        defer { window.isHidden = true }
        session.say(.hostOffline)
        try await waitFor { screen.statusBar.status == .hostOffline }
        screen.composer.text = "Run the tests when you're back"
        #expect(screen.composer.sendButton.isEnabled, "a prompt queues with no host")
        #expect(screen.composer.noteLabel.text?.hasPrefix("host offline") == true)
        screen.sendTyped()
        let sent = try #require(session.commands.withLock { $0 }.last)
        session.bring([.commandMinted(token: sent.token, id: "d-1"), .commandSent(id: "d-1", queued: true)])
        screen.frame()
        let id = TranscriptModel.bubbleID(sent.token)
        #expect(screen.model.item(id)?.caption == "queued — sends when your laptop is back")

        // A turn the host left running: Steer and Stop wait for it.
        session.bring([.logged([Self.event("e7", .turnStarted(turn: "t2"), parent: "e6")])])
        screen.frame()
        screen.composer.text = "steer"
        #expect(screen.composer.sendButton.configuration?.title == "Steer")
        #expect(!screen.composer.sendButton.isEnabled)
        #expect(!screen.composer.stopButton.isEnabled)
        #expect(screen.composer.noteLabel.text?.hasPrefix("host offline") == true)
        let before = session.commands.withLock { $0.count }
        screen.stopTurn()
        screen.sendTyped()
        #expect(session.commands.withLock { $0.count } == before, "nothing sent while Steer and Stop are disabled")

        // The host back: a queued bubble no longer says it waits for it.
        session.say(.live)
        try await waitFor { screen.statusBar.status == .live }
        #expect(screen.model.item(id)?.caption == "queued…")
        #expect(screen.composer.sendButton.isEnabled)
    }

    @Test("R-LAG-8: a refused bubble shows the host's sentence and Edit and resend, which puts the text back in the composer; the resend is a new command")
    func editAndResend() async throws {
        let (session, screen, window) = try await opened()
        defer { window.isHidden = true }
        screen.composer.text = "Ship it"
        screen.sendTyped()
        let first = try #require(session.commands.withLock { $0 }.last)
        let sentence = "session s1 is moving to another machine; send it there once it has moved"
        session.bring([.commandMinted(token: first.token, id: "d-1"), .commandAcked(id: "d-1", error: sentence)])
        screen.frame()
        let id = TranscriptModel.bubbleID(first.token)
        window.layoutIfNeeded()
        let path = try #require(screen.dataSource.indexPath(for: id))
        let cell = try #require(screen.collectionView.cellForItem(at: path) as? TranscriptPendingCell)
        #expect(cell.caption.text == sentence)
        #expect(!cell.resend.isHidden)

        cell.resend.sendActions(for: .primaryActionTriggered)
        #expect(screen.composer.text == "Ship it")
        #expect(screen.model.item(id) == nil)
        screen.sendTyped()
        let again = try #require(session.commands.withLock { $0 }.last)
        #expect(again.command == .prompt("Ship it"))
        #expect(again.token != first.token, "a resend is a new command")
    }

    @Test("R-LAG-8: ⌘↵ sends and ⌘. stops (hardware keyboards); Return alone is a new line")
    func keyCommands() async throws {
        let (_, screen, window) = try await opened()
        defer { window.isHidden = true }
        let commands = screen.keyCommands ?? []
        #expect(commands.contains { $0.input == "\r" && $0.modifierFlags == .command })
        #expect(commands.contains { $0.input == "." && $0.modifierFlags == .command })
        #expect(!commands.contains { $0.input == "\r" && $0.modifierFlags.isEmpty })
    }

    @Test("R-LAG-8: a blank text can't be sent; one too long says so")
    func validation() async throws {
        let (session, screen, window) = try await opened()
        defer { window.isHidden = true }
        screen.composer.text = "   \n "
        #expect(!screen.composer.sendButton.isEnabled)
        screen.sendTyped()
        #expect(session.commands.withLock { $0 }.isEmpty)
        screen.composer.text = String(repeating: "y", count: ComposerRules.maxBytes + 10)
        #expect(!screen.composer.sendButton.isEnabled)
        #expect(screen.composer.noteLabel.text?.hasPrefix("Too long to send") == true)
    }

    @Test("R-HAND-4: a screen left with a command out is parked, its session not stopped, until the host answers and logs it; then it is let go and stopped")
    func parkedUntilAnswered() async throws {
        let session = StubSession()
        let parking = ParkedScreens()
        let screen = TranscriptViewController(row: row, session: session)
        screen.parking = parking
        let list = UIViewController()
        let navigation = UINavigationController(rootViewController: list)
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = navigation
        window.makeKeyAndVisible()
        defer { window.isHidden = true }
        navigation.pushViewController(screen, animated: false)
        window.layoutIfNeeded()
        session.bring(TranscriptScreenTests.turn, attach: true)
        screen.frame()
        session.say(.hostOffline)
        try await waitFor { screen.drewRows && screen.statusBar.status == .hostOffline }
        screen.composer.text = "later"
        screen.sendTyped()
        let sent = try #require(session.commands.withLock { $0 }.last)

        navigation.popViewController(animated: false)
        window.layoutIfNeeded()
        try await waitFor { screen.parked }
        #expect(parking.count == 1)
        #expect(session.stopped.withLock { $0 } == 0, "its connection still runs")

        // What comes while parked is applied as it comes.
        session.bring([.commandMinted(token: sent.token, id: "d-1"), .commandSent(id: "d-1", queued: true)])
        try await waitFor { screen.model.item(TranscriptModel.bubbleID(sent.token))?.caption == "queued — sends when your laptop is back" }
        #expect(screen.parked)

        // Opened again: the same screen, as it was, not parked.
        #expect(parking.take(row.id) === screen)
        #expect(!screen.parked && parking.count == 0)
        navigation.pushViewController(screen, animated: false)
        window.layoutIfNeeded()
        navigation.popViewController(animated: false)
        window.layoutIfNeeded()
        try await waitFor { screen.parked }

        // Answered, and its logged item here: let go, and stopped.
        session.bring([.commandAcked(id: "d-1", error: nil)])
        try await Task.sleep(for: .milliseconds(100))
        #expect(screen.parked, "acked, its logged item still to come")
        session.bring([.logged([Self.event("e7", .userText(item: "u7", text: "later", images: []), parent: "e6")])])
        try await waitFor { !screen.parked }
        #expect(parking.count == 0)
        #expect(session.stopped.withLock { $0 } == 1)
    }

    private func waitFor(_ done: () -> Bool) async throws {
        for _ in 0..<200 where !done() {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(done())
    }
}
