// R-IOS-1, R-IOS-3, R-E2E-1: the phone's on-device cache (ticket 129),
// each test on a file of its own under the temporary directory: the
// index replaced whole, events kept once and paged newest first, the
// transcripts of the 20 newest sessions kept, a schema this build
// doesn't know started afresh, the file's protection and backup
// exclusion, and forgetting it. The hosted CacheTests write it from the
// fake fleet.
import Foundation
@testable import KrowkCache
import KrowkKit
import SQLite3
import Testing

@Suite("R-IOS-1, R-IOS-3: the session cache")
struct SessionCacheTests {
    let url = URL.temporaryDirectory.appending(path: "krowk-cache-tests/\(UUID().uuidString)/krowk-cache.sqlite")

    static func session(_ id: String, updatedMs: UInt64 = 1, title: String = "") -> CachedSession {
        CachedSession(id: id, title: title, cwd: "/home/me/\(id)", updatedMs: updatedMs)
    }

    static func events(_ ids: some Sequence<Int>, in session: String = "s") -> [(id: String, json: String)] {
        ids.map { (id: "\(session)-e\($0)", json: #"{"id":"\#(session)-e\#($0)"}"#) }
    }

    @Test("R-IOS-3: the index round-trips every field; a session gone from the next listing is gone from the cache with its events")
    func testR_IOS_3_indexRoundTrip() async throws {
        let cache = try await SessionCache.open(at: url)
        let full = CachedSession(
            id: "a", title: "Fix the build", cwd: "/home/me/krowk", updatedMs: 1_790_000_000_123,
            headIndex: UInt64.max - 1, headDigest: Data([0, 1, 2, 255]), hostedBy: "dev-a", hostDevice: "dev-b")
        try await cache.putIndex([full, Self.session("b", updatedMs: 5)], listedAtMs: 100)
        var expected = full
        expected.listedAtMs = 100
        let index = try await cache.index()
        #expect(index.map(\.id) == ["a", "b"])
        #expect(index.first == expected)
        // The list draws a cached row as it draws the generated record.
        let records: [any SessionRecord] = index
        #expect(records.map(\.title) == ["Fix the build", ""])

        try await cache.appendEvents(session: "a", Self.events(0..<3, in: "a"))
        try await cache.appendEvents(session: "b", Self.events(0..<3, in: "b"))
        try await cache.putIndex([Self.session("b", updatedMs: 6)], listedAtMs: 200)
        #expect(try await cache.index().map(\.id) == ["b"])
        #expect(try await cache.events(session: "a", limit: 10).isEmpty)
        #expect(try await cache.events(session: "b", limit: 10).count == 3)

        // A listing older than the last one written, arriving late, is dropped.
        try await cache.putIndex([Self.session("c")], listedAtMs: 150)
        #expect(try await cache.index().map(\.id) == ["b"])
    }

    @Test("R-IOS-1: an event id appended twice is kept once, where it first was")
    func testR_IOS_1_appendKeepsAnIdOnce() async throws {
        let cache = try await SessionCache.open(at: url)
        try await cache.putIndex([Self.session("s")], listedAtMs: 1)
        try await cache.appendEvents(session: "s", Self.events(0..<3))
        try await cache.appendEvents(session: "s", Self.events(2..<5))
        try await cache.appendEvents(session: "s", Self.events([1]))
        let kept = try await cache.events(session: "s", limit: 100)
        #expect(kept.map(\.id) == (0..<5).reversed().map { "s-e\($0)" })
        #expect(kept.map(\.ord) == [4, 3, 2, 1, 0])
        #expect(kept.last?.json == #"{"id":"s-e0"}"#)
    }

    @Test("R-IOS-3: events(before:limit:) pages a transcript backwards, newest first, across three pages")
    func testR_IOS_3_pagesNewestFirst() async throws {
        let cache = try await SessionCache.open(at: url)
        try await cache.putIndex([Self.session("s")], listedAtMs: 1)
        try await cache.appendEvents(session: "s", Self.events(0..<25))
        var pages: [[String]] = []
        var before: Int?
        while true {
            let page = try await cache.events(session: "s", before: before, limit: 10)
            if page.isEmpty { break }
            pages.append(page.map(\.id))
            before = page.last?.ord
        }
        #expect(pages.count == 3)
        #expect(pages.map(\.count) == [10, 10, 5])
        #expect(pages.flatMap { $0 } == (0..<25).reversed().map { "s-e\($0)" })
    }

    @Test("R-IOS-1: evict(keep: 20) of 25 sessions keeps the newest 20's events, and every index row")
    func testR_IOS_1_evictKeepsTheNewestTwenty() async throws {
        let cache = try await SessionCache.open(at: url)
        let sessions = (0..<25).map { Self.session("s\($0)", updatedMs: UInt64(1000 + $0)) }
        try await cache.putIndex(sessions.shuffled(), listedAtMs: 1)
        for session in sessions { try await cache.appendEvents(session: session.id, Self.events(0..<2, in: session.id)) }
        try await cache.evict(keep: 20)
        #expect(try await cache.index().count == 25)
        for (n, session) in sessions.enumerated() {
            let kept = try await cache.events(session: session.id, limit: 10).count
            #expect(kept == (n >= 5 ? 2 : 0), "\(session.id)")
        }
    }

    @Test("R-IOS-1: a schema version this build doesn't know is deleted, and the cache starts empty")
    func testR_IOS_1_unknownVersionStartsEmpty() async throws {
        let cache = try await SessionCache.open(at: url)
        try await cache.putIndex([Self.session("s")], listedAtMs: 1)
        try await cache.appendEvents(session: "s", Self.events(0..<3))
        await cache.close()

        var db: OpaquePointer?
        #expect(sqlite3_open(url.path(percentEncoded: false), &db) == SQLITE_OK)
        #expect(sqlite3_exec(db, "PRAGMA user_version = 99", nil, nil, nil) == SQLITE_OK)
        sqlite3_close(db)

        let reopened = try await SessionCache.open(at: url)
        #expect(try await reopened.index().isEmpty)
        #expect(try await reopened.events(session: "s", limit: 10).isEmpty)
        try await reopened.putIndex([Self.session("t")], listedAtMs: 2)
        #expect(try await reopened.index().map(\.id) == ["t"])

        // A file that isn't a database at all, the same.
        await reopened.close()
        try Data("not a database, not even close to one".utf8).write(to: url)
        for side in SessionCache.files(url).dropFirst() { try? FileManager.default.removeItem(at: side) }
        #expect(try await SessionCache.open(at: url).index().isEmpty)
    }

    @Test("R-IOS-1, R-E2E-1: the file has .complete protection and is excluded from backup")
    func testR_IOS_1_fileIsProtectedAndNotBackedUp() async throws {
        let cache = try await SessionCache.open(at: url)
        try await cache.putIndex([Self.session("s")], listedAtMs: 1)
        let values = try url.resourceValues(forKeys: [.fileProtectionKey, .isExcludedFromBackupKey])
        #expect(values.isExcludedFromBackup == true)
        #expect(values.fileProtection == .complete)
        // The WAL beside it too.
        let wal = try #require(SessionCache.files(url).first { $0.path().hasSuffix("-wal") })
        let walValues = try wal.resourceValues(forKeys: [.fileProtectionKey, .isExcludedFromBackupKey])
        #expect(walValues.isExcludedFromBackup == true)
        #expect(walValues.fileProtection == .complete)
    }

    @Test("R-IOS-1: forgetAll removes the file, and nothing writes it again")
    func testR_IOS_1_forgetAllRemovesTheFile() async throws {
        let cache = try await SessionCache.open(at: url)
        try await cache.putIndex([Self.session("s")], listedAtMs: 1)
        #expect(FileManager.default.fileExists(atPath: url.path(percentEncoded: false)))
        try await cache.forgetAll()
        for file in SessionCache.files(url) {
            #expect(!FileManager.default.fileExists(atPath: file.path(percentEncoded: false)), "\(file.lastPathComponent)")
        }
        await #expect(throws: SessionCacheFailure.self) { try await cache.appendEvents(session: "s", Self.events(0..<1)) }
        #expect(!FileManager.default.fileExists(atPath: url.path(percentEncoded: false)))
    }
}
