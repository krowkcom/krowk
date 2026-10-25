// R-IOS-1, R-IOS-3, R-PERF-6 (ticket 131): cache first, on TestFleet's
// paired phone with a cache file of each test's own. A session listed and
// opened once fills the cache; a new list and screen with the registry
// pointed at a dead port then draw from it with no registry call; with the
// network back, the session reads on from the place the cache kept
// (`Account.resumeSession`, never `openSession`) and a turn the fleet ran
// meanwhile is drawn once; the events the cache and the session's own read
// both hold are each drawn once, as a full read of the session draws them.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its URLs to this
// process. Hosted in the app, as the Keychain needs.
import Foundation
import Krowk
import KrowkAccount
import KrowkCache
import KrowkKit
import Testing
import UIKit

extension HostedKeychain {
    @MainActor
    @Suite("R-IOS-1, R-IOS-3, R-PERF-6: sessions drawn from the cache first, then read on from the saved place")
    struct CacheFirstTests {
        /// A registry nothing listens on.
        static let deadAPI = "http://127.0.0.1:9/v1"

        static func cache() -> SessionCache {
            SessionCache(at: URL.temporaryDirectory.appending(path: "krowk-cache-hosted/\(UUID().uuidString)/krowk-cache.sqlite"))
        }

        /// Session `id` opened through `source` and followed until the
        /// cache holds a place for it, a cursor, and every event a read of
        /// it holds; then let go.
        static func fill(_ cache: SessionCache, with source: RegistrySessions, id: String, known: HeadRecord?, phone: PairedAccount) async throws {
            let live = RelayConnection(
                resuming: { try source.resume(id: id, known: known) },
                cache: TranscriptCacheWriter(session: id, cache: cache, interval: .milliseconds(50)))
            live.follow { _ in }
            defer { live.stop() }
            let expected = Set(try await RelayConnectionTests.open(id, phone: phone, known: known).events().map(\.id))
            try await wait(within: .seconds(30), "the cache never held the session's place, cursor and events") {
                let place = try await cache.place(session: id)
                let cursor = try await cache.cursor(session: id)
                let held = Set(try await cache.events(session: id, limit: 100_000).map(\.id))
                return place != nil && cursor != nil && held.isSuperset(of: expected)
            }
        }

        static func window(_ screen: UIViewController) throws -> UIWindow {
            let scene = try #require(UIApplication.shared.connectedScenes.first as? UIWindowScene, "no window scene in the host app")
            let window = UIWindow(windowScene: scene)
            window.rootViewController = UINavigationController(rootViewController: screen)
            window.makeKeyAndVisible()
            return window
        }

        static func row(_ id: String, _ title: String) -> SessionRow {
            SessionRow(id: id, title: title, detail: "", live: false, accessibilityLabel: title)
        }

        @Test("R-IOS-1, R-IOS-3: listed and opened once, a new list and transcript with the registry unreachable draw from the cache, making no registry call to draw, with no spinner and nothing live")
        func testR_IOS_1_theListAndTranscriptDrawFromTheCacheWithTheNetworkDown() async throws {
            let phone = try await TestFleet.shared.paired()
            let cache = Self.cache()
            defer { Task { try? await cache.forgetAll() } }
            let source = RegistrySessions(api: phone.api, storage: phone.storage, cache: cache)
            let listing = try await blockingCall { try source.list() }
            let demo = try #require(
                listing.sessions.compactMap { $0 as? SessionSummary }.first { $0.title == "A demo session" },
                "no demo session in \(listing.sessions.map(\.title))")
            try await Self.wait(within: .seconds(10), "the index was not written") {
                try await cache.index().contains { $0.id == demo.id }
            }
            try await Self.fill(cache, with: source, id: demo.id, known: demo.head, phone: phone)
            let kept = try await cache.events(session: demo.id, limit: CachedTranscript.pageSize).count

            // The network down: a registry nothing listens on.
            let offline = RegistrySessions(api: Self.deadAPI, storage: phone.storage, cache: cache)
            let model = SessionListModel(source: offline, kept: { await cache.listing() })
            await model.prime()
            #expect(offline.calls == RegistrySessions.Calls(), "the kept listing made a registry call")
            let rows = try #require(model.listing?.sessions)
            #expect(rows.contains { $0.id == demo.id && $0.title == "A demo session" })
            #expect(!model.liveTrusted, "a kept listing shows a session live")
            let page = try #require(await KeptTranscripts(cache: cache).transcript(demo.id).newest())
            #expect(page.events.count == kept)
            #expect(offline.calls == RegistrySessions.Calls(), "the kept transcript made a registry call")

