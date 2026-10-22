// R-LAG-8, R-IOS-5, R-HAND-4, R-CLIENT-1: prompt, steer and interrupt from
// the phone (ticket 90), through the fake fleet's relay, on TestFleet's
// paired phone, each in a session of the test's own. The composer's bubble
// shows at once (the same run-loop turn as the tap), settles on the host's
// ack and gives way to the userText the host logs, never both; the mock
// model's answer streams after it. A steer during a streaming turn is
// acked and logged in that turn; Stop reads "stopping…" at once and the
// turn ends interrupted. With the host gone a prompt is shown queued, and
// once the fleet hosts the session again it is sent once and runs once
// (R-HAND-4). A socket dropped between the send and its ack still runs the
// prompt exactly once: the core resends it under its id after the welcome
// and the host answers a repeat with the ack it gave. The core (krowk-ffi's
// Viewer) owns the ids, the queue and the resend; the commands are the
// generated `Command`s (R-CLIENT-1).
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its URLs to this
// process. Hosted in the app, as the Keychain needs.
import Foundation
import Krowk
import KrowkAccount
import KrowkKit
import Testing
import UIKit

extension HostedKeychain {
    @MainActor
    @Suite("R-LAG-8, R-IOS-5, R-HAND-4: prompt, steer and interrupt through the fleet")
    struct ComposerTests {
        @Test("R-LAG-8, R-IOS-5: a prompt's bubble shows at once, settles on the ack and gives way to its logged userText, never both; the model's answer follows")
        func promptReconciles() async throws {
            let (screen, window, _, _) = try await LiveFollowTests.open("Composer prompt")
            defer { window.isHidden = true }
            let model = screen.model
            let before = Set(model.ids)
            let text = "Say something short, please"
            screen.composer.text = text
            screen.sendTyped()
            // No await since the tap: the same run-loop turn.
            let bubble = try #require(model.ids.last)
            #expect(model.item(bubble)?.kind == .pending)
            #expect(model.item(bubble)?.caption == "sending…")
            #expect(screen.dataSource.snapshot().itemIdentifiers.last == bubble)
            Self.keep("composer-sending", window)

            var settled = false
            var both = false
            try await LiveFollowTests.wait(until: {
                if model.item(bubble)?.caption == "" { settled = true }
                let logged = model.ids.compactMap(model.item).contains { $0.kind == .user && $0.text == text }
                if logged && model.item(bubble) != nil { both = true }
                return model.item(bubble) == nil && logged && Self.lastTurnEnded(model, after: before)
            }, within: .seconds(30), "not reconciled: \(TranscriptTests.rows(of: model))")
            #expect(!both, "the bubble and its logged item showed together")
            #expect(settled || model.commandsOut == 0, "never acked")
            let fresh = model.ids.filter { !before.contains($0) }.compactMap(model.item)
            #expect(fresh.filter { $0.kind == .user && $0.text == text }.count == 1)
            let at = try #require(fresh.firstIndex { $0.kind == .user })
            #expect(fresh[at...].contains { $0.kind == .assistant && $0.done && !$0.text.isEmpty }, "no answer after the prompt: \(TranscriptTests.rows(of: model))")
            #expect(fresh.last?.text == "Turn completed")
            Self.keep("composer-answered", window)
        }

        @Test("R-LAG-8: a steer during a running turn is acked, then logged as userText in that turn, which goes on")
        func steerIsLoggedInTheTurn() async throws {
            let (screen, window, session, _) = try await LiveFollowTests.open("Composer steer")
            defer { window.isHidden = true }
            let model = screen.model
            let before = Set(model.ids)
            #expect(try await SessionListTests.control("/\(session)/turn", body: "Please stream a long answer") == "started")
            try await Self.streaming(model, after: before)
            #expect(screen.composer.sendButton.configuration?.title == "Steer")
            #expect(!screen.composer.stopButton.isHidden)
            let turn = try #require(model.runningTurn)

            let text = "Also say which line was last"
            screen.composer.text = text
            screen.sendTyped()
            let bubble = try #require(model.ids.last)
            #expect(model.item(bubble)?.kind == .pending)
            Self.keep("composer-steer", window)
            var settled = false
            try await LiveFollowTests.wait(until: {
                if model.item(bubble)?.caption == "" { settled = true }
                return model.item("turn:\(turn)") != nil
            }, within: .seconds(60), "the turn never ended: \(TranscriptTests.rows(of: model))")
            #expect(settled, "the steer was never acked before its logged item came")
            #expect(model.item(bubble) == nil, "the bubble stayed beside its logged item")
            let rows = model.ids.filter { !before.contains($0) }
            let steered = rows.filter { model.item($0)?.kind == .user && model.item($0)?.text == text }
            #expect(steered.count == 1, "\(TranscriptTests.rows(of: model))")
            // In that turn: before its end, after its streamed answer.
            let steer = try #require(steered.first.flatMap(rows.firstIndex(of:)))
            let end = try #require(rows.firstIndex(of: "turn:\(turn)"))
            #expect(steer < end)
            #expect(rows[..<steer].contains { model.item($0)?.kind == .assistant })
            #expect(model.item("turn:\(turn)")?.text == "Turn completed")
        }

