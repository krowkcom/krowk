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
import UIKit
@testable import KrowkApp

extension HostedKeychain {
    @Suite("R-CLIENT-1, R-LAG-9, R-HAND-4: the phone follows a session live through the fleet's relay")
    struct RelayConnectionTests {
        @Test("R-CLIENT-1, R-LAG-9, R-IOS-1: the demo session reads live within 5 s, its updates reach the mailbox, and the link stays up past the heartbeats")
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

            // R-LAG-9: held past the relay's pings, the viewer's own
            // heartbeats and the 30 s a silent link has before it's dead,
            // the link stays up: no drop, no second dial.
            let dials = live.dials
            try await Task.sleep(for: .seconds(35))
            #expect(live.status == .live)
            #expect(live.dials == dials, "redialed while live: \(dials) → \(live.dials)")
            #expect(!statuses.all().contains(where: Self.waiting), "\(statuses.all())")
        }

        @Test("R-CLIENT-1, R-LAG-9: a dropped connection is joined again at the viewer's cursor, back to live, with no event drained twice")
        func reconnects() async throws {
            let (live, statuses) = try await Self.follow(try await Self.demo())
            defer { live.stop() }
            try await Self.wait(for: .live, on: live, within: .seconds(15))
            var drained = live.mailbox.drain()
            let dials = live.dials

            live.dropConnection()
            try await Self.wait(until: { statuses.all().contains(where: Self.waiting) }, within: .seconds(5), "never reconnecting: \(statuses.all())")
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
            #if DEBUG
            #expect(try secure("ws://127.0.0.1:53012/v1/relay/s?env=development") == false)
            #expect(try secure("ws://localhost:7790") == false)
            #expect(try secure("ws://[::1]:7790") == false)
            #else
            #expect(try secure("ws://127.0.0.1:53012/v1/relay/s?env=development") == nil)
            #endif
            #expect(try secure("ws://relay.example.com") == nil)
            #expect(try secure("ws://127.0.0.1.example.com") == nil)
            #expect(try secure("ws://127.0.0.1@relay.example.com") == nil)
            #expect(try secure("https://relay.krowk.com") == nil)
        }

        // MARK: Ticket 86: backgrounding and network changes

        @Test("R-IOS-2: the scene delegate's willEnterForeground starts the connect of a session followed live, before any view appears")
        @MainActor
        func connectEarly() async throws {
            let (live, _) = try await Self.follow(try await Self.demo())
            defer { live.stop() }
            try await Self.wait(for: .live, on: live, within: .seconds(15))
            LiveSessions.shared.add(live)
            defer { LiveSessions.shared.remove(live) }

            // Lost in the background: held, nothing dialed.
            let scene = try #require(UIApplication.shared.connectedScenes.first)
            SceneDelegate().sceneDidEnterBackground(scene)
            let dials = live.dials
            live.dropConnection()
            try await Task.sleep(for: .seconds(3))
            #expect(live.dials == dials, "redialed in the background: \(dials) → \(live.dials)")
            #expect(Self.waiting(live.status), "\(live.status.text)")

            // No view, no screen: the scene's foreground alone dials.
            SceneDelegate().sceneWillEnterForeground(scene)
            try await Self.wait(until: { live.dials > dials }, within: .seconds(5), "no connect after willEnterForeground: \(live.dials) dials")
            try await Self.wait(for: .live, on: live, within: .seconds(15))
        }

        @Test("R-OFF-2, R-IOS-2: a connection force-closed in a 15 s background comes back on the foreground: a fresh connection, live, no line drained twice")
        func backAfterABackground() async throws {
            let (live, _) = try await Self.follow(try await Self.demo())
            defer { live.stop() }
            try await Self.wait(for: .live, on: live, within: .seconds(15))
            try await Task.sleep(for: .seconds(1))
            var drained = live.mailbox.drain()
            let dials = live.dials

            live.background()
            live.dropConnection()
            try await Task.sleep(for: .seconds(2))
            #expect(live.dials == dials, "tried again in the background")
            live.advanceClock(by: .seconds(15))
            live.foreground()
            try await Self.wait(until: { live.dials == dials + 1 }, within: .seconds(5), "no fresh connection: \(live.dials) dials")
            try await Self.wait(for: .live, on: live, within: .seconds(15))
            try await Task.sleep(for: .seconds(1))
            drained += live.mailbox.drain()

            let ids = drained.flatMap(Self.eventIds)
            #expect(!ids.isEmpty)
            #expect(Set(ids).count == ids.count, "an event drained twice: \(ids)")
            #expect(drained.filter { if case .attached = $0 { true } else { false } }.count == 1)
            // Kept, not dropped, through a 15 s background: replaced at once
            // on the foreground, a fresh dial.
            live.background()
            live.advanceClock(by: .seconds(15))
            live.foreground()
            try await Self.wait(until: { live.dials == dials + 2 }, within: .seconds(5), "not replaced after 15 s away: \(live.dials) dials")
            try await Self.wait(for: .live, on: live, within: .seconds(15))
            drained += live.mailbox.drain()
            let all = drained.flatMap(Self.eventIds)
            #expect(Set(all).count == all.count, "an event drained twice: \(all)")
        }

