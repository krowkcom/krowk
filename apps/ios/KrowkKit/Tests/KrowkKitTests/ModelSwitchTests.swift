// R-SWITCH-4, R-LAG-8, R-INST-6, R-INST-7: a session's model from the
// phone (ticket 135), over the transcript model and a stub session. The
// current model is the newest of the log's turn.started models and
// model.switched targets; a pick shows `switching to <model>…` at once,
// holds through the ack (and a running turn) until model.switched settles
// it, and goes back on an ack's error, which shows the host's sentence; a
// failed switch logged back moves it back. A rate-limit offer is Continue
// only for a turn this phone prompted, and Continue's prompt goes once the
// switch is logged. The picker lists the host's instances with their
// limits, greys one not ready, and offers nothing with the host offline.
// Through the fleet: AppTests' ModelSwitchTests.
import Testing
import UIKit
@testable import KrowkKit

@MainActor
@Suite("R-SWITCH-4, R-LAG-8: the session's model")
struct ModelSwitchModelTests {
    static let session = "s1"
    static let a = ModelChoice(instance: "anthropic", model: "claude-sonnet-4-6")
    static let b = ModelChoice(instance: "anthropic:personal", model: "claude-sonnet-4-6")

    static func event(_ id: String, _ change: TranscriptChange?, at time: Int64? = nil) -> TranscriptEvent {
        TranscriptEvent(id: id, sessionID: session, change: change, timeMs: time)
    }

    /// A model read at rest on `a`, its first turn over.
    static func model() -> TranscriptModel {
        let model = TranscriptModel(sessionID: session)
        model.attach([
            event("e1", .turnStarted(turn: "t1", model: a), at: 1),
            event("e2", .turnCompleted(turn: "t1", end: .completed), at: 2),
        ])
        return model
    }

    @Test("R-LAG-8, R-SWITCH-4: the subtitle shows switching… at once, holds through the ack, and settles on model.switched")
    func switchingUntilLogged() {
        let model = Self.model()
        #expect(model.modelLine == "anthropic/claude-sonnet-4-6")
        let token = model.issue(.switchModel(Self.b))
        #expect(model.modelLine == "switching to anthropic:personal/claude-sonnet-4-6…", "shown before anything crosses into the core")
        #expect(model.commandsOut == 1)
        model.apply(frame: [.commandMinted(token: token, id: "d-1"), .commandSent(id: "d-1", queued: false), .commandAcked(id: "d-1", error: nil)])
        #expect(model.switching == Self.b, "the ack alone doesn't settle it")
        #expect(model.commandsOut == 0)
        model.apply(frame: [.logged([Self.event("e3", .modelSwitched(model: Self.b.model, from: Self.a, to: Self.b), at: 3)])])
        #expect(model.switching == nil)
        #expect(model.currentModel == Self.b)
        #expect(model.modelLine == "anthropic:personal/claude-sonnet-4-6")
        #expect(model.item("switch:e3")?.text == "Now on claude-sonnet-4-6")
        // The next turn names it.
        model.apply(frame: [.logged([Self.event("e4", .turnStarted(turn: "t2", model: Self.b), at: 4)])])
        #expect(model.model(ofTurn: "t2") == Self.b)
    }

    @Test("R-SWITCH-4: an ack with an error puts the host's sentence under the subtitle, and the subtitle goes back")
    func refusedGoesBack() {
        let model = Self.model()
        let token = model.issue(.switchModel(ModelChoice(instance: "no-such-instance", model: "x")))
        model.apply(frame: [.commandMinted(token: token, id: "d-1"), .commandSent(id: "d-1", queued: false)])
        let sentence = "no instance named no-such-instance — the session stays on anthropic/claude-sonnet-4-6"
        let applied = model.apply(frame: [.commandAcked(id: "d-1", error: sentence)])
        #expect(applied.switchRefused == sentence)
        #expect(model.switchRefusal == sentence)
        #expect(model.switching == nil)
        #expect(model.modelLine == "anthropic/claude-sonnet-4-6")
        // The next switch clears it.
        model.issue(.switchModel(Self.b))
        #expect(model.switchRefusal == nil)
    }

