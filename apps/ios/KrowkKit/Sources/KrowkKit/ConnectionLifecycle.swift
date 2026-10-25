import Foundation

/// The sessions followed live, and the app's lifecycle handed to each
/// (ticket 86, R-IOS-2, R-OFF-2): the scene delegate forwards its
/// `willEnterForeground`, `didBecomeActive` and `didEnterBackground`, and
/// Sessions' pull-to-refresh wakes them. A session screen adds its session
/// as it is made, before its view loads, so a foreground reaches the
/// connection before any view appears; it is let go when the screen stops
/// it. Held weakly: the registry never keeps a session alive.
///
/// Connect early (ticket 132, R-IOS-2): at launch and on every return to
/// the foreground, before any view appears, the app starts the connection
/// of the session last open (`startEarly`). That one is held strongly, as
/// no screen holds it yet, until the session's screen is made and takes it
/// (`takeEarly`), so it is never joined twice; one whose screen doesn't
/// come within `earlyFor` (the person opened nothing, or another session)
/// is stopped, so nothing piles up in its mailbox unread.
@MainActor
public final class LiveSessions {
    public static let shared = LiveSessions()

    private struct Held {
        weak var session: (any LiveSession)?
        /// The session's id, when the one adding it said.
        var id: String?
        /// The screen's: what its session brought, applied now.
        var drain: (@MainActor () -> Void)?
    }

    private var held: [ObjectIdentifier: Held] = [:]

    /// A connection started before its screen (`startEarly`), and what
    /// the screen takes with it.
    public struct Early {
        public let id: String
        public let session: any LiveSession
        /// The approvals it hands the screen's sheet (ticket 91).
        public let approvals: ApprovalQueue?

        public init(id: String, session: any LiveSession, approvals: ApprovalQueue? = nil) {
            self.id = id
            self.session = session
            self.approvals = approvals
        }
    }

    /// The connection started early, until a screen takes it.
    public private(set) var early: Early?

    /// The app is in the background (from `didEnterBackground` to
    /// `willEnterForeground`): a screen applies what comes at once, as its
    /// display link doesn't fire there.
    public private(set) var inBackground = false

    /// How long a connection started early waits for its screen.
    let earlyFor: Duration

    public init(earlyFor: Duration = .seconds(120)) {
        self.earlyFor = earlyFor
    }

    /// `session` hears the lifecycle; `drain`, if given, is called as the
    /// app goes to the background, so what its screen holds unapplied is
    /// applied before the display link stops (a mailbox that already held
    /// something wakes nothing when more comes).
    public func add(_ session: any LiveSession, id: String? = nil, drain: (@MainActor () -> Void)? = nil) {
        held[ObjectIdentifier(session)] = Held(session: session, id: id, drain: drain)
    }

    /// Whether session `id` is followed now: a screen's, or started early.
    public func follows(_ id: String) -> Bool {
        held.values.contains { $0.session != nil && $0.id == id }
    }

    /// `early`'s connection, started now (`follow`, its status heard by no
    /// one until its screen takes it) and held until then, hearing the
    /// lifecycle meanwhile. One already followed for the session is kept
    /// and `early` dropped; an earlier early one for another session is
    /// stopped. Whether it started.
    @discardableResult
    public func startEarly(_ early: Early) -> Bool {
        guard !follows(early.id) else { return false }
        if let previous = self.early { stopEarly(previous) }
        self.early = early
        add(early.session, id: early.id)
        early.session.follow { _ in }
        let session = early.session
        Task { [weak self, earlyFor] in
            try? await Task.sleep(for: earlyFor)
            guard let self, let still = self.early, still.session === session else { return }
            self.early = nil
            stopEarly(still)
        }
        return true
    }

    /// The connection started early for session `id`, for its screen to
    /// follow: given once. Asked for another session, the early one is
    /// stopped (its screen won't come) and nil is returned.
    public func takeEarly(_ id: String) -> Early? {
        guard let early else { return nil }
        self.early = nil
        guard early.id == id else {
            stopEarly(early)
            return nil
        }
        return early
    }

    private func stopEarly(_ early: Early) {
        remove(early.session)
        early.session.stop()
    }

    public func remove(_ session: any LiveSession) {
        held[ObjectIdentifier(session)] = nil
    }

    /// The sessions still held, the gone ones forgotten.
    private var sessions: [any LiveSession] {
        held = held.filter { $0.value.session != nil }
        return held.values.compactMap(\.session)
    }

    /// The screens' drains, for the sessions still held.
    private var drains: [@MainActor () -> Void] {
        held = held.filter { $0.value.session != nil }
        return held.values.compactMap(\.drain)
    }

    /// The scene is coming forward: before any view appears (R-IOS-2).
    public func willEnterForeground() {
        inBackground = false
        for s in sessions { s.foreground() }
    }

    /// The scene became active.
    public func didBecomeActive() {
        for s in sessions { s.wake() }
    }

    /// The scene went to the background.
    public func didEnterBackground() {
        inBackground = true
        for drain in drains { drain() }
        for s in sessions { s.background() }
    }

    /// The person pulled to refresh.
    public func wake() {
        for s in sessions { s.wake() }
    }

    /// The phone is no longer on the person's devices (ticket 117): every
    /// session followed live stops — its socket closed, no join or retry
    /// after — and is let go.
    public func stopAll() {
        for s in sessions { s.stop() }
        held = [:]
        early = nil
    }
}

/// `NWPathMonitor`'s callbacks as changes: its first is the path as it is
/// when the monitor starts, not a change, so it wakes nothing.
public struct PathChanges: Sendable {
    private var seen = false

    public init() {}

    /// Whether this callback is a change to act on.
    public mutating func changed() -> Bool {
        defer { seen = true }
        return seen
    }
}
