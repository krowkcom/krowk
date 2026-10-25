// R-NET-2, R-IOS-2, R-CLIENT-1: the phone races the welcome's direct
// candidates over Network.framework and moves onto the first that joins
// with the host there, as the terminal does (ticket 133). The fake fleet
// runs with --direct-lan, so each session's host offers its LAN listener
// on 127.0.0.1, named in the welcome as a `lan` candidate. Rust decides
// the race (one ticket, 3 s a candidate, the first to join); Swift dials.
// A POST to <sessions_url>/<id>/direct/close closes the listener, as a
// LAN path lost would, and the session is back on the relay at its
// cursor. The tailnet path (`direct over Tailscale`) needs a phone with
// Tailscale on: ticket 54's check.
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
    @Suite("R-NET-2: the phone races the welcome's direct candidates and moves onto the first")
    struct DirectPathTests {
        /// What the fleet's model answers a prompt it has no script for.
        static let hello = "Hello! I can read this project, change files in it and run commands, asking you first."

        @Test("R-NET-2, R-CLIENT-1: the session moves to the fleet's direct LAN path within 5 s of going live and a turn's lines arrive over it; with the listener closed the bar reads relay again, and no event is lost or doubled")
        func testR_NET_2_theSessionMovesToTheDirectLANPathAndBackToTheRelay() async throws {
            let phone = try await TestFleet.shared.paired()
            let title = "Direct over LAN"
            let id = try await SessionListTests.control("", body: title)
            let opened = try await RelayConnectionTests.open(id, phone: phone)
            let live = RelayConnection { opened }
            let row = SessionRow(id: id, title: title, detail: "", live: true, accessibilityLabel: title)
            let screen = TranscriptViewController(row: row, session: live)
            let scene = try #require(UIApplication.shared.connectedScenes.first as? UIWindowScene, "no window scene in the host app")
            let window = UIWindow(windowScene: scene)
            window.rootViewController = UINavigationController(rootViewController: screen)
            window.makeKeyAndVisible()
            defer {
                window.isHidden = true
                live.stop()
            }
            let bar = screen.statusBar
            let model = screen.model
            // The session's first turn ("Say hello") answered at rest.
            let answers = { model.ids.compactMap(model.item).filter { $0.kind == .assistant && $0.text == Self.hello }.count }
            try await LiveFollowTests.wait(until: { bar.status == .live && answers() == 1 }, within: .seconds(20), "not live with the first turn drawn: \(bar.status.text), \(TranscriptTests.rows(of: model))")

            try await LiveFollowTests.wait(until: { bar.status == .live && bar.accessibilityValue == "direct over LAN" }, within: .seconds(5), "not on the direct path: \(bar.status.text) \(bar.accessibilityValue ?? "no path")")
            #expect(live.shownPath == "direct over LAN")
            #expect(live.dials == 1, "the relay dialed once: \(live.dials)")

            // A turn's lines come over the direct path.
            #expect(try await SessionListTests.control("/\(id)/turn", body: "Say hello over the LAN") == "started")
            try await LiveFollowTests.wait(until: { answers() == 2 }, afterTurnIn: id, within: .seconds(10), "the turn over the LAN never drew: \(TranscriptTests.rows(of: model))")
            #expect(bar.accessibilityValue == "direct over LAN")

            // The listener gone: the relay joined again at once, at the
            // viewer's cursor, and the bar reads the relay.
            #expect(try await SessionListTests.control("/\(id)/direct/close") == "closed")
            try await LiveFollowTests.wait(until: { bar.status == .live && bar.accessibilityValue == "relay" }, within: .seconds(15), "not back on the relay: \(bar.status.text) \(bar.accessibilityValue ?? "no path")")
            #expect(live.dials == 2, "one join of the relay again: \(live.dials)")
            #expect(try await SessionListTests.control("/\(id)/turn", body: "Say hello on the relay") == "started")
            try await LiveFollowTests.wait(until: { answers() == 3 }, afterTurnIn: id, within: .seconds(10), "the turn on the relay never drew: \(TranscriptTests.rows(of: model))")

            // Nothing lost or doubled across both moves: each turn's answer
            // drawn once.
            #expect(answers() == 3, "\(TranscriptTests.rows(of: model))")
            #expect(Set(model.ids).count == model.ids.count, "a row twice: \(model.ids)")
            #expect(bar.accessibilityValue == "relay")
        }
    }
}
