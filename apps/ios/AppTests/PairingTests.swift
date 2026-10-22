// R-E2E-3, R-CLIENT-1, R-HAND-4: the iPhone joins the person's devices with
// the code `krowk devices add` shows, against the fake fleet, whose host A
// runs the real `krowk devices add` and says yes by itself: the pairing
// screen's model, krowk-ffi's join under it (SPAKE2, the payload check,
// the device-list pin and the claim, all in Rust), and the user keys and
// the device list landing in the Keychain through KeychainKeyStorage.
// A code works once, so each join asks the fleet for a fresh one
// (KROWK_PAIR_URL).
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh. Hosted in the app, as
// the Keychain needs.
import Foundation
import Krowk
import KrowkAccount
import KrowkKeys
import KrowkKit
import Synchronization
import Testing

/// The slots a join keeps, and only a join.
private let joinedSlots = ["user-keys.json", "device-list.json"]

extension HostedKeychain {
    @Suite("R-E2E-3: the iPhone joins the fake fleet's devices with a typed or scanned code")
    @MainActor
    struct PairingTests {
        let api: String
        let storage: KeychainKeyStorage

        init() throws {
            api = try TestFleet.api()
            // A first launch: the storage starts from an empty service.
            storage = try KeychainKeyStorage(
                installMarker: URL.applicationSupportDirectory.appending(path: "krowk-tests/\(UUID().uuidString).installed"))
        }

        @Test("R-E2E-3, R-CLIENT-1, R-HAND-4: TestFleet's phone joined with the fleet's code: the Keychain holds the user keys and a device list naming this phone, the claim was made, and it hosts nothing")
        func joined() async throws {
            let phone = try await TestFleet.shared.paired()
            // Kept by the join, through KeychainKeyStorage.
            for slot in joinedSlots {
                #expect(try phone.storage.read(slot: slot) != nil, "no \(slot)")
            }
            #expect(try phone.account.hasUserKeys())
            // The device list it pinned names this phone, with its name and
            // os, beside host A.
            let devices = try phone.account.devices()
            let me = try #require(devices.first { $0.thisDevice }, "this phone is not on its device list: \(devices)")
            #expect(me.id == phone.account.deviceId())
            #expect(me.name == phone.deviceName)
            #expect(me.os == (await TestFleet.os), "\(me.os)")
            #expect(devices.contains { $0.name == "host-a" && !$0.thisDevice }, "\(devices)")
            // The join returns only once claim_key_device succeeded (its
            // last step, polled every 500 ms for up to 60 s): a join that
            // could not claim is `not_added`, and keeps nothing. A launch
            // now skips onboarding.
            #expect(try Launch.read(api: phone.api, storage: phone.storage) == .paired)
            // R-HAND-4: a viewer, never a host: it reads the person's
            // sessions, and none is hosted by it.
            let account = phone.account
            let sessions = try await blockingCall { try account.listSessions() }
            #expect(!sessions.sessions.isEmpty, "the fleet's session is not listed")
            #expect(sessions.sessions.allSatisfy { $0.hostDevice != phone.account.deviceId() })
        }

        @Test("R-E2E-3: a wrong code says a code works once, keeps nothing, and ends the fleet's pairing; a fresh code then joins through the screen")
        func wrongCodeThenFresh() async throws {
            try await TestFleet.signIn(api: api, storage: storage, name: "Pairing iPhone")
            let name = TestFleet.uniqueName("Pairing iPhone")
            let lines = Lines()
            let model = PairingModel(
                service: RegistryPairing(api: api, storage: storage, name: name, os: await TestFleet.os),
                camera: AllowedCamera(), scannerSupported: false, deviceName: name, log: { lines.append($0) })

            let code = try await TestFleet.freshCode()
            // A whole code starts at once, as typed.
            model.enter((code.first == "2" ? "3" : "2") + code.dropFirst())
            try await eventually(seconds: 60) { model.failure != nil }
            #expect(model.phase == .entering(.codeUsedUp), "\(model.phase)")
            #expect(
                model.failureSentence
                    == "The pairing failed, and a code works once — on 'host-a' run `krowk devices add` again for a new code. Nothing was kept.")
            // Nothing kept: no user keys, no device list.
            for slot in joinedSlots {
                #expect(try storage.read(slot: slot) == nil, "\(slot) was kept")
            }
            #expect(try Launch.read(api: api, storage: storage) != .paired)
            // The fleet's pairing is dead: to this phone, a party to it, the
            // registry says it has ended (`pairing_gone`); nothing else is
            // open to pair with.
            let account = try Account.open(api: api, storage: storage)
            do {
                let open = try account.openPairing()
                Issue.record("a pairing is still open: \(open)")
            } catch KrowkError.Refused(code: let code, _) where code == "pairing_gone" || code == "no_pairing" {}
            #expect(!lines.all().joined().contains(code), "the code was logged")

            // A fresh code joins, through the same screen.
            model.enter(try await TestFleet.freshCode())
            // Named before the join; host A answers by itself, so the name
            // may be on screen only briefly.
            var named = false
            try await eventually(seconds: 90) {
                if model.phase == .pairing(with: "host-a") { named = true }
                return model.phase == .paired || model.failure != nil
            }
            #expect(named || model.laptopName == "host-a", "the laptop was never named")
            #expect(model.phase == .paired, "\(model.phase)")
            #expect(try Launch.read(api: api, storage: storage) == .paired)
            #expect(try account.devices().contains { $0.thisDevice && $0.name == name })
        }