        @Test("R-LAG-8: Stop reads stopping… at once, and the turn ends interrupted, keeping what it wrote")
        func stopInterrupts() async throws {
            let (screen, window, session, _) = try await LiveFollowTests.open("Composer stop")
            defer { window.isHidden = true }
            let model = screen.model
            let before = Set(model.ids)
            #expect(try await SessionListTests.control("/\(session)/turn", body: "Please stream a long answer") == "started")
            try await Self.streaming(model, after: before)
            let turn = try #require(model.runningTurn)

            screen.composer.stopButton.sendActions(for: .primaryActionTriggered)
            #expect(screen.composer.stopButton.configuration?.title == "stopping…")
            #expect(model.stopping)
            Self.keep("composer-stopping", window)
            try await LiveFollowTests.wait(until: { model.item("turn:\(turn)") != nil }, within: .seconds(20), "the turn never ended: \(TranscriptTests.rows(of: model))")
            #expect(model.item("turn:\(turn)")?.text == "Turn interrupted")
            #expect(model.item("turn:\(turn)")?.alarming == true)
            #expect(!model.stopping)
            #expect(screen.composer.stopButton.isHidden)
            let answer = try #require(model.ids.filter { !before.contains($0) }.compactMap(model.item).first { $0.kind == .assistant })
            #expect(answer.text.count > 100, "what it wrote went")
            Self.keep("composer-interrupted", window)
        }

        @Test("R-HAND-4, R-IOS-5: with the host stopped a prompt shows queued and Steer and Stop are off; when the host comes back it is sent once and runs once")
        func r_hand_4_queuedPromptRunsOnceWhenTheHostReturns() async throws {
            let (screen, window, session, _) = try await LiveFollowTests.open("Composer queued")
            defer { window.isHidden = true }
            let model = screen.model
            let before = Set(model.ids)
            #expect(try await SessionListTests.control("/\(session)/release") == "released")
            try await LiveFollowTests.wait(until: { screen.statusBar.status == .hostOffline }, within: .seconds(15), "not host offline: \(screen.statusBar.status.text)")

            let text = "Run this when the laptop wakes"
            screen.composer.text = text
            #expect(screen.composer.sendButton.isEnabled, "a prompt queues with no host")
            #expect(screen.composer.noteLabel.text?.hasPrefix("host offline") == true)
            screen.sendTyped()
            let bubble = try #require(model.ids.last)
            try await LiveFollowTests.wait(until: { model.item(bubble)?.caption == "queued — sends when your laptop is back" }, within: .seconds(5),
                                           "not queued: \(model.item(bubble)?.caption ?? "gone")")
            Self.keep("composer-queued", window)
            // Nothing runs while the host is away.
            try await Task.sleep(for: .seconds(2))
            #expect(model.item(bubble) != nil)

            #expect(try await SessionListTests.control("/\(session)/host") == "hosted")
            try await LiveFollowTests.wait(until: {
                model.item(bubble) == nil && Self.lastTurnEnded(model, after: before)
                    && model.ids.compactMap(model.item).contains { $0.kind == .user && $0.text == text }
            }, within: .seconds(30), "the queued prompt never ran: \(screen.statusBar.status.text), \(TranscriptTests.rows(of: model))")
            // A while longer: it never runs again.
            try await Task.sleep(for: .seconds(3))
            let fresh = model.ids.filter { !before.contains($0) }.compactMap(model.item)
            #expect(fresh.filter { $0.kind == .user && $0.text == text }.count == 1, "\(TranscriptTests.rows(of: model))")
            #expect(fresh.filter { $0.id.hasPrefix("turn:") }.count == 1, "one turn: \(TranscriptTests.rows(of: model))")
            #expect(model.commandsOut == 0)
        }

