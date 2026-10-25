// R-IOS-2, R-IOS-7, R-HAND-4 (ticket 132): connect early and honest
// staleness, on TestFleet's paired phone with a cache file of each test's
// own. A cold launch whose cache names the session last open starts that
// session's connection and the list's read before any screen exists, as
// the scene delegate's `ConnectEarly` does, and the session's screen then
// follows that connection rather than join again. A session whose host
// lets it go reads "last seen" on its row and its screen, which says the
// host is offline and queues a prompt; hosted again, both are live.
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
    @Suite("R-IOS-2, R-IOS-7: connect early, and cached state marked last seen")
    struct ConnectEarlyTests {
        typealias Cached = CacheFirstTests

        @Test("R-IOS-2: a cold launch with a cached last open session starts its connection, and the list's read, before the transcript screen exists; the screen made later follows that connection, joined once")
        func testR_IOS_2_aColdLaunchConnectsTheLastOpenSessionBeforeItsScreen() async throws {
            let phone = try await TestFleet.shared.paired()
            let cache = Cached.cache()
            defer { Task { try? await cache.forgetAll() } }
            // A session of the test's own, listed and opened once: the cache
            // holds it, and names it last open, as its screen appearing does.
            let id = try await SessionListTests.control("", body: "An early session")
            let filling = RegistrySessions(api: phone.api, storage: phone.storage, cache: cache)
            _ = try await blockingCall { try filling.list() }
            try await Cached.wait(within: .seconds(10), "the index was not written") {
                try await cache.index().contains { $0.id == id }
            }
            try await Cached.fill(cache, with: filling, id: id, known: nil, phone: phone)
            try await cache.putLastOpen(id)

            // The launch: a fresh source, list and lifecycle, as the scene
            // delegate makes them, and nothing on screen.
            let source = RegistrySessions(api: phone.api, storage: phone.storage, cache: cache)
            let lifecycle = LiveSessions()
            let list = SessionListModel(source: source, kept: { await cache.listing() })
            let early = ConnectEarly(cache: cache, list: list, lifecycle: lifecycle) { id, known in
                LiveSessions.Early(id: id, session: RelayConnection(resuming: { try source.resume(id: id, known: known) }))
            }
            early.start()
            // At once, before any screen: the connection is followed.
            #expect(lifecycle.follows(id), "the last open session's connection did not start before its screen")
            #expect(lifecycle.early?.id == id)
            let live = try #require(lifecycle.early?.session as? RelayConnection)
            // The foreground iOS sends after the launch finds both under way.
            early.start()
            #expect(lifecycle.early?.session === live, "started twice")
            try await RelayConnectionTests.wait(until: { live.status == .live }, within: .seconds(30), "the early connection never went live: \(live.status.text)")
            try await RelayConnectionTests.wait(until: { list.listedAt != nil }, within: .seconds(30), "the list's read never landed")
            #expect(source.calls.lists == 1, "\(source.calls)")
            #expect(live.dials == 1)

            // The person opens it: the screen takes that connection.
            let taken = try #require(early.take(id))
            #expect(taken.session === live)
            #expect(early.take(id) == nil, "given twice")
            let screen = TranscriptViewController(
                row: Cached.row(id, "An early session"), session: taken.session, kept: KeptTranscripts(cache: cache).transcript(id), lifecycle: lifecycle)
            let window = try Cached.window(screen)
            defer { window.isHidden = true }
            try await LiveFollowTests.wait(until: { screen.statusBar.status == .live && screen.model.attached }, within: .seconds(10), "the screen never heard the early connection: \(screen.statusBar.status.text)")
            #expect(live.dials == 1, "joined again for its screen")
            #expect(source.calls.resumes + source.calls.opens == 1, "opened again for its screen: \(source.calls)")
        }

        @Test("R-IOS-7, R-HAND-4: the host lets the open session go: its row and its screen read last seen, the screen says the host is offline and a prompt queues; hosted again, both are live and the notice goes")
        func testR_IOS_7_aReleasedSessionReadsLastSeenThenLiveAgain() async throws {
            let phone = try await TestFleet.shared.paired()
            let title = "A session let go"
            let id = try await SessionListTests.control("", body: title)
            let source = RegistrySessions(api: phone.api, storage: phone.storage)
            let list = SessionListModel(source: source)
            let formatter = SessionRowFormatter()
            let row = { () -> SessionRow? in
                list.listing?.sessions.first { $0.id == id }.map {
                    formatter.row($0, holderName: nil, liveTrusted: list.liveTrusted, now: list.now)
                }
            }
            /// Reads the list until `id`'s row is `live` (or not).
            func listed(live: Bool) async throws {
                let deadline = ContinuousClock.now + .seconds(90)
                repeat {
                    await list.refresh()
                    if row()?.live == live { return }
                    try await Task.sleep(for: .milliseconds(500))
                } while ContinuousClock.now < deadline
                Issue.record("\(title) never listed \(live ? "live" : "not live"): \(row()?.detail ?? "not listed")")
            }
            try await listed(live: true)
            let opened = try #require(row())

            let screen = TranscriptViewController(
                row: opened, session: RelayConnection(resuming: { try source.resume(id: id, known: nil) }))
            screen.lastSeen = { row()?.lastSeen }
            let window = try CacheFirstTests.window(screen)
            defer { window.isHidden = true }
            try await LiveFollowTests.wait(until: { screen.statusBar.status == .live }, within: .seconds(30), "never live: \(screen.statusBar.status.text)")
            #expect(screen.noticeShown == nil)
            #expect(!screen.hostAway)

            #expect(try await SessionListTests.control("/\(id)/release") == "released")
            try await LiveFollowTests.wait(until: { screen.statusBar.status == .hostOffline && screen.noticeShown == .hostOffline }, within: .seconds(30), "the screen never said the host is offline: \(screen.statusBar.status.text)")
            let notice = try #require(screen.notice.text)
            #expect(notice.contains("The host is offline (last seen "), "\(notice)")
            #expect(!screen.notice.isHidden)
            #expect(screen.hostAway)
            #expect(screen.composer.noteLabel.text?.hasPrefix("host offline") == true, "\(screen.composer.noteLabel.text ?? "")")
            screen.composer.text = "Run this when you're back"
            #expect(screen.composer.sendButton.isEnabled, "no prompt queues with the host away")
            try await listed(live: false)
            #expect(row()?.detail.contains("last seen") == true, "\(row()?.detail ?? "")")
            // The transcript stays: scrollable, and nothing went.
            #expect(screen.collectionView.isScrollEnabled)
            #expect(screen.drewRows)

            #expect(try await SessionListTests.control("/\(id)/host") == "hosted")
            try await LiveFollowTests.wait(until: { screen.statusBar.status == .live && screen.noticeShown == nil }, within: .seconds(60), "never live again: \(screen.statusBar.status.text)")
            #expect(!screen.hostAway)
            try await listed(live: true)
            #expect(row()?.detail.contains("live") == true, "\(row()?.detail ?? "")")
            screen.composer.text = ""
        }
    }
}
