// R-SWITCH-4, R-SWITCH-1, R-LAG-8, R-INST-6, R-INST-7: switching a
// session's model from the phone (ticket 135) against the fake fleet, on
// TestFleet's paired phone, each in a session of the test's own. Host A
// has two instances ready on the mock model, `anthropic` and
// `anthropic:personal`. The subtitle names the session's model; the picker
// lists host A's instances (`listHosts`) with their limits; a pick reads
// `switching to …` at once and settles once the host logs model.switched,
// and the next turn's turn.started names the model picked. A pick of an
// instance host A lacks (injected through the picker's test seam) shows
// the host's sentence and the subtitle stays. A turn on `anthropic` at its
// limit (POST KROWK_HOSTS_URL/limit: the mock answers 429 with
// `retry-after: 0`) raises the offer, and Continue runs the prompt again
// on `anthropic:personal`. The commands are the generated `switchModel`
// and `prompt` (R-CLIENT-1), handed to the core as the composer's are.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its URLs to this
// process. Hosted in the app, as the Keychain needs.
import Foundation
import Krowk
import KrowkAccount
import KrowkKit
import Testing
import UIKit

/// An instance host A doesn't have, for the picker's seam.
struct InjectedInstance: AnnouncedInstance {
    var name = "no-such-instance"
    var ready = true
    var fix: String?
    var models = ["no-such-model"]
    var limitState: String?
    var limitUsedPercent: Double?
    var limitResetsAtMs: Int64?
}

extension HostedKeychain {
    @MainActor
    @Suite("R-SWITCH-4, R-INST-6, R-INST-7: switching a session's model from the phone through the fleet")
    struct ModelSwitchTests {
        static let personal = "anthropic:personal"

        @Test("R-SWITCH-4, R-SWITCH-1, R-LAG-8: a pick reads switching… at once, the subtitle settles on the new model after model.switched, and the next turn's turn.started names it")
        func testR_SWITCH_4_aSwitchFromThePhoneRunsTheNextTurnOnThatModel() async throws {
            let (screen, window, _) = try await Self.open("Switch from the phone")
            defer { window.isHidden = true }
            let model = screen.model
            let before = try #require(model.currentModel)
            #expect(screen.modelButton.configuration?.title == before.name)
            let picker = try await Self.picker(screen)
            let target = try await Self.choice(in: picker, instance: Self.personal)
            Self.pick(target.identifier, in: picker)
            // R-LAG-8: no await since the pick.
            #expect(screen.modelButton.configuration?.title == "switching to \(target.choice.name)…")
            ComposerTests.keep("model-switching", window)
            try await LiveFollowTests.wait(until: { model.switching == nil && model.currentModel == target.choice }, within: .seconds(30),
                                           "the switch never settled: \(model.modelLine), \(TranscriptTests.rows(of: model))")
            #expect(screen.modelButton.configuration?.title == target.choice.name)
            #expect(model.ids.contains { $0.hasPrefix("switch:") && model.item($0)?.text == "Now on \(target.choice.model)" })

            // The next turn runs there (R-SWITCH-1: the session goes on).
            let turns = Set(model.ids.filter { $0.hasPrefix("turn:") })
            screen.composer.text = "Say hello"
            screen.sendTyped()
            try await LiveFollowTests.wait(until: { ComposerTests.lastTurnEnded(model, after: turns) }, within: .seconds(60),
                                           "the next turn never ended: \(TranscriptTests.rows(of: model))")
            let turn = try #require(model.ids.first { $0.hasPrefix("turn:") && !turns.contains($0) }).dropFirst("turn:".count)
            #expect(model.model(ofTurn: String(turn)) == target.choice, "the next turn.started names the model picked")
            #expect(model.item("turn:\(turn)")?.text == "Turn completed")
            ComposerTests.keep("model-switched", window)
        }

        @Test("R-SWITCH-4: a pick of an instance the host lacks is refused by it: its sentence shows under the subtitle, and the subtitle and the session stay")
        func testR_SWITCH_4_aRefusedSwitchShowsTheFixAndTheSessionStays() async throws {
            let (screen, window, _) = try await Self.open("Refused switch")
            defer { window.isHidden = true }
            let model = screen.model
            let before = try #require(model.currentModel)
            screen.pickerInstances = { $0 + [InjectedInstance()] }
            let picker = try await Self.picker(screen)
            _ = try await Self.choice(in: picker, instance: "no-such-instance")
            Self.pick("switch-model-no-such-instance/no-such-model", in: picker)
            #expect(screen.modelButton.configuration?.title == "switching to no-such-instance/no-such-model…")
            try await LiveFollowTests.wait(until: { model.switchRefusal != nil }, within: .seconds(30), "never refused: \(model.modelLine)")
            let said = try #require(model.switchRefusal)
            #expect(said.hasSuffix("the session stays on \(before.name)"), "\(said)")
            #expect(!screen.switchNote.isHidden)
            #expect(screen.switchNote.text?.contains(said) == true)
            #expect(screen.modelButton.configuration?.title == before.name)
            #expect(model.currentModel == before)
            ComposerTests.keep("model-refused", window)
        }

