import CryptoKit
import Foundation
import UserNotifications

/// The system's notification permission, as `PushRegistration` reads and
/// asks for it: `UNUserNotificationCenter` in the app (`SystemNotifications`),
/// a fake in the tests.
public protocol NotificationAuthorizing: Sendable {
    /// Shows iOS's "Allow krowk to send notifications?" the first time;
    /// after that answers what the person chose, without asking.
    func requestAuthorization(options: UNAuthorizationOptions) async throws -> Bool
    /// What the person chose, now: Settings can change it at any time.
    func authorizationStatus() async -> UNAuthorizationStatus
}

/// Where the phone's APNs token goes: the registry, through krowk-ffi's
/// `Account`, which signs each call with this device's key in Rust
/// (R-CLIENT-1). KrowkAccount implements it (`RegistryPushTokens`), off
/// the main actor; the tests fake it.
public protocol PushTokenSink: Sendable {
    /// This device's id: part of what a registration is compared by, so a
    /// phone that became another device registers again.
    func device() async throws -> String
    /// Registers `tokenHex` for the APNs host `environment` names
    /// (`sandbox` or `production`), replacing this device's last.
    func set(tokenHex: String, environment: String) async throws
    /// This device takes no more pushes. Fine when it had no token.
    func clear() async throws
}

/// `NotificationAuthorizing` over `UNUserNotificationCenter`.
public struct SystemNotifications: NotificationAuthorizing {
    public init() {}

    public func requestAuthorization(options: UNAuthorizationOptions) async throws -> Bool {
        try await UNUserNotificationCenter.current().requestAuthorization(options: options)
    }

    public func authorizationStatus() async -> UNAuthorizationStatus {
        await UNUserNotificationCenter.current().notificationSettings().authorizationStatus
    }
}

/// Keeps the registry's copy of this phone's APNs token current, so the
/// person's other devices can ring it when a session needs an approval or
/// ends a turn (R-IOS-12, R-PERM-2). Asks for permission right after
/// pairing, never at launch or on coming forward (save to finish an ask
/// after pairing that never got its answer: the app quit with the prompt
/// up); re-registers each time the app comes forward and whenever iOS
/// hands over a token, sending nothing when nothing changed since the last
/// registration that landed; clears the registry's copy when the person
/// says no, then or later in Settings, until a clear has landed.
///
/// Keeps only a digest of what was last registered, where the registry
/// stands, and whether an ask after pairing is still owed, in
/// `UserDefaults`: never the token, which it never logs either. The
/// network calls run one at a time, in the order they were asked for.
@MainActor
public final class PushRegistration {
    /// Where the registry stands, as far as this phone knows.
    public enum State: String, Sendable {
        /// The token the digest names landed.
        case registered
        /// A clear was asked for and has not landed yet: tried again while
        /// notifications are off.
        case clearing
        /// A clear landed: nothing to clear again.
        case cleared
    }

    static let digestKey = "krowk.push.digest"
    static let stateKey = "krowk.push.state"
    static let askKey = "krowk.push.askPending"

    private let notifications: any NotificationAuthorizing
    private let sink: any PushTokenSink
    private let environment: String
    private let defaults: UserDefaults
    private let register: @MainActor () -> Void
    private let log: @Sendable (String) -> Void
    /// The call under way, which the next one waits for.
    private var last: Task<Void, Never>?

    /// - Parameters:
    ///   - environment: the APNs host this build's tokens belong to,
    ///     `PushEnvironment.current` in the app.
    ///   - defaults: where the digest and the state are kept.
    ///   - register: `UIApplication.shared.registerForRemoteNotifications()`
    ///     in the app, whose answer comes back through `didRegister`.
    ///   - log: a failure's kind and the core's words; never a token.
    public init(
        notifications: any NotificationAuthorizing,
        sink: any PushTokenSink,
        environment: String,
        defaults: UserDefaults = .standard,
        register: @escaping @MainActor () -> Void,
        log: @escaping @Sendable (String) -> Void = { _ in }
    ) {
        self.notifications = notifications
        self.sink = sink
        self.environment = environment
        self.defaults = defaults
        self.register = register
        self.log = log
    }

    /// Where the registry stands; nil before anything landed.
    public var state: State? {
        defaults.string(forKey: Self.stateKey).flatMap(State.init(rawValue:))
    }

    /// The phone is on the person's devices: the time the app asks.
    /// Allowed, it asks iOS for the token; refused, it clears any token
    /// this device had. Whatever was registered before is registered again.
    public func afterPairing() async {
        defaults.removeObject(forKey: Self.digestKey)
        defaults.set(true, forKey: Self.askKey)
        await ask()
    }

    /// The app came forward (and a paired launch): ask iOS for the token
    /// again when notifications are allowed, clear what the registry may
    /// still have when they are off. Asks the person only to finish the
    /// ask after pairing, when it never got an answer.
    public func onForeground() async {
        let status = await notifications.authorizationStatus()
        if status == .notDetermined, defaults.bool(forKey: Self.askKey) {
            await ask()
        } else if Self.allows(status) {
            register()
        } else if status == .denied {
            await clear(always: false)
        }
    }