    @Test("R-LAG-8: a switch the host keeps for after the running turn reads switching… until the turn ends and it is logged")
    func heldThroughTheTurn() {
        let model = Self.model()
        model.apply(frame: [.logged([Self.event("e3", .turnStarted(turn: "t2", model: Self.a), at: 3)])])
        let token = model.issue(.switchModel(Self.b))
        model.apply(frame: [.commandMinted(token: token, id: "d-1"), .commandAcked(id: "d-1", error: nil)])
        model.apply(frame: [.logged([Self.event("e4", .turnCompleted(turn: "t2", end: .completed), at: 4)])])
        #expect(model.modelLine == "switching to anthropic:personal/claude-sonnet-4-6…")
        model.apply(frame: [.logged([Self.event("e5", .modelSwitched(model: Self.b.model, from: Self.a, to: Self.b), at: 5)])])
        #expect(model.modelLine == "anthropic:personal/claude-sonnet-4-6")
    }

    @Test("R-SWITCH-4: a switch that fails once it runs is logged back (switchFailed), and the subtitle moves back; the connection ending lets go of a switch out")
    func failedMovesBack() {
        let model = Self.model()
        model.apply(frame: [.logged([
            Self.event("e3", .modelSwitched(model: Self.b.model, from: Self.a, to: Self.b), at: 3),
            Self.event("e4", .turnStarted(turn: "t2", model: Self.b), at: 4),
            Self.event("e5", .turnCompleted(turn: "t2", end: .failed("no key")), at: 5),
            Self.event("e6", .modelSwitched(model: Self.a.model, from: Self.b, to: Self.a, failed: true), at: 6),
        ])])
        #expect(model.currentModel == Self.a)
        // A catch-up of an older line doesn't move it.
        model.apply(frame: [.caughtUp([Self.event("e0", .turnStarted(turn: "t0", model: Self.b), at: 0)])])
        #expect(model.currentModel == Self.a)
        model.issue(.switchModel(Self.b))
        model.connectionEnded()
        #expect(model.switching == nil)
        #expect(model.commandsOut == 0)
    }

    @Test("R-INST-7: an offer for a turn this phone prompted offers Continue; Continue's prompt goes once the switch is logged, not at its ack")
    func offerContinues() throws {
        let model = Self.model()
        let prompt = model.issue(.prompt("Do the thing"))
        model.apply(frame: [.commandMinted(token: prompt, id: "d-1"), .commandAcked(id: "d-1", error: nil)])
        model.apply(frame: [.logged([
            Self.event("e3", .turnStarted(turn: "t2", model: Self.a), at: 3),
            Self.event("e4", .userText(item: "u2", text: "Do the thing", images: [], turn: "t2"), at: 4),
            Self.event("e5", .turnCompleted(turn: "t2", end: .failed("limited")), at: 5),
        ])])
        model.apply(frame: [.result(session: Self.session, turn: "t2", offer: SwitchOfferInfo(from: Self.a, to: Self.b))])
        let offer = try #require(model.offer)
        #expect(offer.again == "Do the thing")
        #expect(offer.line == "anthropic/claude-sonnet-4-6 is at its limit — continue on anthropic:personal/claude-sonnet-4-6?")

        let taken = try #require(model.takeOffer())
        #expect(model.offer == nil)
        let token = model.issue(.switchModel(taken.info.to))
        model.prompt("Do the thing", afterSwitch: token)
        let acked = model.apply(frame: [.commandMinted(token: token, id: "d-2"), .commandAcked(id: "d-2", error: nil)])
        #expect(acked.promptAgain == nil, "not before the switch is logged")
        let logged = model.apply(frame: [.logged([Self.event("e6", .modelSwitched(model: Self.b.model, from: Self.a, to: Self.b), at: 6)])])
        #expect(logged.promptAgain == "Do the thing")
    }