            // Its rows "last seen", never live.
            let formatter = SessionRowFormatter()
            #expect(rows.map { formatter.row($0, holderName: nil, liveTrusted: model.liveTrusted, now: model.now) }.allSatisfy { !$0.live })

            // The transcript's screen, as the app makes it: its rows from
            // the cache, while the session can't be opened.
            let live = RelayConnection(resuming: { try offline.resume(id: demo.id, known: demo.head) })
            defer { live.stop() }
            let screen = TranscriptViewController(row: Self.row(demo.id, "A demo session"), session: live, kept: KeptTranscripts(cache: cache).transcript(demo.id))
            let window = try Self.window(screen)
            defer { window.isHidden = true }
            try await LiveFollowTests.wait(until: { TranscriptTests.tool(in: screen.model)?.output != nil }, within: .seconds(5), "the kept transcript was not drawn: \(TranscriptTests.rows(of: screen.model))")
            #expect(TranscriptTests.rows(of: screen.model).contains { $0.hasPrefix("user: Say hello") })
            #expect(!TranscriptTests.visibleIndicator(in: screen.view), "a spinner over kept rows")
            #expect(screen.statusBar.status != .live, "live with the registry down")
        }

        @Test("R-IOS-1, R-PERF-6: with a place saved, the session is read on with resumeSession, not openSession; a turn the fleet ran meanwhile is drawn once, and a place is kept again")
        func testR_IOS_1_R_PERF_6_aTurnRunMeanwhileIsReadOnFromThePlaceAndDrawnOnce() async throws {
            let phone = try await TestFleet.shared.paired()
            let cache = Self.cache()
            defer { Task { try? await cache.forgetAll() } }
            // A session of the test's own: the fleet runs turns only in those.
            let id = try await SessionListTests.control("", body: "A cache-first session")
            let source = RegistrySessions(api: phone.api, storage: phone.storage, cache: cache)
            _ = try await blockingCall { try source.list() }
            try await Self.wait(within: .seconds(10), "the index was not written") {
                try await cache.index().contains { $0.id == id }
            }
            try await Self.fill(cache, with: source, id: id, known: nil, phone: phone)
            #expect(source.calls.opens == 1 && source.calls.resumes == 0, "\(source.calls)")

            // Meanwhile, with no screen open, a turn on the fleet.
            let prompt = "Please stream a long answer"
            #expect(try await SessionListTests.control("/\(id)/turn", body: prompt) == "started")
            try await Self.wait(within: .seconds(180), "the turn never ended at the host") {
                try await SessionListTests.control("/\(id)/turn-ended") != "running"
            }

            let screen = TranscriptViewController(
                row: Self.row(id, "A cache-first session"),
                session: RelayConnection(
                    resuming: { try source.resume(id: id, known: nil) },
                    cache: TranscriptCacheWriter(session: id, cache: cache, interval: .milliseconds(50))),
                kept: KeptTranscripts(cache: cache).transcript(id))
            let window = try Self.window(screen)
            defer { window.isHidden = true }
            let model = screen.model
            try await LiveFollowTests.wait(
                until: { TranscriptTests.rows(of: model).contains("user: \(prompt)") && model.ids.contains { $0.hasPrefix("turn:") } },
                within: .seconds(30), "the turn run meanwhile never drew: \(TranscriptTests.rows(of: model))")
            #expect(source.calls.resumes == 1, "\(source.calls)")
            #expect(source.calls.opens == 1, "openSession called again with a place saved: \(source.calls)")
            #expect(TranscriptTests.rows(of: model).filter { $0 == "user: \(prompt)" }.count == 1)
            #expect(Set(model.ids).count == model.ids.count)
            #expect(try await cache.place(session: id) != nil, "the resumed read kept no place")
        }

