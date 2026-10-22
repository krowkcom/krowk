// R-LAG-8, R-IOS-5, R-HAND-4: the bubble reconciler (ticket 90). What the
// composer typed shows at once, keyed by the core's command id once it's
// minted: sending or queued, settled by the host's ack or refused with its
// sentence; a logged `userText` takes the place of the oldest live bubble
// with its text, FIFO, so two identical prompts reconcile one each; a
// catch-up replaying a logged item, or the session read at rest, never
// adds a bubble back or takes another. "stopping…" lasts until the
// interrupt's ack or the turn's end. The composer's states over a stub
// session are ComposerScreenTests; through the fleet, AppTests'
// ComposerTests.
import Testing
@testable import KrowkKit

@Suite("R-LAG-8, R-IOS-5: the pending-command reconciler")
struct PendingCommandsTests {
    @Test("R-LAG-8, R-IOS-5: a prompt shows sending at once, its id minted by the core, queued with no host, settled by the ack; an ack for an unknown id changes nothing")
    func sendingQueuedSettled() {
        var book = PendingCommands()
        let token = book.add(.prompt, text: "Fix the build")
        #expect(book.entry(token)?.state == .sending)
        #expect(book.out == 1)
        book.minted(token, id: "dev-0a1b-1")
        #expect(book.sent("dev-0a1b-1", queued: true) == token)
        #expect(book.entry(token)?.state == .queued)
        #expect(book.acked("someone-else-7", error: nil) == nil)
        #expect(book.acked("dev-0a1b-1", error: nil) == .bubble(token: token))
        #expect(book.entry(token)?.state == .settled)
        #expect(book.out == 0)
        // A late `sent` (a resend after a reconnect) never moves it back.
        #expect(book.sent("dev-0a1b-1", queued: false) == nil)
        #expect(book.entry(token)?.state == .settled)
    }

    @Test("R-LAG-8, R-IOS-5: two identical prompts reconcile FIFO, one each, with their logged userText items")
    func fifoWithDuplicateTexts() {
        var book = PendingCommands()
        let first = book.add(.prompt, text: "again")
        let other = book.add(.steer, text: "something else")
        let second = book.add(.prompt, text: "again")
        #expect(book.logged("again") == first)
        #expect(book.bubbles.map(\.token) == [other, second])
        #expect(book.logged("again") == second)
        #expect(book.logged("again") == nil, "a third logged item has no bubble left to take")
        #expect(book.logged("something else") == other)
        #expect(book.entries.isEmpty)
    }

    @Test("R-LAG-8: a refused ack turns the bubble into the host's sentence; a logged item never takes a refused bubble; edit-and-resend removes it")
    func refused() {
        var book = PendingCommands()
        let token = book.add(.prompt, text: "move it")
        book.minted(token, id: "d-r-1")
        let sentence = "session s1 is moving to another machine; send it there once it has moved"
        #expect(book.acked("d-r-1", error: sentence) == .bubble(token: token))
        #expect(book.entry(token)?.state == .refused(sentence))
        #expect(book.out == 0)
        #expect(book.logged("move it") == nil)
        // An empty reason still reads as a sentence.
        let blank = book.add(.steer, text: "x")
        book.minted(blank, id: "d-r-2")
        _ = book.acked("d-r-2", error: "  ")
        #expect(book.entry(blank)?.state == .refused("The host refused this."))
        book.remove(token)
        #expect(book.entry(token) == nil)
    }

    @Test("R-LAG-8: stopping lasts until the interrupt's ack or the turn's end, whichever first; a refused stop says why")
    func stopping() {
        var book = PendingCommands()
        let stop = book.add(.interrupt)
        #expect(book.stopping)
        #expect(book.bubbles.isEmpty, "an interrupt draws no bubble")
        book.minted(stop, id: "d-s-1")
        #expect(book.turnEnded().isEmpty, "no steer went unread")
        #expect(!book.stopping)
        #expect(book.acked("d-s-1", error: nil) == nil, "its ack after the turn's end changes nothing")

        let again = book.add(.interrupt)
        book.minted(again, id: "d-s-2")
        #expect(book.acked("d-s-2", error: "no turn running") == .interrupt(token: again, error: "no turn running"))
        #expect(!book.stopping)
    }

