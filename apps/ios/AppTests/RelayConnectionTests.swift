// R-CLIENT-1, R-LAG-9, R-HAND-4, R-IOS-1: the phone follows a session live
// through the fake fleet's relay, on TestFleet's paired phone. Swift owns
// the socket (Network.framework's WebSocket) and the timers; krowk-ffi's
// Viewer decides the join, the heartbeat and every rejoin. The fleet's
// demo session reads `live` within 5 s with updates in the mailbox; a
// connection dropped as a network would drop it is joined again at the
// viewer's cursor, back to `live`, and nothing drained shows twice; a
// session whose host let it go reads `host offline`, and nothing redials.
// The receive path holds dispatchPrecondition(.notOnQueue(.main)), so a
// callback on the main queue would stop this test process.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its URLs to this
// process (KROWK_API_URL, KROWK_RELAY_URL, KROWK_PAIR_URL,
// KROWK_SESSIONS_URL). Hosted in the app, as the Keychain needs.
import Foundation
import Krowk
import KrowkAccount
import KrowkKit
import Synchronization
import Testing

extension HostedKeychain {
    @Suite("R-CLIENT-1, R-LAG-9, R-HAND-4: the phone follows a session live through the fleet's relay")
    struct RelayConnectionTests {
        @Test("R-CLIENT-1, R-LAG-9, R-IOS-1: the demo session reads live within 5 s, and its updates reach the mailbox")
        func demoIsLive() async throws {
            let (live, statuses) = try await Self.follow(try await Self.demo())
            defer { live.stop() }
            try await Self.wait(for: .live, on: live, within: .seconds(5))
            let updates = live.mailbox.drain()
            #expect(!updates.isEmpty)
            #expect(updates.contains { if case .attached = $0 { true } else { false } }, "\(updates.map(Self.kind))")
            #expect(updates.contains(.host(present: true)))
            // The main queue has the statuses in order, after the fact.
            try await Self.wait(until: { statuses.all().last == .live }, within: .seconds(5), "\(statuses.all())")
            #expect(statuses.all().first == .connecting)
        }

        @Test("R-CLIENT-1, R-LAG-9: a dropped connection is joined again at the viewer's cursor, back to live, with no event drained twice")
        func reconnects() async throws {
            let (live, statuses) = try await Self.follow(try await Self.demo())
            defer { live.stop() }
            try await Self.wait(for: .live, on: live, within: .seconds(15))
            var drained = live.mailbox.drain()
            let dials = live.dials

            live.dropConnection()
            try await Self.wait(until: { statuses.all().contains(.reconnecting) }, within: .seconds(5), "never reconnecting: \(statuses.all())")
            try await Self.wait(for: .live, on: live, within: .seconds(15))
            #expect(live.dials > dials, "\(dials) → \(live.dials) dials")
            // The welcome's catch-up, if any, lands after `live`.
            try await Task.sleep(for: .seconds(1))
            drained += live.mailbox.drain()

            let ids = drained.flatMap(Self.eventIds)
            #expect(!ids.isEmpty, "\(drained.map(Self.kind))")
            #expect(Set(ids).count == ids.count, "an event drained twice: \(ids)")
            #expect(drained.filter { if case .attached = $0 { true } else { false } }.count == 1)
        }

        @Test("R-HAND-4: a session whose host goes reads host offline, and no reconnect loop runs; one opened with no host is the same")
        func hostOffline() async throws {
            let phone = try await TestFleet.shared.paired()
            let id = try await SessionListTests.control("", body: "Host goes away")
            let (live, _) = try await Self.follow(try await Self.open(id, phone: phone))
            defer { live.stop() }
            try await Self.wait(for: .live, on: live, within: .seconds(15))

            #expect(try await SessionListTests.control("/\(id)/release") == "released")
            try await Self.wait(for: .hostOffline, on: live, within: .seconds(15))
            let dials = live.dials
            // Past the core's one-second retry several times over: the
            // viewer waits for the relay's presence notice.
            try await Task.sleep(for: .seconds(4))
            #expect(live.status == .hostOffline)
            #expect(live.dials == dials, "redialed with no host: \(dials) → \(live.dials)")
            #expect(live.mailbox.drain().contains(.host(present: false)))

            let (later, _) = try await Self.follow(try await Self.open(id, phone: phone))
            defer { later.stop() }
            try await Self.wait(for: .hostOffline, on: later, within: .seconds(15))
            try await Task.sleep(for: .seconds(3))
            #expect(later.status == .hostOffline)
            #expect(later.dials == 1, "\(later.dials) dials")
        }

