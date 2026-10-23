// R-IOS-12, R-PERM-2: when the phone asks to send notifications, and what
// it sends the registry, against fakes for iOS's permission and for the
// registry (no Rust, no APNs): asked once after pairing, never at launch or
// on coming forward; a token sent once and again only when it changed; a
// refusal clears it; a failure is tried again. And which APNs environment a
// build's provisioning profile names. The calls through krowk-ffi against
// the stand-in registry are crates/krowk-ffi/tests/push.rs, and the whole
// path on the simulator PairingUITests.
import Foundation
import Synchronization
import Testing
import UserNotifications
@testable import KrowkKit

@Suite("R-IOS-12: PushRegistrationTests, against a fake permission and a fake registry")
@MainActor
struct PushRegistrationTests {
    static let token = Data((0..<32).map { UInt8($0) })
    static let tokenHex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
    static let rotated = Data((0..<32).map { UInt8(255 - $0) })

    /// The registration, with iOS's register call counted and its token
    /// handed back by the test, as the app delegate would.
    @MainActor
    final class Rig {
        let permission: FakePermission
        let registry = FakeRegistry()
        let defaults: UserDefaults
        var registered = 0
        var push: PushRegistration!

        init(_ status: UNAuthorizationStatus = .notDetermined, answer: Bool = true, environment: String = "sandbox") {
            permission = FakePermission(status: status, answer: answer)
            let suite = "krowk.push.tests.\(UUID().uuidString)"
            defaults = UserDefaults(suiteName: suite)!
            defaults.removePersistentDomain(forName: suite)
            push = PushRegistration(
                notifications: permission, sink: registry, environment: environment, defaults: defaults,
                register: { [unowned self] in registered += 1 })
        }
    }

    @Test("R-IOS-12: permission is asked after pairing, never at launch or on foreground")
    func askedOnlyAfterPairing() async {
        let rig = Rig()
        // Made at launch, and brought forward twice: nothing asked or sent.
        await rig.push.onForeground()
        await rig.push.onForeground()
        #expect(rig.permission.asked == 0)
        #expect(rig.registered == 0)
        #expect(rig.registry.calls.isEmpty)

        await rig.push.afterPairing()
        #expect(rig.permission.asked == 1)
        #expect(rig.permission.options == [.alert, .sound, .badge])
        #expect(rig.registered == 1, "allowed: iOS is asked for the token")

        await rig.push.onForeground()
        #expect(rig.permission.asked == 1, "never asked on coming forward")
    }

    @Test("R-IOS-12: a granted permission registers the token once, and an unchanged token on the next foreground sends nothing")
    func registersOnce() async {
        let rig = Rig()
        await rig.push.afterPairing()
        await rig.push.didRegister(deviceToken: Self.token)
        #expect(rig.registry.calls == [.set(Self.tokenHex, "sandbox")])
        #expect(rig.push.state == .registered)

        await rig.push.onForeground()
        #expect(rig.registered == 2, "iOS is asked again each time the app comes forward")
        await rig.push.didRegister(deviceToken: Self.token)
        #expect(rig.registry.calls == [.set(Self.tokenHex, "sandbox")], "nothing sent for the same token")
    }

    @Test("R-IOS-12: a rotated token is sent again")
    func rotatedTokenIsSent() async {
        let rig = Rig()
        await rig.push.afterPairing()
        await rig.push.didRegister(deviceToken: Self.token)
        await rig.push.onForeground()
        await rig.push.didRegister(deviceToken: Self.rotated)
        #expect(rig.registry.calls == [.set(Self.tokenHex, "sandbox"), .set(Self.rotated.map { String(format: "%02x", $0) }.joined(), "sandbox")])
    }

    @Test("R-IOS-12: denied clears the token once and a later allow registers again")
    func deniedClearsOnce() async {
        let rig = Rig()
        await rig.push.afterPairing()
        await rig.push.didRegister(deviceToken: Self.token)

        // Turned off in Settings.
        rig.permission.status = .denied
        await rig.push.onForeground()
        await rig.push.onForeground()
        #expect(rig.registry.calls == [.set(Self.tokenHex, "sandbox"), .clear], "cleared once")
        #expect(rig.push.state == .cleared)
        #expect(rig.registered == 1, "no token asked for while denied")
        // A token iOS hands over anyway is not sent.
        await rig.push.didRegister(deviceToken: Self.token)
        #expect(rig.registry.calls.count == 2)

        // Turned on again: the same token is sent again.
        rig.permission.status = .authorized
        await rig.push.onForeground()
        await rig.push.didRegister(deviceToken: Self.token)
        #expect(rig.registry.calls == [.set(Self.tokenHex, "sandbox"), .clear, .set(Self.tokenHex, "sandbox")])
        #expect(rig.push.state == .registered)
    }

