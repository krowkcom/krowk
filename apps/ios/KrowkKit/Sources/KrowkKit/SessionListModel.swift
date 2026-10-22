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
    /// When it was read, by `clock`.
    public private(set) var listedAt: Date?
    /// Why the last refresh failed; nil once one succeeds.
    public private(set) var failure: SessionListFailure?
    /// What the rows' relative times are measured from: set at each
    /// refresh and each minute's tick.
    public private(set) var now: Date

    /// How often the list is read again while on screen: the lease's
    /// renewal period.
    public static let refreshPeriod: Duration = .seconds(20)
    /// A lease's TTL: no listing older than this shows a session live.
    public static let leaseTTL: TimeInterval = 60

    private let source: any SessionSource
    private let clock: @Sendable () -> Date
    private let log: @MainActor (String) -> Void

    /// Refreshes asked for and completed, so a refresh asked for while one
    /// runs gets one that starts after it was asked for.
    private var asked = 0
    private var done = 0
    private var reading: Task<Void, Never>?
    private var timers: [Task<Void, Never>] = []

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

    /// On screen: read now, then every 20 s, and tick the clock each minute.
    public func appeared() {
        guard timers.isEmpty else { return }
        now = clock()
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
    }

    /// Reads the list again, off the main thread, and returns once a read
    /// that started after this call has ended. Never throws: a failure is
    /// kept in `failure`, and the listing before it stays, shown as not
    /// live.
    public func refresh() async {
        asked += 1
        let ticket = asked
        if reading == nil {
            reading = Task { await read() }
        }
        while done < ticket, let reading {
            await reading.value
        }
    }

    /// Reads until no refresh asked for is left unread.
    private func read() async {
        while done < asked {
            let target = asked
            let source = source
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
                listedAt = now
                failure = nil
            case .failure(let error):
                failure = error
                log("listing sessions failed: \(error.kind)")
            }
            done = target
        }
        reading = nil
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
