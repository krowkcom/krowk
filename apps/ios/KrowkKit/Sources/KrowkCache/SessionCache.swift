import Foundation
import KrowkKit
import SQLite3

/// One listed session as the cache keeps it (R-IOS-3): the fields of
/// krowk-ffi's generated `SessionSummary`, stored, not a second model of
/// the index (R-PROTO-2). A `SessionRecord`, so the list draws either.
public struct CachedSession: SessionRecord, Hashable {
    public var id: String
    public var title: String
    public var cwd: String
    public var updatedMs: UInt64
    /// Where its log ended when the index was last written: `HeadRecord`'s
    /// index and digest, both or neither.
    public var headIndex: UInt64?
    public var headDigest: Data?
    public var hostedBy: String?
    public var hostDevice: String?
    /// When the listing that named it was read, in milliseconds since the
    /// epoch.
    public var listedAtMs: UInt64
    /// Where the last read of it stopped (ticket 126), opaque here:
    /// `putRead` writes it with the events read. `putIndex` never writes
    /// it: a session listed again keeps its own.
    public var place: Data?

    public init(
        id: String, title: String, cwd: String, updatedMs: UInt64, headIndex: UInt64? = nil, headDigest: Data? = nil,
        hostedBy: String? = nil, hostDevice: String? = nil, listedAtMs: UInt64 = 0, place: Data? = nil
    ) {
        self.id = id
        self.title = title
        self.cwd = cwd
        self.updatedMs = updatedMs
        self.headIndex = headIndex
        self.headDigest = headDigest
        self.hostedBy = hostedBy
        self.hostDevice = hostDevice
        self.listedAtMs = listedAtMs
        self.place = place
    }
}

/// One logged event as the cache keeps it: its id, its JSON text as
/// krowk-ffi's `encodeLogEvent` wrote it (Swift never builds or parses
/// it, R-PROTO-2), and its place in the session's order.
public struct CachedEvent: Hashable, Sendable {
    public var id: String
    public var json: String
    public var ord: Int
}

/// Why the cache couldn't do something. A cache: the caller drops what
/// it was writing, and the network has it.
public struct SessionCacheFailure: Error, CustomStringConvertible, Sendable {
    public var description: String
    /// The file is not one this build reads (another schema, not a
    /// database): it is deleted and started afresh. Anything else (the
    /// phone locked) may pass, and leaves the file alone.
    var unreadable = false
    /// The cache was forgotten: nothing it is handed is ever written.
    public internal(set) var forgotten = false
}

