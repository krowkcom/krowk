// R-PERM-2, R-IOS-7, R-IOS-12: a tapped notification's route over the real
// screens (ticket 106), in a window of the host app's scene (where a sheet
// rises): the session list on a stub listing, each session's transcript on
// a stub connection (ApprovalScreenTests' ScreenStubSession) with its own
// approval queue. A route that comes before the list is held and applied
// once it's up; the session's screen is pushed once, never twice; an
// approval's route raises the sheet for the oldest request the welcome
// brought, or says nothing waits; with the host offline the session opens
// as last seen, and no sheet. The parsing and the router alone are
// KrowkKit's NotificationRouteTests; through the fleet, UITests'
// NotificationUITests.
import KrowkKit
import QuartzCore
import Synchronization
import Testing
import UIKit

/// A session whose host's welcome reaches the screen as RelayConnection's
/// does: its requests posted first, handed to the approval queue only
/// when the screen drains the mailbox (`takeFrame`), then the status. No
/// wake: only the screen's own drain brings them.
final class WelcomeStubSession: LiveSession {
    private let told = Mutex<(@MainActor @Sendable (RelayStatus) -> Void)?>(nil)
    private let posted = Mutex<[ApprovalPrompt]>([])
    @MainActor weak var queue: ApprovalQueue?

    func follow(status: @escaping @MainActor @Sendable (RelayStatus) -> Void) {
        told.withLock { $0 = status }
    }

    var following: Bool { told.withLock { $0 != nil } }

    func stop() {}
    func onUpdates(_ wake: @escaping @MainActor @Sendable () -> Void) {}
    func send(_ command: LiveCommand, token: UInt64) {}
    func foreground() {}
    func background() {}
    func wake() {}

    @MainActor func takeFrame() -> [TranscriptUpdate] {
        let requests = posted.withLock { posted in
            defer { posted = [] }
            return posted
        }
        for request in requests { queue?.requested(request) }
        return []
    }

    /// A welcome: `requests` posted, then `status` published.
    func welcome(_ requests: [ApprovalPrompt], status: RelayStatus) {
        posted.withLock { $0.append(contentsOf: requests) }
        say(status)
    }

    /// Requests that come live, with the link already up.
    func post(_ requests: [ApprovalPrompt]) {
        posted.withLock { $0.append(contentsOf: requests) }
    }

    func say(_ status: RelayStatus) {
        guard let told = told.withLock({ $0 }) else { return }
        Task { @MainActor in told(status) }
    }
}

@MainActor
@Suite("R-PERM-2, R-IOS-7: NotificationRoutingTests, a route over the session list and its transcript", .serialized)
struct NotificationRoutingTests {
    private struct AlwaysYes: OwnerCheck {
        @MainActor func confirm(reason: String) async -> Bool { true }
    }

    private struct Record: SessionRecord {
        let id: String
        let title: String
        var cwd = "/home/me/app"
        /// Twelve minutes ago.
        var updatedMs = UInt64((Date().timeIntervalSince1970 - 12 * 60) * 1000)
        var hostedBy: String? = nil
    }

    private struct Listing: SessionSource {
        let sessions: [Record]
        func list() throws(SessionListFailure) -> SessionListing {
            SessionListing(sessions: sessions, unreadable: 0)
        }
    }

    static let a = "0b9e7c1a-55f2-4c8e-9d3b-7f1e2a6c4d10"
    static let b = "1c2d3e4f-0000-4000-8000-000000000001"
    static let notListed = "2d3e4f5a-0000-4000-8000-000000000002"

    /// The list in a window, its sessions' screens on stubs made as a tap
    /// would open them, and the router not yet told the list is up.
    @MainActor
    final class Rig {
        let router = NotificationRouter()
        let list: SessionListViewController
        let navigation: UINavigationController
        let window: UIWindow
        var sessions: [String: WelcomeStubSession] = [:]
        var queues: [String: ApprovalQueue] = [:]
        var opened = 0