    @Test("R-LAG-8: a steer the host took that its turn ended without reading is refused, to be sent again as a prompt; a prompt isn't")
    func unreadSteer() {
        var book = PendingCommands()
        let steer = book.add(.steer, text: "too late")
        book.minted(steer, id: "d-u-1")
        _ = book.acked("d-u-1", error: nil)
        let prompt = book.add(.prompt, text: "next")
        book.minted(prompt, id: "d-u-2")
        _ = book.acked("d-u-2", error: nil)
        #expect(book.turnEnded() == [steer])
        #expect(book.entry(steer)?.state == .refused(PendingCommands.unread))
        #expect(book.entry(prompt)?.state == .settled)
        // A prompt retyped from it is what a logged item with its text takes
        // first; a gap's catch-up bringing the steer's own takes the steer.
        let retyped = book.add(.prompt, text: "too late")
        #expect(book.logged("too late") == retyped)
        #expect(book.logged("too late") == steer)

        // One acked after its turn ended is refused as unread too.
        let late = book.add(.steer, text: "later still")
        book.minted(late, id: "d-u-3")
        #expect(book.acked("d-u-3", error: nil, turnRunning: false) == .bubble(token: late))
        #expect(book.entry(late)?.state == .refused(PendingCommands.unread))
    }

    @Test("R-HAND-4, R-LAG-8: a queued bubble the host logs after a welcome is taken by its logged item even before its ack; the late ack changes nothing")
    func loggedBeforeAck() {
        var book = PendingCommands()
        let token = book.add(.prompt, text: "while you slept")
        book.minted(token, id: "d-q-1")
        book.sent("d-q-1", queued: true)
        #expect(book.logged("while you slept") == token)
        #expect(book.acked("d-q-1", error: nil) == nil)
        #expect(book.out == 0)
    }

    @Test("R-LAG-8: a connection that stops for good refuses what's out, saying whether it may have run, and drops the stop")
    func connectionEnded() {
        var book = PendingCommands()
        let queued = book.add(.prompt, text: "a")
        book.minted(queued, id: "d-e-1")
        book.sent("d-e-1", queued: true)
        let sending = book.add(.prompt, text: "b")
        book.minted(sending, id: "d-e-2")
        let settled = book.add(.prompt, text: "c")
        book.minted(settled, id: "d-e-3")
        _ = book.acked("d-e-3", error: nil)
        book.add(.interrupt)
        let never = book.add(.prompt, text: "d")
        #expect(Set(book.connectionEnded()) == [queued, sending, never])
        #expect(book.entry(never)?.state == .refused("Not sent: the connection to this session stopped. Open the session again to send it."), "the core never gave it an id")
        guard case .refused(let notSent)? = book.entry(queued)?.state, case .refused(let mayHave)? = book.entry(sending)?.state else {
            Issue.record("not refused: \(book.entries)")
            return
        }
        #expect(notSent.hasPrefix("Not sent"))
        #expect(mayHave.contains("may have run"))
        #expect(book.entry(settled)?.state == .settled)
        #expect(!book.stopping)
        #expect(book.out == 0)
    }

    @Test("R-LAG-8: a blank text can't be sent; one past 256 KiB says how long it is")
    func rules() {
        #expect(ComposerRules.problem("hello") == nil)
        #expect(ComposerRules.problem(" \n\t ") == "")
        let long = String(repeating: "x", count: ComposerRules.maxBytes + 1)
        #expect(ComposerRules.problem(long)?.hasPrefix("Too long to send") == true)
        #expect(ComposerRules.problem(String(repeating: "x", count: ComposerRules.maxBytes)) == nil)
    }
}

@MainActor
@Suite("R-LAG-8, R-IOS-5, R-HAND-4: the transcript model's bubbles")
struct PendingBubbleModelTests {
    static let session = "s1"

    static func event(_ id: String, _ change: TranscriptChange?, parent: String? = nil) -> TranscriptEvent {
        TranscriptEvent(id: id, parentID: parent, sessionID: session, change: change)
    }

    static func user(_ id: String, item: String, _ text: String, parent: String? = nil) -> TranscriptEvent {
        event(id, .userText(item: item, text: text, images: []), parent: parent)
    }

    @Test("R-LAG-8: a bubble is a row at once, at the end, below the rows that come after it; its caption follows sent, queued and the ack")
    func bubbleRow() {
        let model = TranscriptModel(sessionID: Self.session)
        model.attach([Self.event("e0", nil), Self.user("e1", item: "u1", "first")])
        let token = model.issue(.prompt("second"))
        let id = TranscriptModel.bubbleID(token)
        #expect(model.ids == ["item:u1", id], "shown before anything crosses into the core")
        #expect(model.item(id)?.kind == .pending)
        #expect(model.item(id)?.caption == "sending…")

        model.apply(frame: [.commandMinted(token: token, id: "d-1"), .commandSent(id: "d-1", queued: true)])
        #expect(model.item(id)?.caption == "queued…")
        model.setHostOffline(true)
        #expect(model.item(id)?.caption == "queued — sends when your laptop is back")
        model.setHostOffline(false)
        // Another device's prompt logged meanwhile: above the bubble.
        model.apply(frame: [.logged([Self.user("e2", item: "u2", "from the laptop", parent: "e1")])])
        #expect(model.ids == ["item:u1", "item:u2", id])
        model.apply(frame: [.commandAcked(id: "d-1", error: nil)])
        #expect(model.item(id)?.caption == "")
        #expect(model.commandsOut == 0)
    }