        @Test("R-INST-6: the picker shows each instance's limit, as the session's turns on it last reported")
        func testR_INST_6_thePickerShowsEachInstancesLimit() async throws {
            let (screen, window, _) = try await Self.open("Limits in the picker")
            defer { window.isHidden = true }
            let model = screen.model
            // A turn on each instance: each reports how near its limit it is.
            try await Self.turn(screen, "Say hello")
            let picker = try await Self.picker(screen)
            let target = try await Self.choice(in: picker, instance: Self.personal)
            Self.pick(target.identifier, in: picker)
            try await LiveFollowTests.wait(until: { model.switching == nil && model.currentModel == target.choice }, within: .seconds(30), "the switch never settled")
            try await Self.turn(screen, "Say hello")
            #expect(model.limits["anthropic"]?.state == "warning", "\(model.limits)")
            #expect(model.limits[Self.personal]?.state == "allowed", "\(model.limits)")

            let shown = try await Self.picker(screen)
            let table = shown.tableView!
            try await LiveFollowTests.wait(until: { Self.header(shown, "anthropic") != nil && Self.header(shown, Self.personal) != nil }, within: .seconds(30),
                                           "the picker never listed both instances")
            #expect(Self.header(shown, "anthropic") == "anthropic · warning: 85% used")
            #expect(Self.header(shown, Self.personal) == "\(Self.personal) · allowed")
            #expect(table.numberOfSections >= 2)
            ComposerTests.keep("model-picker", window)
        }

        @Test("R-INST-7, R-SWITCH-4: a prompt from the phone at anthropic's limit raises the offer; Continue switches to the instance offered and the prompt runs there")
        func testR_INST_7_anOfferFromALimitContinuesOnTheInstanceOffered() async throws {
            let (screen, window, _) = try await Self.open("Limit offer")
            defer { window.isHidden = true }
            let model = screen.model
            let before = try #require(model.currentModel)
            #expect(try await NewSessionTests.hostsControl("/limit") == "limited")
            let text = "Please say something at the limit"
            let turns = Set(model.ids.filter { $0.hasPrefix("turn:") })
            screen.composer.text = text
            screen.sendTyped()
            try await LiveFollowTests.wait(until: { model.offer != nil }, within: .seconds(60), "no offer: \(TranscriptTests.rows(of: model))")
            let offer = try #require(model.offer)
            #expect(offer.info.from == before)
            #expect(offer.info.to.instance == Self.personal)
            #expect(offer.again == text, "this phone sent the limited turn's prompt")
            #expect(!screen.offerBanner.isHidden)
            #expect(screen.offerLabel.text == "\(before.name) is at its limit — continue on \(offer.info.to.name)?")
            #expect(!screen.offerContinue.isHidden)
            ComposerTests.keep("model-offer", window)

            let limited = Set(model.ids.filter { $0.hasPrefix("turn:") })
            #expect(limited.count == turns.count + 1)
            screen.offerContinue.sendActions(for: .primaryActionTriggered)
            #expect(screen.offerBanner.isHidden)
            #expect(screen.modelButton.configuration?.title == "switching to \(offer.info.to.name)…")
            try await LiveFollowTests.wait(until: { ComposerTests.lastTurnEnded(model, after: limited) }, within: .seconds(60),
                                           "the prompt never ran again: \(TranscriptTests.rows(of: model))")
            let turn = try #require(model.ids.first { $0.hasPrefix("turn:") && !limited.contains($0) }).dropFirst("turn:".count)
            #expect(model.model(ofTurn: String(turn)) == offer.info.to, "the prompt ran on the instance offered")
            #expect(model.item("turn:\(turn)")?.text == "Turn completed")
            let prompts = model.ids.compactMap(model.item).filter { $0.kind == .user && $0.text == text }
            #expect(prompts.count == 2, "the limited turn's prompt, and the same again: \(TranscriptTests.rows(of: model))")
            #expect(model.currentModel == offer.info.to)
        }

        // MARK: Helpers