        @Test("R-E2E-3: scanned QR codes: another app's is ignored, another account's or another device's pairing's is refused before the code is spent, and krowk's own joins")
        func scanned() async throws {
            try await TestFleet.signIn(api: api, storage: storage, name: "Scanning iPhone")
            let name = TestFleet.uniqueName("Scanning iPhone")
            let model = PairingModel(
                service: RegistryPairing(api: api, storage: storage, name: name, os: await TestFleet.os),
                camera: AllowedCamera(), scannerSupported: true, deviceName: name)
            let code = try await TestFleet.freshCode().replacingOccurrences(of: "-", with: "")
            let account = try Account.open(api: api, storage: storage)
            let laptop = try account.openPairing()
            let user = try await TestFleet.userId(api: api, storage: storage)
            let qr = { (user: String, device: String) in "krowk://pair?v=1&code=\(code)&user=\(user)&device=\(device)" }

            model.scan()
            #expect(model.scanner == .open)
            // Not krowk's: ignored, the scanner still open.
            model.scanned("https://example.com/menu")
            #expect(model.scanner == .open && model.phase == .entering(nil))

            // Another account's: naming this pairing's device, or another.
            for device in [laptop.device, "0123456789abcdef0123456789abcdef"] {
                model.scanned(qr("0192f5a0-7e1c-7cc3-9d1a-5b6f00000001", device))
                try await eventually(seconds: 30) { model.failure != nil }
                #expect(model.phase == .entering(.otherAccount))
                #expect(model.failureSentence == "This code is for another krowk account.")
                #expect(try account.openPairing() == laptop, "the pairing is still open: nothing was spent")
            }

            // Shown for a pairing that isn't the open one.
            model.scanned(qr(user, "0123456789abcdef0123456789abcdef"))
            try await eventually(seconds: 30) { model.phase == .entering(.otherDevice) }
            #expect(try account.openPairing() == laptop, "the pairing is still open: nothing was spent")

            // krowk's own, from the open pairing: joins.
            model.scanned(qr(user, laptop.device))
            try await eventually(seconds: 90) { model.phase == .paired || model.failure != nil }
            #expect(model.phase == .paired, "\(model.phase)")
            for slot in joinedSlots {
                #expect(try storage.read(slot: slot) != nil, "no \(slot)")
            }
        }
    }
}

@Suite("R-E2E-3, R-CLIENT-1: what the pairing screen takes, read by krowk-client's parsers")
@MainActor
struct PairingParseTests {
    @Test(
        "R-E2E-3: a code with a symbol outside krowk's alphabet, or of the wrong length, is refused with no network call",
        arguments: ["K7QF-9M3I", "K7QF-9M3O", "K7QF-9M31", "K7QF-9M3", "K7QF-9M3XA", "K7QF_9M3X", ""]
    )
    func notACode(text: String) async throws {
        let registry = Counting()
        let model = PairingModel(service: registry, camera: AllowedCamera(), scannerSupported: false, deviceName: "iPhone")
        model.enter(text)
        model.submitCode()
        #expect(model.phase == .entering(.notACode))
        try await Task.sleep(for: .milliseconds(100))
        #expect(registry.attempts.value == 0, "a network call was made")
    }

    @Test("R-E2E-3: case, spaces and dashes are ignored")
    func codes() {
        let registry = Counting()
        #expect(registry.parseCode("k7qf 9m3x") == "K7QF-9M3X")
        #expect(registry.parseCode("K7QF9M3X") == "K7QF-9M3X")
        #expect(registry.parseCode(" k7-qf-9m-3x ") == "K7QF-9M3X")
    }

    @Test("R-E2E-3: krowk's QR code gives its code and ids; anything else is not an invite")
    func invites() {
        let registry = Counting()
        let invite = registry.parseInvite(
            "krowk://pair?v=1&code=K7QF9M3X&user=0192f5a0-7e1c-7cc3-9d1a-5b6f00000001&device=0123456789abcdef0123456789abcdef")
        #expect(
            invite
                == PairingInvitation(
                    code: "K7QF-9M3X", userId: "0192f5a0-7e1c-7cc3-9d1a-5b6f00000001", device: "0123456789abcdef0123456789abcdef"))
        let shown = invite.map { String(describing: $0) } ?? ""
        #expect(!shown.isEmpty && !shown.contains("K7QF"), "an invite's description shows its code: \(shown)")
        for other in [
            "https://example.com/menu",
            "K7QF-9M3X",
            "krowk://pair?v=2&code=K7QF9M3X&user=u&device=0123456789abcdef0123456789abcdef",
            "krowk://pair?v=1&code=K7QF9M3X&user=u&device=0123456789ABCDEF0123456789ABCDEF",
            "krowk://pair?v=1&code=K7QF9M3X&user=u&device=0123456789abcdef0123456789abcdef&extra=1",
        ] {
            #expect(registry.parseInvite(other) == nil, "\(other)")
        }
    }
}

/// krowk-ffi's parsers, with a registry that only counts what would reach
/// it: no attempt may be made.
final class Counting: PairingService {
    let parsers = RegistryPairing(api: "http://127.0.0.1:9/v1", storage: Refusing(), name: "iPhone", os: "iOS")
    let attempts = Count()

    func parseCode(_ text: String) -> String? { parsers.parseCode(text) }
    func parseInvite(_ payload: String) -> PairingInvitation? { parsers.parseInvite(payload) }

    func attempt() -> any PairingAttempt {
        attempts.add()
        return parsers.attempt()
    }
}

final class Count: Sendable {
    private let n = Mutex(0)
    func add() { n.withLock { $0 += 1 } }
    var value: Int { n.withLock { $0 } }
}

/// A camera the person allowed.
@MainActor
final class AllowedCamera: CameraAccess {
    var status: CameraStatus { .allowed }
    func request() async -> Bool { true }
}