/// The phone's on-device cache of the person's sessions (R-IOS-1,
/// R-IOS-3): the index of every listed session, and the logged transcript
/// of the 20 most recently updated, written as sessions sync, and where
/// each viewer stood in its session (`cursors`), written with the events it
/// names. Ticket 131 draws from it at launch, and reads each session on
/// from the place its last read stopped at (`putRead`).
///
/// One SQLite file, `Library/Application Support/krowk-cache.sqlite`
/// beside the install marker, through the system's SQLite (no package).
/// It holds decrypted session content (R-E2E-1: on the person's own phone,
/// never sent anywhere) and no key: `FileProtectionType.complete`, so
/// nothing reads it while the phone is locked, and excluded from backups,
/// as a restored phone is a new device. WAL, kept between connections
/// (`SQLITE_FCNTL_PERSIST_WAL`) so the side files keep their flags too.
/// `PRAGMA user_version` is the schema's version: one this build doesn't
/// know is deleted and started empty.
///
/// An actor on a serial queue of its own: SQLite blocks, and never on the
/// main thread or the concurrency runtime's. The file opens on first use,
/// so a phone locked at launch opens it on a later call. Once
/// `forgetAll` ran, every call is refused: a write still under way after a
/// reset never makes the file again.
public actor SessionCache {
    /// Where the app keeps it: beside the install marker
    /// (`KeychainKeyStorage.installMarker`), and purged with it.
    public static var defaultURL: URL {
        URL.applicationSupportDirectory.appending(path: "krowk-cache.sqlite", directoryHint: .notDirectory)
    }

    /// How many sessions keep their transcripts (`evict`).
    public static let transcripts = 20

    /// The schema this build writes.
    static let schemaVersion: Int32 = 1

    public let url: URL
    private let queue: DispatchSerialQueue
    public nonisolated var unownedExecutor: UnownedSerialExecutor { queue.asUnownedSerialExecutor() }
    /// The open connection; closed when it goes.
    private var connection: Connection?
    private var forgotten = false
    /// The phone is locked (`suspend`): the file is not opened until
    /// `resume`.
    private var suspended = false

    /// No I/O: the file opens on first use.
    public init(at url: URL = SessionCache.defaultURL) {
        self.url = url
        queue = DispatchSerialQueue(label: "krowk.cache")
    }

    /// The cache at `url`, opened now.
    public static func open(at url: URL = SessionCache.defaultURL) async throws(SessionCacheFailure) -> SessionCache {
        let cache = SessionCache(at: url)
        try await cache.ready()
        return cache
    }

    // MARK: The index

    /// Replaces the whole index with `sessions`, in one transaction: a
    /// session gone from the listing goes from the cache, with its events
    /// and cursor. A session listed again keeps its saved place.
    public func putIndex(_ sessions: [CachedSession], listedAtMs: UInt64) throws(SessionCacheFailure) {
        let db = try opened()
        try transaction(db) { () throws(SessionCacheFailure) in
            try exec(db, "CREATE TEMP TABLE IF NOT EXISTS listed(id TEXT PRIMARY KEY)")
            try exec(db, "DELETE FROM temp.listed")
            let upsert = try Statement(db, """
                INSERT INTO sessions(id, title, cwd, updated_ms, head_index, head_digest, hosted_by, host_device, listed_at_ms)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(id) DO UPDATE SET title = excluded.title, cwd = excluded.cwd, updated_ms = excluded.updated_ms,
                    head_index = excluded.head_index, head_digest = excluded.head_digest, hosted_by = excluded.hosted_by,
                    host_device = excluded.host_device, listed_at_ms = excluded.listed_at_ms
                """)
            let listed = try Statement(db, "INSERT OR IGNORE INTO temp.listed(id) VALUES (?)")
            for session in sessions {
                upsert.bind(1, session.id)
                upsert.bind(2, session.title)
                upsert.bind(3, session.cwd)
                upsert.bind(4, session.updatedMs)
                upsert.bind(5, session.headIndex)
                upsert.bind(6, session.headDigest)
                upsert.bind(7, session.hostedBy)
                upsert.bind(8, session.hostDevice)
                upsert.bind(9, listedAtMs)
                try upsert.run()
                listed.bind(1, session.id)
                try listed.run()
            }
            try exec(db, "DELETE FROM events WHERE session_id NOT IN (SELECT id FROM temp.listed)")
            try exec(db, "DELETE FROM cursors WHERE session_id NOT IN (SELECT id FROM temp.listed)")
            try exec(db, "DELETE FROM sessions WHERE id NOT IN (SELECT id FROM temp.listed)")
            try exec(db, "DELETE FROM temp.listed")
        }
    }

    /// The index, most recently updated first.
    public func index() throws(SessionCacheFailure) -> [CachedSession] {
        let db = try opened()
        let rows = try Statement(db, """
            SELECT id, title, cwd, updated_ms, head_index, head_digest, hosted_by, host_device, listed_at_ms, place
            FROM sessions ORDER BY updated_ms DESC, id
            """)
        var out: [CachedSession] = []
        while try rows.step() {
            let index = rows.uint64(4)
            let digest = rows.data(5)
            out.append(CachedSession(
                id: rows.text(0) ?? "", title: rows.text(1) ?? "", cwd: rows.text(2) ?? "", updatedMs: rows.uint64(3) ?? 0,
                headIndex: digest == nil ? nil : index, headDigest: index == nil ? nil : digest,
                hostedBy: rows.text(6), hostDevice: rows.text(7), listedAtMs: rows.uint64(8) ?? 0, place: rows.data(9)))
        }
        return out
    }

    // MARK: Transcripts

    /// Appends `session`'s logged events, in order, and with them, when
    /// given, the viewer's place as of the last of them (`putCursor`), in
    /// one transaction: a saved place never names an event the cache does
    /// not hold. The place is written only if, with the events in, the
    /// cache holds every id in `holding` (the event it names, and the first
    /// its writer wrote: a session evicted meanwhile holds neither); false
    /// when it was not. An event id already kept is kept once, where it
    /// was; the rest take the places after the session's last.
    @discardableResult
    public func appendEvents(
        session: String, _ events: [(id: String, json: String)], cursor: String? = nil, holding: [String] = []
    ) throws(SessionCacheFailure) -> Bool {
        guard !events.isEmpty || cursor != nil else { return false }
        let db = try opened()
        var placed = false
        try transaction(db) { () throws(SessionCacheFailure) in
            try insert(db, events, session: session)
            guard let cursor else { return }
            let held = try Statement(db, "SELECT 1 FROM events WHERE session_id = ? AND event_id = ?")
            for id in holding {
                held.bind(1, session)
                held.bind(2, id)
                let found = try held.step()
                held.reset()
                guard found else { return }
            }
            try put(db, cursor: cursor, session: session)
            placed = true
        }
        return placed
    }

    /// `events` after the session's last, in order; one already kept stays
    /// where it was.
    private func insert(_ db: OpaquePointer, _ events: [(id: String, json: String)], session: String) throws(SessionCacheFailure) {
        guard !events.isEmpty else { return }
        let last = try Statement(db, "SELECT MAX(ord) FROM events WHERE session_id = ?")
        last.bind(1, session)
        var ord = try last.step() ? (last.int(0).map { $0 + 1 } ?? 0) : 0
        let insert = try Statement(db, "INSERT OR IGNORE INTO events(session_id, ord, event_id, json) VALUES (?, ?, ?, ?)")
        for event in events {
            insert.bind(1, session)
            insert.bind(2, ord)
            insert.bind(3, event.id)
            insert.bind(4, event.json)
            try insert.run()
            if sqlite3_changes(db) > 0 { ord += 1 }
        }
    }

    /// What a read of `session` from the registry brought (ticket 131):
    /// its events, appended as `appendEvents` does, and the place the read
    /// stopped at (krowk-ffi's `encodeReadPlace`, opaque here), in one
    /// transaction, so a place kept never stands past an event the cache
    /// does not hold. `replacing`, the session's events and cursor go
    /// first: a read that started over (the session rolled back past what
    /// the cache held). A session not in the index keeps no place.
    public func putRead(session: String, _ events: [(id: String, json: String)], place: Data?, replacing: Bool = false) throws(SessionCacheFailure) {
        let db = try opened()
        try transaction(db) { () throws(SessionCacheFailure) in
            if replacing {
                for table in ["events", "cursors"] {
                    let drop = try Statement(db, "DELETE FROM \(table) WHERE session_id = ?")
                    drop.bind(1, session)
                    try drop.run()
                }
            }
            try insert(db, events, session: session)
            let put = try Statement(db, "UPDATE sessions SET place = ? WHERE id = ?")
            put.bind(1, place)
            put.bind(2, session)
            try put.run()
        }
    }

    /// The place the last read of `session` stopped at (`putRead`), if any.
    public func place(session: String) throws(SessionCacheFailure) -> Data? {
        let db = try opened()
        let row = try Statement(db, "SELECT place FROM sessions WHERE id = ?")
        row.bind(1, session)
        return try row.step() ? row.data(0) : nil
    }

    /// Where the viewer of `session` stands, as krowk-ffi's `encodeCursor`
    /// wrote it (Swift never builds or parses it, R-PROTO-2), replacing the
    /// one kept. Only with the events it names: `appendEvents(cursor:)`.
    public func putCursor(session: String, json: String) throws(SessionCacheFailure) {
        let db = try opened()
        try transaction(db) { () throws(SessionCacheFailure) in try put(db, cursor: json, session: session) }
    }

    /// The place kept for `session`, if any.
    public func cursor(session: String) throws(SessionCacheFailure) -> String? {
        let db = try opened()
        let row = try Statement(db, "SELECT cursor FROM cursors WHERE session_id = ?")
        row.bind(1, session)
        return try row.step() ? row.text(0) : nil
    }

    private func put(_ db: OpaquePointer, cursor: String, session: String) throws(SessionCacheFailure) {
        let upsert = try Statement(db, "INSERT INTO cursors(session_id, cursor) VALUES (?, ?) ON CONFLICT(session_id) DO UPDATE SET cursor = excluded.cursor")
        upsert.bind(1, session)
        upsert.bind(2, cursor)
        try upsert.run()
    }

    /// Up to `limit` of `session`'s events before `ord` (all, when nil),
    /// newest first: a transcript paged backwards (R-IOS-3).
    public func events(session: String, before ord: Int? = nil, limit: Int) throws(SessionCacheFailure) -> [CachedEvent] {
        let db = try opened()
        let rows = try Statement(db, """
            SELECT event_id, json, ord FROM events WHERE session_id = ? AND ord < ? ORDER BY ord DESC LIMIT ?
            """)
        rows.bind(1, session)
        rows.bind(2, ord ?? Int.max)
        rows.bind(3, max(limit, 0))
        var out: [CachedEvent] = []
        while try rows.step() {
            out.append(CachedEvent(id: rows.text(0) ?? "", json: rows.text(1) ?? "", ord: rows.int(2) ?? 0))
        }
        return out
    }

    /// Drops the events, cursors and read places of every session but the
    /// `keep` with the newest `updatedMs`: a place over no events would
    /// read on past a transcript the cache no longer holds. The index
    /// stays whole.
    public func evict(keep: Int = SessionCache.transcripts) throws(SessionCacheFailure) {
        let db = try opened()
        try transaction(db) { () throws(SessionCacheFailure) in
            for table in ["events", "cursors"] {
                let drop = try Statement(db, """
                    DELETE FROM \(table) WHERE session_id NOT IN (SELECT id FROM sessions ORDER BY updated_ms DESC, id LIMIT ?)
                    """)
                drop.bind(1, max(keep, 0))
                try drop.run()
            }
            let forget = try Statement(db, """
                UPDATE sessions SET place = NULL
                WHERE place IS NOT NULL AND id NOT IN (SELECT id FROM sessions ORDER BY updated_ms DESC, id LIMIT ?)
                """)
            forget.bind(1, max(keep, 0))
            try forget.run()
        }
    }

    // MARK: Forgetting

    /// Closes the cache and deletes its file: the reset of a removed
    /// phone, a sign-out. Every later call is refused.
    public func forgetAll() throws(SessionCacheFailure) {
        forgotten = true
        close()
        try Self.forgetFiles(at: url)
    }

    /// Deletes the cache at `url` and its side files, with nothing open
    /// on it: the first launch after an install purges what an earlier
    /// one left. Missing files are no failure.
    public nonisolated static func forgetFiles(at url: URL = SessionCache.defaultURL) throws(SessionCacheFailure) {
        // The side files first: a WAL left beside no database would be
        // taken up by the next one made there.
        for file in files(url).reversed() {
            do {
                try FileManager.default.removeItem(at: file)
            } catch CocoaError.fileNoSuchFile {
            } catch {
                throw SessionCacheFailure(description: "the cache was not deleted: \(error.localizedDescription)")
            }
        }
    }

    /// The database and the side files SQLite keeps beside it.
    public nonisolated static func files(_ url: URL = SessionCache.defaultURL) -> [URL] {
        let path = url.path(percentEncoded: false)
        return [url] + ["-wal", "-shm", "-journal"].map { URL(filePath: path + $0) }
    }

    // MARK: Opening

    private func ready() throws(SessionCacheFailure) {
        _ = try opened()
    }

    private func opened() throws(SessionCacheFailure) -> OpaquePointer {
        guard !forgotten else { throw SessionCacheFailure(description: "the cache was forgotten", forgotten: true) }
        guard !suspended else { throw SessionCacheFailure(description: "the phone is locked") }
        if let connection { return connection.db }
        do {
            connection = Connection(try connect())
        } catch where error.unreadable {
            // A file this build can't read is a cache: started afresh.
            close()
            try Self.forgetFiles(at: url)
            connection = Connection(try connect())
        }
        return try opened()
    }

    /// Opens the file, making it `.complete` and kept out of backups;
    /// throws for a schema this build doesn't know, or a file that isn't
    /// one.
    private func connect() throws(SessionCacheFailure) -> OpaquePointer {
        let path = url.path(percentEncoded: false)
        do {
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        } catch {
            throw SessionCacheFailure(description: "no directory for the cache: \(error.localizedDescription)")
        }
        if !FileManager.default.fileExists(atPath: path) {
            // Protected from its first byte, before SQLite writes any.
            guard FileManager.default.createFile(atPath: path, contents: nil, attributes: [.protectionKey: FileProtectionType.complete]) else {
                throw SessionCacheFailure(description: "the cache file could not be made")
            }
        }
        var db: OpaquePointer?
        let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_NOMUTEX | SQLITE_OPEN_FILEPROTECTION_COMPLETE
        let opened = sqlite3_open_v2(path, &db, flags, nil)
        guard opened == SQLITE_OK, let db else {
            let why = db.map { String(cString: sqlite3_errmsg($0)) } ?? "SQLite \(opened)"
            if let db { sqlite3_close_v2(db) }
            throw SessionCacheFailure(description: "the cache did not open: \(why)")
        }
        do throws(SessionCacheFailure) {
            sqlite3_busy_timeout(db, 1000)
            var persist: Int32 = 1
            sqlite3_file_control(db, "main", SQLITE_FCNTL_PERSIST_WAL, &persist)
            try exec(db, "PRAGMA journal_mode = WAL")
            try exec(db, "PRAGMA foreign_keys = OFF")
            // A temp table never spills to an unprotected file in tmp/.
            try exec(db, "PRAGMA temp_store = MEMORY")
            // A session gone from the listing, or evicted, leaves no
            // content behind in free pages.
            try exec(db, "PRAGMA secure_delete = ON")
            // Nor in old frames of the WAL, which is kept between
            // connections: cut back each time it starts over.
            try exec(db, "PRAGMA journal_size_limit = 0")
            let version = try Statement(db, "PRAGMA user_version")
            let found: Int32 = try version.step() ? Int32(truncatingIfNeeded: version.int(0) ?? 0) : 0
            switch found {
            case Self.schemaVersion: break
            case 0: try create(db)
            default:
                throw SessionCacheFailure(description: "the cache's schema is version \(found), not \(Self.schemaVersion)", unreadable: true)
            }
            try protect()
            return db
        } catch {
            let code = sqlite3_errcode(db)
            sqlite3_close_v2(db)
            if code == SQLITE_NOTADB || code == SQLITE_CORRUPT {
                throw SessionCacheFailure(description: error.description, unreadable: true)
            }
            throw error
        }
    }

    private func create(_ db: OpaquePointer) throws(SessionCacheFailure) {
        try transaction(db) { () throws(SessionCacheFailure) in
            try exec(db, """
                CREATE TABLE IF NOT EXISTS sessions(
                    id TEXT PRIMARY KEY, title TEXT NOT NULL, cwd TEXT NOT NULL, updated_ms INTEGER NOT NULL,
                    head_index INTEGER, head_digest BLOB, hosted_by TEXT, host_device TEXT,
                    listed_at_ms INTEGER NOT NULL, place BLOB)
                """)
            try exec(db, """
                CREATE TABLE IF NOT EXISTS events(
                    session_id TEXT NOT NULL, ord INTEGER NOT NULL, event_id TEXT NOT NULL, json TEXT NOT NULL,
                    PRIMARY KEY(session_id, event_id))
                """)
            try exec(db, "CREATE INDEX IF NOT EXISTS events_by_ord ON events(session_id, ord)")
            try exec(db, "CREATE TABLE IF NOT EXISTS cursors(session_id TEXT PRIMARY KEY, cursor BLOB)")
            try exec(db, "PRAGMA user_version = \(Self.schemaVersion)")
        }
    }

    /// Out of backups, the database and every side file SQLite made
    /// beside it, and `.complete`, the database and its WAL, which hold
    /// the content: set at each open, as a side file may be new. The
    /// shared-memory index (`-shm`) keeps the class SQLite gives it: it
    /// is mapped into memory, holds no content, and a mapping whose key
    /// went at lock faults rather than fails.
    private func protect() throws(SessionCacheFailure) {
        for var file in Self.files(url) where FileManager.default.fileExists(atPath: file.path(percentEncoded: false)) {
            do {
                if !file.path(percentEncoded: false).hasSuffix("-shm") {
                    try FileManager.default.setAttributes(
                        [.protectionKey: FileProtectionType.complete], ofItemAtPath: file.path(percentEncoded: false))
                }
                var values = URLResourceValues()
                values.isExcludedFromBackup = true
                try file.setResourceValues(values)
            } catch {
                throw SessionCacheFailure(description: "the cache could not be protected: \(error.localizedDescription)")
            }
        }
    }

    /// The phone is locking (`protectedDataWillBecomeUnavailable`): the
    /// connection is closed, and every call refused until `resume`, so
    /// none is held over a file whose key went.
    public func suspend() {
        suspended = true
        close()
    }

    /// The phone is unlocked again (`protectedDataDidBecomeAvailable`).
    public func resume() {
        suspended = false
    }

    /// Closes the connection; the next call opens it again.
    func close() {
        connection = nil
    }

    private func transaction(_ db: OpaquePointer, _ body: () throws(SessionCacheFailure) -> Void) throws(SessionCacheFailure) {
        try exec(db, "BEGIN IMMEDIATE")
        do {
            try body()
            try exec(db, "COMMIT")
        } catch {
            _ = sqlite3_exec(db, "ROLLBACK", nil, nil, nil)
            throw error
        }
    }
}

