// R-IOS-12, R-PERM-2, R-IOS-7: what a tapped notification leads to (ticket
// 106), without UIKit: the payload parsed into a route (or none), whether a
// foreground alert shows, what a route lands on once the session's
// connection says, and the router holding a cold launch's route until the
// paired phone's list is up. The same over real screens, in the host app's
// window, is AppTests' NotificationRoutingTests; through the fleet,
// UITests' NotificationUITests.
import Foundation
import Testing
import UserNotifications
@testable import KrowkKit

@Suite("R-IOS-12: NotificationRouteTests, the alert's payload as untrusted input")
struct NotificationRouteTests {
    static let id = "0b9e7c1a-55f2-4c8e-9d3b-7f1e2a6c4d10"

    static func payload(_ krowk: Any?) -> [AnyHashable: Any] {
        let aps: [String: Any] = ["alert": ["title": "krowk", "body": "A turn finished"], "sound": "default", "thread-id": id]
        var info: [AnyHashable: Any] = ["aps": aps]
        if let krowk { info["krowk"] = krowk }
        return info
    }

    @Test("R-IOS-12 a done alert routes to its session; an approval alert routes to the approval")
    func routes() {
        #expect(NotificationRoute(userInfo: Self.payload(["session": Self.id, "kind": "done"])) == .session(Self.id))
        #expect(NotificationRoute(userInfo: Self.payload(["session": Self.id, "kind": "approval"])) == .approval(Self.id))
        // Upper case is the same session: compared as the list names it.
        #expect(NotificationRoute(userInfo: Self.payload(["session": Self.id.uppercased(), "kind": "done"]))?.sessionID == Self.id)
        // As the registry's JSON reaches userInfo: an NSDictionary.
        let bridged = NSDictionary(dictionary: ["session": Self.id, "kind": "approval"])
        #expect(NotificationRoute(userInfo: Self.payload(bridged)) == .approval(Self.id))
    }

    @Test("R-IOS-12: malformed, non-UUID or unknown-kind payloads route nowhere")
    func nowhere() {
        let id = Self.id
        let hostile: [Any?] = [
            nil,
            "not a dictionary",
            ["kind": "done"] as [String: Any],
            ["session": id] as [String: Any],
            ["session": id, "kind": "Done"] as [String: Any],
            ["session": id, "kind": "approve"] as [String: Any],
            ["session": id, "kind": ""] as [String: Any],
            ["session": id, "kind": 1] as [String: Any],
            ["session": 42, "kind": "done"] as [String: Any],
            ["session": "", "kind": "done"] as [String: Any],
            ["session": "A demo session", "kind": "done"] as [String: Any],
            ["session": "{\(id)}", "kind": "done"] as [String: Any],
            ["session": "\(id)x", "kind": "done"] as [String: Any],
            ["session": " \(id)", "kind": "done"] as [String: Any],
            ["session": "\(id)/../../etc", "kind": "done"] as [String: Any],
            ["session": "krowk://session/\(id)", "kind": "approval"] as [String: Any],
            ["session": "0b9e7c1a55f24c8e9d3b7f1e2a6c4d10", "kind": "done"] as [String: Any],
            ["session": [id], "kind": "done"] as [String: Any],
        ]
        for (at, krowk) in hostile.enumerated() {
            #expect(NotificationRoute(userInfo: Self.payload(krowk)) == nil, "case \(at)")
        }
    }

    @Test("R-IOS-12: the aps thread-id names no route on its own")
    func threadIDAlone() {
        #expect(NotificationRoute(userInfo: ["aps": ["thread-id": Self.id]]) == nil)
        #expect(NotificationRoute(userInfo: [:]) == nil)
    }

    @Test("R-IOS-12: in the foreground, no banner for the session on screen and live; a banner with sound otherwise")
    func foreground() {
        let route = NotificationRoute.approval(Self.id)
        let other = NotificationRoute.session("1c2d3e4f-0000-4000-8000-000000000001")
        #expect(NotificationRoute.presentation(of: route, shown: ShownSession(id: Self.id, live: true)) == [])
        #expect(NotificationRoute.presentation(of: .session(Self.id), shown: ShownSession(id: Self.id.uppercased(), live: true)) == [])
        #expect(NotificationRoute.presentation(of: other, shown: ShownSession(id: Self.id, live: true)) == [.banner, .list, .sound])
        // On screen but not live (host away, reconnecting): it says something new.
        #expect(NotificationRoute.presentation(of: route, shown: ShownSession(id: Self.id, live: false)) == [.banner, .list, .sound])
        #expect(NotificationRoute.presentation(of: route, shown: nil) == [.banner, .list, .sound])
        #expect(NotificationRoute.presentation(of: nil, shown: ShownSession(id: Self.id, live: true)) == [.banner, .list, .sound])
    }

    @Test("R-PERM-2, R-IOS-7: a route lands once the connection says: the sheet or nothing waiting when welcomed, the host away when none, nothing while joining")
    func landing() {
        let approval = NotificationRoute.approval(Self.id)
        let done = NotificationRoute.session(Self.id)
        #expect(RouteLanding.of(approval, status: .live, waiting: 2) == .sheet)
        #expect(RouteLanding.of(approval, status: .catchingUp, waiting: 1) == .sheet)
        #expect(RouteLanding.of(approval, status: .catchingUp, waiting: 0) == nil, "what the relay dropped may hold it")
        #expect(RouteLanding.of(done, status: .catchingUp, waiting: 0) == .shown)
        #expect(RouteLanding.of(approval, status: .live, waiting: 0) == .nothingWaiting)
        #expect(RouteLanding.of(done, status: .live, waiting: 1) == .shown)
        #expect(RouteLanding.of(approval, status: .hostOffline, waiting: 1) == .hostOffline, "R-IOS-7: no sheet with the host away")
        #expect(RouteLanding.of(done, status: .hostOffline, waiting: 0) == .hostOffline)
        #expect(RouteLanding.of(approval, status: .ended("gone"), waiting: 0) == .shown)
        for joining: RelayStatus in [.connecting, .reconnecting, .unreachable("no route"), .retrying(at: .now, why: nil)] {
            #expect(RouteLanding.of(approval, status: joining, waiting: 1) == nil, "\(joining)")
        }
        #expect(RouteLanding.offlineNotice(lastSeen: "last seen 12m ago").contains("(last seen 12m ago)"))
    }
}

