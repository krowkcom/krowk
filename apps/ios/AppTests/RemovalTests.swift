// R-E2E-3, R-IOS-8, R-IOS-12 (ticket 117): a phone that host A removes
// from the person's devices (`krowk devices remove`, the real CLI) is told
// so and stops, and a reset forgets it whole. Against the removal tests'
// own fleet (`RemovalFleet`): removing a device rotates host A's user key,
// which would stop the shared fleet's hosts. A phone signs in and pairs
// over a Keychain storage of its own service, registers a push token and
// lists and follows the fleet's session; host A removes it; then the
// followed link, dropped, asks for a ticket the registry refuses and stops
// without joining again, the listing is
// refused as removed and the session list stops reading, opening a
// session stops its connection before any dial, the registry holds no
// push token for it, and the reset leaves no Keychain item under its
// service — the wrapping key included — so a launch reads it signed out.
//
// Needs the fleets: apps/ios/scripts/with-fleet.sh hands the removal
// fleet's URLs to this process as KROWK_REMOVAL_API_URL,
// KROWK_REMOVAL_RELAY_URL, KROWK_REMOVAL_PAIR_URL and
// KROWK_REMOVAL_DEVICES_URL. Hosted in the app,
// as the Keychain needs.
import Foundation
import Krowk
import KrowkAccount
import KrowkKeys
import KrowkKit
import Security
import Synchronization
import Testing

extension HostedKeychain {
    @Suite("R-E2E-3: a phone removed from host A's devices is told so, stops, and resets to sign-in")
    @MainActor
    struct RemovalTests {
        @Test("R-E2E-3, R-IOS-8, R-IOS-12: removed by host A, the phone lists nothing, stops reading and connecting, and its reset forgets every key and leaves no push token")
        func removedPhoneStopsAndResets() async throws {
            let fleet = try RemovalFleet()
            let service = "\(TestFleet.servicePrefix)removal.\(UUID().uuidString)"
            let marker = URL.temporaryDirectory.appending(path: "krowk-removal-\(UUID().uuidString).installed")
            defer {
                SecItemDelete([kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service] as CFDictionary)
                try? FileManager.default.removeItem(at: marker)
            }
            let storage = try KeychainKeyStorage(service: service, installMarker: marker)
            try await TestFleet.signIn(api: fleet.api, storage: storage, name: "Removal iPhone")
            let name = TestFleet.uniqueName("Removal iPhone")
            let (code, os) = (try await fleet.freshCode(), await TestFleet.os)
            let account = try await blockingCall { try Account.open(api: fleet.api, storage: storage) }
            _ = try await blockingCall { try account.joinPairing(code: code, name: name, os: os) }
            let device = account.deviceId()
            try await blockingCall { try account.setPushToken(tokenHex: String(repeating: "c4", count: 32), environment: "sandbox") }
            #expect(try await fleet.registered()[device] == "sandbox")

            let told = Told()
            let source = RegistrySessions(api: fleet.api, storage: storage, onGone: { told.append($0) })
            #expect(try await blockingCall { source.standing() } == nil, "on the list, before the removal")
            let listed = try await blockingCall { try source.list() }
            let session = try #require(listed.sessions.first, "the removal fleet's session is not listed")
            #expect(told.all().isEmpty)

            // A session followed live from before the removal, on the
            // removal fleet's relay.
            let relay = fleet.relay
            let following = RelayConnection(
                opening: { try account.openSession(id: session.id, known: nil, relayOverride: relay) },
                standing: { source.standing() })
            following.follow { _ in }
            defer { following.stop() }
            try await Self.wait(until: { following.status == .live || following.status == .hostOffline }, within: .seconds(60), "never joined: \(following.status.text)")

            #expect(try await fleet.remove(name) == "removed")

            // The relay doesn't know: the link joined before goes on until
            // it drops. Joining again needs a ticket, which the registry
            // refuses; the device list says why, and nothing joins again.
            let dialed = following.dials
            following.dropConnection()
            try await Self.wait(until: { if case .ended = following.status { true } else { false } }, within: .seconds(60), "never stopped: \(following.status.text)")
            #expect(following.status == .ended(DeviceGone.removed(detail: "").title + "."), "\(following.status.text)")
            #expect(following.dials == dialed, "joined again after it was removed")

            // The session list: refused as removed, onGone told, and it
            // reads no more.
            let counting = CountingSource(source)
            let model = SessionListModel(source: counting)
            model.appeared()
            try await Self.wait(until: { model.gone != nil }, within: .seconds(30), "the list never found the phone removed: \(String(describing: model.failure))")
            guard case .removed(let detail)? = model.gone else {
                Issue.record("not removed: \(String(describing: model.gone))")
                return
            }
            #expect(detail.contains("reset it"), "\(detail)")
            #expect(model.listing == nil)
            let reads = counting.reads
            await model.refresh()
            model.appeared()
            await model.settled()
            #expect(counting.reads == reads, "read again once removed")
            #expect(!told.all().isEmpty && told.all().allSatisfy { if case .removed = $0 { true } else { false } }, "onGone: \(told.all())")

            // A session screen opened now: refused as removed, stopped,
            // never dialed.
            let live = RelayConnection(opening: { try source.openSession(id: session.id, known: nil) })
            live.follow { _ in }
            defer { live.stop() }
            try await Self.wait(until: { if case .ended = live.status { true } else { false } }, within: .seconds(30), live.status.text)
            #expect(live.status == .ended(DeviceGone.removed(detail: "").title + "."), "\(live.status.text)")
            #expect(live.dials == 0)

            // The registry dropped the removed phone's push token with it.
            #expect(try await fleet.registered()[device] == nil)

            // The reset, as the app runs it: the token cleared first (the
            // registry refuses the key: logged, and on), then every key.
            let logged = Told.Lines()
            let reset = PhoneReset(
                clearPushToken: { try await RegistryPushTokens(api: fleet.api, storage: storage).clear() },
                forgetPush: {},
                forgetKeys: { try storage.forgetAll() },
                forgetCache: {},
                log: { logged.append($0) }
            )
            try await reset.run()
            #expect(logged.all().count == 1, "the registry refuses a removed phone's key: \(logged.all())")
            #expect(Self.items(service) == 0, "a Keychain item is left under the phone's service")
            #expect(throws: KrowkError.self) { try storage.read(slot: "credentials.json") }
            // A launch over the same service: signed out, a fresh wrapping
            // key, nothing of the old device.
            let again = try KeychainKeyStorage(service: service, installMarker: marker)
            #expect(try await blockingCall { try Launch.read(api: fleet.api, storage: again) } == .signedOut)
            #expect(try again.read(slot: "device.json") == nil)
        }

