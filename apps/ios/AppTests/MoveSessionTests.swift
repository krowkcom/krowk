// R-HAND-4, R-CRED-1: moving a session to another host from the phone
// (ticket 136) against the fake fleet, on TestFleet's paired phone. The
// fleet has one host, A, so the move itself is the CLI's test
// (crates/krowk/tests/sync_cli.rs, r_hand_4_a_third_device_moves_a_session_
// from_a_to_b); here, Move to… in the navigation bar's menu lists the
// online hosts but the session's holder, each with its directories, and a
// move host A refuses reads `moving to host-a…` at once, then shows A's
// sentence under the bar while the session stays live where it was.
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
    @Suite("R-HAND-4: moving a session to another host from the phone")
    struct MoveSessionTests {
        @Test("R-HAND-4: Move to… lists the online hosts but the one holding the session, each with its directories")
        func testR_HAND_4_moveToListsOnlineHostsButTheHolder() async throws {
            let phone = try await TestFleet.shared.paired()
            let source = RegistrySessions(api: phone.api, storage: phone.storage)
            let host = try await NewSessionTests.host(source, online: true)
            let project = try #require(host.directories.first { $0.hasSuffix("/project") }, "no project in \(host.directories)")

            // Held by host A: no other host to move to.
            let (held, window, _) = try await Self.open("Move: held by A", holder: host.device)
            let none = try await Self.picker(held)
            try await LiveFollowTests.wait(until: { Self.footers(none).contains { $0.hasPrefix("No other host is online") } }, within: .seconds(30),
                                           "the picker never said no host: \(Self.footers(none))")
            #expect(ModelSwitchTests.rows(none).isEmpty)
            none.dismiss(animated: false)
            window.isHidden = true

            // Held by another device: host A, with its directories.
            let (elsewhere, other, _) = try await Self.open("Move: held elsewhere", holder: Self.elsewhere)
            defer { other.isHidden = true }
            let listed = try await Self.picker(elsewhere)
            let row = "move-\(host.name)-\(project)"
            try await LiveFollowTests.wait(until: { Self.identifiers(listed).contains(row) }, within: .seconds(30),
                                           "host A's project never listed: \(Self.identifiers(listed))")
            let headers = (0..<listed.numberOfSections(in: listed.tableView)).compactMap { listed.tableView(listed.tableView, titleForHeaderInSection: $0) }
            #expect(headers == [host.name])
            ComposerTests.keep("move-picker", other)
        }

        @Test("R-HAND-4, R-CRED-1, R-LAG-8: a move host A refuses reads moving to host-a… at once, then shows A's sentence in place, and the session stays")
        func testR_HAND_4_aRefusedMoveShowsTheHostsSentenceAndTheSessionStays() async throws {
            let phone = try await TestFleet.shared.paired()
            let source = RegistrySessions(api: phone.api, storage: phone.storage)
            let host = try await NewSessionTests.host(source, online: true)
            let project = try #require(host.directories.first { $0.hasSuffix("/project") })
            let (screen, window, _) = try await Self.open("Refused move", holder: Self.elsewhere)
            defer { window.isHidden = true }
            let mover = try #require(screen.mover)
            let picker = try await Self.picker(screen)
            let row = "move-\(host.name)-\(project)"
            try await LiveFollowTests.wait(until: { Self.identifiers(picker).contains(row) }, within: .seconds(30), "host A never listed")
            ModelSwitchTests.pick(row, in: picker)
            // R-LAG-8: no await since the pick.
            #expect(screen.statusBar.move == "moving to \(host.name)…")
            #expect(mover.isMoving)
            ComposerTests.keep("move-moving", window)

            // The fleet's host runs no take: it refuses, in its own words.
            try await LiveFollowTests.wait(until: { mover.refusal != nil }, within: .seconds(60), "host A never answered: \(String(describing: mover.state))")
            let said = try #require(mover.refusal)
            #expect(said.contains(host.name), "\(said)")
            #expect(!screen.moveNote.isHidden)
            #expect(screen.moveNote.text?.contains(said) == true)
            #expect(screen.statusBar.move == nil)
            #expect(mover.holder == Self.elsewhere, "the session stays where it was")
            #expect(screen.isLive)
            ComposerTests.keep("move-refused", window)
        }

        /// A device that holds the session in these tests' rows, not host A.
        static let elsewhere = "00000000000000000000000000000000"

        /// A session of the test's own on host A, open live on its
        /// transcript in a window, its row naming `holder` as the device
        /// holding it.
        static func open(_ title: String, holder: String) async throws -> (TranscriptViewController, UIWindow, String) {
            let phone = try await TestFleet.shared.paired()
            let source = RegistrySessions(api: phone.api, storage: phone.storage)
            let id = try await SessionListTests.control("", body: title)
            let opened = try await RelayConnectionTests.open(id, phone: phone)
            let row = SessionRow(id: id, title: title, detail: "", live: true, accessibilityLabel: title, hostDevice: holder)
            let screen = TranscriptViewController(row: row, session: RelayConnection { opened }, hosts: source)
            let scene = try #require(UIApplication.shared.connectedScenes.first as? UIWindowScene, "no window scene in the host app")
            let window = UIWindow(windowScene: scene)
            window.rootViewController = UINavigationController(rootViewController: screen)
            window.makeKeyAndVisible()
            try await LiveFollowTests.wait(until: { screen.isLive }, within: .seconds(20), "never live: \(screen.statusBar.status.text)")
            let menu = try #require(screen.navigationItem.rightBarButtonItems?.first { $0.accessibilityIdentifier == "session-menu" }, "no menu in the navigation bar")
            let titles = menu.menu?.children.map { $0.title }
            #expect(titles == ["Move to…"])
            return (screen, window, id)
        }

        /// Move to…, opened as the menu's action does, once it's up.
        static func picker(_ screen: TranscriptViewController) async throws -> UITableViewController {
            screen.showMovePicker()
            try await LiveFollowTests.wait(until: { screen.movePicker?.viewIfLoaded?.window != nil }, within: .seconds(10), "Move to… never rose")
            return try #require(screen.movePicker)
        }

        static func identifiers(_ picker: UITableViewController) -> [String] {
            ModelSwitchTests.rows(picker).compactMap { picker.tableView(picker.tableView, cellForRowAt: $0).accessibilityIdentifier }
        }

        static func footers(_ picker: UITableViewController) -> [String] {
            (0..<picker.numberOfSections(in: picker.tableView)).compactMap { picker.tableView(picker.tableView, titleForFooterInSection: $0) }
        }
    }
}
