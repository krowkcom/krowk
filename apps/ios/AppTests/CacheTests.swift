// R-IOS-1, R-IOS-3, R-PROTO-2: the cache written as sessions sync, on
// TestFleet's paired phone: a listing through RegistrySessions writes the
// index, and the demo session followed through RelayConnection writes its
// logged events, each as krowk-ffi's encodeLogEvent wrote it, reading
// back through decodeLogEvent; after a turn, the cache's cursor names the
// newest event it holds. The cache is a file of the test's own.
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

        @Test("R-IOS-1: a place the cache refused while the phone was locked is held with its events, and written with them after unlock")
        func testR_IOS_1_aLockedPlaceIsWrittenWithItsEvents() async throws {
            let url = URL.temporaryDirectory.appending(path: "krowk-cache-hosted/\(UUID().uuidString)/krowk-cache.sqlite")
            let cache = try await SessionCache.open(at: url)
            defer { Task { try? await cache.forgetAll() } }
            let (events, session) = try await MainActor.run { (try TranscriptModelTests.events(), TranscriptModelTests.session) }
            let writer = TranscriptCacheWriter(session: session, cache: cache, interval: .milliseconds(10))
            let place = CursorRecord(stream: String(repeating: "ab", count: 16), afterSeq: 3, newest: events[2].id)

            await cache.suspend()
            writer.add(Array(events.prefix(3)).map { ViewerUpdate.line(line: .log($0)) } + [.saved(cursor: place)])
            await writer.written()
            await cache.resume()
            #expect(try await cache.cursor(session: session) == nil, "written while locked")
            writer.add(events: [])
            await writer.written()
            #expect(try await cache.events(session: session, limit: 100).count == 3)
            let kept = try #require(try await cache.cursor(session: session))
            #expect(try decodeCursor(json: kept) == place)
        }

        @Test("R-IOS-1: once a frame says some events were left out, no place is written again: a later place would skip them")
        func testR_IOS_1_noPlaceAfterEventsLeftOut() async throws {
            let url = URL.temporaryDirectory.appending(path: "krowk-cache-hosted/\(UUID().uuidString)/krowk-cache.sqlite")
            let cache = try await SessionCache.open(at: url)
            defer { Task { try? await cache.forgetAll() } }
            let (events, session) = try await MainActor.run { (try TranscriptModelTests.events(), TranscriptModelTests.session) }
            let writer = TranscriptCacheWriter(session: session, cache: cache, interval: .milliseconds(10))
            let place = { (n: Int) in CursorRecord(stream: String(repeating: "ab", count: 16), afterSeq: UInt64(n), newest: events[n - 1].id) }

            writer.add([.line(line: .log(events[0])), .saved(cursor: place(1))])
            await writer.written()
            #expect(try await cache.cursor(session: session).map { try decodeCursor(json: $0) } == place(1))
            // The core's caught-up events, one of which this build couldn't
            // decode: krowk-ffi crosses the rest and says `failed`.
            writer.add([.caughtUp(events: [events[1]]), .failed(detail: "1 event(s) this build cannot show were left out"), .saved(cursor: place(3))])
            writer.add([.line(line: .log(events[3])), .saved(cursor: place(4))])
            await writer.written()
            #expect(try await cache.events(session: session, limit: 100).count == 3, "the events still go in")
            #expect(try await cache.cursor(session: session).map { try decodeCursor(json: $0) } == place(1), "no place past what was left out")
        }

        @Test("R-IOS-1: a session evicted while its screen is open keeps no place: what the cache holds of it is a tail")
        func testR_IOS_1_noPlaceAfterEviction() async throws {
            let url = URL.temporaryDirectory.appending(path: "krowk-cache-hosted/\(UUID().uuidString)/krowk-cache.sqlite")
            let cache = try await SessionCache.open(at: url)
            defer { Task { try? await cache.forgetAll() } }
            let (events, session) = try await MainActor.run { (try TranscriptModelTests.events(), TranscriptModelTests.session) }
            let writer = TranscriptCacheWriter(session: session, cache: cache, interval: .milliseconds(10))
            let place = { (n: Int) in CursorRecord(stream: String(repeating: "ab", count: 16), afterSeq: UInt64(n), newest: events[n - 1].id) }

            writer.add(Array(events.prefix(2)).map { ViewerUpdate.line(line: .log($0)) } + [.saved(cursor: place(2))])
            await writer.written()
            #expect(try await cache.cursor(session: session) != nil)
            try await cache.evict(keep: 0)
            writer.add([.line(line: .log(events[2])), .saved(cursor: place(3))])
            await writer.written()
            writer.add([.saved(cursor: place(3))])
            await writer.written()
            #expect(try await cache.cursor(session: session) == nil, "a place over a tail")
        }

        @Test("R-IOS-1: a session of the fleet's followed through a turn: the cache's cursor names the newest event the cache holds, and never one it doesn't")
        func testR_IOS_1_theCursorNamesTheNewestEventHeld() async throws {
            let phone = try await TestFleet.shared.paired()
            let url = URL.temporaryDirectory.appending(path: "krowk-cache-hosted/\(UUID().uuidString)/krowk-cache.sqlite")
            let cache = SessionCache(at: url)
            defer { Task { try? await cache.forgetAll() } }
            // A session of the test's own: the fleet runs turns only in those.
            let id = try await SessionListTests.control("", body: "A cached session")
            let opened = try await RelayConnectionTests.open(id, phone: phone)
            let live = RelayConnection(
                opening: { opened },
                cache: TranscriptCacheWriter(session: id, cache: cache, interval: .milliseconds(50)))
            live.follow { _ in }
            defer { live.stop() }

            /// The events held, the place kept, and the turns ended among
            /// them; the place, whenever there is one, names an event held.
            func read() async throws -> (held: [String], cursor: CursorRecord?, turns: Int) {
                // The place first: written with its events, so the events
                // read after it hold every one it names.
                let cursor = try await cache.cursor(session: id).map { try decodeCursor(json: $0) }
                let held = try await cache.events(session: id, limit: 100_000)
                if let newest = cursor?.newest {
                    #expect(held.contains { $0.id == newest }, "the place names \(newest), which the cache doesn't hold")
                }
                let turns = held.filter { event in
                    guard case .turnCompleted? = (try? decodeLogEvent(json: event.json))?.body else { return false }
                    return true
                }.count
                return (held.map(\.id), cursor, turns)
            }

            try await Self.wait(within: .seconds(30), "no place was saved") { try await read().cursor != nil }
            let before = try await read().turns
            #expect(try await SessionListTests.control("/\(id)/turn", body: "Please stream a long answer") == "started")
            try await Self.wait(within: .seconds(180), "the turn never ended at the host") {
                try await SessionListTests.control("/\(id)/turn-ended") != "running"
            }
            try await Self.wait(within: .seconds(30), "the turn's end was not cached with a place naming the newest event held") {
                let (held, cursor, turns) = try await read()
                return turns > before && cursor?.newest != nil && cursor?.newest == held.max()
            }
            let cursor = try #require(try await read().cursor)
            #expect(cursor.afterSeq > 0, "no batch of the turn counted")
            #expect(cursor.stream.count == 32, "the stream, in hex: \(cursor.stream)")
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
