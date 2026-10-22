import Foundation

/// One synced session as the list reads it: the fields of krowk-ffi's
/// generated `SessionSummary` (R-PROTO-2), which KrowkAccount conforms to
/// this. KrowkKit does not import the bindings, so it reads the record
/// through these, and keeps no copy or `Codable` mirror of the index.
public protocol SessionRecord: Sendable {
    var id: String { get }
    /// Empty when the session has none yet.
    var title: String { get }
    var cwd: String { get }
    /// When its host last wrote it, in milliseconds since the epoch.
    var updatedMs: UInt64 { get }
    /// The device id holding the session's lease, hosting it now, when one
    /// does: the registry names it only while the lease is unexpired.
    var hostedBy: String? { get }
}

/// What one listing read: the sessions this phone can open, newest first,
/// and how many it can't.
public struct SessionListing: Sendable {
    public var sessions: [any SessionRecord]
    /// Sessions sealed under a user key this phone doesn't hold, or signed
    /// by a device not on its verified list: counted, never shown.
    public var unreadable: Int
    /// The names on the verified device list, by device id, for "live on
    /// <name>".
    public var deviceNames: [String: String]

    public init(sessions: [any SessionRecord], unreadable: Int, deviceNames: [String: String] = [:]) {
        self.sessions = sessions
        self.unreadable = unreadable
        self.deviceNames = deviceNames
    }

    /// One per id, the first kept (the diffable data source refuses a
    /// duplicate identifier), most recently written first.
    func deduplicated() -> SessionListing {
        var seen = Set<String>()
        let unique = sessions.filter { seen.insert($0.id).inserted }
        let ordered = unique.enumerated().sorted { a, b in
            a.element.updatedMs != b.element.updatedMs ? a.element.updatedMs > b.element.updatedMs : a.offset < b.offset
        }.map(\.element)
        return SessionListing(sessions: ordered, unreadable: unreadable, deviceNames: deviceNames)
    }
}

/// Lists the person's synced sessions (R-IOS-3): KrowkAccount over
/// krowk-ffi's `Account.listSessions`, a stub in the tests.
public protocol SessionSource: Sendable {
    /// Blocking: one registry call per page and one per session. Never on
    /// the main thread.
    func list() throws(SessionListFailure) -> SessionListing
}

/// Why a listing failed, as the screen tells it.
public enum SessionListFailure: Error, Sendable, Equatable {
    /// The registry couldn't be reached.
    case network(detail: String)
    /// `not_paired`: the phone holds no user keys.
    case notPaired
    /// Anything else; the core's words, never shown.
    case other(detail: String)

    /// One sentence for the screen.
    public var sentence: String {
        switch self {
        case .network: "krowk's registry couldn't be reached."
        case .notPaired: "This iPhone isn't on your devices yet."
        case .other: "Your sessions couldn't be listed."
        }
    }

    /// What the log may say: the kind, never the core's words, which can
    /// name a session.
    public var kind: String {
        switch self {
        case .network: "network"
        case .notPaired: "not_paired"
        case .other: "other"
        }
    }
}

/// A row as the list shows it.
public struct SessionRow: Hashable, Sendable {
    public let id: String
    /// The index's title, or the cwd's last component when it has none.
    public let title: String
    /// The cwd's last component, then `● live` (on <name>) or `last seen
    /// 12m ago`.
    public let detail: String
    public let live: Bool
    /// What VoiceOver reads: the title, the place and the status in
    /// words ("live on host-a", "last seen 12 minutes ago").
    public let accessibilityLabel: String
}

/// Makes rows (R-IOS-7): `live` only for a session whose lease a host
/// holds, and only while the listing that said so is the latest; anything
/// else is "last seen", from the index's `updatedMs`.
@MainActor
public final class SessionRowFormatter {
    private let relative: RelativeDateTimeFormatter
    /// The same in whole words, for VoiceOver ("12m" may be read "metres").
    private let spoken: RelativeDateTimeFormatter

