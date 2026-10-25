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
/// batch that couldn't be written (the phone locked) is held and tried
/// again with the next, so the transcript keeps no hole while the screen
/// is open; one a forgotten cache refused is dropped.
///
/// The viewer's place (`saved`, the last update of a frame that moved it)
/// is written in the same transaction as every event held up to it, so the
/// cache's cursor never names an event it does not hold. Once an event was
/// left out — one this build could not decode (the frame's `failed` says
/// so) or encode, or events held for a write that kept failing were let
/// go — no place is written again: the transcript has a hole a place after
/// it would skip.
public final class TranscriptCacheWriter: Sendable {
    public let session: String
    private let cache: SessionCache
    private let interval: Duration
    private struct Held {
        var events: [LogEvent] = []
        /// A batch that couldn't be written, encoded: first in the next.
        var unwritten: [(id: String, json: String)] = []
        /// The newest place, encoded, to write with the events before it.
        var cursor: String?
        /// An event was left out: no place is written again.
        var holed = false
        var scheduled = false
        /// The write last scheduled: the next waits for it, so batches
        /// land in order.
        var last: Task<Void, Never>?
    }
    private let held = Mutex(Held())
    /// The most events held for a write that keeps failing: the newest.
    static let mostHeld = 10_000

    public init(session: String, cache: SessionCache, interval: Duration = .seconds(1)) {
        self.session = session
        self.cache = cache
        self.interval = interval
    }

    /// The logged events in what the viewer showed, in order, and the
    /// place the frame ended at, if it moved. A frame that says `failed`
    /// left events out (ones this build cannot decode), or ends the viewer:
    /// either way no place is written from it on.
    public func add(_ updates: [ViewerUpdate]) {
        var cursor: String?
        var holed = false
        let events = updates.flatMap { update -> [LogEvent] in
            switch update {
            case .attached(let events, _), .caughtUp(let events): return events
            case .line(.log(let event)): return [event]
            case .saved(let saved):
                cursor = (try? encodeCursor(cursor: saved)) ?? cursor
                return []
            case .failed:
                holed = true
                return []
            default: return []
            }
        }
        if holed { held.withLock { $0.holed = true; $0.cursor = nil } }
        add(events: events, cursor: cursor)
    }

    /// Logged events, in order: written within `interval`, with whatever
    /// else came meanwhile. With none, a batch still held from a failed
    /// write is tried again: the viewer showing anything at all (it does
    /// on every join) is a moment to.
    public func add(events: [LogEvent], cursor: String? = nil) {
        held.withLock { held in
            held.events += events
            if let cursor, !held.holed { held.cursor = cursor }
            guard !held.events.isEmpty || !held.unwritten.isEmpty || held.cursor != nil, !held.scheduled else { return }
            held.scheduled = true
            let previous = held.last
            held.last = Task.detached { [self] in
                await previous?.value
                try? await Task.sleep(for: interval)
                let (unwritten, batch, saved) = self.held.withLock { held in
                    held.scheduled = false
                    defer {
                        held.events = []
                        held.unwritten = []
                        held.cursor = nil
                    }
                    return (held.unwritten, held.events, held.cursor)
                }
                let encoded = batch.compactMap { event in
                    (try? encodeLogEvent(event: event)).map { (id: event.id, json: $0) }
                }
                var cursor = saved
                if encoded.count < batch.count {
                    // An event left out: no place past it, now or later.
                    cursor = nil
                    self.held.withLock { $0.holed = true; $0.cursor = nil }
                }
                let rows = unwritten + encoded
                do throws(SessionCacheFailure) {
                    try await cache.appendEvents(session: session, rows, cursor: cursor)
                } catch where !error.forgotten {
                    self.held.withLock { held in
                        let kept = rows + held.unwritten
                        held.unwritten = Array(kept.suffix(Self.mostHeld))
                        if kept.count > Self.mostHeld {
                            held.holed = true
                            held.cursor = nil
                        } else if held.cursor == nil, !held.holed {
                            held.cursor = cursor
                        }
                    }
                } catch {}
            }
        }
    }

    /// Once every write scheduled so far has run: what it couldn't write
    /// is still held.
    public func written() async {
        while let last = held.withLock({ $0.last }) {
            await last.value
            if held.withLock({ $0.last == last && !$0.scheduled }) { return }
        }
    }
}
