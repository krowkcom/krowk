// R-HOST-3, R-HOST-4, R-IOS-7, R-SWITCH-4, R-LAG-8, R-HAND-4: starting a
// session from the phone against the fake fleet, on TestFleet's paired
// phone. The fleet's host A announces itself with one trusted repository
// (the fleet's `project`) and the mock model's instance ready; krowk-ffi's
// `listHosts` reads it (never this phone: a phone never hosts), and
// `startSession` asks it to start a session there. The start goes through
// Sessions' optimistic row, `starting on host-a…`, which gives way to the
// session, whose transcript opens and streams the first turn. Taken
// offline (POST KROWK_HOSTS_URL/offline), host A is listed greyed and
// can't be chosen. A start on an instance it lacks is refused by the host,
// and the row says the host's words.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its URLs to this
// process (KROWK_API_URL, KROWK_PAIR_URL, KROWK_HOSTS_URL). Hosted in the
// app, as the Keychain needs.
import Foundation
import Krowk
import KrowkAccount
import KrowkKit
import Testing
import UIKit

extension HostedKeychain {
    @MainActor
    @Suite("R-HOST-4, R-IOS-7, R-SWITCH-4: starting a session on the fake fleet's host")
    struct NewSessionTests {
        @Test("R-HOST-3, R-HOST-4, R-LAG-8, R-HAND-4: listHosts finds host-a online with its repository and a ready model; startSession starts there, the row reads starting on host-a… then gives way, and the transcript streams the turn")
        func testR_HOST_4_startOnTheFleetsHostAndFollowIt() async throws {
            let phone = try await TestFleet.shared.paired()
            let source = RegistrySessions(api: phone.api, storage: phone.storage)
            let host = try await Self.host(source, online: true)
            let listing = try await blockingCall { try source.hosts() }
            #expect(!listing.hosts.contains { $0.device == phone.account.deviceId() }, "R-HAND-4: this phone is never a host")
            let project = try #require(host.directories.first { $0.hasSuffix("/project") }, "no project in \(host.directories)")
            let instance = try #require(host.announcedInstances.first { $0.ready && !$0.models.isEmpty }, "no instance ready")

            let (starting, window, opened) = Self.sessions(source)
            defer { window.isHidden = true }
            let request = StartRequest(
                id: source.newCommandID(), device: host.device, hostName: host.name, cwd: project,
                instance: instance.name, model: instance.models[0], prompt: "Please stream a long answer")
            starting.start(request)
            // R-LAG-8: at once, before any answer.
            let first = try #require(starting.rows.first)
            #expect(first.state == .starting)
            #expect(first.request == request)

            try await LiveFollowTests.wait(until: { opened.screen != nil }, within: .seconds(60), "the start never opened its session: \(starting.rows)")
            #expect(starting.rows.isEmpty, "the row gave way to the session")
            let screen = try #require(opened.screen)
            let model = screen.model
            try await LiveFollowTests.wait(until: {
                model.ids.compactMap(model.item).contains { $0.kind == .assistant && $0.done && $0.text.contains("line 00120") }
            }, within: .seconds(180), "the started session's turn never streamed whole: \(TranscriptTests.rows(of: model))")
        }

        @Test("R-IOS-7: with host-a offline, it is listed greyed with when it was last seen, and can't be chosen")
        func testR_IOS_7_anOfflineHostCannotBeChosen() async throws {
            let phone = try await TestFleet.shared.paired()
            let source = RegistrySessions(api: phone.api, storage: phone.storage)
            _ = try await Self.host(source, online: true)
            #expect(try await Self.hostsControl("/offline") == "offline")
            do {
                let host = try await Self.host(source, online: false)
                // The host step, on screen, reading the hosts itself.
                let flow = NewSessionFlow.make(source: source) { _ in Issue.record("nothing starts") }
                let scene = try #require(UIApplication.shared.connectedScenes.first as? UIWindowScene, "no window scene in the host app")
                let window = UIWindow(windowScene: scene)
                defer { window.isHidden = true }
                window.rootViewController = flow
                window.makeKeyAndVisible()
                let hosts = try #require(flow.viewControllers.first as? UITableViewController)
                let table = hosts.tableView!
                let row = { () -> IndexPath? in
                    guard table.numberOfSections > 0 else { return nil }
                    return (0..<table.numberOfRows(inSection: 0)).map { IndexPath(row: $0, section: 0) }.first {
                        hosts.tableView(table, cellForRowAt: $0).accessibilityIdentifier == "host-\(host.name)"
                    }
                }
                try await LiveFollowTests.wait(until: { row() != nil }, within: .seconds(30), "host-a never shown")
                let path = try #require(row())
                let cell = hosts.tableView(table, cellForRowAt: path)
                let detail = (cell.contentConfiguration as? UIListContentConfiguration)?.secondaryText ?? ""
                #expect(detail.hasPrefix("last seen "), "\(detail)")
                #expect(hosts.tableView(table, willSelectRowAt: path) == nil, "an offline host can't be chosen")
                #expect(!hosts.tableView(table, shouldHighlightRowAt: path))
            } catch {
                _ = try? await Self.hostsControl("/online")
                throw error
            }
            #expect(try await Self.hostsControl("/online") == "online")
        }