    @Test("R-INST-7: a limited turn's prompt caught up after its result still gives the offer Continue; Continue's prompt waiting for its switch counts as out, so a screen left meanwhile is kept")
    func offerCaughtUpLate() throws {
        let model = Self.model()
        model.issue(.prompt("Late"))
        model.apply(frame: [.result(session: Self.session, turn: "t2", offer: SwitchOfferInfo(from: Self.a, to: Self.b))])
        #expect(model.offer?.again == nil, "its prompt hasn't come yet")
        model.apply(frame: [.caughtUp([Self.event("e3", .userText(item: "u2", text: "Late", images: [], turn: "t2"), at: 3)])])
        let offer = try #require(model.offer)
        #expect(offer.again == "Late")
        model.takeOffer()
        let token = model.issue(.switchModel(offer.info.to))
        model.prompt("Late", afterSwitch: token)
        model.apply(frame: [.commandMinted(token: token, id: "d-1"), .commandAcked(id: "d-1", error: nil)])
        #expect(model.commandsOut == 1, "the prompt still to send keeps a parked screen")
        let logged = model.apply(frame: [.logged([Self.event("e4", .modelSwitched(model: Self.b.model, from: Self.a, to: Self.b), at: 4)])])
        #expect(logged.promptAgain == "Late")
        #expect(model.commandsOut == 0)
    }

    @Test("R-INST-7: an offer for another device's turn only says the limit; a turn starting withdraws it; a refused Continue sends nothing")
    func offerElsewhere() {
        let model = Self.model()
        model.issue(.prompt("Mine"))
        model.apply(frame: [.logged([Self.event("e3", .userText(item: "u2", text: "Theirs", images: [], turn: "t2"), at: 3)])])
        model.apply(frame: [.result(session: Self.session, turn: "t2", offer: SwitchOfferInfo(from: Self.a, to: Self.b))])
        #expect(model.offer?.again == nil)
        #expect(model.offer?.line == "anthropic/claude-sonnet-4-6 is at its limit")
        model.apply(frame: [.logged([Self.event("e4", .turnStarted(turn: "t3", model: Self.a), at: 4)])])
        #expect(model.offer == nil, "another turn withdraws it")

        let token = model.issue(.switchModel(Self.b))
        model.prompt("Mine", afterSwitch: token)
        let refused = model.apply(frame: [.commandMinted(token: token, id: "d-9"), .commandAcked(id: "d-9", error: "no — the session stays on anthropic/claude-sonnet-4-6")])
        #expect(refused.promptAgain == nil)
        let later = model.apply(frame: [.logged([Self.event("e5", .modelSwitched(model: Self.b.model, from: Self.a, to: Self.b), at: 5)])])
        #expect(later.promptAgain == nil, "a refused Continue never sends its prompt")
    }
}

@MainActor
@Suite("R-SWITCH-4, R-INST-6, R-HAND-4: the subtitle and the model picker")
struct ModelSwitchScreenTests {
    let row = SessionRow(id: ModelSwitchModelTests.session, title: "A demo session", detail: "app", live: true, accessibilityLabel: "A demo session", hostDevice: "a1")

    @Test("R-LAG-8, R-SWITCH-4: the subtitle is the model; a pick sends switchModel and reads switching… at once; a refusal shows under it and the subtitle goes back")
    func subtitle() async throws {
        let session = StubSession()
        let hosts = StubHosts([StubHost(announcedInstances: [StubInstance(), StubInstance(name: "anthropic:personal")])])
        let screen = TranscriptViewController(row: row, session: session, hosts: hosts)
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        defer { window.isHidden = true }
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        session.bring([.attached([ModelSwitchModelTests.event("e1", .turnStarted(turn: "t1", model: ModelSwitchModelTests.a)),
                                  ModelSwitchModelTests.event("e2", .turnCompleted(turn: "t1", end: .completed))])])
        screen.frame()
        session.say(.live)
        for _ in 0..<200 where screen.statusBar.status != .live { try await Task.sleep(for: .milliseconds(10)) }
        #expect(screen.modelButton.configuration?.title == "anthropic/claude-sonnet-4-6")
        #expect(screen.navigationItem.subtitleView === screen.modelButton)

        #expect(screen.switchModel(to: ModelSwitchModelTests.b))
        #expect(screen.modelButton.configuration?.title == "switching to anthropic:personal/claude-sonnet-4-6…")
        let sent = try #require(session.commands.withLock { $0 }.last)
        #expect(sent.command == .switchModel(ModelSwitchModelTests.b))
        #expect(!screen.switchModel(to: ModelSwitchModelTests.a), "one switch at a time")

        let sentence = "anthropic:personal is not signed in — the session stays on anthropic/claude-sonnet-4-6"
        session.bring([.commandMinted(token: sent.token, id: "d-1"), .commandAcked(id: "d-1", error: sentence)])
        screen.frame()
        #expect(screen.modelButton.configuration?.title == "anthropic/claude-sonnet-4-6")
        #expect(!screen.switchNote.isHidden)
        #expect(screen.switchNote.text?.contains(sentence) == true)
    }