private func exec(_ db: OpaquePointer, _ sql: String) throws(SessionCacheFailure) {
    guard sqlite3_exec(db, sql, nil, nil, nil) == SQLITE_OK else {
        throw SessionCacheFailure(description: String(cString: sqlite3_errmsg(db)))
    }
}

/// One open database, closed when it goes.
private final class Connection {
    let db: OpaquePointer

    init(_ db: OpaquePointer) { self.db = db }

    deinit { sqlite3_close_v2(db) }
}

/// SQLite copies what it's bound (SQLITE_TRANSIENT).
private let transient = unsafeBitCast(-1, to: sqlite3_destructor_type.self)

/// One prepared statement, finalized when it goes.
private final class Statement {
    private let db: OpaquePointer
    private let statement: OpaquePointer

    init(_ db: OpaquePointer, _ sql: String) throws(SessionCacheFailure) {
        var statement: OpaquePointer?
        guard sqlite3_prepare_v2(db, sql, -1, &statement, nil) == SQLITE_OK, let statement else {
            throw SessionCacheFailure(description: String(cString: sqlite3_errmsg(db)))
        }
        self.db = db
        self.statement = statement
    }

    deinit { sqlite3_finalize(statement) }

    func bind(_ at: Int32, _ value: String?) {
        if let value { sqlite3_bind_text(statement, at, value, -1, transient) } else { sqlite3_bind_null(statement, at) }
    }