        @Test("R-SWITCH-4, R-HOST-4: a start on an instance host-a lacks is refused by the host, nothing starts, and the row says its words with Edit and retry")
        func testR_SWITCH_4_aModelTheHostCannotRunShowsItsFixAndNothingStarts() async throws {
            let phone = try await TestFleet.shared.paired()
            let source = RegistrySessions(api: phone.api, storage: phone.storage)
            let host = try await Self.host(source, online: true)
            let project = try #require(host.directories.first { $0.hasSuffix("/project") })
            let (starting, window, opened) = Self.sessions(source)
            defer { window.isHidden = true }
            let request = StartRequest(
                id: source.newCommandID(), device: host.device, hostName: host.name, cwd: project,
                instance: "no-such-instance", model: "no-such-model", prompt: "Say hello")
            starting.start(request)
            try await LiveFollowTests.wait(until: { starting.rows.first?.state != .starting }, within: .seconds(60), "the host never answered")
            guard case .refused(let message)? = starting.rows.first?.state else {
                Issue.record("not refused: \(starting.rows)")
                return
            }
            #expect(!message.isEmpty)
            #expect(opened.screen == nil, "nothing started")
            // Edit and retry takes the row back to be sent as a new command.
            #expect(starting.takeForRetry("start:\(request.id)") == request)
        }

        /// Sessions with `+`, in a window of its own, over `source`: its
        /// starts, and the screen a started session opened, once it has.
        static func sessions(_ source: RegistrySessions) -> (StartingSessions, UIWindow, Opened) {
            let opened = Opened()
            let starting = StartingSessions(source: source)
            let newSessions = NewSessions(source: source, starting: starting) { session, _ in
                let row = SessionRow(id: session, title: "project", detail: "", live: true, accessibilityLabel: "project")
                let screen = TranscriptViewController(row: row, session: RelayConnection { try source.openStartedSession(id: session) })
                opened.screen = screen
                return screen
            }
            let list = SessionListViewController(model: SessionListModel(source: source), newSessions: newSessions)
            let scene = UIApplication.shared.connectedScenes.first as? UIWindowScene
            let window = scene.map { UIWindow(windowScene: $0) } ?? UIWindow()
            window.rootViewController = UINavigationController(rootViewController: list)
            window.makeKeyAndVisible()
            return (starting, window, opened)
        }

        @MainActor
        final class Opened {
            var screen: TranscriptViewController?
        }

        /// Host A as `listHosts` reads it, once it is listed `online` (or
        /// offline), within 60 s: its announcement refreshes every 2 s, and
        /// lapses 10 s after the last.
        static func host(_ source: RegistrySessions, online: Bool) async throws -> any AnnouncedHost {
            let deadline = Date.now.addingTimeInterval(60)
            var last = ""
            while Date.now < deadline {
                let listing = try await blockingCall { try source.hosts() }
                if let host = listing.hosts.first(where: { $0.name == "host-a" }), host.online == online {
                    return host
                }
                last = listing.hosts.map { "\($0.name) online=\($0.online)" }.joined(separator: ", ")
                try await Task.sleep(for: .milliseconds(500))
            }
            Issue.record("host-a is not listed online=\(online): \(last)")
            throw CancellationError()
        }

        /// POSTs to the fleet's hosts URL plus `path`: its reply's body.
        static func hostsControl(_ path: String) async throws -> String {
            let base = try #require(
                ProcessInfo.processInfo.environment["KROWK_HOSTS_URL"].flatMap { $0.isEmpty ? nil : $0 },
                "KROWK_HOSTS_URL is unset: run the tests through apps/ios/scripts/with-fleet.sh")
            var request = URLRequest(url: try #require(URL(string: base + path)))
            request.httpMethod = "POST"
            request.timeoutInterval = 60
            let (data, response) = try await URLSession.shared.data(for: request)
            let reply = String(decoding: data, as: UTF8.self)
            try #require((response as? HTTPURLResponse)?.statusCode == 200, "POST \(base + path): \(reply)")
            return reply
        }
    }
}