    @Test("R-HAND-4: with no live link a pick sends nothing, never queued for a later host")
    func needsTheLink() async throws {
        let session = StubSession()
        let screen = TranscriptViewController(row: row, session: session, hosts: StubHosts([StubHost()]))
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        defer { window.isHidden = true }
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        session.say(.hostOffline)
        for _ in 0..<200 where screen.statusBar.status != .hostOffline { try await Task.sleep(for: .milliseconds(10)) }
        #expect(!screen.switchModel(to: ModelSwitchModelTests.b))
        #expect(session.commands.withLock { $0 }.isEmpty)
    }

    @Test("R-INST-6, R-HAND-4: the picker lists each instance under its limit (the session's own line first), greys one not ready with its fix, marks the current model, and offers nothing with the host offline")
    func pickerSections() {
        let instances: [any AnnouncedInstance] = [
            StubInstance(limitState: "allowed"),
            StubInstance(name: "anthropic:personal"),
            StubInstance(name: "openai", ready: false, fix: "set OPENAI_API_KEY", models: []),
        ]
        let host = StubHost(announcedInstances: instances)
        let session = ModelPickerViewController.Session(current: ModelSwitchModelTests.a, live: true, hostOffline: false,
                                                        limits: ["anthropic": InstanceLimit(state: "warning", usedPercent: 85)])
        let (sections, names) = ModelPickerViewController.sections(host: host, listed: true, failed: false, instances: instances, session: session)
        #expect(names == ["anthropic", "anthropic:personal", "openai"])
        #expect(sections.map(\.header) == ["anthropic · warning: 85% used", "anthropic:personal", "openai"])
        #expect(sections[0].choices.first?.enabled == false, "the current model")
        #expect(sections[0].choices.first?.detail == "current")
        #expect(sections[1].choices.first?.enabled == true)
        #expect(sections[2].choices.first?.enabled == false)
        #expect(sections[2].choices.first?.detail == "set OPENAI_API_KEY")

        var offline = host
        offline.online = false
        let (none, _) = ModelPickerViewController.sections(host: offline, listed: true, failed: false, instances: instances, session: session)
        #expect(none.allSatisfy(\.choices.isEmpty))
        #expect(none.first?.footer == "desktop is offline: the model can be switched once it's back.")

        var away = session
        away.live = false
        let (waiting, _) = ModelPickerViewController.sections(host: host, listed: true, failed: false, instances: instances, session: away)
        #expect(waiting.flatMap(\.choices).allSatisfy { !$0.enabled }, "switching waits for the live link")
        #expect(waiting.last?.footer == "Switching waits for the link to desktop.")

        var busy = session
        busy.switching = ModelSwitchModelTests.b
        let (out, _) = ModelPickerViewController.sections(host: host, listed: true, failed: false, instances: instances, session: busy)
        #expect(out.flatMap(\.choices).allSatisfy { !$0.enabled }, "one switch at a time")
        #expect(out.last?.footer == "Switching to anthropic:personal/claude-sonnet-4-6…")
        let (unread, _) = ModelPickerViewController.sections(host: nil, listed: false, failed: true, instances: [], session: session)
        #expect(unread.first?.footer == "Your hosts couldn't be read just now: the model can be switched once they are.")
    }
}