        /// Waits up to `limit` for `done`, on the main actor.
        static func wait(until done: () -> Bool, within limit: Duration, _ why: @autoclosure () -> String) async throws {
            let clock = ContinuousClock()
            let deadline = clock.now + limit
            while !done() && clock.now < deadline {
                try await Task.sleep(for: .milliseconds(20))
            }
            try #require(done(), Comment(rawValue: why()))
        }

        /// How many Keychain items are kept under `service`.
        static func items(_ service: String) -> Int {
            var found: CFTypeRef?
            let status = SecItemCopyMatching([
                kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: service,
                kSecMatchLimit as String: kSecMatchLimitAll,
                kSecReturnAttributes as String: true,
            ] as CFDictionary, &found)
            return status == errSecSuccess ? (found as? [[String: Any]])?.count ?? 0 : 0
        }
    }
}

/// The removal tests' own fake fleet: its registry, fresh codes from its
/// host A's `devices add`, and its control's removal.
struct RemovalFleet {
    let api: String
    let relay: String
    let pairURL: String
    let devicesURL: String

    init() throws {
        let env = { (name: String) throws -> String in
            try #require(
                ProcessInfo.processInfo.environment[name].flatMap { $0.isEmpty ? nil : $0 },
                "\(name) is unset: run the tests through apps/ios/scripts/with-fleet.sh, which starts the removal tests' fleet")
        }
        api = try env("KROWK_REMOVAL_API_URL")
        relay = try env("KROWK_REMOVAL_RELAY_URL")
        pairURL = try env("KROWK_REMOVAL_PAIR_URL")
        devicesURL = try env("KROWK_REMOVAL_DEVICES_URL")
    }

    var site: String { api.hasSuffix("/v1") ? String(api.dropLast(3)) : api }

    /// A fresh code: a code works once.
    func freshCode() async throws -> String {
        try await post(pairURL)
    }

    /// `krowk devices remove <name>` as host A: `removed` once the list no
    /// longer names it.
    func remove(_ name: String) async throws -> String {
        try await post(devicesURL + "/remove", body: name)
    }

    /// What the stand-in holds a push token for: each device's environment.
    func registered() async throws -> [String: String] {
        let (body, _) = try await URLSession.shared.data(from: try #require(URL(string: site + "/_push_tokens")))
        let json = try #require(try JSONSerialization.jsonObject(with: body) as? [String: Any])
        let tokens = try #require(json["push_tokens"] as? [[String: Any]])
        return Dictionary(tokens.compactMap { t in (t["device"] as? String).map { ($0, t["environment"] as? String ?? "") } },
                          uniquingKeysWith: { a, _ in a })
    }

    private func post(_ url: String, body: String = "") async throws -> String {
        var request = URLRequest(url: try #require(URL(string: url)))
        request.httpMethod = "POST"
        request.httpBody = Data(body.utf8)
        request.timeoutInterval = 120
        let (data, response) = try await URLSession.shared.data(for: request)
        let reply = String(decoding: data, as: UTF8.self)
        try #require((response as? HTTPURLResponse)?.statusCode == 200, "POST \(url): \(reply)")
        return reply
    }
}

/// What `onGone` was told, in order.
final class Told: Sendable {
    private let seen = Mutex<[DeviceGone]>([])
    func append(_ gone: DeviceGone) { seen.withLock { $0.append(gone) } }
    func all() -> [DeviceGone] { seen.withLock { $0 } }

    /// Lines logged.
    final class Lines: Sendable {
        private let seen = Mutex<[String]>([])
        func append(_ line: String) { seen.withLock { $0.append(line) } }
        func all() -> [String] { seen.withLock { $0 } }
    }
}

/// A source that counts its reads.
final class CountingSource: SessionSource {
    private let source: RegistrySessions
    private let count = Mutex(0)

    init(_ source: RegistrySessions) {
        self.source = source
    }

    var reads: Int { count.withLock { $0 } }

    func list() throws(SessionListFailure) -> SessionListing {
        count.withLock { $0 += 1 }
        return try source.list()
    }
}
