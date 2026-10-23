import Foundation

/// The sessions followed live, and the app's lifecycle handed to each
/// (ticket 86, R-IOS-2, R-OFF-2): the scene delegate forwards its
/// `willEnterForeground`, `didBecomeActive` and `didEnterBackground`, and
/// Sessions' pull-to-refresh wakes them. A session screen adds its session
/// as it is made, before its view loads, so a foreground reaches the
/// connection before any view appears; it is let go when the screen stops
/// it. Held weakly: the registry never keeps a session alive.
@MainActor
public final class LiveSessions {
    public static let shared = LiveSessions()

    private struct Held {
        weak var session: (any LiveSession)?
        /// The screen's: what its session brought, applied now.
        var drain: (@MainActor () -> Void)?
    }

    private var held: [ObjectIdentifier: Held] = [:]

    /// The app is in the background (from `didEnterBackground` to
    /// `willEnterForeground`): a screen applies what comes at once, as its
    /// display link doesn't fire there.
    public private(set) var inBackground = false

    public init() {}

    /// `session` hears the lifecycle; `drain`, if given, is called as the
    /// app goes to the background, so what its screen holds unapplied is
    /// applied before the display link stops (a mailbox that already held
    /// something wakes nothing when more comes).
    public func add(_ session: any LiveSession, drain: (@MainActor () -> Void)? = nil) {
        held[ObjectIdentifier(session)] = Held(session: session, drain: drain)
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