@Suite("R-PERM-2: NotificationRouter, a route held until the paired list is up", .serialized)
@MainActor
struct NotificationRouterTests {
    final class FakeScreen: RoutedScreen {
        var landed: [NotificationRoute] = []
        func land(_ route: NotificationRoute) { landed.append(route) }
    }

    /// Opens what it's asked for, once each, after `delay`; nil for an id
    /// it doesn't list.
    final class FakeNavigator: RouteNavigator {
        var listed: Set<String>
        var asked: [String] = []
        var screens: [String: FakeScreen] = [:]
        var delay: Duration = .zero
        var shownSession: ShownSession?

        init(_ listed: Set<String>) { self.listed = listed }

        func show(session id: String) async -> (any RoutedScreen)? {
            asked.append(id)
            if delay > .zero { try? await Task.sleep(for: delay) }
            guard listed.contains(id) else { return nil }
            if let open = screens[id] { return open }
            let made = FakeScreen()
            screens[id] = made
            return made
        }
    }

    static let a = "0b9e7c1a-55f2-4c8e-9d3b-7f1e2a6c4d10"
    static let b = "1c2d3e4f-0000-4000-8000-000000000001"

    /// Until `count` routes have finished looking for their session.
    func finished(_ count: Int, _ router: NotificationRouter) async throws {
        for _ in 0..<1000 where router.finished < count {
            try await Task.sleep(for: .milliseconds(5))
        }
        #expect(router.finished == count)
    }

    @Test("R-PERM-2: a route at cold launch is held, and applied once the root exists")
    func coldLaunch() async throws {
        let router = NotificationRouter()
        router.handle(.approval(Self.a))
        #expect(router.held == .approval(Self.a))
        let navigator = FakeNavigator([Self.a])
        router.paired(navigator)
        #expect(router.held == nil)
        try await finished(1, router)
        #expect(navigator.asked == [Self.a])
        #expect(navigator.screens[Self.a]?.landed == [.approval(Self.a)])
    }

    @Test("R-IOS-12: an unpaired phone ignores routes, and drops one held")
    func unpaired() async throws {
        let router = NotificationRouter()
        router.handle(.session(Self.a))
        router.unpaired()
        #expect(router.held == nil)
        router.handle(.approval(Self.a))
        #expect(router.held == nil)
        #expect(router.shownSession == nil)
        // Paired later (signed in and paired in this run): only a later tap goes.
        let navigator = FakeNavigator([Self.a])
        router.paired(navigator)
        router.handle(.session(Self.a))
        try await finished(1, router)
        #expect(navigator.asked == [Self.a], "nothing held from before pairing")
        #expect(navigator.screens[Self.a]?.landed == [.session(Self.a)])
    }

    @Test("R-PERM-2: a route that comes while the list is gone (its scene disconnected) is held for the next list")
    func listGone() async throws {
        let router = NotificationRouter()
        do {
            let gone = FakeNavigator([Self.a])
            router.paired(gone)
        }
        router.handle(.approval(Self.a))
        #expect(router.held == .approval(Self.a))
        let navigator = FakeNavigator([Self.a])
        router.paired(navigator)
        try await finished(1, router)
        #expect(navigator.screens[Self.a]?.landed == [.approval(Self.a)])
    }

    @Test("R-IOS-12: no route, or a session not listed (another account's), changes nothing")
    func notListed() async throws {
        let router = NotificationRouter()
        let navigator = FakeNavigator([Self.a])
        router.paired(navigator)
        router.handle(nil)
        router.handle(.approval(Self.b))
        try await finished(1, router)
        #expect(navigator.asked == [Self.b])
        #expect(navigator.screens.isEmpty)
    }

    @Test("R-PERM-2: the same session tapped twice lands on the one screen; a later tap supersedes one still finding its session")
    func sameAndLater() async throws {
        let router = NotificationRouter()
        let navigator = FakeNavigator([Self.a, Self.b])
        router.paired(navigator)
        router.handle(.session(Self.a))
        try await finished(1, router)
        router.handle(.approval(Self.a))
        try await finished(2, router)
        #expect(navigator.screens.count == 1)
        #expect(navigator.screens[Self.a]?.landed == [.session(Self.a), .approval(Self.a)])

        navigator.delay = .milliseconds(50)
        router.handle(.approval(Self.b))
        router.handle(.session(Self.a))
        try await finished(4, router)
        #expect(navigator.screens[Self.b]?.landed ?? [] == [], "the earlier route gave way")
        #expect(navigator.screens[Self.a]?.landed.last == .session(Self.a))
    }

    @Test("R-IOS-12: the session on screen is the paired list's to say")
    func shown() {
        let router = NotificationRouter()
        let navigator = FakeNavigator([Self.a])
        navigator.shownSession = ShownSession(id: Self.a, live: true)
        #expect(router.shownSession == nil)
        router.paired(navigator)
        #expect(router.shownSession == ShownSession(id: Self.a, live: true))
    }
}
