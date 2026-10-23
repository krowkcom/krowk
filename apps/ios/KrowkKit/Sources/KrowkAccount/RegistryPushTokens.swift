import Foundation
import Krowk
import KrowkKit
import Synchronization

/// KrowkKit's `PushTokenSink` over krowk-ffi's `Account` (R-IOS-12,
/// R-CLIENT-1): Rust sends the token to the registry, signed with this
/// device's key, and never logs or keeps it. Each call blocks (a registry
/// call, retried in Rust), so it runs off the main actor, on a thread of
/// its own.
public final class RegistryPushTokens: PushTokenSink {
    public let api: String
    private let storage: any KeyStorage
    /// Opened on first use, off the main actor: opening reads the Keychain.
    private let account = Mutex<Account?>(nil)

    public init(api: String, storage: any KeyStorage) {
        self.api = api
        self.storage = storage
    }

    public func device() async throws -> String {
        try await offMain { account in account.deviceId() }
    }

    public func set(tokenHex: String, environment: String) async throws {
        try await offMain { account in try account.setPushToken(tokenHex: tokenHex, environment: environment) }
    }

    public func clear() async throws {
        try await offMain { account in try account.clearPushToken() }
    }

    /// Runs `call` on a thread of its own: never the main thread, and never
    /// one of Swift's cooperative pool, which a blocking call would hold.
    private func offMain<T: Sendable>(_ call: @escaping @Sendable (Account) throws -> T) async throws -> T {
        try await withCheckedThrowingContinuation { (done: CheckedContinuation<T, any Error>) in
            let thread = Thread { [self] in done.resume(with: Result { try call(try opened()) }) }
            thread.qualityOfService = .utility
            thread.start()
        }
    }

    /// The signed-in account. Blocking.
    private func opened() throws -> Account {
        if let account = account.withLock({ $0 }) { return account }
        let opened = try Account.open(api: api, storage: storage)
        return account.withLock { kept in
            if let kept { return kept }
            kept = opened
            return opened
        }
    }
}
