import Foundation
import Krowk
import KrowkCache
import KrowkKit
import Synchronization

// Cache first (R-IOS-1, R-IOS-3, ticket 131): the list and a session's
// transcript drawn from what the phone kept, before the network says
// anything; the session then read on from the place its last read stopped
// at, and followed live from the newest event the cache holds.

extension SessionCache {
    /// The index as the list draws it, most recently updated first; nil
    /// when it holds nothing or can't be read (the phone locked). Never
    /// live: a kept row has no lease (`SessionListModel.liveTrusted`).
    public func listing() -> SessionListing? {
        guard let rows = try? index(), !rows.isEmpty else { return nil }
        return SessionListing(sessions: rows, unreadable: 0)
    }
}

/// Where a listed session's log ended, as its record says: a fresh
/// listing's, or the cache's index row.
public func listedHead(_ record: any SessionRecord) -> HeadRecord? {
    switch record {
    case let summary as SessionSummary: summary.head
    case let kept as CachedSession: kept.headIndex.flatMap { index in kept.headDigest.map { HeadRecord(index: index, digest: $0) } }
    default: nil
    }
}

/// A session opened to follow live (`RelayConnection(resuming:)`), and the
/// newest event the phone already holds with every one before it (its
/// cache's cursor), which the viewer starts at: its welcome's catch-up
/// asks the host only for what came after.
public struct SessionOpening: @unchecked Sendable {
    public let session: OpenedSession
    public let held: String?

    public init(session: OpenedSession, held: String? = nil) {
        self.session = session
        self.held = held
    }
}

/// What the cache kept of one session's transcript, a page at a time
/// (`KeptTranscript`): the newest 200 events, then 200 before each page as
/// the person scrolls up. Read on the cache's queue and decoded
/// (`decodeLogEvent`) off the main thread; an event this build can't
/// decode is left out. The newest page may be in memory already
/// (`KeptTranscripts.prefetch`).
public final class CachedTranscript: KeptTranscript {
    public static let pageSize = 200

    public let session: String
    private let cache: SessionCache
    private let shelf: KeptTranscripts?

    public init(session: String, cache: SessionCache, shelf: KeptTranscripts? = nil) {
        self.session = session
        self.cache = cache
        self.shelf = shelf
    }

    @MainActor public func ready() -> KeptPage? {
        shelf?.take(session)
    }

    public func newest() async -> KeptPage? {
        await read(before: nil)
    }

    public func page(before older: Int) async -> KeptPage? {
        await read(before: older)
    }

    private func read(before: Int?) async -> KeptPage? {
        guard let kept = try? await cache.events(session: session, before: before, limit: Self.pageSize), !kept.isEmpty else { return nil }
        return Self.page(kept)
    }

    /// A page of `kept` (newest first, as the cache reads them), oldest
    /// first. Nonisolated: never on the main thread.
    nonisolated static func page(_ kept: [CachedEvent]) -> KeptPage {
        let events = kept.reversed().compactMap { (try? decodeLogEvent(json: $0.json)).map(TranscriptEvent.init) }
        let oldest = kept.map(\.ord).min() ?? 0
        return KeptPage(events: events, older: kept.count < pageSize || oldest == 0 ? nil : oldest)
    }
}

/// Newest pages read ahead from the cache only, never the network (R-IOS-3,
/// ticket 131): when a session opens, those of the sessions above and below
/// it in the list, so switching to one draws from memory. A few at most;
/// each is taken by the screen that draws it.
@MainActor
public final class KeptTranscripts {
    private let cache: SessionCache
    private var pages: [String: KeptPage] = [:]
    private var order: [String] = []
    private var reading: Set<String> = []
    /// The most pages held.
    static let most = 4

    public init(cache: SessionCache) {
        self.cache = cache
    }

    /// Session `id`'s transcript as the cache kept it, its newest page
    /// from here if it was read ahead.
    public func transcript(_ id: String) -> CachedTranscript {
        CachedTranscript(session: id, cache: cache, shelf: self)
    }

    /// The newest pages of the sessions either side of `id` in `ids` (the
    /// list's order), read ahead from the cache.
    public func prefetch(around id: String, in ids: [String]) {
        guard let at = ids.firstIndex(of: id) else { return }
        for neighbour in [at - 1, at + 1] where ids.indices.contains(neighbour) {
            prefetch(ids[neighbour])
        }
    }

    /// Session `id`'s newest page, read ahead unless it is held or being
    /// read.
    public func prefetch(_ id: String) {
        guard pages[id] == nil, !reading.contains(id) else { return }
        reading.insert(id)
        let transcript = CachedTranscript(session: id, cache: cache)
        Task { [weak self] in
            let page = await transcript.newest()
            guard let self else { return }
            reading.remove(id)
            guard let page else { return }
            pages[id] = page
            order.removeAll { $0 == id }
            order.append(id)
            while order.count > Self.most { pages[order.removeFirst()] = nil }
        }
    }

    /// Whether session `id`'s newest page is held.
    public func holds(_ id: String) -> Bool { pages[id] != nil }

    /// Session `id`'s newest page, if held: it goes.
    func take(_ id: String) -> KeptPage? {
        order.removeAll { $0 == id }
        return pages.removeValue(forKey: id)
    }
}