    /// iOS's answer to `register`, a new token or the same one again: sent
    /// unless it is what the last registration that landed sent, for this
    /// environment and this device. A token handed over once the person
    /// has turned notifications off is not sent.
    public func didRegister(deviceToken: Data) async {
        let token = deviceToken.map { String(format: "%02x", $0) }.joined()
        await serially { [self] in
            guard Self.allows(await notifications.authorizationStatus()) else { return }
            let digest: String
            do {
                digest = Self.digest(token: token, environment: environment, device: try await sink.device())
            } catch {
                log("push: this device could not be read: \(error)")
                return
            }
            if state == .registered, defaults.string(forKey: Self.digestKey) == digest { return }
            do {
                try await sink.set(tokenHex: token, environment: environment)
            } catch {
                // Not stored: the next time the app comes forward sends it again.
                log("push: the token was not registered: \(error)")
                return
            }
            defaults.set(digest, forKey: Self.digestKey)
            defaults.set(State.registered.rawValue, forKey: Self.stateKey)
        }
    }

    /// iOS could not give a token (no network, or a build without the
    /// entitlement): logged, and tried again the next time the app comes
    /// forward.
    public func didFailToRegister(_ error: any Error) {
        log("push: iOS gave no token: \(error)")
    }

    /// The phone is signed out: what was registered is forgotten here, so
    /// signing in again registers afresh. Clearing the registry's token
    /// takes the key, so a sign-out clears it first (`Account.clearPushToken`
    /// before `signOut`); the registry drops the token of a device revoked
    /// or removed from the person's list by itself.
    public static func signedOut(defaults: UserDefaults = .standard) {
        for key in [digestKey, stateKey, askKey] {
            defaults.removeObject(forKey: key)
        }
    }

    /// Asks the person. An ask that failed, or never got its answer, is
    /// owed still: the next time the app comes forward asks again.
    private func ask() async {
        let granted: Bool
        do {
            granted = try await notifications.requestAuthorization(options: [.alert, .sound, .badge])
        } catch {
            log("push: the permission could not be asked for: \(error)")
            return
        }
        defaults.removeObject(forKey: Self.askKey)
        if granted {
            register()
        } else {
            await clear(always: true)
        }
    }

    /// Clears the registry's token: `always`, or when a token may be there
    /// (registered, or a clear that has not landed), read once the calls
    /// before it have ended. Marked `clearing` until it lands, so one that
    /// failed is tried again the next time the app comes forward with
    /// notifications off; the digest goes only once it landed.
    private func clear(always: Bool) async {
        await serially { [self] in
            guard always || state == .registered || state == .clearing else { return }
            defaults.set(State.clearing.rawValue, forKey: Self.stateKey)
            do {
                try await sink.clear()
            } catch {
                log("push: the token was not cleared: \(error)")
                return
            }
            defaults.removeObject(forKey: Self.digestKey)
            defaults.set(State.cleared.rawValue, forKey: Self.stateKey)
        }
    }

    /// Runs `work` once every call asked for before it has ended.
    private func serially(_ work: @escaping @MainActor () async -> Void) async {
        let previous = last
        let task = Task { @MainActor in
            await previous?.value
            await work()
        }
        last = task
        await task.value
    }

    private static func allows(_ status: UNAuthorizationStatus) -> Bool {
        switch status {
        case .authorized, .provisional, .ephemeral: true
        case .denied, .notDetermined: false
        @unknown default: false
        }
    }

    /// sha256 of `token|environment|device`, hex: what is kept instead of
    /// the token.
    static func digest(token: String, environment: String, device: String) -> String {
        SHA256.hash(data: Data("\(token)|\(environment)|\(device)".utf8)).map { String(format: "%02x", $0) }.joined()
    }
}

/// Which APNs host this build's tokens belong to.
public enum PushEnvironment {
    /// This build's: the simulator's tokens are the sandbox's; on a phone,
    /// what `apsEnvironment` reads from the bundle's provisioning profile.
    /// Not `#if DEBUG`: a Release build run from Xcode still gets a
    /// sandbox token.
    public static var current: String {
        #if targetEnvironment(simulator)
        "sandbox"
        #else
        apsEnvironment(provisioningProfile: Bundle.main.url(forResource: "embedded", withExtension: "mobileprovision").flatMap { try? Data(contentsOf: $0) })
        #endif
    }
}

/// The APNs environment a build signed with `provisioningProfile` (its
/// `embedded.mobileprovision`) gets tokens for. An App Store or TestFlight
/// build carries no profile: `production`. A development build's profile
/// grants `aps-environment` `development`: `sandbox`; an ad hoc one,
/// `production`. The profile is CMS-signed with its plist inside as text,
/// read from `<plist` to `</plist>`. A profile that grants no
/// aps-environment, or can't be read, is not an App Store build's:
/// `sandbox`.
public func apsEnvironment(provisioningProfile: Data?) -> String {
    guard let profile = provisioningProfile else { return "production" }
    guard let start = profile.range(of: Data("<plist".utf8)),
          let end = profile.range(of: Data("</plist>".utf8), in: start.upperBound..<profile.endIndex),
          let plist = try? PropertyListSerialization.propertyList(from: profile[start.lowerBound..<end.upperBound], format: nil),
          let entitlements = (plist as? [String: Any])?["Entitlements"] as? [String: Any],
          let aps = entitlements["aps-environment"] as? String
    else { return "sandbox" }
    return aps == "production" ? "production" : "sandbox"
}