    @Test("R-IOS-12: refused after pairing, the token is cleared and none is asked for")
    func refusedAfterPairing() async {
        let rig = Rig(answer: false)
        await rig.push.afterPairing()
        #expect(rig.permission.asked == 1)
        #expect(rig.registered == 0)
        #expect(rig.registry.calls == [.clear])
        #expect(rig.push.state == .cleared)
    }

    @Test("R-IOS-12: a failed set is retried on the next foreground")
    func failedSetIsRetried() async {
        let rig = Rig()
        await rig.push.afterPairing()
        rig.registry.failing = true
        await rig.push.didRegister(deviceToken: Self.token)
        #expect(rig.push.state == .pending, "no digest for a set that failed, and a token may be there")

        rig.registry.failing = false
        await rig.push.onForeground()
        await rig.push.didRegister(deviceToken: Self.token)
        #expect(rig.registry.calls == [.set(Self.tokenHex, "sandbox"), .set(Self.tokenHex, "sandbox")])
        #expect(rig.push.state == .registered)
    }

    @Test("R-IOS-12: a failed clear is tried again on the next foreground")
    func failedClearIsRetried() async {
        let rig = Rig()
        await rig.push.afterPairing()
        await rig.push.didRegister(deviceToken: Self.token)
        rig.permission.status = .denied
        rig.registry.failing = true
        await rig.push.onForeground()
        #expect(rig.push.state == .pending)
        rig.registry.failing = false
        await rig.push.onForeground()
        #expect(rig.registry.calls == [.set(Self.tokenHex, "sandbox"), .clear, .clear])
        #expect(rig.push.state == .cleared)
    }

    @Test("R-IOS-12: a set that failed may have landed, so notifications turned off then clear it")
    func failedSetIsClearedOnDenial() async {
        let rig = Rig()
        await rig.push.afterPairing()
        rig.registry.failing = true
        await rig.push.didRegister(deviceToken: Self.token)
        rig.registry.failing = false
        rig.permission.status = .denied
        await rig.push.onForeground()
        #expect(rig.registry.calls == [.set(Self.tokenHex, "sandbox"), .clear])
        #expect(rig.push.state == .cleared)
    }

    @Test("R-IOS-12: a set under way when notifications are turned off is followed by a clear")
    func setThenDenied() async throws {
        let rig = Rig(.authorized)
        rig.registry.delay = .milliseconds(100)
        async let set: Void = rig.push.didRegister(deviceToken: Self.token)
        try await until { rig.registry.inFlight > 0 }
        rig.permission.status = .denied
        await rig.push.onForeground()
        await set
        #expect(rig.registry.calls == [.set(Self.tokenHex, "sandbox"), .clear])
        #expect(rig.push.state == .cleared)
        #expect(rig.registry.overlapped == false)
    }

    @Test("R-IOS-12: a token handed over while a clear is under way, allowed again, is sent after it")
    func clearThenAllowed() async throws {
        let rig = Rig()
        await rig.push.afterPairing()
        await rig.push.didRegister(deviceToken: Self.token)
        rig.permission.status = .denied
        rig.registry.delay = .milliseconds(100)
        async let clear: Void = rig.push.onForeground()
        try await until { rig.registry.inFlight > 0 }
        rig.permission.status = .authorized
        await rig.push.didRegister(deviceToken: Self.token)
        await clear
        #expect(rig.registry.calls == [.set(Self.tokenHex, "sandbox"), .clear, .set(Self.tokenHex, "sandbox")])
        #expect(rig.push.state == .registered)
    }

    @Test("R-IOS-12: refused after pairing, a clear that failed is tried again on the next foreground until it lands")
    func failedClearAfterPairingIsRetried() async {
        let rig = Rig(answer: false)
        rig.registry.failing = true
        await rig.push.afterPairing()
        #expect(rig.push.state == .pending)
        await rig.push.onForeground()
        rig.registry.failing = false
        await rig.push.onForeground()
        await rig.push.onForeground()
        #expect(rig.registry.calls == [.clear, .clear, .clear], "tried each time until one landed, then not again")
        #expect(rig.push.state == .cleared)
        #expect(rig.permission.asked == 1)
    }