        @Test("R-CLIENT-1: a ticket goes over TLS (wss), or in a DEBUG build plain to this machine only (the fleet's ws://)")
        func onlyTLSOrLoopback() throws {
            let secure = { (url: String) in RelayConnection.secure(try #require(URL(string: url))) }
            #expect(try secure("wss://relay.krowk.com") == true)
            #expect(try secure("WSS://relay.krowk.com/v1/relay/s") == true)
            #expect(try secure("ws://127.0.0.1:53012/v1/relay/s?env=development") == false)
            #expect(try secure("ws://localhost:7790") == false)
            #expect(try secure("ws://[::1]:7790") == false)
            #expect(try secure("ws://relay.example.com") == nil)
            #expect(try secure("ws://127.0.0.1.example.com") == nil)
            #expect(try secure("https://relay.krowk.com") == nil)
        }

        // MARK: Helpers

        /// The fleet's demo session, opened on the paired phone.
        static func demo() async throws -> OpenedSession {
            let phone = try await TestFleet.shared.paired()
            let list = try await blockingCall { try phone.account.listSessions() }
            let demo = try #require(list.sessions.first { $0.title == "A demo session" }, "no demo session in \(list.sessions.map(\.title))")
            return try await open(demo.id, phone: phone, known: demo.head)
        }

        /// Session `id`, opened against the fleet's relay, off the main thread.
        static func open(_ id: String, phone: PairedAccount, known: HeadRecord? = nil) async throws -> OpenedSession {
            let relay = try #require(RelayConnection.relayOverride(), "KROWK_RELAY_URL is unset: run the tests through apps/ios/scripts/with-fleet.sh")
            return try await blockingCall { try phone.account.openSession(id: id, known: known, relayOverride: relay) }
        }

        /// A connection following `session`, started, and every status it
        /// published, in order.
        static func follow(_ session: OpenedSession) async throws -> (RelayConnection, Statuses) {
            let live = RelayConnection { session }
            let statuses = Statuses()
            live.follow { statuses.append($0) }
            return (live, statuses)
        }

        static func wait(for status: RelayStatus, on live: RelayConnection, within limit: Duration) async throws {
            try await wait(until: { live.status == status }, within: limit, "not \(status.text) within \(limit): \(live.status.text)")
        }

        static func wait(until done: () -> Bool, within limit: Duration, _ why: @autoclosure () -> String) async throws {
            let clock = ContinuousClock()
            let deadline = clock.now + limit
            while !done() && clock.now < deadline {
                try await Task.sleep(for: .milliseconds(20))
            }
            try #require(done(), Comment(rawValue: why()))
        }

        /// The logged events an update carries, by id.
        static func eventIds(_ update: ViewerUpdate) -> [String] {
            switch update {
            case .attached(let events, _), .caughtUp(let events): events.map(\.id)
            case .line(.log(let event)): [event.id]
            default: []
            }
        }

        static func kind(_ update: ViewerUpdate) -> String {
            String(describing: update).prefix { $0 != "(" }.description
        }
    }
}

/// Every status a connection published, in order.
final class Statuses: Sendable {
    private let seen = Mutex<[RelayStatus]>([])
    func append(_ status: RelayStatus) { seen.withLock { $0.append(status) } }
    func all() -> [RelayStatus] { seen.withLock { $0 } }
}