        init() throws {
            let model = SessionListModel(source: Listing(sessions: [
                Record(id: NotificationRoutingTests.a, title: "Fix the build"),
                Record(id: NotificationRoutingTests.b, title: "Write the docs"),
            ]))
            var made: (@MainActor (SessionRow) -> UIViewController)?
            list = SessionListViewController(model: model) { row in made!(row) }
            navigation = UINavigationController(rootViewController: list)
            let scene = try #require(UIApplication.shared.connectedScenes.first as? UIWindowScene, "no window scene in the host app")
            window = UIWindow(windowScene: scene)
            window.rootViewController = navigation
            window.makeKeyAndVisible()
            window.layoutIfNeeded()
            made = { [unowned self] row in
                opened += 1
                let session = WelcomeStubSession()
                let queue = ApprovalQueue(sessionID: row.id, ownerCheck: AlwaysYes())
                session.queue = queue
                sessions[row.id] = session
                queues[row.id] = queue
                return TranscriptViewController(row: row, session: session, approvals: queue, lifecycle: LiveSessions())
            }
        }

        var top: TranscriptViewController? { navigation.topViewController as? TranscriptViewController }

        /// Session `id`'s screen on top and following its stub.
        func onTop(_ id: String) async throws -> TranscriptViewController {
            try await waitFor { self.top?.row.id == id && self.top?.isViewLoaded == true && self.sessions[id]?.following == true }
            return try #require(top)
        }

        static func request(_ id: String, _ request: String) -> ApprovalPrompt {
            ApprovalPrompt(sessionID: id, turnID: "turn2", requestID: request, tool: "bash", input: #"{"command":"touch approved.txt"}"#, summary: "bash `touch approved.txt`", reason: "bash isn't allowed by a rule")
        }

        /// The host's welcome: `requests` re-sent, oldest first, posted
        /// before the status the session publishes after them.
        func welcome(_ id: String, _ requests: [String], status: RelayStatus = .live) throws {
            try #require(sessions[id]).welcome(requests.map { Self.request(id, $0) }, status: status)
        }
    }

    @Test("R-PERM-2 an approval route presents the sheet for the oldest pending request once the welcome is applied; a route at cold launch is applied once the root exists; the same session is not pushed twice")
    func approvalAtColdLaunch() async throws {
        let rig = try Rig()
        defer { rig.window.isHidden = true }
        // Before the root: held.
        rig.router.handle(.approval(Self.a))
        #expect(rig.router.held == .approval(Self.a))
        #expect(rig.navigation.viewControllers.count == 1)
        rig.router.paired(rig.list)
        let screen = try await rig.onTop(Self.a)
        #expect(screen.landing == .approval(Self.a), "waits for the welcome")
        #expect(screen.approvalSheet == nil)

        try rig.welcome(Self.a, ["r1", "r2"])
        try await waitFor { screen.approvalSheet.map { screen.presentedViewController === $0 && !$0.isBeingPresented } ?? false }
        #expect(rig.queues[Self.a]?.current?.prompt.requestID == "r1", "the oldest first")
        #expect(screen.landing == nil)
        #expect(screen.noticeShown == nil)

        // Tapped again: the same screen, its one sheet.
        rig.router.handle(.approval(Self.a))
        try await waitFor { rig.router.finished == 2 }
        #expect(screen.landing == nil)
        #expect(rig.navigation.viewControllers.count == 2)
        #expect(rig.top === screen)
        #expect(rig.opened == 1)
        #expect(screen.approvalSheet != nil)
    }

    @Test("R-PERM-2: a welcome with nothing waiting opens the session with the notice, no sheet; a request that comes later raises it and the notice goes; settled elsewhere, the notice comes back")
    func nothingWaiting() async throws {
        let rig = try Rig()
        defer { rig.window.isHidden = true }
        rig.router.paired(rig.list)
        // On another session's screen first: back to the list, then on.
        rig.router.handle(.session(Self.b))
        _ = try await rig.onTop(Self.b)
        try rig.welcome(Self.b, [])
        rig.router.handle(.approval(Self.a))
        let screen = try await rig.onTop(Self.a)
        #expect(rig.navigation.viewControllers.count == 2, "the list, then the session")
        try rig.welcome(Self.a, [])
        try await waitFor { screen.noticeShown == .nothingWaiting }
        #expect(screen.notice.text?.contains(RouteLanding.nothingWaitingNotice) == true)
        #expect(!screen.notice.isHidden)
        #expect(screen.approvalSheet == nil)

        // A request live afterwards: the sheet, and the notice goes.
        try #require(rig.sessions[Self.a]).post([Rig.request(Self.a, "r3")])
        screen.driver?.tick(at: CACurrentMediaTime())
        try await waitFor { screen.approvalSheet != nil }
        #expect(screen.noticeShown == nil)

        // The turn's catch-up says it was answered at the terminal: the
        // notice comes back and stays, no toast in its place.
        rig.queues[Self.a]?.resolved(requestID: "r3", resolution: .allowed)
        try await waitFor { screen.approvalSheet == nil && screen.noticeShown == .nothingWaiting }
        #expect(!screen.notice.isHidden)
        #expect(screen.toast.alpha == 0)
    }

