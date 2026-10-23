import Foundation
import UserNotifications

/// Where a tapped notification leads (ticket 106, R-IOS-12): a session's
/// transcript, or that session with its waiting approval. Read from the
/// alert's `userInfo["krowk"]`, which the registry fills with the
/// session's id and the kind of event and nothing else (`Apns.alert`):
/// `["session": "<uuid>", "kind": "approval" | "done"]`.
///
/// The payload is untrusted input, like everything else that reaches the
/// phone: whoever can send this app a push (the registry, or anyone holding
/// its APNs key) chooses it. So it is held to a session id shaped as a
/// UUID and one of two kinds, anything else routes nowhere, and a route
/// grants nothing: it only picks which of the person's listed sessions to
/// open, and a sheet rises only for a request the session's host itself
/// sends over the relay (its welcome, or live).
public enum NotificationRoute: Equatable, Sendable {
    /// "A turn finished": the session's transcript, followed live.
    case session(String)
    /// "A session needs your approval": the session, and the sheet for its
    /// oldest waiting request once its host's welcome has come.
    case approval(String)

    /// The session it opens: a UUID, lower case.
    public var sessionID: String {
        switch self {
        case .session(let id), .approval(let id): id
        }
    }

    /// The route a notification's `userInfo` names, or nil: no `krowk`
    /// dictionary, a session that isn't a UUID, or a kind other than
    /// `approval` and `done`. Nil opens the app as it was.
    public init?(userInfo: [AnyHashable: Any]) {
        guard let krowk = userInfo["krowk"] as? [AnyHashable: Any],
              let session = krowk["session"] as? String,
              let kind = krowk["kind"] as? String,
              // UUID's parser takes exactly 36 characters in the 8-4-4-4-12
              // shape: nothing longer, no braces, no URL, no path.
              session.utf8.count == 36,
              let uuid = UUID(uuidString: session)
        else { return nil }
        let id = uuid.uuidString.lowercased()
        switch kind {
        case "approval": self = .approval(id)
        case "done": self = .session(id)
        default: return nil
        }
    }

    /// How an alert shows while the app is in the foreground
    /// (`willPresent`): nothing for the session on screen and followed
    /// live (the person is watching it), a banner with its sound for
    /// anything else (another session, the session on screen with its host
    /// away or its connection down, or an alert that names no route).
    public static func presentation(of route: NotificationRoute?, shown: ShownSession?) -> UNNotificationPresentationOptions {
        if let route, let shown, shown.live, shown.id.lowercased() == route.sessionID { return [] }
        // Kept in Notification Center too, once the banner goes.
        return [.banner, .list, .sound]
    }
}

/// The session on screen, as the foreground's alerts ask about it.
public struct ShownSession: Equatable, Sendable {
    public let id: String
    /// Joined and welcomed by a host: what comes shows as it comes.
    public let live: Bool

    public init(id: String, live: Bool) {
        self.id = id
        self.live = live
    }
}

/// What a route that has reached its session does, once the session's
/// connection says enough (R-PERM-2, R-IOS-7).
public enum RouteLanding: Equatable, Sendable {
    /// The approval sheet, for the oldest request waiting.
    case sheet
    /// The host welcomed this phone with nothing waiting: answered
    /// elsewhere, denied after its five minutes, or its turn over.
    case nothingWaiting
    /// No host on the session: what's shown is as it was last seen, and
    /// nothing can be answered.
    case hostOffline
    /// Nothing to add: a finished turn's session, live; or the connection
    /// stopped, which the bar says.
    case shown

    /// The landing `route` makes with the connection at `status` and
    /// `waiting` requests in the session's queue, all the welcome brought
    /// applied; nil while the connection hasn't said yet (joining,
    /// reconnecting, waiting to try again): the route then waits for as
    /// long as the connection takes, and lands when it says.
    public static func of(_ route: NotificationRoute, status: RelayStatus, waiting: Int) -> RouteLanding? {
        switch status {
        case .live:
            guard case .approval = route else { return .shown }
            return waiting > 0 ? .sheet : .nothingWaiting
        case .catchingUp:
            // What the relay dropped may hold the request: an approval's
            // route waits for the catch-up.
            guard case .approval = route else { return .shown }
            return waiting > 0 ? .sheet : nil
        case .hostOffline: return .hostOffline
        case .ended: return .shown
        case .connecting, .reconnecting, .unreachable, .retrying: return nil
        }
    }

