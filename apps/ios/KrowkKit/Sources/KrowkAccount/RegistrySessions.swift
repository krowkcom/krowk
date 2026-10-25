import Foundation
import Krowk
import KrowkCache
import KrowkKit
import Synchronization

/// The generated record is what a row reads (R-PROTO-2): no Swift mirror.
extension SessionSummary: SessionRecord {}

/// KrowkKit's `SessionSource` over krowk-ffi's `Account` (R-IOS-3,
/// R-CLIENT-1): Rust lists the person's synced sessions from their sealed
/// indexes, each record checked against the device list this phone pinned,
/// and counts those it can't open. It never reads a transcript to list.
/// The holder's name comes from that verified list (`Account.devices`,
/// read from the Keychain, no network).
///
/// Every listing first checks that the phone is still on the person's
/// devices (`Account.checkDeviceList`, ticket 117), which also takes up a
/// list that grew and a rotated user key: a phone removed, or left off a
/// list that was started over, lists nothing (`SessionListFailure.gone`),
/// and `onGone` hears it, as it does when opening a session is refused
/// for that reason. A list this phone won't trust (forked, shorter, one
/// that starts elsewhere and doesn't verify) lists nothing either, and
/// says so; it is no removal.
///
/// Each listing is written to `cache`, when there is one, as its index
/// (ticket 129), off the calling thread.
public final class RegistrySessions: SessionSource {
    public let api: String
    private let storage: any KeyStorage
    /// Opened on first use, off the main actor: opening reads the Keychain.
    private let account = Mutex<Account?>(nil)
    private let onGone: @Sendable (DeviceGone) -> Void
    /// Where each listing is kept (R-IOS-1, R-IOS-3).
    public let cache: SessionCache?
    /// The last listing's write to `cache`.
    private let caching = Mutex<Task<Void, Never>?>(nil)
    private let made = Mutex(Calls())

    /// The registry reads made so far, by kind (ticket 131's tests: a
    /// session drawn from the cache makes none).
    public struct Calls: Equatable, Sendable {
        public var lists = 0
        public var opens = 0
        public var resumes = 0

        public init() {}
    }

    #if DEBUG
    public var calls: Calls { made.withLock { $0 } }
    #endif

    /// - Parameters:
    ///   - onGone: told, on whatever thread found it, each time a check
    ///     finds the phone gone; the app shows why, once.
    ///   - cache: each listing's index goes there, and the transcripts of
    ///     all but the 20 newest sessions go from it.
    public init(
        api: String, storage: any KeyStorage, cache: SessionCache? = nil,
        onGone: @escaping @Sendable (DeviceGone) -> Void = { _ in }
    ) {
        self.api = api
        self.storage = storage
        self.cache = cache
        self.onGone = onGone
    }

    /// Blocking: the device list's check, then a registry call per page
    /// of 100 and one per session.
    public func list() throws(SessionListFailure) -> SessionListing {
        // Never on the main thread: a long list takes a while.
        dispatchPrecondition(condition: .notOnQueue(.main))
        switch check() {
        case .gone(let gone): throw .gone(gone)
        case .untrusted(let why): throw .untrusted(detail: why)
        case .listed, .unknown: break
        }
        do {
            let account = try opened()
            made.withLock { $0.lists += 1 }
            let list = try account.listSessions()
            let devices = (try? account.devices()) ?? []
            let names = Dictionary(devices.map { ($0.id, $0.name) }, uniquingKeysWith: { first, _ in first })
            if let cache {
                let sessions = list.sessions
                let at = UInt64(Date.now.timeIntervalSince1970 * 1000)
                // In the order the listings came back: each write waits
                // for the one before.
                caching.withLock { last in
                    let previous = last
                    last = Task.detached {
                        await previous?.value
                        await cache.record(sessions, listedAtMs: at)
                    }
                }
            }
            return SessionListing(sessions: list.sessions, unreadable: Int(list.unreadable), deviceNames: names)
        } catch {
            throw SessionListFailure(error)
        }
    }

    /// Whether the phone is gone from the person's devices, as the
    /// registry's device list says now; nil while it is on it, and when
    /// the check couldn't say (the registry out of reach, a list this
    /// phone won't trust): the listing that follows says why. Told to
    /// `onGone` too. Blocking: a registry call or two.
    public func standing() -> DeviceGone? {
        if case .gone(let gone) = check() { gone } else { nil }
    }