        /// A session of the test's own on host A, open on its transcript in
        /// a window, its row naming host A and the picker reading the
        /// fleet's hosts; live, its first turn drawn and its model known.
        static func open(_ title: String) async throws -> (TranscriptViewController, UIWindow, String) {
            let phone = try await TestFleet.shared.paired()
            let source = RegistrySessions(api: phone.api, storage: phone.storage)
            let host = try await NewSessionTests.host(source, online: true)
            let ready = host.announcedInstances.filter { $0.ready && !$0.models.isEmpty }.map(\.name)
            try #require(ready.contains("anthropic") && ready.contains(personal), "host A has both instances ready: \(ready)")
            let id = try await SessionListTests.control("", body: title)
            let opened = try await RelayConnectionTests.open(id, phone: phone)
            let row = SessionRow(id: id, title: title, detail: "", live: true, accessibilityLabel: title, hostDevice: host.device)
            let screen = TranscriptViewController(row: row, session: RelayConnection { opened }, hosts: source)
            let scene = try #require(UIApplication.shared.connectedScenes.first as? UIWindowScene, "no window scene in the host app")
            let window = UIWindow(windowScene: scene)
            window.rootViewController = UINavigationController(rootViewController: screen)
            window.makeKeyAndVisible()
            try await LiveFollowTests.wait(until: {
                screen.isLive && screen.model.ids.last?.hasPrefix("turn:") == true && screen.model.currentModel != nil
            }, within: .seconds(20), "not live with the first turn and its model: \(screen.statusBar.status.text), \(TranscriptTests.rows(of: screen.model))")
            return (screen, window, id)
        }

        /// `text` sent from the composer, and its turn ended.
        static func turn(_ screen: TranscriptViewController, _ text: String) async throws {
            let before = Set(screen.model.ids)
            screen.composer.text = text
            screen.sendTyped()
            try await LiveFollowTests.wait(until: { ComposerTests.lastTurnEnded(screen.model, after: before) }, within: .seconds(60),
                                           "the turn never ended: \(TranscriptTests.rows(of: screen.model))")
        }

        /// The picker, opened from the subtitle as a tap does, once it's up.
        static func picker(_ screen: TranscriptViewController) async throws -> UITableViewController {
            if let up = screen.presentedViewController {
                up.dismiss(animated: false)
                try await LiveFollowTests.wait(until: { screen.presentedViewController == nil }, within: .seconds(5), "the last sheet never went")
            }
            screen.modelButton.sendActions(for: .primaryActionTriggered)
            try await LiveFollowTests.wait(until: { screen.modelPicker?.viewIfLoaded?.window != nil }, within: .seconds(10), "the picker never rose")
            return try #require(screen.modelPicker)
        }

        /// The first model of `instance` the picker lists, once it does,
        /// and its row's identifier.
        static func choice(in picker: UITableViewController, instance: String) async throws -> (choice: ModelChoice, identifier: String) {
            let prefix = "switch-model-\(instance)/"
            var found: String?
            try await LiveFollowTests.wait(until: {
                found = Self.rows(picker).compactMap { picker.tableView(picker.tableView, cellForRowAt: $0).accessibilityIdentifier }.first { $0.hasPrefix(prefix) }
                return found != nil
            }, within: .seconds(30), "the picker never listed \(instance)")
            let identifier = try #require(found)
            return (ModelChoice(instance: instance, model: String(identifier.dropFirst(prefix.count))), identifier)
        }

        /// The row `identifier` tapped, as the table's delegate hears it.
        static func pick(_ identifier: String, in picker: UITableViewController) {
            let table = picker.tableView!
            guard let path = rows(picker).first(where: { picker.tableView(table, cellForRowAt: $0).accessibilityIdentifier == identifier }) else {
                Issue.record("no row \(identifier)")
                return
            }
            #expect(picker.tableView(table, willSelectRowAt: path) == path, "\(identifier) can be chosen")
            picker.tableView(table, didSelectRowAt: path)
        }

        static func rows(_ picker: UITableViewController) -> [IndexPath] {
            let table = picker.tableView!
            return (0..<picker.numberOfSections(in: table)).flatMap { s in
                (0..<picker.tableView(table, numberOfRowsInSection: s)).map { IndexPath(row: $0, section: s) }
            }
        }

        /// The header of `instance`'s section: its name, then its limit.
        static func header(_ picker: UITableViewController, _ instance: String) -> String? {
            let table = picker.tableView!
            return (0..<picker.numberOfSections(in: table)).compactMap { picker.tableView(table, titleForHeaderInSection: $0) }
                .first { $0 == instance || $0.hasPrefix("\(instance) · ") }
        }
    }
}
