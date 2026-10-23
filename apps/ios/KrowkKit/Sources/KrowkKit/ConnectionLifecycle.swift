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
    }

    private var held: [ObjectIdentifier: Held] = [:]

    public init() {}

    public func add(_ session: any LiveSession) {
        held[ObjectIdentifier(session)] = Held(session: session)
    }

    public func remove(_ session: any LiveSession) {
        held[ObjectIdentifier(session)] = nil
    }

    /// The sessions still held, the gone ones forgotten.
    private var sessions: [any LiveSession] {
        held = held.filter { $0.value.session != nil }
        return held.values.compactMap(\.session)
    }

    /// The scene is coming forward: before any view appears (R-IOS-2).
    public func willEnterForeground() {
        for s in sessions { s.foreground() }
    }

    /// The scene became active.
    public func didBecomeActive() {
        for s in sessions { s.wake() }
    }

    /// The scene went to the background.
    public func didEnterBackground() {
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
