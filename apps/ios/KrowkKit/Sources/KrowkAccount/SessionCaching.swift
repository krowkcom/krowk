import Foundation
import Krowk
import KrowkCache
import Synchronization

extension CachedSession {
    /// The generated record as the cache stores it (R-PROTO-2): its own
    /// fields, and when its listing was read.
    public init(_ summary: SessionSummary, listedAtMs: UInt64) {
        self.init(
            id: summary.id, title: summary.title, cwd: summary.cwd, updatedMs: summary.updatedMs,
            headIndex: summary.head?.index, headDigest: summary.head?.digest,
            hostedBy: summary.hostedBy, hostDevice: summary.hostDevice, listedAtMs: listedAtMs)
    }
}

extension SessionCache {
    /// A listing written as the cache's index (R-IOS-3), then the
    /// transcripts of all but the 20 newest sessions dropped. A cache:
    /// what fails is left to the next listing.
    func record(_ sessions: [SessionSummary], listedAtMs: UInt64) async {
        try? putIndex(sessions.map { CachedSession($0, listedAtMs: listedAtMs) }, listedAtMs: listedAtMs)
        try? evict(keep: SessionCache.transcripts)
    }
}

/// One session screen's logged events written to the cache as they come
/// (R-IOS-1): what the session opened on (`attached`), each catch-up, and
/// each logged line the connection applies, never a live delta. Batched,
/// at most one write a second, off the main thread and the connection's
/// queue; each event as krowk-ffi's `encodeLogEvent` writes it, so Swift
/// never builds its JSON (R-PROTO-2). Under the screen's session, whatever
/// session an event names (a subagent's lines are its transcript too). A
/// cache: a write that fails (the phone locked) is dropped, and the
/// network has it.
public final class TranscriptCacheWriter: Sendable {
    public let session: String
    private let cache: SessionCache
    private let interval: Duration
    private struct Held {
        var events: [LogEvent] = []
        var scheduled = false
        /// The write last scheduled: the next waits for it, so batches
        /// land in order.
        var last: Task<Void, Never>?
    }
    private let held = Mutex(Held())

    public init(session: String, cache: SessionCache, interval: Duration = .seconds(1)) {
        self.session = session
        self.cache = cache
        self.interval = interval
    }

    /// The logged events in what the viewer showed, in order.
    public func add(_ updates: [ViewerUpdate]) {
        add(events: updates.flatMap { update -> [LogEvent] in
            switch update {
            case .attached(let events, _), .caughtUp(let events): events
            case .line(.log(let event)): [event]
            default: []
            }
        })
    }

    /// Logged events, in order: written within `interval`, with whatever
    /// else came meanwhile.
    public func add(events: [LogEvent]) {
        guard !events.isEmpty else { return }
        held.withLock { held in
            held.events += events
            guard !held.scheduled else { return }
            held.scheduled = true
            let previous = held.last
            held.last = Task.detached { [self] in
                await previous?.value
                try? await Task.sleep(for: interval)
                let batch = self.held.withLock { held in
                    held.scheduled = false
                    defer { held.events = [] }
                    return held.events
                }
                let rows = batch.compactMap { event in (try? encodeLogEvent(event: event)).map { (id: event.id, json: $0) } }
                try? await cache.appendEvents(session: session, rows)
            }
        }
    }

    /// Once every event added so far is written (or dropped).
    public func written() async {
        while let last = held.withLock({ $0.last }) {
            await last.value
            if held.withLock({ $0.last == last && !$0.scheduled }) { return }
        }
    }
}