        @Test("R-IOS-5, R-LAG-8: the socket dropped between the send and its ack, the prompt still runs exactly once after the reconnect and welcome")
        func dropBetweenSendAndAck() async throws {
            let (screen, window, _, live) = try await LiveFollowTests.open("Composer drop")
            defer { window.isHidden = true }
            let model = screen.model
            let before = Set(model.ids)
            let dials = live.dials
            let text = "Answer once, even over a bad link"
            screen.composer.text = text
            screen.sendTyped()
            // Queued on the connection's queue right behind the send: the
            // link goes before any ack can come back on it.
            live.dropConnection()
            try await LiveFollowTests.wait(until: { live.dials > dials && screen.statusBar.status == .live }, within: .seconds(20), "never back: \(screen.statusBar.status.text)")
            try await LiveFollowTests.wait(until: {
                model.commandsOut == 0 && Self.lastTurnEnded(model, after: before)
                    && model.ids.compactMap(model.item).contains { $0.kind == .user && $0.text == text }
            }, within: .seconds(30), "the prompt never ran: \(TranscriptTests.rows(of: model))")
            try await Task.sleep(for: .seconds(3))
            let fresh = model.ids.filter { !before.contains($0) }.compactMap(model.item)
            #expect(fresh.filter { $0.kind == .user && $0.text == text }.count == 1, "\(TranscriptTests.rows(of: model))")
            #expect(fresh.filter { $0.id.hasPrefix("turn:") }.count == 1, "one turn: \(TranscriptTests.rows(of: model))")
            #expect(!fresh.contains { $0.kind == .pending })
        }

        @Test("R-HAND-4: a screen left with a prompt queued keeps its connection; when the host returns the prompt runs and the screen is let go")
        func queuedPromptSurvivesLeavingTheScreen() async throws {
            let (screen, window, session, _) = try await LiveFollowTests.open("Composer left")
            defer { window.isHidden = true }
            let parking = ParkedScreens()
            screen.parking = parking
            let navigation = try #require(screen.navigationController)
            let list = UIViewController()
            navigation.setViewControllers([list, screen], animated: false)
            window.layoutIfNeeded()
            #expect(try await SessionListTests.control("/\(session)/release") == "released")
            try await LiveFollowTests.wait(until: { screen.statusBar.status == .hostOffline }, within: .seconds(15), "not host offline")
            let text = "Still here after you left"
            screen.composer.text = text
            screen.sendTyped()

            navigation.popViewController(animated: false)
            window.layoutIfNeeded()
            try await LiveFollowTests.wait(until: { screen.parked }, within: .seconds(5), "not parked")
            #expect(parking.count == 1)
            #expect(try await SessionListTests.control("/\(session)/host") == "hosted")
            try await LiveFollowTests.wait(until: { !screen.parked }, within: .seconds(30), "never let go: \(TranscriptTests.rows(of: screen.model))")
            #expect(parking.count == 0)
            #expect(screen.model.ids.compactMap(screen.model.item).filter { $0.kind == .user && $0.text == text }.count == 1)
        }

        @Test("R-PROTO-2, R-CLIENT-1: the viewer's generated sent and acked updates, with the minted id posted before them, map onto the bubble's states")
        func mapping() throws {
            let model = TranscriptModel(sessionID: "s1")
            let token = model.issue(.prompt("hi"))
            let posted: [MailboxItem] = [
                .minted(token: token, id: "dev-run-1"),
                .viewer(.sent(id: "dev-run-1", queued: true)),
            ]
            model.apply(frame: TranscriptUpdate.from(posted: posted, approvals: NoApprovals()))
            #expect(model.item(TranscriptModel.bubbleID(token))?.caption == "queued…")
            model.apply(frame: TranscriptUpdate.from([.acked(id: "dev-run-1", error: "refused, and why")], approvals: NoApprovals()))
            #expect(model.item(TranscriptModel.bubbleID(token))?.caption == "refused, and why")
            #expect(model.item(TranscriptModel.bubbleID(token))?.refused == true)
        }

        // MARK: Helpers

        /// The turn under way, its answer streaming a while.
        static func streaming(_ model: TranscriptModel, after before: Set<String>) async throws {
            try await LiveFollowTests.wait(until: {
                model.isTurnRunning && model.ids.filter { !before.contains($0) }.compactMap(model.item).contains { $0.kind == .assistant && $0.text.count > 200 }
            }, within: .seconds(30), "no answer streaming: \(TranscriptTests.rows(of: model))")
        }

        /// A turn ended since `before`, and none runs.
        static func lastTurnEnded(_ model: TranscriptModel, after before: Set<String>) -> Bool {
            !model.isTurnRunning && model.ids.contains { !before.contains($0) && $0.hasPrefix("turn:") }
        }

        /// `window` as drawn, light then dark, kept in the results as
        /// `krowk-<name>-light` and `-dark` (CI's screenshots artifact).
        static func keep(_ name: String, _ window: UIWindow) {
            for style in [UIUserInterfaceStyle.light, .dark] {
                window.overrideUserInterfaceStyle = style
                window.layoutIfNeeded()
                let image = UIGraphicsImageRenderer(bounds: window.bounds).image { _ in
                    _ = window.drawHierarchy(in: window.bounds, afterScreenUpdates: true)
                }
                if let png = image.pngData() {
                    Attachment.record(png, named: "krowk-\(name)-\(style == .dark ? "dark" : "light")")
                }
            }
            window.overrideUserInterfaceStyle = .unspecified
        }
    }
}