    @Test("R-LAG-8, R-IOS-5: the logged userText takes the bubble's place, never both; a catch-up replaying it adds no bubble back and takes no other; one read at rest takes none")
    func reconcileAndReplay() {
        let model = TranscriptModel(sessionID: Self.session)
        model.attach([Self.event("e0", nil)])
        let first = model.issue(.prompt("same"))
        let second = model.issue(.prompt("same"))
        model.apply(frame: [.commandMinted(token: first, id: "d-1"), .commandSent(id: "d-1", queued: false),
                            .commandMinted(token: second, id: "d-2"), .commandSent(id: "d-2", queued: false)])
        model.apply(frame: [.logged([Self.user("e1", item: "u1", "same", parent: "e0")])])
        #expect(model.ids == ["item:u1", TranscriptModel.bubbleID(second)], "the oldest bubble went, FIFO")
        // The same event again, by a catch-up: applied once already.
        model.apply(frame: [.caughtUp([Self.user("e1", item: "u1", "same", parent: "e0")])])
        #expect(model.ids == ["item:u1", TranscriptModel.bubbleID(second)])
        // A catch-up bringing the second's logged item: it goes too.
        model.apply(frame: [.caughtUp([Self.user("e2", item: "u2", "same", parent: "e1")])])
        #expect(model.ids == ["item:u1", "item:u2"])

        // Read at rest (a new screen of the same session): history never
        // takes a bubble typed now.
        let fresh = TranscriptModel(sessionID: Self.session)
        let typed = fresh.issue(.prompt("same"))
        fresh.attach([Self.event("e0", nil), Self.user("e1", item: "u1", "same")])
        #expect(fresh.ids == ["item:u1", TranscriptModel.bubbleID(typed)])
    }

    @Test("R-LAG-8: a refused bubble shows the host's sentence; taken back, its text returns and the row goes")
    func refusedAndTakenBack() {
        let model = TranscriptModel(sessionID: Self.session)
        let token = model.issue(.steer("go left"))
        let id = TranscriptModel.bubbleID(token)
        let applied = model.apply(frame: [.commandMinted(token: token, id: "d-1"), .commandAcked(id: "d-1", error: "the turn in session s1 is finishing and reads no more input — send it as the next prompt")])
        #expect(model.item(id)?.refused == true)
        #expect(model.item(id)?.caption.hasPrefix("the turn in session s1 is finishing") == true)
        #expect(applied.refused.map(\.id) == [id])
        // A logged item with its text doesn't take a refused bubble.
        model.apply(frame: [.logged([Self.user("e1", item: "u1", "go left")])])
        #expect(model.ids == ["item:u1", id])
        #expect(model.takeBack(token) == "go left")
        #expect(model.ids == ["item:u1"])
        #expect(model.takeBack(token) == nil)
    }

    @Test("R-LAG-8: a turn runs from turn.started (or an item started live) to turn.completed; stopping ends with the turn's end, which reads interrupted")
    func turnAndStop() {
        let model = TranscriptModel(sessionID: Self.session)
        model.attach([Self.event("e0", nil)])
        #expect(!model.isTurnRunning)
        model.apply(frame: [.logged([Self.event("e1", .turnStarted(turn: "t1"), parent: "e0")])])
        #expect(model.isTurnRunning)
        let stop = model.issue(.interrupt)
        #expect(model.stopping)
        #expect(model.ids.isEmpty, "an interrupt draws no row")
        model.apply(frame: [.commandMinted(token: stop, id: "d-1"), .commandSent(id: "d-1", queued: false)])
        model.apply(frame: [.logged([Self.event("e2", .turnCompleted(turn: "t1", end: .interrupted), parent: "e1")])])
        #expect(!model.isTurnRunning)
        #expect(!model.stopping)
        #expect(model.item("turn:t1")?.text == "Turn interrupted")

        // An item started live sets a turn running too; a late start of an
        // ended turn (a catch-up's) doesn't.
        model.apply(frame: [.started(session: Self.session, turn: "t2", item: "a2", kind: .assistant)])
        #expect(model.runningTurn == "t2")
        model.apply(frame: [.logged([Self.event("e3", .turnCompleted(turn: "t2", end: .completed), parent: "e2")])])
        model.apply(frame: [.caughtUp([Self.event("e1b", .turnStarted(turn: "t2"), parent: "e2")])])
        #expect(!model.isTurnRunning)
    }

