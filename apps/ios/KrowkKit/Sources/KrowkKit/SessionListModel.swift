import Foundation
import Observation

/// The session list's state (R-IOS-3, R-IOS-7): the latest listing, read
/// off the main thread, refreshed every 20 s while the list is on screen
/// and on every return to the foreground, and the clock the rows' "last
/// seen" reads, ticked each minute.
///
/// "Live" is the lease and nothing more: the registry names a holder only
/// while its lease is unexpired, and a host renews it every 20 s on a 60 s
/// TTL. A listing older than the last refresh (one that failed), or older
/// than a lease's TTL (the app was away), shows no session live.
@MainActor
@Observable
public final class SessionListModel {
    /// The latest listing that succeeded; nil until one has.
    public private(set) var listing: SessionListing?
    /// When the read that gave it started, by `clock`: a lease it shows
    /// was unexpired no later than then.
    public private(set) var listedAt: Date?
    /// Why the last refresh failed; nil once one succeeds.
    public private(set) var failure: SessionListFailure?
    /// The phone is no longer on the person's devices: set by the read
    /// that found it, after which nothing reads or ticks again.
    public private(set) var gone: DeviceGone?
    /// What the rows' relative times are measured from: set at each
    /// refresh and each minute's tick.
    public private(set) var now: Date

    /// How long after a timed read the next starts while on screen: the
    /// lease's renewal period (so reads come every 20 s plus a read's own
    /// time, well inside the lease's 60 s TTL).
    public static let refreshPeriod: Duration = .seconds(20)
    /// A lease's TTL: no listing older than this shows a session live.
    public static let leaseTTL: TimeInterval = 60

    /// Called on each read that succeeded, after its listing is kept: the
    /// screen's foreground budget (ticket 128). Not observed.
    @ObservationIgnored public var listed: (@MainActor () -> Void)?

    private let source: any SessionSource
    private let clock: @Sendable () -> Date
    private let log: @MainActor (String) -> Void

    /// Refreshes asked for and completed, so a refresh asked for while one
    /// runs gets one that starts after it was asked for.
    private var asked = 0
    private var done = 0
    private var reading: Task<Void, Never>?
    private var timers: [Task<Void, Never>] = []
    /// Ticks the clock once the listing's live marks are a lease's TTL
    /// old, so they go even when reads hang.
    private var expiry: Task<Void, Never>?

    /// - Parameters:
    ///   - log: told the kind of a failure, never a session's id, title or
    ///     the core's words.
    public init(source: any SessionSource, clock: @escaping @Sendable () -> Date = { Date() }, log: @escaping @MainActor (String) -> Void = { _ in }) {
        self.source = source
        self.clock = clock
        self.log = log
        self.now = clock()
    }

    /// Whether a live mark may be shown now: the listing is the latest
    /// refresh's, and younger than a lease's TTL.
    public var liveTrusted: Bool {
        guard let listedAt, failure == nil else { return false }
        return now.timeIntervalSince(listedAt) < Self.leaseTTL
    }

    /// On screen, or back in the foreground: the clock ticked now (so a
    /// listing older than a lease's TTL shows none live) and again when
    /// its live marks reach that age, a read now, then every 20 s, and the
    /// clock ticked each minute. Called again while already on screen (a
    /// launch's first foreground follows the first appearance), it only
    /// ticks the clock: the read already under way is enough.
    public func appeared() {
        guard gone == nil else { return }
        now = clock()
        if let listedAt { expire(at: listedAt.addingTimeInterval(Self.leaseTTL)) }
        guard timers.isEmpty else { return }
        timers = [
            repeating(every: Self.refreshPeriod) { await $0.refresh() },
            repeating(every: .seconds(60)) { $0.now = $0.clock() },
        ]
        Task { await refresh() }
    }

    /// Off screen, or the app in the background: nothing runs.
    public func disappeared() {
        timers.forEach { $0.cancel() }
        timers = []
        expiry?.cancel()
        expiry = nil
    }

    /// Reads the list again, off the main thread, and returns once a read
    /// that started after this call has ended. Never throws: a failure is
    /// kept in `failure`, and the listing before it stays, shown as not
    /// live.
    public func refresh() async {
        guard gone == nil else { return }
        asked += 1
        let ticket = asked
        if reading == nil {
            reading = Task { await read() }
        }
        while done < ticket, let reading {
            await reading.value
        }
    }

    /// Returns once a listing is known, or a read of its own failed: at
    /// once if one is, after the read under way if one runs (and one more
    /// if that one failed), else after a read of its own.
    /// A route from a notification waits on this before it looks for its
    /// session (ticket 106); one that fails is tried once more.
    public func settled() async {
        guard listing == nil else { return }
        if let reading {
            await reading.value
        }
        // Never read, or the last read failed (the registry briefly out of
        // reach as the app launched): one more try.
        if listing == nil { await refresh() }
    }

    /// Reads until no refresh asked for is left unread.
    private func read() async {
        while done < asked {
            let target = asked
            let source = source
            let started = clock()
            let result = await blocking { () -> Result<SessionListing, SessionListFailure> in
                // The listing blocks for a round trip per session.
                dispatchPrecondition(condition: .notOnQueue(.main))
                do throws(SessionListFailure) {
                    return .success(try source.list())
                } catch {
                    return .failure(error)
                }
            }
            now = clock()
            switch result {
            case .success(let listing):
                self.listing = listing.deduplicated()
                listedAt = started
                failure = nil
                // Only on screen: appearing arms it again.
                if !timers.isEmpty { expire(at: started.addingTimeInterval(Self.leaseTTL)) }
                listed?()
            case .failure(.gone(let found)):
                // Stops here: no timer, no retry. The app shows why.
                log("listing sessions failed: \(found.kind)")
                failure = .gone(found)
                gone = found
                disappeared()
                done = asked
                reading = nil
                return
            case .failure(let error):
                // Once per kind, not every 20 s while it lasts.
                if failure?.kind != error.kind { log("listing sessions failed: \(error.kind)") }
                failure = error
            }
            done = target
        }
        reading = nil
    }

    private func expire(at deadline: Date) {
        expiry?.cancel()
        let wait = max(0, deadline.timeIntervalSince(now))
        expiry = Task { [weak self] in
            try? await Task.sleep(for: .seconds(wait))
            guard !Task.isCancelled, let self else { return }
            now = clock()
        }
    }

    private func repeating(every period: Duration, _ body: @escaping @MainActor (SessionListModel) async -> Void) -> Task<Void, Never> {
        Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: period)
                guard !Task.isCancelled, let self else { return }
                await body(self)
            }
        }
    }
}