        @Test("R-IOS-1: the cache's page and the session read on from its place hold some of the same events (a turn followed live, then written to the log): each is drawn once, in the order a full read draws them; with no place kept, the full read goes above the kept page")
        func testR_IOS_1_eventsTheCacheAndTheCatchUpBothHoldAreDrawnOnce() async throws {
            let phone = try await TestFleet.shared.paired()
            let cache = Self.cache()
            defer { Task { try? await cache.forgetAll() } }
            let id = try await SessionListTests.control("", body: "A merged session")
            let source = RegistrySessions(api: phone.api, storage: phone.storage, cache: cache)
            _ = try await blockingCall { try source.list() }
            try await Self.wait(within: .seconds(10), "the index was not written") {
                try await cache.index().contains { $0.id == id }
            }
            try await Self.fill(cache, with: source, id: id, known: nil, phone: phone)

            // A turn followed live: the cache takes its events past the
            // place the read stopped at.
            let live = RelayConnection(
                resuming: { try source.resume(id: id, known: nil) },
                cache: TranscriptCacheWriter(session: id, cache: cache, interval: .milliseconds(50)))
            live.follow { _ in }
            try await Self.wait(within: .seconds(30), "the follow never kept a place") { try await cache.place(session: id) != nil }
            #expect(try await SessionListTests.control("/\(id)/turn", body: "Please stream a long answer") == "started")
            try await Self.wait(within: .seconds(180), "the turn never ended at the host") {
                try await SessionListTests.control("/\(id)/turn-ended") != "running"
            }
            try await Self.wait(within: .seconds(30), "the turn's end was never cached") {
                try await cache.events(session: id, limit: 100_000).contains { event in
                    guard case .turnCompleted? = (try? decodeLogEvent(json: event.json))?.body else { return false }
                    return true
                }
            }
            live.stop()
            // Once the host wrote the turn to the log, a read on from the
            // place holds events the cache holds too.
            let kept = try #require(try await cache.place(session: id))
            let place = try decodeReadPlace(json: String(decoding: kept.place, as: UTF8.self))
            let relay = try #require(RelayConnection.relayOverride())
            try await Self.wait(within: .seconds(60), "the turn never reached the log past the place") {
                let read = try await blockingCall { try phone.account.resumeSession(id: id, place: place, known: nil, relayOverride: relay).events() }.map(\.id)
                let cached = Set(try await cache.events(session: id, limit: 100_000).map(\.id))
                return read.contains(where: cached.contains)
            }

            /// A new screen of the session, read until its turn's end is
            /// drawn: drawn once each, as a full read draws them.
            func reopened() async throws {
                let screen = TranscriptViewController(
                    row: Self.row(id, "A merged session"),
                    session: RelayConnection(resuming: { try source.resume(id: id, known: nil) }),
                    kept: KeptTranscripts(cache: cache).transcript(id))
                let window = try Self.window(screen)
                defer { window.isHidden = true }
                let model = screen.model
                try await LiveFollowTests.wait(until: { model.attached && model.ids.contains { $0.hasPrefix("turn:") } }, within: .seconds(30), "never read: \(TranscriptTests.rows(of: model))")
                let full = TranscriptModel(sessionID: id)
                full.attach(try await RelayConnectionTests.open(id, phone: phone).events().map(TranscriptEvent.init))
                let drawn = model.ids.filter { !$0.hasPrefix("notice:") && !$0.hasPrefix("pending:") }
                #expect(Set(drawn).count == drawn.count, "a row twice: \(drawn)")
                // Live lines of a turn still under way may add rows a full
                // read hasn't logged yet; every row it draws is there, in
                // its order.
                let read = Set(full.ids)
                #expect(drawn.filter(read.contains) == full.ids, "drawn \(TranscriptTests.rows(of: model)), a full read draws \(TranscriptTests.rows(of: full))")
            }

            let calls = source.calls
            try await reopened()
            #expect(source.calls.resumes == calls.resumes + 1 && source.calls.opens == calls.opens, "\(source.calls)")

            // No place kept (a cache an earlier build wrote): read in full,
            // what is older than the kept page goes above it.
            let now = try #require(try await cache.place(session: id))
            try await cache.putRead(session: id, [], place: nil, held: nil, from: now.place)
            #expect(try await cache.place(session: id) == nil)
            try await reopened()
            #expect(source.calls.opens == calls.opens + 1, "\(source.calls)")
        }

        /// Polls `done` every 50 ms until it holds, or fails at `limit`.
        static func wait(within limit: Duration, _ why: String, until done: () async throws -> Bool) async throws {
            let deadline = ContinuousClock.now + limit
            while !(try await done()) {
                try #require(ContinuousClock.now < deadline, Comment(rawValue: why))
                try await Task.sleep(for: .milliseconds(50))
            }
        }
    }
}