        @Test("R-OFF-2: sync resumes as connectivity returns: a link that keeps dropping backs off, and a usable network path joins it at once, not at its backoff")
        func pathWakes() async throws {
            let (live, statuses) = try await Self.follow(try await Self.demo())
            defer { live.stop() }
            try await Self.wait(for: .live, on: live, within: .seconds(15))
            // Five drops in a row: 250 ms, 500 ms, 1 s, 2 s, 4 s of backoff,
            // each cut by up to half; the sixth waits 4 to 8 s.
            for _ in 0..<5 {
                live.dropConnection()
                try await Self.wait(until: { statuses.all().last.map(Self.waiting) == true }, within: .seconds(5), "\(statuses.all())")
                try await Self.wait(for: .live, on: live, within: .seconds(20))
            }
            let dials = live.dials
            live.dropConnection()
            try await Self.wait(until: {
                if case .retrying(let at, _) = live.status { ContinuousClock.now.duration(to: at) > .seconds(3) } else { false }
            }, within: .seconds(5), "no long wait: \(live.status.text)")
            #expect(live.status.text.hasPrefix("reconnecting in "), "\(live.status.text)")

            live.pathChanged(satisfied: true)
            try await Self.wait(until: { live.dials > dials }, within: .seconds(2), "the path change did not join at once")
            try await Self.wait(for: .live, on: live, within: .seconds(15))
        }

        @Test("R-LAG-9: a probe sent while large batches are being fed still goes out and is answered on time: a busy host never causes a reconnect")
        func probeWhileFeeding() async throws {
            let phone = try await TestFleet.shared.paired()
            let id = try await SessionListTests.control("", body: "Probed while it streams")
            let (live, _) = try await Self.follow(try await Self.open(id, phone: phone))
            defer { live.stop() }
            try await Self.wait(for: .live, on: live, within: .seconds(15))
            #expect(try await SessionListTests.control("/\(id)/turn", body: "Please stream a very long answer") == "started")
            // Streaming: batches arriving and fed to the core on the queue.
            try await Self.wait(until: { live.mailbox.waiting > 0 }, within: .seconds(30), "nothing streamed")
            let dials = live.dials
            // A wake a second for ten seconds: each a probe while the batches
            // feed, each answered within its 5 s or the link is replaced.
            for _ in 0..<10 {
                live.wake()
                _ = live.mailbox.drain()
                try await Task.sleep(for: .seconds(1))
            }
            try await Task.sleep(for: .seconds(6))
            _ = live.mailbox.drain()
            #expect(live.dials == dials, "a probe went unanswered while feeding: \(dials) → \(live.dials) dials")
            #expect(live.status == .live, "\(live.status.text)")
            #expect(try await SessionListTests.control("/\(id)/release") == "released")
        }

        @Test("R-OFF-2: a session whose opening failed on the network is opened again on the backoff, the bar saying why and when, until it is live")
        func openRetried() async throws {
            let session = try await Self.demo()
            let attempts = Attempts()
            let live = RelayConnection {
                let n = attempts.count.withLock { $0 += 1; return $0 }
                if n < 3 { throw KrowkError.Network(detail: "krowk's registry cannot be reached") }
                return session
            }
            let statuses = Statuses()
            live.follow { statuses.append($0) }
            defer { live.stop() }
            try await Self.wait(for: .live, on: live, within: .seconds(15))
            #expect(attempts.count.withLock { $0 } == 3)
            let waits = statuses.all().compactMap { if case .retrying(_, let why) = $0 { why } else { nil } }
            #expect(waits.contains("couldn't open the session: krowk's registry cannot be reached"), "\(statuses.all())")

            // Refused (not the network): stopped, as before.
            let refused = RelayConnection { throw KrowkError.Refused(code: "not_found", detail: "no such session") }
            refused.follow { _ in }
            defer { refused.stop() }
            try await Self.wait(until: { if case .ended = refused.status { true } else { false } }, within: .seconds(5), "\(refused.status.text)")
        }

        // MARK: Helpers

        /// Waiting to join again after a drop: reconnecting, at once or on
        /// the backoff.
        static func waiting(_ status: RelayStatus) -> Bool {
            switch status {
            case .reconnecting, .retrying: true
            default: false
            }
        }

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

/// How many times a session was opened.
final class Attempts: Sendable {
    let count = Mutex(0)
}

/// Every status a connection published, in order.
final class Statuses: Sendable {
    private let seen = Mutex<[RelayStatus]>([])
    func append(_ status: RelayStatus) { seen.withLock { $0.append(status) } }
    func all() -> [RelayStatus] { seen.withLock { $0 } }
}