    @Test("R-PERM-2: tapped with the session already open and live, a request that comes within the grace still raises the sheet; with none, the notice comes after the grace")
    func alreadyLive() async throws {
        let rig = try Rig()
        defer { rig.window.isHidden = true }
        rig.router.paired(rig.list)
        rig.router.handle(.session(Self.a))
        let screen = try await rig.onTop(Self.a)
        try rig.welcome(Self.a, [])
        try await waitFor { screen.isLive && screen.landing == nil }

        rig.router.handle(.approval(Self.a))
        try await waitFor { rig.router.finished == 2 }
        #expect(screen.landing == .approval(Self.a), "no notice yet: a request may be on its way")
        #expect(screen.noticeShown == nil)
        try #require(rig.sessions[Self.a]).post([Rig.request(Self.a, "r4")])
        screen.driver?.tick(at: CACurrentMediaTime())
        try await waitFor { screen.approvalSheet != nil }
        #expect(screen.landing == nil)
        #expect(screen.noticeShown == nil)
        #expect(rig.opened == 1)

        // Answered elsewhere; tapped again with nothing waiting: the notice, after the grace.
        rig.queues[Self.a]?.resolved(requestID: "r4", resolution: .allowed)
        try await waitFor { screen.approvalSheet == nil }
        #expect(screen.noticeShown == .nothingWaiting, "the route's request, settled elsewhere")
        rig.router.handle(.approval(Self.a))
        try await waitFor { rig.router.finished == 3 }
        #expect(screen.noticeShown == nil)
        try await waitFor { screen.noticeShown == .nothingWaiting }
        #expect(screen.landing == nil)
        #expect(screen.approvalSheet == nil)
    }

    @Test("R-IOS-7 a route to a session whose host is offline opens it read-only with last seen, and no sheet")
    func hostOffline() async throws {
        let rig = try Rig()
        defer { rig.window.isHidden = true }
        rig.router.paired(rig.list)
        rig.router.handle(.approval(Self.a))
        let screen = try await rig.onTop(Self.a)
        try #require(rig.sessions[Self.a]).say(.hostOffline)
        try await waitFor { screen.noticeShown == .hostOffline }
        let notice = try #require(screen.notice.text)
        #expect(notice.contains("offline"), "\(notice)")
        #expect(notice.contains("last seen 12"), "\(notice)")
        #expect(screen.approvalSheet == nil)
        #expect(screen.statusBar.status == .hostOffline)
        #expect(rig.queues[Self.a]?.connected == false, "nothing can be answered")
        #expect(screen.isLive == false)
        #expect(rig.list.shownSession == ShownSession(id: Self.a, live: false))
    }

    @Test("R-IOS-12: a route to a session not in the listing opens nothing; in the foreground, the session on screen and live rings no banner")
    func notListedAndForeground() async throws {
        let rig = try Rig()
        defer { rig.window.isHidden = true }
        rig.router.paired(rig.list)
        rig.router.handle(.approval(Self.notListed))
        try await waitFor { rig.router.finished == 1 }
        #expect(rig.navigation.viewControllers.count == 1)
        #expect(rig.opened == 0)

        rig.router.handle(.session(Self.a))
        let screen = try await rig.onTop(Self.a)
        try rig.welcome(Self.a, [])
        try await waitFor { screen.isLive }
        #expect(rig.router.shownSession == ShownSession(id: Self.a, live: true))
        #expect(NotificationRoute.presentation(of: .approval(Self.a), shown: rig.router.shownSession) == [])
        #expect(NotificationRoute.presentation(of: .approval(Self.b), shown: rig.router.shownSession) == [.banner, .list, .sound])
        #expect(screen.noticeShown == nil, "a finished turn's route says nothing more")
    }
}

@MainActor
private func waitFor(_ done: () -> Bool) async throws {
    for _ in 0..<500 where !done() {
        try await Task.sleep(for: .milliseconds(10))
    }
    #expect(done())
}