    /// The device list checked now, as `Standing`; a phone found gone is
    /// told to `onGone`. Blocking: a registry call or two.
    public func check() -> Standing {
        let found = checkQuietly()
        if case .gone(let gone) = found { onGone(gone) }
        return found
    }

    /// Whether the device list this phone kept names no other device and
    /// no recovery kit: it may hold the person's only copy of their keys.
    /// From the Keychain, no network; false when it can't be read.
    /// Blocking.
    public func onlyCopy() -> Bool {
        dispatchPrecondition(condition: .notOnQueue(.main))
        guard let account = try? opened(), let devices = try? account.devices(), let kit = try? account.hasRecoveryKit() else { return false }
        return !kit && devices.allSatisfy(\.thisDevice)
    }

    /// `check`, telling no one: what the removed screen's "Check again"
    /// asks. Blocking.
    public func checkQuietly() -> Standing {
        dispatchPrecondition(condition: .notOnQueue(.main))
        do {
            try opened().checkDeviceList()
            return .listed
        } catch {
            return Standing(error)
        }
    }

    /// Session `id`, opened to follow live (`RelayConnection`), held to at
    /// least `known`, the head its listing named. On the relay
    /// `RelayConnection.relayOverride` names, else the one Rust picks for
    /// the registry. A refusal is checked against the device list: a phone
    /// gone from it is refused as such, and `onGone` hears it. Blocking:
    /// registry calls.
    public func openSession(id: String, known: HeadRecord?) throws -> OpenedSession {
        dispatchPrecondition(condition: .notOnQueue(.main))
        return try checked {
            let account = try opened()
            made.withLock { $0.opens += 1 }
            return try account.openSession(id: id, known: known, relayOverride: RelayConnection.relayOverride())
        }
    }

    /// Session `id`, opened to follow live from what `cache` kept of it
    /// (ticket 131, R-IOS-1, R-PERF-6). With a place its last read stopped
    /// at, read on from there (`Account.resumeSession`), held to the higher
    /// of `known` and the place's head: only the chunks after it. Without
    /// one, `openSession`. A resume refused for good (`session_rolled_back`:
    /// the registry rolled back past what the cache holds) opens the
    /// session afresh, held to `known` alone, and the cache's events of it
    /// are replaced by what that read (`SessionOpening.replaced`). What a
    /// read brought and the place it stopped at go into the cache in one
    /// transaction, off this thread. The viewer starts at the newest event
    /// the cache holds with every one before it (`SessionOpening.held`:
    /// its cursor's, or the last read's), none after a replace. With no
    /// cache, `openSession`. Blocking: the cache, then registry calls.
    /// Never on the main thread nor the concurrency runtime's: it waits
    /// on the cache.
    public func resume(id: String, known: HeadRecord?) throws -> SessionOpening {
        dispatchPrecondition(condition: .notOnQueue(.main))
        guard let cache else { return SessionOpening(session: try openSession(id: id, known: known)) }
        let (kept, cursorText) = Self.waiting { () async -> ((place: Data, held: String?)?, String?) in
            ((try? await cache.place(session: id)) ?? nil, try? await cache.cursor(session: id))
        }
        let held = Self.newer(cursorText.flatMap { try? decodeCursor(json: $0) }?.newest, kept?.held)
        if let kept, let place = String(data: kept.place, encoding: .utf8).flatMap({ try? decodeReadPlace(json: $0) }) {
            do {
                let session = try checked {
                    let account = try opened()
                    made.withLock { $0.resumes += 1 }
                    let higher = known.map { $0.index >= place.head.index ? $0 : place.head } ?? place.head
                    return try account.resumeSession(id: id, place: place, known: higher, relayOverride: RelayConnection.relayOverride())
                }
                keep(session, id: id, in: cache, held: held, from: kept.place)
                return SessionOpening(session: session, held: held)
            } catch KrowkError.Refused(code: "session_rolled_back", _) {
                // Held to the listing's head alone: the place's is on the
                // log that was rolled back.
                let session = try openSession(id: id, known: known)
                keep(session, id: id, in: cache, held: nil, from: kept.place, replacing: true)
                return SessionOpening(session: session, replaced: true)
            }
        }
        let session = try openSession(id: id, known: known)
        keep(session, id: id, in: cache, held: nil, from: kept?.place)
        return SessionOpening(session: session, held: held)
    }

