import Foundation
import Observation
import UIKit

// Starting a session from the phone (R-HOST-4, ticket 134): the hosts
// that announce themselves (R-HOST-3), one of their directories and a model
// one of their instances has ready, and the first prompt. The start carries
// nothing else: no mode, budget, toolset or effort is ever offered, and the
// host runs the session under its own settings.

/// One of a host's instances, as its announcement says: the fields of
/// krowk-ffi's generated `InstanceRecord` (R-PROTO-2), which KrowkAccount
/// conforms to this. KrowkKit does not import the bindings.
public protocol AnnouncedInstance: Sendable {
    /// The instance's name: "anthropic".
    var name: String { get }
    var ready: Bool { get }
    /// The fix line, when it is not ready.
    var fix: String? { get }
    /// Model ids, newest first; empty when not ready.
    var models: [String] { get }
    /// Its limit, as the host last saw it (R-INST-6): "allowed",
    /// "warning" or "limited"; nil when none was seen.
    var limitState: String? { get }
    /// How much of the limit is used, 0–100, when the provider says.
    var limitUsedPercent: Double? { get }
    /// When a reached limit lifts, in milliseconds since the epoch.
    var limitResetsAtMs: Int64? { get }
}

/// One host as its announcement says: the fields of krowk-ffi's generated
/// `HostRecord`. The name is the verified device list's.
public protocol AnnouncedHost: Sendable {
    /// Its device id: what a start is addressed to.
    var device: String { get }
    var name: String { get }
    var online: Bool { get }
    /// When it last refreshed its announcement, in milliseconds since the
    /// epoch; 0 when unknown.
    var lastSeenMs: UInt64 { get }
    var version: String { get }
    /// Absolute paths it starts a session in.
    var directories: [String] { get }
    var announcedInstances: [any AnnouncedInstance] { get }
}

/// What one read of the hosts found: online first, then by name, never
/// this phone (R-HAND-4); and how many announcements it couldn't open.
public struct HostListing: Sendable {
    public var hosts: [any AnnouncedHost]
    public var unreadable: Int

    public init(hosts: [any AnnouncedHost], unreadable: Int) {
        self.hosts = hosts
        self.unreadable = unreadable
    }
}

/// A start as the person asked for it: the host, the directory, the model
/// and the words, under `id`, a command id minted once per tap of Start.
/// Kept with the row it makes, so "Edit and retry" opens the flow filled
/// in.
public struct StartRequest: Sendable, Equatable {
    public var id: String
    public var device: String
    public var hostName: String
    public var cwd: String
    public var instance: String
    public var model: String
    public var prompt: String

    public init(id: String, device: String, hostName: String, cwd: String, instance: String, model: String, prompt: String) {
        self.id = id
        self.device = device
        self.hostName = hostName
        self.cwd = cwd
        self.instance = instance
        self.model = model
        self.prompt = prompt
    }
}

/// How a start came out.
public enum StartOutcome: Sendable, Equatable {
    case started(sessionID: String)
    /// Nothing started; `message` is what the row says.
    case refused(code: String, message: String)
}

/// The person's hosts, and a session started on one: KrowkAccount over
/// krowk-ffi's `Account.listHosts`, `startSession` and `newCommandId`, a
/// stub in the tests. Both calls block: never on the main thread.
public protocol HostSource: Sendable {
    /// One registry call.
    func hosts() throws -> HostListing
    /// Sends the start and waits for the host's answer, up to 30 s: a
    /// failure of any kind comes back `refused`, in words for the row.
    func start(_ request: StartRequest) -> StartOutcome
    /// A fresh command id, a random UUID.
    func newCommandID() -> String
}

// MARK: The host list

/// The host step's state: the latest listing, read off the main thread,
/// every 10 s while on screen and on a pull. A read that fails keeps the
/// rows it had, shows none of them online, and says so.
@MainActor
@Observable
public final class HostListModel {
    /// The latest listing read; nil until one has been.
    public private(set) var listing: HostListing?
    /// The last read failed.
    public private(set) var failed = false
    /// What "last seen" is measured from: set at each read.
    public private(set) var now: Date