    public init(locale: Locale = .autoupdatingCurrent) {
        relative = RelativeDateTimeFormatter()
        relative.locale = locale
        relative.unitsStyle = .abbreviated
        relative.dateTimeStyle = .numeric
        spoken = RelativeDateTimeFormatter()
        spoken.locale = locale
        spoken.unitsStyle = .full
        spoken.dateTimeStyle = .numeric
    }

    /// - Parameters:
    ///   - holderName: the holder's name from the verified device list;
    ///     nil or empty shows no name, and a raw device id never shows.
    ///   - liveTrusted: whether the listing is the latest refresh's: a live
    ///     mark from anything older is not shown.
    public func row(_ session: some SessionRecord, holderName: String?, liveTrusted: Bool, now: Date) -> SessionRow {
        let place = Self.basename(session.cwd)
        let title = session.title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            ? (place.isEmpty ? "Untitled session" : place)
            : session.title
        let live = liveTrusted && session.hostedBy != nil
        let status: String
        let said: String
        if live {
            let on = holderName.map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }.flatMap { $0.isEmpty ? nil : " on \($0)" } ?? ""
            status = "● live\(on)"
            said = "live\(on)"
        } else {
            status = "last seen \(lastSeen(session.updatedMs, now: now, in: relative))"
            said = "last seen \(lastSeen(session.updatedMs, now: now, in: spoken))"
        }
        return SessionRow(
            id: session.id, title: title, detail: place.isEmpty ? status : "\(place) · \(status)", live: live,
            accessibilityLabel: [title, place, said].filter { !$0.isEmpty }.joined(separator: ", "))
    }

    /// "12m ago", "3h ago"; "just now" under a minute, and for a time
    /// ahead of this phone's clock.
    private func lastSeen(_ updatedMs: UInt64, now: Date, in formatter: RelativeDateTimeFormatter) -> String {
        let updated = Date(timeIntervalSince1970: TimeInterval(updatedMs) / 1000)
        guard now.timeIntervalSince(updated) >= 60 else { return "just now" }
        return formatter.localizedString(for: updated, relativeTo: now)
    }

    /// The last path component of `cwd`, or "" for none.
    static func basename(_ cwd: String) -> String {
        cwd.split(separator: "/", omittingEmptySubsequences: true).last.map(String.init) ?? ""
    }
}

/// Coalesces rebuilds of a screen's state to at most one per `delay`
/// (10 a second at the default 100 ms): the first trigger schedules a
/// rebuild `delay` later and triggers while one is pending are dropped;
/// the rebuild reads the state and applies it only when it differs from
/// the last applied. Unthrottled rebuilds keep the main thread busy.
@MainActor
final class SnapshotThrottle<State: Equatable> {
    /// Runs the closure `delay` from now, on the main actor.
    typealias Schedule = @MainActor (_ delay: Duration, _ fire: @escaping @MainActor () -> Void) -> Void

    /// The real clock's.
    static var afterSleeping: Schedule {
        { delay, fire in
            Task { @MainActor in
                try? await Task.sleep(for: delay)
                fire()
            }
        }
    }

    private let delay: Duration
    private let schedule: Schedule
    private let read: @MainActor () -> State
    private let apply: @MainActor (_ previous: State?, _ new: State) -> Void
    private var pending = false
    /// The state last applied.
    private(set) var current: State?

    init(
        delay: Duration = .milliseconds(100), schedule: @escaping Schedule = SnapshotThrottle.afterSleeping,
        read: @escaping @MainActor () -> State, apply: @escaping @MainActor (State?, State) -> Void
    ) {
        self.delay = delay
        self.schedule = schedule
        self.read = read
        self.apply = apply
    }

    func trigger() {
        guard !pending else { return }
        pending = true
        schedule(delay) { [weak self] in self?.rebuild() }
    }

    /// Reads and applies now, outside the throttle: the first paint.
    func rebuild() {
        pending = false
        let new = read()
        guard new != current else { return }
        let previous = current
        current = new
        apply(previous, new)
    }
}
