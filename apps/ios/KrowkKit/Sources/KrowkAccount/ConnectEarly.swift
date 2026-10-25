import Foundation
import Krowk
import KrowkCache
import KrowkKit

/// Connect early (R-IOS-2, ticket 132): at launch and on every return to
/// the foreground, before any view appears, the session list's read starts
/// (the kept listing first, R-IOS-1) and so does the connection of the
/// session last open (`SessionCache.lastOpen`), read on from the cache's
/// place (ticket 131) through the relay. The connection is held by
/// `LiveSessions` until the session's screen takes it (`take`), so it is
/// never joined twice. Racing the relay against the host's direct paths
/// is the viewer's once welcomed (ticket 133).
///
/// The scene delegate makes one per session list; the hosted
/// `ConnectEarlyTests` drive it as the app does.
@MainActor
public final class ConnectEarly {
    private let cache: SessionCache
    private let list: SessionListModel
    private let lifecycle: LiveSessions
    private let connect: @MainActor (_ id: String, _ known: HeadRecord?) -> LiveSessions.Early

    /// `connect` makes session `id`'s connection, unstarted, as its screen
    /// would (`known`, the head its listing named), with what its screen
    /// takes with it.
    public init(
        cache: SessionCache, list: SessionListModel, lifecycle: LiveSessions = .shared,
        connect: @escaping @MainActor (_ id: String, _ known: HeadRecord?) -> LiveSessions.Early
    ) {
        self.cache = cache
        self.list = list
        self.lifecycle = lifecycle
        self.connect = connect
    }

    /// Before any view appears: the list's read, unless one runs, and the
    /// last open session's connection, unless it is followed already (its
    /// screen still open, or started at launch and the foreground that
    /// follows). Waits on nothing but one row of the cache, read on its
    /// queue; none when the phone is locked.
    public func start() {
        list.readEarly()
        guard let last = cache.lastOpenNow(), !lifecycle.follows(last.id) else { return }
        lifecycle.startEarly(connect(last.id, listedHead(last)))
    }

    /// The connection started early for session `id`, if one was: its
    /// screen follows it. Another session's is stopped.
    public func take(_ id: String) -> LiveSessions.Early? {
        lifecycle.takeEarly(id)
    }

    /// Session `id`'s screen appeared (`true`) or was left: kept as the
    /// session last open, or not, off the main thread. A cache: what fails
    /// is left.
    public func onScreen(_ id: String, _ shown: Bool) {
        let cache = cache
        Task {
            if shown { try? await cache.putLastOpen(id) } else { try? await cache.clearLastOpen(id) }
        }
    }
}