    @Test("R-IOS-12: an ask after pairing that never got its answer is asked on the next foreground, once")
    func unansweredAskIsFinished() async {
        let rig = Rig()
        rig.permission.failing = true
        await rig.push.afterPairing()
        #expect(rig.permission.asked == 1)
        #expect(rig.registered == 0)
        rig.permission.failing = false
        await rig.push.onForeground()
        #expect(rig.permission.asked == 2)
        #expect(rig.registered == 1)
        await rig.push.onForeground()
        #expect(rig.permission.asked == 2, "answered: never asked again")
        #expect(rig.registered == 2)

        // Signed out, the ask owed is forgotten with the rest.
        let other = Rig()
        other.permission.failing = true
        await other.push.afterPairing()
        PushRegistration.signedOut(defaults: other.defaults)
        other.permission.failing = false
        await other.push.onForeground()
        #expect(other.permission.asked == 1)
    }

    @Test("R-IOS-12: another environment, another device, a new pairing or a sign-out registers the same token again")
    func whatElseRegistersAgain() async {
        let rig = Rig()
        await rig.push.afterPairing()
        await rig.push.didRegister(deviceToken: Self.token)
        rig.registry.deviceID = "ffffffffffffffffffffffffffffffff"
        await rig.push.didRegister(deviceToken: Self.token)
        await rig.push.afterPairing()
        await rig.push.didRegister(deviceToken: Self.token)
        PushRegistration.signedOut(defaults: rig.defaults)
        await rig.push.didRegister(deviceToken: Self.token)
        #expect(rig.registry.calls == Array(repeating: .set(Self.tokenHex, "sandbox"), count: 4))

        let production = Rig(.authorized, environment: "production")
        production.registry.deviceID = rig.registry.deviceID
        production.defaults.set(rig.defaults.string(forKey: PushRegistration.digestKey), forKey: PushRegistration.digestKey)
        production.defaults.set("registered", forKey: PushRegistration.stateKey)
        await production.push.didRegister(deviceToken: Self.token)
        #expect(production.registry.calls == [.set(Self.tokenHex, "production")])
    }

    @Test("R-IOS-12: the app keeps a digest and a state, never the token")
    func keepsNoToken() async {
        let rig = Rig()
        await rig.push.afterPairing()
        await rig.push.didRegister(deviceToken: Self.token)
        let kept = rig.defaults.dictionaryRepresentation().filter { $0.key.hasPrefix("krowk.push.") }
        #expect(Set(kept.keys) == [PushRegistration.digestKey, PushRegistration.stateKey], "the ask is answered: nothing owed")
        #expect(!kept.values.contains { "\($0)".contains(Self.tokenHex) })
        #expect(kept[PushRegistration.digestKey] as? String == PushRegistration.digest(token: Self.tokenHex, environment: "sandbox", device: FakeRegistry.device))
    }

    @Test("R-IOS-12: calls that overlap reach the registry one at a time, in order")
    func oneAtATime() async {
        let rig = Rig(.authorized)
        rig.registry.delay = .milliseconds(50)
        async let first: Void = rig.push.didRegister(deviceToken: Self.token)
        async let second: Void = rig.push.didRegister(deviceToken: Self.token)
        _ = await (first, second)
        #expect(rig.registry.calls == [.set(Self.tokenHex, "sandbox")], "the second saw the first's digest")
        #expect(rig.registry.overlapped == false)
    }
}

@Suite("R-IOS-12: apsEnvironment, from a build's provisioning profile")
struct APSEnvironmentTests {
    /// A profile as Xcode embeds it: CMS bytes around the plist's text.
    static func profile(_ entitlements: String) -> Data {
        var data = Data([0x30, 0x82, 0x2f, 0xa1, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02, 0xa0])
        data.append(Data("""
            <?xml version="1.0" encoding="UTF-8"?>
            <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
            <plist version="1.0">
            <dict>
                <key>AppIDName</key><string>krowk</string>
                <key>Entitlements</key>
                <dict>
                    <key>application-identifier</key><string>TEAM.com.krowk.app</string>
                    \(entitlements)
                </dict>
            </dict>
            </plist>
            """.utf8))
        data.append(Data([0xa0, 0x82, 0x03, 0xff, 0x00]))
        return data
    }