    public static let refreshPeriod: Duration = .seconds(10)

    private let source: any HostSource
    private let clock: @Sendable () -> Date
    private var timer: Task<Void, Never>?
    private var reading: Task<Void, Never>?

    public init(source: any HostSource, clock: @escaping @Sendable () -> Date = { Date() }) {
        self.source = source
        self.clock = clock
        now = clock()
    }

    /// The hosts to show: as listed, none online when the last read
    /// failed.
    public var rows: [HostRow] {
        let formatter = HostRowFormatter()
        return (listing?.hosts ?? []).map { formatter.row($0, trusted: !failed, now: now) }
    }

    /// The host `device` as last listed.
    public func host(_ device: String) -> (any AnnouncedHost)? {
        listing?.hosts.first { $0.device == device }
    }

    /// On screen: a read now, then every 10 s.
    public func appeared() {
        guard timer == nil else { return }
        Task { await refresh() }
        timer = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.refreshPeriod)
                guard !Task.isCancelled, let self else { return }
                await refresh()
            }
        }
    }

    public func disappeared() {
        timer?.cancel()
        timer = nil
    }

    /// Reads the hosts again, off the main thread; returns once a read has
    /// ended (the one under way, if any). Never throws.
    public func refresh() async {
        if let reading {
            await reading.value
            return
        }
        let source = source
        let task = Task {
            let result = await blocking { () -> HostListing? in
                dispatchPrecondition(condition: .notOnQueue(.main))
                return try? source.hosts()
            }
            now = clock()
            if let result {
                listing = result
                failed = false
            } else {
                failed = true
            }
        }
        reading = task
        await task.value
        reading = nil
    }
}

/// A host as the host step shows it.
public struct HostRow: Hashable, Sendable {
    public let device: String
    public let name: String
    /// `krowk <version>` online; `last seen 12m ago` offline.
    public let detail: String
    /// Only an online host can be chosen (R-IOS-7).
    public let choosable: Bool
}

/// Makes host rows: online with its version, else greyed with when it was
/// last seen, the session list's relative time.
@MainActor
public final class HostRowFormatter {
    private let relative: RelativeDateTimeFormatter

    public init(locale: Locale = .autoupdatingCurrent) {
        relative = RelativeDateTimeFormatter()
        relative.locale = locale
        relative.unitsStyle = .abbreviated
        relative.dateTimeStyle = .numeric
    }

    /// - Parameter trusted: whether the listing is the latest read's: a
    ///   host is shown online only then.
    public func row(_ host: some AnnouncedHost, trusted: Bool, now: Date) -> HostRow {
        let name = SafeText.clean(host.name)
        if host.online && trusted {
            return HostRow(device: host.device, name: name, detail: "krowk \(SafeText.clean(host.version))", choosable: true)
        }
        return HostRow(device: host.device, name: name, detail: "last seen \(lastSeen(host.lastSeenMs, now: now))", choosable: false)
    }

    private func lastSeen(_ ms: UInt64, now: Date) -> String {
        guard ms > 0 else { return "a while ago" }
        let seen = Date(timeIntervalSince1970: TimeInterval(ms) / 1000)
        guard now.timeIntervalSince(seen) >= 60 else { return "just now" }
        return relative.localizedString(for: seen, relativeTo: now)
    }
}

/// How the flow writes what a host announced.
public enum NewSessionText {
    /// A path on another machine with its home shown as `~`: `/home/<name>`,
    /// `/Users/<name>` or `/root`, the homes krowk's platforms make.
    public static func homeAsTilde(_ path: String) -> String {
        for prefix in ["/home/", "/Users/"] where path.hasPrefix(prefix) {
            let rest = path.dropFirst(prefix.count)
            guard let slash = rest.firstIndex(of: "/") else { return rest.isEmpty ? path : "~" }
            return rest[..<slash].isEmpty ? path : "~" + rest[slash...]
        }
        if path == "/root" { return "~" }
        if path.hasPrefix("/root/") { return "~" + path.dropFirst("/root".count) }
        return path
    }