    func bind(_ at: Int32, _ value: Int) {
        sqlite3_bind_int64(statement, at, Int64(value))
    }

    /// Its bits as SQLite's signed integer: a `u64` past `Int64.max` reads
    /// back the same.
    func bind(_ at: Int32, _ value: UInt64?) {
        if let value { sqlite3_bind_int64(statement, at, Int64(bitPattern: value)) } else { sqlite3_bind_null(statement, at) }
    }

    func bind(_ at: Int32, _ value: Data?) {
        guard let value else { return _ = sqlite3_bind_null(statement, at) }
        // An empty buffer has no base address, which would bind NULL.
        guard !value.isEmpty else { return _ = sqlite3_bind_zeroblob(statement, at, 0) }
        value.withUnsafeBytes { bytes in
            _ = sqlite3_bind_blob(statement, at, bytes.baseAddress, Int32(bytes.count), transient)
        }
    }

    /// A row is ready (true), or there are no more (false).
    func step() throws(SessionCacheFailure) -> Bool {
        switch sqlite3_step(statement) {
        case SQLITE_ROW: return true
        case SQLITE_DONE: return false
        default: throw SessionCacheFailure(description: String(cString: sqlite3_errmsg(db)))
        }
    }

    /// Readies it to bind again, mid-result or not.
    func reset() {
        sqlite3_reset(statement)
        sqlite3_clear_bindings(statement)
    }

    /// Runs it to its end, and readies it to bind again.
    func run() throws(SessionCacheFailure) {
        defer {
            sqlite3_reset(statement)
            sqlite3_clear_bindings(statement)
        }
        while try step() {}
    }

    func text(_ column: Int32) -> String? {
        guard let text = sqlite3_column_text(statement, column) else { return nil }
        return String(cString: text)
    }

    func int(_ column: Int32) -> Int? {
        sqlite3_column_type(statement, column) == SQLITE_NULL ? nil : Int(sqlite3_column_int64(statement, column))
    }

    func uint64(_ column: Int32) -> UInt64? {
        sqlite3_column_type(statement, column) == SQLITE_NULL ? nil : UInt64(bitPattern: sqlite3_column_int64(statement, column))
    }

    func data(_ column: Int32) -> Data? {
        guard sqlite3_column_type(statement, column) != SQLITE_NULL else { return nil }
        let count = Int(sqlite3_column_bytes(statement, column))
        guard count > 0, let bytes = sqlite3_column_blob(statement, column) else { return Data() }
        return Data(bytes: bytes, count: count)
    }
}
