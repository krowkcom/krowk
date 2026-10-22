// R-IOS-7, R-IOS-3, R-PROTO-2, R-HAND-4: the session list against the fake
// fleet, on TestFleet's paired phone: krowk-ffi's listing (each record
// checked against the pinned device list, its sealed index unsealed, no
// transcript read), through the list's model and row formatter. The
// fleet's demo session is listed live, on host A, under its title; a
// session whose host lets its lease go reads "last seen" after the next
// refresh. The listing runs off the main thread: RegistrySessions.list
// holds dispatchPrecondition(.notOnQueue(.main)), so a call from the main
// thread would stop the test process.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its URLs to this
// process (KROWK_API_URL, KROWK_PAIR_URL, KROWK_SESSIONS_URL). Hosted in
// the app, as the Keychain needs.
import Foundation
import Krowk
import KrowkAccount
import KrowkKit
import Testing

extension HostedKeychain {
    @Suite("R-IOS-7: the paired phone lists the fake fleet's sessions, live only while a host holds the lease")
    @MainActor
    struct SessionListTests {
        @Test("R-IOS-7, R-IOS-3, R-PROTO-2: the fleet's demo session is listed live on host-a with its title, from the generated SessionSummary")
        func demoIsLive() async throws {
            let phone = try await TestFleet.shared.paired()
            let lines = Lines()
            let model = SessionListModel(source: RegistrySessions(api: phone.api, storage: phone.storage), log: { lines.append($0) })
            await model.refresh()
            #expect(model.failure == nil, "\(String(describing: model.failure))")
            let listing = try #require(model.listing)
            // R-PROTO-2: the rows read krowk-ffi's own record.
            let demo = try #require(
                listing.sessions.compactMap { $0 as? SessionSummary }.first { $0.title == "A demo session" },
                "no demo session in \(listing.sessions.map(\.title))")
            #expect(demo.hostedBy != nil)
            #expect(listing.unreadable == 0)
            let row = SessionRowFormatter().row(demo, holderName: demo.hostedBy.flatMap { listing.deviceNames[$0] }, liveTrusted: model.liveTrusted, now: model.now)
            #expect(row.title == "A demo session")
            #expect(row.live)
            #expect(row.detail.hasSuffix("● live on host-a"), "\(row.detail)")
            // R-HAND-4: listed, never hosted by this phone.
            #expect(listing.sessions.allSatisfy { $0.hostedBy != phone.account.deviceId() })
            #expect(lines.all().isEmpty, "\(lines.all())")
        }

        @Test("R-IOS-7: after the host lets a session's lease go, its row reads \"last seen …\" within one refresh, never live")
        func leaseLapses() async throws {
            let phone = try await TestFleet.shared.paired()
            let model = SessionListModel(source: RegistrySessions(api: phone.api, storage: phone.storage))
            let formatter = SessionRowFormatter()
            let row = { (id: String) throws -> SessionRow in
                let listing = try #require(model.listing)
                let session = try #require(listing.sessions.first { $0.id == id }, "\(id) is not listed")
                return formatter.row(session, holderName: session.hostedBy.flatMap { listing.deviceNames[$0] }, liveTrusted: model.liveTrusted, now: model.now)
            }

            let id = try await Self.control("", body: "Lease lapses")
            await model.refresh()
            let live = try row(id)
            #expect(live.title == "Lease lapses")
            #expect(live.live, "\(live.detail)")

            // The fleet stops that session's bridge, which lets the lease
            // go, and answers once the registry shows none.
            #expect(try await Self.control("/\(id)/release") == "released")
            await model.refresh()
            let gone = try row(id)
            #expect(!gone.live)
            #expect(gone.detail.contains("last seen "), "\(gone.detail)")
            #expect(!gone.detail.contains("live"), "\(gone.detail)")
        }

        /// POSTs to the fleet's sessions URL plus `path`: its reply's body.
        static func control(_ path: String, body: String = "") async throws -> String {
            let base = try #require(
                ProcessInfo.processInfo.environment["KROWK_SESSIONS_URL"].flatMap { $0.isEmpty ? nil : $0 },
                "KROWK_SESSIONS_URL is unset: run the tests through apps/ios/scripts/with-fleet.sh")
            var request = URLRequest(url: try #require(URL(string: base + path)))
            request.httpMethod = "POST"
            request.httpBody = Data(body.utf8)
            request.timeoutInterval = 60
            let (data, response) = try await URLSession.shared.data(for: request)
            let reply = String(decoding: data, as: UTF8.self)
            try #require((response as? HTTPURLResponse)?.statusCode == 200, "POST \(base + path): \(reply)")
            return reply
        }
    }
}