    @Test("R-IOS-12: no profile (App Store, TestFlight) is production")
    func noProfile() {
        #expect(apsEnvironment(provisioningProfile: nil) == "production")
    }

    @Test("R-IOS-12: a development profile is sandbox")
    func development() {
        #expect(apsEnvironment(provisioningProfile: Self.profile("<key>aps-environment</key><string>development</string>")) == "sandbox")
    }

    @Test("R-IOS-12: a production (ad hoc) profile is production")
    func production() {
        #expect(apsEnvironment(provisioningProfile: Self.profile("<key>aps-environment</key><string>production</string>")) == "production")
    }

    @Test("R-IOS-12: a profile without aps-environment, or unreadable, is not the App Store's: sandbox")
    func unreadable() {
        #expect(apsEnvironment(provisioningProfile: Self.profile("")) == "sandbox")
        #expect(apsEnvironment(provisioningProfile: Data([0x30, 0x82, 0x00])) == "sandbox")
        #expect(apsEnvironment(provisioningProfile: Data("<plist version=\"1.0\"><dict>".utf8)) == "sandbox")
    }
}

/// iOS's permission: what it answers when asked, and what Settings says now.
final class FakePermission: NotificationAuthorizing {
    private let state: Mutex<(status: UNAuthorizationStatus, answer: Bool, asked: Int, options: UNAuthorizationOptions, failing: Bool)>

    init(status: UNAuthorizationStatus, answer: Bool) {
        state = Mutex((status, answer, 0, [], false))
    }

    struct Unasked: Error {}

    /// Asks fail, as when the app quits with the prompt up.
    var failing: Bool {
        get { state.withLock { $0.failing } }
        set { state.withLock { $0.failing = newValue } }
    }

    var status: UNAuthorizationStatus {
        get { state.withLock { $0.status } }
        set { state.withLock { $0.status = newValue } }
    }
    var asked: Int { state.withLock { $0.asked } }
    var options: UNAuthorizationOptions { state.withLock { $0.options } }

    func requestAuthorization(options: UNAuthorizationOptions) async throws -> Bool {
        try state.withLock { s in
            s.asked += 1
            s.options = options
            if s.failing { throw Unasked() }
            // Asked once; after that iOS answers what was chosen.
            if s.status == .notDetermined { s.status = s.answer ? .authorized : .denied }
            return s.status == .authorized
        }
    }

    func authorizationStatus() async -> UNAuthorizationStatus {
        status
    }
}

/// The registry: every call that landed or failed, in order.
final class FakeRegistry: PushTokenSink {
    enum Call: Equatable, Sendable {
        case set(String, String)
        case clear
    }

    static let device = "0123456789abcdef0123456789abcdef"

    private struct State {
        var calls: [Call] = []
        var failing = false
        var deviceID = FakeRegistry.device
        var delay: Duration = .zero
        var inFlight = 0
        var overlapped = false
    }

    private let state = Mutex(State())

    var calls: [Call] { state.withLock { $0.calls } }
    var inFlight: Int { state.withLock { $0.inFlight } }
    var overlapped: Bool { state.withLock { $0.overlapped } }
    var failing: Bool {
        get { state.withLock { $0.failing } }
        set { state.withLock { $0.failing = newValue } }
    }
    var deviceID: String {
        get { state.withLock { $0.deviceID } }
        set { state.withLock { $0.deviceID = newValue } }
    }
    var delay: Duration {
        get { state.withLock { $0.delay } }
        set { state.withLock { $0.delay = newValue } }
    }

    struct Refused: Error {}

    func device() async throws -> String {
        deviceID
    }

    func set(tokenHex: String, environment: String) async throws {
        try await call(.set(tokenHex, environment))
    }

    func clear() async throws {
        try await call(.clear)
    }

    private func call(_ call: Call) async throws {
        let delay = state.withLock { s in
            s.inFlight += 1
            if s.inFlight > 1 { s.overlapped = true }
            return s.delay
        }
        if delay > .zero { try? await Task.sleep(for: delay) }
        let failing = state.withLock { s in
            s.inFlight -= 1
            s.calls.append(call)
            return s.failing
        }
        if failing { throw Refused() }
    }
}