    /// An instance's limit (R-INST-6): `allowed`, `warning: 82% used`,
    /// `limited until 14:05`; nil when none was seen.
    public static func limit(_ instance: some AnnouncedInstance, timeZone: TimeZone = .autoupdatingCurrent, locale: Locale = .autoupdatingCurrent) -> String? {
        switch instance.limitState {
        case nil: return nil
        case "allowed": return "allowed"
        case "warning":
            guard let used = instance.limitUsedPercent else { return "warning" }
            return "warning: \(Int(used.rounded()))% used"
        case "limited":
            guard let resets = instance.limitResetsAtMs else { return "limited" }
            let style = Date.FormatStyle(date: .omitted, time: .shortened, locale: locale, timeZone: timeZone)
            return "limited until \(Date(timeIntervalSince1970: TimeInterval(resets) / 1000).formatted(style))"
        case let other?: return SafeText.clean(other)
        }
    }
}

// MARK: The optimistic rows

/// What Sessions needs to start a session on a host: the hosts, the starts
/// under way, and how a started session's transcript opens.
@MainActor
public struct NewSessions {
    public let source: any HostSource
    public let starting: StartingSessions
    /// The started session's screen, followed live: its id and the start
    /// that made it. Opening retries while the registry doesn't list it
    /// yet: automatic sync takes it up at its first turn.
    public let open: @MainActor (_ sessionID: String, _ request: StartRequest) -> UIViewController

    public init(source: any HostSource, starting: StartingSessions, open: @escaping @MainActor (_ sessionID: String, _ request: StartRequest) -> UIViewController) {
        self.source = source
        self.starting = starting
        self.open = open
    }
}

/// A start under way, or refused, as a row at the top of Sessions
/// (R-LAG-8): keyed by its command id.
public struct StartingRow: Hashable, Sendable {
    public enum State: Hashable, Sendable {
        case starting
        /// The host's words, or why it couldn't be asked.
        case refused(String)
    }

    public let request: StartRequest
    public var state: State

    /// The row's id in the list: never a session's.
    public var id: String { "start:\(request.id)" }

    public static func == (a: StartingRow, b: StartingRow) -> Bool { a.request == b.request && a.state == b.state }
    public func hash(into h: inout Hasher) {
        h.combine(request.id)
        h.combine(state)
    }
}

/// The starts this phone has asked for: each shows at once, runs in a
/// detached task off the main thread, and is replaced by its session once
/// the host answers `started`, or by the host's words once it refuses.
@MainActor
@Observable
public final class StartingSessions {
    /// Newest first.
    public private(set) var rows: [StartingRow] = []

    private let source: any HostSource
    /// Told of each start the host answered, with its session's id.
    @ObservationIgnored public var started: (@MainActor (_ sessionID: String, _ request: StartRequest) -> Void)?

    public init(source: any HostSource) {
        self.source = source
    }

    /// A fresh command id for the next start.
    public func newCommandID() -> String {
        source.newCommandID()
    }

    /// Shows `request` as starting and sends it.
    public func start(_ request: StartRequest) {
        rows.removeAll { $0.request.id == request.id }
        rows.insert(StartingRow(request: request, state: .starting), at: 0)
        let source = source
        Task.detached { [weak self] in
            let outcome = await blocking { source.start(request) }
            await self?.answered(request, outcome)
        }
    }

    /// The refused start `id`, taken off the list to be edited and sent
    /// again as a new command.
    public func takeForRetry(_ id: String) -> StartRequest? {
        guard let i = rows.firstIndex(where: { $0.id == id }) else { return nil }
        return rows.remove(at: i).request
    }

    private func answered(_ request: StartRequest, _ outcome: StartOutcome) {
        guard let i = rows.firstIndex(where: { $0.request.id == request.id }) else { return }
        switch outcome {
        case .started(let session):
            rows.remove(at: i)
            started?(session, request)
        case .refused(_, let message):
            rows[i].state = .refused(SafeText.clean(message))
        }
    }
}