    /// The line over the transcript for a landing, if it has one.
    public static let nothingWaitingNotice = "Nothing is waiting for you here any more"

    /// "The host is offline (last seen 12m ago): …", R-IOS-7's honest
    /// staleness.
    public static func offlineNotice(lastSeen: String?) -> String {
        let seen = lastSeen.map { " (\($0))" } ?? ""
        return "The host is offline\(seen): this is the session as it was, and nothing here can be answered until it's back."
    }
}

/// What a route opens its session through: the session list (ticket 87),
/// which pops back to itself and pushes the session's transcript (ticket
/// 88), never a second copy of one already open. A fake in the tests.
@MainActor
public protocol RouteNavigator: AnyObject {
    /// Shows session `id` (a lower-case UUID) if it is one of the person's
    /// listed sessions, reading the list first if it hasn't been: its
    /// screen, or nil when it isn't listed (another account's, deleted,
    /// unreadable here), and then nothing changes.
    func show(session id: String) async -> (any RoutedScreen)?
    /// The session on screen, if a session's transcript is.
    var shownSession: ShownSession? { get }
}

/// A session's screen, as a route lands on it: the transcript, which
/// watches its connection until the welcome (or the host's absence) says
/// what to show. A fake in the tests.
@MainActor
public protocol RoutedScreen: AnyObject {
    func land(_ route: NotificationRoute)
}

/// Takes the routes of tapped notifications to the session list (ticket
/// 106). A route that comes before the app's window has a root (a tap that
/// launched the app) is held until the root is set up; a paired phone's
/// root is the session list, and the route is applied to it then. A phone
/// not paired ignores routes, and drops one held. The latest route wins: a
/// second tap while the first is still finding its session replaces it.
@MainActor
public final class NotificationRouter {
    private enum Root {
        /// No root yet: a route waits.
        case pending
        /// Paired: routes go to the list.
        case paired
        /// Not paired (signing in, pairing, no keys): routes are dropped.
        case unpaired
    }

    private var root = Root.pending
    private weak var navigator: (any RouteNavigator)?
    /// The route that came before the root, and when.
    public private(set) var held: NotificationRoute? {
        didSet { heldAt = held == nil ? nil : clock.now }
    }
    private var heldAt: ContinuousClock.Instant?
    private let clock = ContinuousClock()
    /// A route held longer (the keys came back long after the tap) is
    /// dropped: the person has moved on.
    public static let heldFor: Duration = .seconds(60)
    /// Routes taken to the list: a later one supersedes an earlier still
    /// finding its session.
    private var taken = 0
    /// Routes that have finished looking for their session, found or not:
    /// what the tests wait on.
    public private(set) var finished = 0

    public init() {}

    /// A tapped notification's route (nil: it named none, and the app just
    /// opens).
    public func handle(_ route: NotificationRoute?) {
        guard let route else { return }
        switch root {
        case .pending: held = route
        case .unpaired: break
        case .paired:
            // The list gone (its scene disconnected, or replaced by a key
            // failure that may pass): held for the next.
            if let navigator { go(route, navigator) } else { held = route }
        }
    }

    /// The paired phone's session list is the root: a route held is
    /// applied now.
    public func paired(_ navigator: any RouteNavigator) {
        root = .paired
        self.navigator = navigator
        if let route = held, let heldAt {
            held = nil
            if heldAt.duration(to: clock.now) <= Self.heldFor { go(route, navigator) }
        }
    }

    /// The root is onboarding or pairing: no route applies.
    public func unpaired() {
        root = .unpaired
        navigator = nil
        held = nil
        taken += 1
    }

    /// The session on screen, for the foreground's alerts.
    public var shownSession: ShownSession? {
        root == .paired ? navigator?.shownSession : nil
    }

    private func go(_ route: NotificationRoute, _ navigator: any RouteNavigator) {
        taken += 1
        let this = taken
        Task { [weak self] in
            let screen = await navigator.show(session: route.sessionID)
            guard let self else { return }
            defer { self.finished += 1 }
            guard this == self.taken, let screen else { return }
            screen.land(route)
        }
    }
}

extension TranscriptViewController: RoutedScreen {}