    /// The later of two event ids (UUIDv7), either missing.
    static func newer(_ a: String?, _ b: String?) -> String? {
        guard let a else { return b }
        guard let b else { return a }
        return max(a, b)
    }

    /// What `session`'s read brought and where it stopped, into `cache` in
    /// one transaction, off the calling thread: the place only if the
    /// cache's is still `from`, and with it the newest event held, the
    /// later of `held` (before the read) and the read's newest. A read
    /// whose events don't all decode or encode keeps no place: it would
    /// stand past what the cache lacks.
    private func keep(_ session: OpenedSession, id: String, in cache: SessionCache, held: String?, from: Data?, replacing: Bool = false) {
        Task.detached {
            let events = try? session.events()
            let rows = events?.compactMap { event in (try? encodeLogEvent(event: event)).map { (id: event.id, json: $0) } } ?? []
            let whole = events.map { rows.count == $0.count } ?? false
            let place = whole ? session.place().flatMap { try? encodeReadPlace(place: $0) }.map { Data($0.utf8) } : nil
            let newest = Self.newer(held, rows.map(\.id).max())
            try? await cache.putRead(session: id, rows, place: place, held: newest, from: from, replacing: replacing)
        }
    }

    /// `open`'s refusal checked against the device list: a phone gone from
    /// it is refused as such, and `onGone` hears it.
    private func checked<T>(_ open: () throws -> T) throws -> T {
        do {
            return try open()
        } catch KrowkError.Refused(let code, let detail) {
            if let gone = standing() { throw KrowkError.Refused(code: gone.kind, detail: gone.title + ".") }
            throw KrowkError.Refused(code: code, detail: detail)
        }
    }

    /// `work` run to its end, from a thread that may block (never the main
    /// one, nor the concurrency runtime's, which it would hold): the cache
    /// is an actor on a queue of its own.
    private static func waiting<T: Sendable>(_ work: @escaping @Sendable () async -> T) -> T {
        let result = Waited<T>()
        Task.detached {
            result.value = await work()
            result.done.signal()
        }
        result.done.wait()
        return result.value!
    }

    func opened() throws -> Account {
        if let account = account.withLock({ $0 }) { return account }
        let opened = try Account.open(api: api, storage: storage)
        return account.withLock { kept in
            if let kept { return kept }
            kept = opened
            return opened
        }
    }
}

/// What `waiting` waited for: written before `done` is signalled, read
/// after it is waited on.
private final class Waited<T>: @unchecked Sendable {
    var value: T?
    let done = DispatchSemaphore(value: 0)
}

extension DeviceGone {
    /// `Account.checkDeviceList`'s refusals that mean the phone is gone
    /// from the person's devices; nil for anything else.
    init?(_ error: any Error) {
        switch error as? KrowkError {
        case .Refused(code: "device_removed", let detail)?: self = .removed(detail: detail)
        case .Refused(code: "device_list_reset", let detail)?: self = .listReset(detail: detail)
        default: return nil
        }
    }
}

extension Standing {
    /// `Account.checkDeviceList`'s refusal: gone, a list or key this phone
    /// won't trust, or a check that couldn't say.
    init(_ error: any Error) {
        if let gone = DeviceGone(error) {
            self = .gone(gone)
            return
        }
        switch error as? KrowkError {
        case .Refused(code: "device_list_refused", let detail)?, .Refused(code: "user_key_refused", let detail)?:
            self = .untrusted(detail)
        case .Refused(_, let detail)?, .Internal(let detail)?, .Decode(let detail)?, .InvalidInput(let detail)?, .Network(let detail)?:
            self = .unknown(detail)
        case nil:
            self = .unknown(String(describing: error))
        }
    }
}

extension SessionListFailure {
    /// The core's refusals, as `Account.listSessions` names them.
    init(_ error: any Error) {
        if let gone = DeviceGone(error) {
            self = .gone(gone)
            return
        }
        switch error as? KrowkError {
        case .Network(let detail)?: self = .network(detail: detail)
        case .Refused(code: "not_paired", _)?: self = .notPaired
        case .Refused(_, let detail)?, .Internal(let detail)?, .Decode(let detail)?, .InvalidInput(let detail)?:
            self = .other(detail: detail)
        case nil: self = .other(detail: String(describing: error))
        }
    }
}
