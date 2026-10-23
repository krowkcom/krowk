import Foundation

/// The phone is no longer one of the person's devices (R-E2E-3; Canon
/// devices.md → Removing a device, Starting over), as krowk-ffi's
/// `Account.checkDeviceList` says: removed, or left off a device list that
/// was started over. Either way it has stopped syncing, and the one way on
/// is to reset it and pair it again. It never adopts a new list by itself.
public enum DeviceGone: Error, Equatable, Sendable {
    /// `device_removed`: `krowk devices remove` took it off the list, or
    /// the registry refuses its key (revoked on the dashboard, or left
    /// behind by a start-over).
    case removed(detail: String)
    /// `device_list_reset`: the list was started over, under a new chain
    /// epoch, without it.
    case listReset(detail: String)

    /// The screen's headline.
    public var title: String {
        switch self {
        case .removed: "This iPhone was removed from your devices"
        case .listReset: "Your device list was started over"
        }
    }

    /// What it means, and what to do.
    public var explanation: String {
        "It has stopped syncing and can't open your sessions. Reset this iPhone to forget its keys, then sign in and pair it again from one of your devices."
    }

    /// The core's words: which of the cases, and when a list was started
    /// over. Names no session.
    public var detail: String {
        switch self {
        case .removed(let detail), .listReset(let detail): detail
        }
    }

    /// What the log may say.
    public var kind: String {
        switch self {
        case .removed: "device_removed"
        case .listReset: "device_list_reset"
        }
    }
}

/// Checks, while the paired app is in the foreground, that the phone is
/// still on the person's devices: at once, then every `period`, and again
/// whenever the app comes forward (`checkNow`). The first check that finds
/// it gone tells `onGone`, once, and the watch stops: nothing checks or
/// retries after that. The check blocks (a registry call or two): it runs
/// on a thread of its own, never the main thread. A check that fails for
/// any other reason (the registry out of reach) is tried at the next tick.
@MainActor
public final class StandingWatch {
    /// How often the foreground checks: a session screen open for a long
    /// time hears of a removal within a minute, and the session list
    /// checks with every read besides.
    public static let period: Duration = .seconds(60)

    private let check: @Sendable () -> DeviceGone?
    private let period: Duration
    private let onGone: @MainActor (DeviceGone) -> Void
    private var timer: Task<Void, Never>?
    private var checking = false
    /// Found gone: told once, and nothing runs again.
    public private(set) var gone: DeviceGone?

    /// - Parameters:
    ///   - check: blocking; nil while the phone is on the list, or when
    ///     the check couldn't say.
    public init(
        check: @escaping @Sendable () -> DeviceGone?,
        period: Duration = StandingWatch.period,
        onGone: @escaping @MainActor (DeviceGone) -> Void
    ) {
        self.check = check
        self.period = period
        self.onGone = onGone
    }

    /// Checks now, then every `period` until `stop`.
    public func start() {
        guard gone == nil, timer == nil else { return }
        let period = period
        timer = Task { [weak self] in
            while !Task.isCancelled {
                do {
                    guard let watch = self else { return }
                    await watch.runCheck()
                }
                try? await Task.sleep(for: period)
            }
        }
    }

    /// The app came forward: checked now, unless a check is under way.
    public func checkNow() {
        Task { await runCheck() }
    }

    /// The app went to the background, or the screens it watched for
    /// went away.
    public func stop() {
        timer?.cancel()
        timer = nil
    }

    private func runCheck() async {
        guard gone == nil, !checking else { return }
        checking = true
        let check = check
        let found = await blocking { check() }
        checking = false
        guard gone == nil, let found else { return }
        report(found)
    }

    /// Another part of the app found it gone (the session list's read, a
    /// session screen's open): the watch stops, and `onGone` hears it once.
    public func report(_ found: DeviceGone) {
        guard gone == nil else { return }
        gone = found
        stop()
        onGone(found)
    }
}

/// Resetting a phone that is no longer on the person's devices, in order:
/// the registry's copy of its push token cleared first (it takes the
/// phone's key, so it must come before the keys go; the registry already
/// dropped the token of a removed device, and refuses the key, so a
/// failure here is logged and the reset goes on), then what this phone
/// keeps about push forgotten and iOS's registration dropped, then every
/// key forgotten — the device's keys, the user keys, the device list, the
/// API key and the Secure Enclave wrapping key that sealed them — so the
/// next launch is a fresh install's: sign-in, and a new device to pair.
public struct PhoneReset: Sendable {
    private let clearPushToken: @Sendable () async throws -> Void
    private let forgetPush: @MainActor @Sendable () -> Void
    private let forgetKeys: @Sendable () throws -> Void
    private let log: @Sendable (String) -> Void

    /// - Parameters:
    ///   - clearPushToken: `Account.clearPushToken`, off the main thread.
    ///   - forgetPush: `PushRegistration.signedOut` and iOS's
    ///     `unregisterForRemoteNotifications`.
    ///   - forgetKeys: every Keychain item the app keeps, deleted; blocking.
    ///   - log: a failure's kind and the core's words; never a key.
    public init(
        clearPushToken: @escaping @Sendable () async throws -> Void,
        forgetPush: @escaping @MainActor @Sendable () -> Void,
        forgetKeys: @escaping @Sendable () throws -> Void,
        log: @escaping @Sendable (String) -> Void = { _ in }
    ) {
        self.clearPushToken = clearPushToken
        self.forgetPush = forgetPush
        self.forgetKeys = forgetKeys
        self.log = log
    }

    /// Runs the reset. Throws only when the keys couldn't be forgotten
    /// (the Keychain refused), and then it may be run again.
    public func run() async throws {
        do {
            try await clearPushToken()
        } catch {
            log("reset: the push token was not cleared at the registry (it drops a removed device's by itself): \(error)")
        }
        await forgetPush()
        let forgetKeys = forgetKeys
        let forgotten = await blocking { Result { try forgetKeys() } }
        try forgotten.get()
    }
}

/// The reminder after pairing (ticket 117), in the words the terminal uses
/// for the recovery kit (`krowk sync init`, `krowk sync status`; Canon
/// devices.md → Recovery): shown once, on the screen that says the phone
/// is on the person's devices. What it says hangs on whether the person's
/// device list holds a kit.
public enum RecoveryKitReminder {
    public static func sentence(hasKit: Bool) -> String {
        if hasKit {
            "Your recovery kit: the only way back in if you lose every device. Keep its 12 words somewhere safe, off your devices; `krowk sync recovery check` on your computer tests them."
        } else {
            "No recovery kit: if you lose every device you lose every session, and a stolen device that removes the others first can lock you out — `krowk sync recovery new` on your computer makes one."
        }
    }
}
