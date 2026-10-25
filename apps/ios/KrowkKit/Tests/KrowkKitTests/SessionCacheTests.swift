// R-IOS-1, R-IOS-3, R-E2E-1: the phone's on-device cache (ticket 129),
// each test on a file of its own under the temporary directory: the
// index replaced whole, events kept once and paged newest first, the
// transcripts of the 20 newest sessions kept, a schema this build
// doesn't know started afresh, the file's protection and backup
// exclusion, forgetting it, and a viewer's place kept in one write with
// the events it names (ticket 130). The hosted CacheTests write it from the
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

        // An empty digest is kept as one, not as none.
        try await cache.putIndex([CachedSession(id: "c", title: "", cwd: "", updatedMs: 1, headIndex: 0, headDigest: Data())], listedAtMs: 300)
        #expect(try await cache.index().first?.headDigest == Data())
        #expect(try await cache.index().first?.headIndex == 0)
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

    @Test("R-IOS-1: a place is kept with the events it names, replaced by the next, and read back as written")
    func testR_IOS_1_aCursorIsKeptWithItsEvents() async throws {
        let cache = try await SessionCache.open(at: url)
        try await cache.putIndex([Self.session("s"), Self.session("t")], listedAtMs: 1)
        #expect(try await cache.cursor(session: "s") == nil)
        try await cache.appendEvents(session: "s", Self.events(0..<3), cursor: #"{"newest":"s-e2"}"#)
        #expect(try await cache.cursor(session: "s") == #"{"newest":"s-e2"}"#)
        #expect(try await cache.events(session: "s", limit: 10).count == 3)
        // A place alone, with no new event (a frame of seen lines).
        try await cache.appendEvents(session: "s", [], cursor: #"{"newest":"s-e2","afterSeq":4}"#)
        #expect(try await cache.cursor(session: "s") == #"{"newest":"s-e2","afterSeq":4}"#)
        try await cache.putCursor(session: "t", json: "{}")
        #expect(try await cache.cursor(session: "t") == "{}")
        // Gone from the listing: gone with its events.
        try await cache.putIndex([Self.session("s")], listedAtMs: 2)
        #expect(try await cache.cursor(session: "t") == nil)
        try await cache.evict(keep: 0)
        #expect(try await cache.cursor(session: "s") == nil)
    }

    @Test("R-IOS-1: a place is written only while the cache holds the events it is held to: none for a session evicted under it")
    func testR_IOS_1_aCursorNeedsItsEventsHeld() async throws {
        let cache = try await SessionCache.open(at: url)
        try await cache.putIndex([Self.session("s")], listedAtMs: 1)
        #expect(try await cache.appendEvents(session: "s", Self.events(0..<3), cursor: "one", holding: ["s-e0", "s-e2"]))
        #expect(try await cache.cursor(session: "s") == "one")
        // Evicted, then written on from where the screen was: a tail.
        try await cache.evict(keep: 0)
        #expect(try await cache.appendEvents(session: "s", Self.events(3..<5), cursor: "two", holding: ["s-e0", "s-e4"]) == false)
        #expect(try await cache.cursor(session: "s") == nil, "a place over a tail")
        #expect(try await cache.events(session: "s", limit: 10).count == 2, "the events still go in")
        #expect(try await cache.appendEvents(session: "s", [], cursor: "three", holding: ["s-e9"]) == false)
        #expect(try await cache.cursor(session: "s") == nil)
    }

    @Test("R-IOS-1: events and the place they come with are one write: when the place is refused, the events are not kept either")
    func testR_IOS_1_eventsAndCursorAreOneTransaction() async throws {
        let cache = try await SessionCache.open(at: url)
        try await cache.putIndex([Self.session("s")], listedAtMs: 1)
        await cache.close()
        var db: OpaquePointer?
        #expect(sqlite3_open(url.path(percentEncoded: false), &db) == SQLITE_OK)
        #expect(sqlite3_exec(db, "CREATE TRIGGER refuse BEFORE INSERT ON cursors BEGIN SELECT RAISE(ABORT, 'refused'); END", nil, nil, nil) == SQLITE_OK)
        sqlite3_close(db)
        await #expect(throws: SessionCacheFailure.self) {
            try await cache.appendEvents(session: "s", Self.events(0..<3), cursor: #"{"newest":"s-e2"}"#)
        }
        #expect(try await cache.events(session: "s", limit: 10).isEmpty, "events kept without their place")
        #expect(try await cache.cursor(session: "s") == nil)
        try await cache.appendEvents(session: "s", Self.events(0..<3))
        #expect(try await cache.events(session: "s", limit: 10).count == 3, "without a place they go in")
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

    @Test("R-IOS-2: the session last open is kept as an index row, read back at once on the caller's thread, cleared only by its own screen, and gone with its session from the listing")
    func testR_IOS_2_theLastOpenSessionIsKept() async throws {
        let cache = try await SessionCache.open(at: url)
        #expect(cache.lastOpenNow() == nil)
        try await cache.putIndex([Self.session("a", title: "A"), Self.session("b")], listedAtMs: 1)
        try await cache.putLastOpen("a")
        #expect(cache.lastOpenNow()?.title == "A")
        try await cache.putLastOpen("b")
        // Left after another opened: the other stays.
        try await cache.clearLastOpen("a")
        #expect(try await cache.lastOpen()?.id == "b")
        try await cache.clearLastOpen("b")
        #expect(cache.lastOpenNow() == nil)
        // Not in the index: never kept.
        try await cache.putLastOpen("z")
        #expect(cache.lastOpenNow() == nil)
        try await cache.putLastOpen("a")
        try await cache.putIndex([Self.session("b")], listedAtMs: 2)
        #expect(cache.lastOpenNow() == nil, "a session gone from the listing is not last open")
        // Locked: none read, and nothing blocks.
        try await cache.putLastOpen("b")
        await cache.suspend()
        #expect(cache.lastOpenNow() == nil)
        await cache.resume()
        #expect(cache.lastOpenNow()?.id == "b")
    }

    @Test("R-IOS-2: a version 2 file (ticket 131) is taken up, not wiped: its rows and places stay, and it keeps a last open session from then")
    func testR_IOS_2_aVersionTwoFileIsTakenUp() async throws {
        let cache = try await SessionCache.open(at: url)
        try await cache.putIndex([Self.session("s")], listedAtMs: 1)
        try await cache.appendEvents(session: "s", Self.events(0..<3))
        await cache.close()

        var db: OpaquePointer?
        #expect(sqlite3_open(url.path(percentEncoded: false), &db) == SQLITE_OK)
        #expect(sqlite3_exec(db, "DROP TABLE last_open; PRAGMA user_version = 2", nil, nil, nil) == SQLITE_OK)
        sqlite3_close(db)

        let reopened = try await SessionCache.open(at: url)
        #expect(try await reopened.index().map(\.id) == ["s"])
        #expect(try await reopened.events(session: "s", limit: 10).count == 3)
        try await reopened.putLastOpen("s")
        #expect(reopened.lastOpenNow()?.id == "s")
    }

    @Test("R-IOS-1, R-E2E-1: the file has .complete protection (on a device; the simulator has none) and is excluded from backup")
    func testR_IOS_1_fileIsProtectedAndNotBackedUp() async throws {
        let cache = try await SessionCache.open(at: url)
        try await cache.putIndex([Self.session("s")], listedAtMs: 1)
        // The database, and the WAL beside it.
        let wal = try #require(SessionCache.files(url).first { $0.path().hasSuffix("-wal") })
        for file in [url, wal] {
            let values = try file.resourceValues(forKeys: [.fileProtectionKey, .isExcludedFromBackupKey])
            #expect(values.isExcludedFromBackup == true, "\(file.lastPathComponent)")
            #if targetEnvironment(simulator)
            // The simulator has no Data Protection: whatever class is set,
            // it reads back its own default. A device reads `.complete`
            // (ticket 107's real-device checks).
            #expect(values.fileProtection != nil && values.fileProtection != URLFileProtection.none, "\(file.lastPathComponent)")
            #else
            #expect(values.fileProtection == .complete, "\(file.lastPathComponent)")
            #endif
        }
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

    @Test("R-IOS-1: locked, the cache lets its file go and opens nothing until unlocked; then it reads as before")
    func testR_IOS_1_suspendedUntilUnlocked() async throws {
        let cache = try await SessionCache.open(at: url)
        try await cache.putIndex([Self.session("s")], listedAtMs: 1)
        await cache.suspend()
        await #expect(throws: SessionCacheFailure.self) { try await cache.index() }
        await cache.resume()
        #expect(try await cache.index().map(\.id) == ["s"])
    }
}
