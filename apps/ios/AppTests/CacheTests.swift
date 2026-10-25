// R-IOS-1, R-IOS-3, R-PROTO-2: the cache written as sessions sync, on
// TestFleet's paired phone: a listing through RegistrySessions writes the
// index, and the demo session followed through RelayConnection writes its
// logged events, each as krowk-ffi's encodeLogEvent wrote it, reading
// back through decodeLogEvent. The cache is a file of the test's own.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its URLs to this
// process. Hosted in the app, as the Keychain needs.
import Foundation
import Krowk
import KrowkAccount
import KrowkCache
import KrowkKit
import Testing

extension HostedKeychain {
    @Suite("R-IOS-1, R-IOS-3: the cache is written as the fleet's sessions sync")
    struct CacheTests {
        @Test("R-IOS-1, R-IOS-3, R-PROTO-2: listed, then the demo session opened: the cache holds its index row and its logged events, by id")
        func testR_IOS_1_listingAndOpeningFillTheCache() async throws {
            let phone = try await TestFleet.shared.paired()
            let url = URL.temporaryDirectory.appending(path: "krowk-cache-hosted/\(UUID().uuidString)/krowk-cache.sqlite")
            let cache = SessionCache(at: url)
            let source = RegistrySessions(api: phone.api, storage: phone.storage, cache: cache)

            let listing = try await blockingCall { try source.list() }
            let demo = try #require(
                listing.sessions.compactMap { $0 as? SessionSummary }.first { $0.title == "A demo session" },
                "no demo session in \(listing.sessions.map(\.title))")
            try await Self.wait(within: .seconds(10), "the demo session's index row was not written") {
                try await cache.index().contains { $0.id == demo.id }
            }
            let row = try #require(try await cache.index().first { $0.id == demo.id })
            #expect(row == CachedSession(demo, listedAtMs: row.listedAtMs))
            #expect(row.title == "A demo session")
            #expect(try await cache.index().count == listing.sessions.count)

            // What the session holds, by id, read on its own.
            let expected = try await blockingCall {
                try source.openSession(id: demo.id, known: demo.head).events()
            }.map(\.id)
            try #require(!expected.isEmpty, "the demo session logged nothing")

            let live = RelayConnection(
                opening: { try source.openSession(id: demo.id, known: demo.head) },
                cache: TranscriptCacheWriter(session: demo.id, cache: cache))
            live.follow { _ in }
            defer { live.stop() }
            try await Self.wait(within: .seconds(30), "the demo session's events were not all written") {
                Set(try await cache.events(session: demo.id, limit: 10_000).map(\.id)).isSuperset(of: expected)
            }
            let kept = try await cache.events(session: demo.id, limit: 10_000)
            #expect(Set(kept.map(\.id)).count == kept.count, "an event kept twice")
            // Oldest first, as the session logged them.
            #expect(kept.reversed().map(\.id).filter(Set(expected).contains) == expected)
            for event in kept {
                #expect(try decodeLogEvent(json: event.json).id == event.id)
            }

            try await cache.forgetAll()
            #expect(!FileManager.default.fileExists(atPath: url.path(percentEncoded: false)))
        }

        @Test("R-IOS-1: events the cache refused while the phone was locked are held, and written in order with what comes after unlock")
        func testR_IOS_1_aLockedBatchIsWrittenAfterUnlock() async throws {
            let url = URL.temporaryDirectory.appending(path: "krowk-cache-hosted/\(UUID().uuidString)/krowk-cache.sqlite")
            let cache = try await SessionCache.open(at: url)
            defer { Task { try? await cache.forgetAll() } }
            let (events, session) = try await MainActor.run { (try TranscriptModelTests.events(), TranscriptModelTests.session) }
            let writer = TranscriptCacheWriter(session: session, cache: cache, interval: .milliseconds(10))

            await cache.suspend()
            writer.add(events: Array(events.prefix(3)))
            await writer.written()
            await cache.resume()
            #expect(try await cache.events(session: session, limit: 100).isEmpty, "written while locked")
            // Unlocked: a frame with no logged event retries what's held.
            writer.add(events: [])
            await writer.written()
            #expect(try await cache.events(session: session, limit: 100).count == 3)
            writer.add(events: Array(events.dropFirst(3)))
            await writer.written()
            let kept = try await cache.events(session: session, limit: 100)
            #expect(kept.reversed().map(\.id) == events.map(\.id))
        }

        /// Polls `done` every 50 ms until it holds, or fails at `limit`.
        static func wait(within limit: Duration, _ why: String, until done: () async throws -> Bool) async throws {
            let deadline = ContinuousClock.now + limit
            while !(try await done()) {
                try #require(ContinuousClock.now < deadline, Comment(rawValue: why))
                try await Task.sleep(for: .milliseconds(50))
            }
        }
    }
}