    @Test("R-LAG-8: a steer the host refuses while the log shows a turn running (one whose end never reached it) lets the composer send prompts again")
    func staleTurn() {
        let model = TranscriptModel(sessionID: Self.session)
        model.attach([Self.event("e0", nil), Self.event("e1", .turnStarted(turn: "t1"), parent: "e0")])
        #expect(model.isTurnRunning)
        let steer = model.issue(.steer("go on"))
        model.apply(frame: [.commandMinted(token: steer, id: "d-1"), .commandAcked(id: "d-1", error: "session s1 has no turn running to steer — send it as a prompt instead")])
        #expect(!model.isTurnRunning)
        #expect(model.item(TranscriptModel.bubbleID(steer))?.refused == true)
        // A later item of that turn sets it running again; another refusal
        // (a session moving) leaves a running turn running.
        model.apply(frame: [.started(session: Self.session, turn: "t1", item: "a9", kind: .assistant)])
        #expect(model.isTurnRunning)
        let other = model.issue(.steer("again"))
        model.apply(frame: [.commandMinted(token: other, id: "d-2"), .commandAcked(id: "d-2", error: "session s1 is moving to another machine; send it there once it has moved")])
        #expect(model.isTurnRunning)
    }

    @Test("R-LAG-8: an older turn's end a catch-up brings late refuses no steer of the turn running now")
    func lateOldTurnEnd() {
        let model = TranscriptModel(sessionID: Self.session)
        model.apply(frame: [.logged([Self.event("e1", .turnStarted(turn: "t2"))])])
        let steer = model.issue(.steer("for t2"))
        model.apply(frame: [.commandMinted(token: steer, id: "d-1"), .commandAcked(id: "d-1", error: nil)])
        let applied = model.apply(frame: [.caughtUp([Self.event("e0", .turnCompleted(turn: "t1", end: .completed))])])
        #expect(applied.refused.isEmpty)
        #expect(model.item(TranscriptModel.bubbleID(steer))?.refused == false)
        #expect(model.isTurnRunning)
    }

    @Test("R-LAG-8: a stop the host refuses while the turn runs is reported; one refused after the turn ended is not")
    func stopRefused() {
        let model = TranscriptModel(sessionID: Self.session)
        model.apply(frame: [.logged([Self.event("e1", .turnStarted(turn: "t1"))])])
        let stop = model.issue(.interrupt)
        let applied = model.apply(frame: [.commandMinted(token: stop, id: "d-1"), .commandAcked(id: "d-1", error: "not_stoppable")])
        #expect(applied.stopRefused == "not_stoppable")
        #expect(!model.stopping)
        let late = model.issue(.interrupt)
        model.apply(frame: [.logged([Self.event("e2", .turnCompleted(turn: "t1", end: .completed), parent: "e1")])])
        let after = model.apply(frame: [.commandMinted(token: late, id: "d-2"), .commandAcked(id: "d-2", error: "no turn running")])
        #expect(after.stopRefused == nil)
    }

    @Test("R-LAG-8: refused bubbles a screen let go are restored on the session's next, refused, below its rows")
    func restored() throws {
        let old = TranscriptModel(sessionID: Self.session)
        let token = old.issue(.prompt("keep me"))
        old.apply(frame: [.commandUnsent(token: token, why: "Not sent: the connection to this session stopped.")])
        let fresh = TranscriptModel(sessionID: Self.session)
        fresh.attach([Self.user("e1", item: "u1", "earlier")])
        fresh.restore(old.refusals)
        let id = try #require(fresh.ids.last)
        #expect(fresh.ids.count == 2)
        #expect(fresh.item(id)?.refused == true)
        #expect(fresh.item(id)?.text == "keep me")
        #expect(fresh.commandsOut == 0)
    }

    @Test("R-LAG-8: a command that never reached the core, or a connection that stopped, refuses the bubble saying so")
    func unsentAndEnded() {
        let model = TranscriptModel(sessionID: Self.session)
        let a = model.issue(.prompt("a"))
        model.apply(frame: [.commandUnsent(token: a, why: "Not sent: the connection to this session stopped.")])
        #expect(model.item(TranscriptModel.bubbleID(a))?.refused == true)
        let b = model.issue(.prompt("b"))
        let refused = model.connectionEnded()
        #expect(refused.map(\.id) == [TranscriptModel.bubbleID(b)])
        #expect(model.commandsOut == 0)
    }
}
