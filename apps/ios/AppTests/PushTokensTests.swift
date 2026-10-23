// R-IOS-12, R-CLIENT-1: the paired phone's APNs token reaches the fake
// fleet's registry through RegistryPushTokens, krowk-ffi's
// `Account.setPushToken` and `clearPushToken` under it, signed by the
// phone's device key in Rust, the blocking calls off the main thread; and
// PushRegistration over it sends a token once and clears it when the
// person says no. Read back through the stand-in's `GET /_push_tokens`,
// which names devices and environments, never a token.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh. Hosted in the app, as
// the Keychain needs.
import Foundation
import KrowkAccount
import KrowkKit
import Synchronization
import Testing
import UserNotifications

extension HostedKeychain {
    @Suite("R-IOS-12: the phone's push token at the fake fleet's registry")
    @MainActor
    struct PushTokensTests {
        /// What the stand-in holds a token for: each device's environment.
        static func registered() async throws -> [String: String] {
            let url = try #require(URL(string: try TestFleet.site() + "/_push_tokens"))
            let (body, _) = try await URLSession.shared.data(from: url)
            let json = try #require(try JSONSerialization.jsonObject(with: body) as? [String: Any])
            let tokens = try #require(json["push_tokens"] as? [[String: Any]])
            return Dictionary(tokens.compactMap { t in (t["device"] as? String).map { ($0, t["environment"] as? String ?? "") } },
                              uniquingKeysWith: { a, _ in a })
        }

        @Test("R-IOS-12, R-CLIENT-1: RegistryPushTokens sets, replaces and clears this phone's token, signed in Rust")
        func sinkReachesTheRegistry() async throws {
            let phone = try await TestFleet.shared.paired()
            let sink = RegistryPushTokens(api: phone.api, storage: phone.storage)
            let device = try await sink.device()
            #expect(device == phone.account.deviceId())

            try await sink.set(tokenHex: String(repeating: "a1", count: 32), environment: "sandbox")
            #expect(try await Self.registered()[device] == "sandbox")
            try await sink.set(tokenHex: String(repeating: "b2", count: 32), environment: "production")
            #expect(try await Self.registered()[device] == "production")
            try await sink.clear()
            #expect(try await Self.registered()[device] == nil)
        }

        @Test("R-IOS-12: PushRegistration over the fleet registers a token after pairing, and a refusal in Settings clears it")
        func registrationReachesTheRegistry() async throws {
            let phone = try await TestFleet.shared.paired()
            let sink = RegistryPushTokens(api: phone.api, storage: phone.storage)
            let device = try await sink.device()
            let suite = "krowk.push.hosted.\(UUID().uuidString)"
            let defaults = try #require(UserDefaults(suiteName: suite))
            defer { defaults.removePersistentDomain(forName: suite) }
            let permission = Permission()
            let push = PushRegistration(notifications: permission, sink: sink, environment: "sandbox", defaults: defaults, register: {})

            await push.afterPairing()
            await push.didRegister(deviceToken: Data(repeating: 0xc3, count: 32))
            #expect(push.state == .registered)
            #expect(try await Self.registered()[device] == "sandbox")

            permission.status.withLock { $0 = .denied }
            await push.onForeground()
            #expect(push.state == .cleared)
            #expect(try await Self.registered()[device] == nil)
        }
    }
}

/// iOS's permission, allowed when asked, then turned off by the test.
private final class Permission: NotificationAuthorizing {
    let status = Mutex(UNAuthorizationStatus.notDetermined)

    func requestAuthorization(options: UNAuthorizationOptions) async throws -> Bool {
        status.withLock { $0 = .authorized }
        return true
    }

    func authorizationStatus() async -> UNAuthorizationStatus {
        status.withLock { $0 }
    }
}
