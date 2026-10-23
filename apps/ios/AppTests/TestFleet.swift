// The fake fleet as the phone's tests meet it (R-IOS-15): its registry's
// URL, its browser approval, fresh pairing codes from its `devices add`,
// and `TestFleet.shared.paired()`, a phone signed in and joined to host A's
// devices, made once per test process and kept for every later test
// (tickets 85–91 open sessions and approvals with it).
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its URLs to this
// process as KROWK_API_URL and KROWK_PAIR_URL. Hosted in the app, as the
// Keychain needs.
import Foundation
import Krowk
import KrowkAccount
import KrowkKeys
import KrowkKit
import Security
import Testing
import UIKit

/// A phone on the person's devices: its account, opened over its own
/// Keychain storage, and the name it joined under.
struct PairedAccount: Sendable {
    let api: String
    let storage: KeychainKeyStorage
    let account: Account
    let deviceName: String
}

actor TestFleet {
    static let shared = TestFleet()

    /// Every fixture's Keychain service starts so: beside the app's
    /// `com.krowk.keys`, which the hosted tests clear after each test, so
    /// this phone outlives them.
    static let servicePrefix = "com.krowk.keys.testfleet."
    /// This test process's own.
    static let service = "\(servicePrefix)\(UUID().uuidString)"

    /// The phone, once asked for: one join per process, however many
    /// tests ask at once.
    private var making: Task<PairedAccount, any Error>?

    /// The paired phone, made on first use: signed in through the fleet's
    /// approval, joined with a fresh code from its `devices add`. A first
    /// attempt that failed is not tried again: every later caller gets its
    /// error.
    func paired() async throws -> PairedAccount {
        if let making { return try await making.value }
        let task = Task { try await Self.make() }
        making = task
        return try await task.value
    }

    private static func make() async throws -> PairedAccount {
        // A test process is often killed rather than let exit, so what an
        // earlier one left is purged here, and this one's at exit too.
        purgeLeftovers()
        atexit { TestFleet.purge() }
        let api = try Self.api()
        let storage = try KeychainKeyStorage(
            service: Self.service,
            installMarker: URL.temporaryDirectory.appending(path: "krowk-testfleet-\(UUID().uuidString).installed"))
        try await Self.signIn(api: api, storage: storage, name: "TestFleet iPhone")
        let name = Self.uniqueName("TestFleet iPhone")
        let (code, os) = (try await Self.freshCode(), await Self.os)
        let account = try await blockingCall { try Account.open(api: api, storage: storage) }
        _ = try await blockingCall { try account.joinPairing(code: code, name: name, os: os) }
        return PairedAccount(api: api, storage: storage, account: account, deviceName: name)
    }

    /// Deletes every item under this process's service.
    nonisolated static func purge() {
        SecItemDelete([
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
        ] as CFDictionary)
    }

    /// Deletes every item of any fixture's service: what an earlier test
    /// process left.
    nonisolated static func purgeLeftovers() {
        var found: CFTypeRef?
        let status = SecItemCopyMatching([
            kSecClass as String: kSecClassGenericPassword,
            kSecMatchLimit as String: kSecMatchLimitAll,
            kSecReturnAttributes as String: true,
        ] as CFDictionary, &found)
        guard status == errSecSuccess, let items = found as? [[String: Any]] else { return }
        let services = Set(items.compactMap { $0[kSecAttrService as String] as? String }.filter { $0.hasPrefix(servicePrefix) })
        for service in services {
            SecItemDelete([kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service] as CFDictionary)
        }
    }

    // MARK: The fleet

    /// The fleet's `/v1` URL.
    static func api() throws -> String {
        try #require(
            ProcessInfo.processInfo.environment["KROWK_API_URL"].flatMap { $0.isEmpty ? nil : $0 },
            "KROWK_API_URL is unset: run the tests through apps/ios/scripts/with-fleet.sh, which starts the fake fleet"
        )
    }

    /// The stand-in's site, which `/v1` hangs off.
    static func site() throws -> String {
        let api = try api()
        return api.hasSuffix("/v1") ? String(api.dropLast(3)) : api
    }

    /// What this phone says it runs, as the app sends it.
    static var os: String {
        get async { await MainActor.run { "iOS \(UIDevice.current.systemVersion)" } }
    }

    /// A name no earlier phone of this fleet run has: host A refuses one
    /// that reads like a listed device's.
    static func uniqueName(_ base: String) -> String {
        "\(base) \(UUID().uuidString.prefix(8))"
    }

    /// A fresh code: the fleet ends its running `devices add` (that code's
    /// pairing) and starts another. A code works once.
    static func freshCode() async throws -> String {
        let url = try #require(
            ProcessInfo.processInfo.environment["KROWK_PAIR_URL"].flatMap { $0.isEmpty ? nil : URL(string: $0) },
            "KROWK_PAIR_URL is unset: run the tests through apps/ios/scripts/with-fleet.sh"
        )
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.timeoutInterval = 60
        let (body, response) = try await URLSession.shared.data(for: request)
        let code = String(decoding: body, as: UTF8.self)
        try #require((response as? HTTPURLResponse)?.statusCode == 200, "POST \(url): \(code)")
        return code
    }

    /// Signs `storage` in through the fleet, playing the browser: the
    /// stand-in approves the sign-in's code as its page's button would.
    /// `api` is a fleet's `/v1` URL: this one's, or the removal tests' own
    /// (`RemovalFleet`).
    static func signIn(api: String, storage: any KeyStorage, name: String) async throws {
        let attempt = try await blockingCall { () throws -> any SignInAttempt in
            try RegistrySignIn(api: api, storage: storage).start(deviceName: name)
        }
        let site = api.hasSuffix("/v1") ? String(api.dropLast(3)) : api
        try await post("/_approve/cli/authorizations/\(attempt.code)/approval", site: site)
        _ = try await blockingCall { try attempt.wait() }
    }

    /// What the approval page's buttons post, to this fleet's site or
    /// `site`.
    static func post(_ path: String, site: String? = nil) async throws {
        let site = try site ?? Self.site()
        var request = URLRequest(url: try #require(URL(string: site + path)))
        request.httpMethod = "POST"
        let (_, response) = try await URLSession.shared.data(for: request)
        try #require((response as? HTTPURLResponse)?.statusCode == 200, "POST \(path)")
    }

    /// The person the key in `storage` speaks for, as the registry's key
    /// lookup names it: what a krowk QR code carries.
    static func userId(api: String, storage: any KeyStorage) async throws -> String {
        let saved = try #require(try storage.read(slot: "credentials.json"))
        let creds = try #require(try JSONSerialization.jsonObject(with: saved) as? [String: Any])
        let token = try #require(creds["token"] as? String)
        var request = URLRequest(url: try #require(URL(string: api + "/key")))
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        let (body, _) = try await URLSession.shared.data(for: request)
        let key = try #require(try JSONSerialization.jsonObject(with: body) as? [String: Any])
        return try #require(key["user_id"] as? String, "no user_id in the key lookup")
    }
}

/// Runs `work`, which blocks (a registry call), on a thread of its own.
func blockingCall<T: Sendable>(_ work: @escaping @Sendable () throws -> T) async throws -> T {
    try await withCheckedThrowingContinuation { (done: CheckedContinuation<T, any Error>) in
        let thread = Thread { done.resume(with: Result { try work() }) }
        thread.start()
    }
}
