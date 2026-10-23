// R-E2E-3: the pairing screen's rules, against a stubbed registry (no
// Rust, no Keychain): what a typed code and a scanned QR code start, what
// is refused before a code is spent, the camera, cancelling, and what each
// ending says. The same model over krowk-ffi, its parsers and the fake
// fleet is AppTests' PairingTests.
import Foundation
import Synchronization
import Testing
@testable import KrowkKit

@Suite("R-E2E-3: the pairing screen, against a stubbed registry")
@MainActor
struct PairingModelTests {
    nonisolated static let laptop = OpenedPairing(device: "0123456789abcdef0123456789abcdef", deviceName: "Elvinas's MacBook")
    nonisolated static let invite = PairingInvitation(code: "K7QF-9M3X", userId: "u_1", device: laptop.device)
    nonisolated static let qr = "krowk://pair?v=1&code=K7QF-9M3X&user=u_1&device=0123456789abcdef0123456789abcdef"

    @Test("R-E2E-3: a typed code is upper-cased as typed, starts as soon as the parser takes it, names the laptop and joins with that code once")
    func typedCodeJoins() async throws {
        let registry = StubPairing(open: .success(Self.laptop))
        let paired = Box(false)
        let joined = Box(0)
        let model = Self.model(registry, onPaired: { paired.value = true }, onJoined: { joined.value += 1 })
        model.enter("k7qf 9m3")
        #expect(model.codeText == "K7QF 9M3")
        #expect(model.phase == .entering(nil))
        model.enter("k7qf 9m3x")
        #expect(model.phase == .finding, "a whole code starts at once")
        #expect(model.codeText.isEmpty, "a spent code is cleared")
        // Another whole code while one is under way starts nothing.
        model.enter("ABCD-EFGH")
        model.submitCode()
        #expect(model.phase == .finding && model.codeText.isEmpty)
        try await until { model.phase == .pairing(with: "Elvinas's MacBook") }
        registry.finish(.success(()))
        try await until { model.phase == .paired }
        // R-IOS-12: on the devices, before Continue: where the app asks to
        // send notifications, once.
        #expect(joined.value == 1 && !paired.value)
        #expect(registry.joined == [.code("K7QF-9M3X")])
        #expect(registry.parsedCodes == ["K7QF 9M3", "K7QF 9M3X"])
        model.finish()
        #expect(paired.value)
        #expect(joined.value == 1)
    }

    @Test("R-E2E-3: what isn't a code is refused before anything is sent")
    func notACode() async throws {
        let registry = StubPairing(open: .success(Self.laptop))
        let model = Self.model(registry)
        model.enter("K7QF-9M3I")
        #expect(model.phase == .entering(nil))
        model.submitCode()
        #expect(model.phase == .entering(.notACode))
        #expect(registry.opened == 0 && registry.joined.isEmpty)
        #expect(model.failureSentence?.contains("Nothing was sent") == true)
    }

    @Test("R-E2E-3: a scanned krowk QR code from the open pairing's device joins with its invite")
    func scannedInviteJoins() async throws {
        let registry = StubPairing(open: .success(Self.laptop))
        let model = Self.model(registry, scanner: true)
        model.scan()
        #expect(model.scanner == .open)
        model.scanned(Self.qr)
        #expect(model.scanner == .closed)
        try await until { model.phase == .pairing(with: "Elvinas's MacBook") }
        registry.finish(.success(()))
        try await until { model.phase == .paired }
        #expect(registry.joined == [.invite(Self.invite)])
    }

    @Test("R-E2E-3: a QR code that isn't krowk's is ignored and the scanner keeps looking")
    func otherQRIgnored() async throws {
        let registry = StubPairing(open: .success(Self.laptop))
        let model = Self.model(registry, scanner: true)
        model.scan()
        model.scanned("https://example.com/menu")
        #expect(model.scanner == .open)
        #expect(model.phase == .entering(nil))
        #expect(registry.opened == 0 && registry.joined.isEmpty)
    }

    @Test("R-E2E-3: a QR code shown for another device's pairing never names the open one, and the join's refusal (before the code is spent) says so")
    func otherDeviceRefused() async throws {
        let registry = StubPairing(open: .success(OpenedPairing(device: "fedcba9876543210fedcba9876543210", deviceName: "Other")))
        let model = Self.model(registry, scanner: true)
        model.scan()
        model.scanned(Self.qr)
        try await until { registry.joined.count == 1 }
        #expect(model.phase == .finding, "not \"Pairing with Other\": the code wasn't shown there")
        registry.finish(.failure(.otherDevice))
        try await until { model.phase == .entering(.otherDevice) }
        #expect(model.failureSentence == "This code is from a different device's pairing — run `krowk devices add` again.")
    }

    @Test("R-E2E-3: another account's QR code says so, whichever pairing is open, or none")
    func otherAccountRefused() async throws {
        for open: Result<OpenedPairing, PairingFailure> in [
            .success(Self.laptop), .success(OpenedPairing(device: "fedcba9876543210fedcba9876543210", deviceName: "Other")),
            .failure(.noPairing),
        ] {
            let registry = StubPairing(open: open)
            let model = Self.model(registry, scanner: true)
            model.scan()
            model.scanned(Self.qr)
            try await until { registry.joined.count == 1 }
            registry.finish(.failure(.otherAccount))
            try await until { model.phase == .entering(.otherAccount) }
            #expect(model.failureSentence == "This code is for another krowk account.")
        }
    }

    @Test("R-E2E-3: a typed code with no pairing open says to run krowk devices add first, and never joins")
    func noPairing() async throws {
        let registry = StubPairing(open: .failure(.noPairing))
        let model = Self.model(registry)
        model.enter("K7QF-9M3X")
        try await until { model.phase == .entering(.noPairing) }
        #expect(registry.joined.isEmpty)
    }

    @Test("R-E2E-3: the scan button is absent where the scanner is unsupported, and scan does nothing there")
    func noScannerWhereUnsupported() {
        let model = Self.model(StubPairing(open: .success(Self.laptop)), scanner: false)
        #expect(!model.scannerSupported)
        model.scan()
        #expect(model.scanner == .closed)
    }

    @Test("R-E2E-3: a camera refused says so, a restricted one or one that won't start says to type; one not asked yet is asked once, and opens the scanner when allowed")
    func cameraPermission() async throws {
        let camera = StubCamera(.refused)
        let model = Self.model(StubPairing(open: .success(Self.laptop)), scanner: true, camera: camera)
        model.scan()
        #expect(model.scanner == .refused)
        model.closeScanner()
        camera.status = .restricted
        model.scan()
        #expect(model.scanner == .unavailable)
        camera.status = .allowed
        model.scan()
        #expect(model.scanner == .open)
        model.scannerFailed()
        #expect(model.scanner == .unavailable)
        model.closeScanner()
        camera.status = .notAsked
        camera.answer = false
        model.scan()
        try await until { model.scanner == .refused }
        camera.status = .notAsked
        camera.answer = true
        model.scan()
        try await until { model.scanner == .open }
        #expect(camera.asked == 2)
    }

    @Test("R-E2E-3: a wrong code says a code works once, naming the laptop, and goes back to the code")
    func wrongCode() async throws {
        let registry = StubPairing(open: .success(Self.laptop))
        let model = Self.model(registry)
        model.enter("K7QF-9M3X")
        try await until { if case .pairing = model.phase { true } else { false } }
        registry.finish(.failure(.codeUsedUp))
        try await until { model.phase == .entering(.codeUsedUp) }
        #expect(
            model.failureSentence
                == "The pairing failed, and a code works once — on 'Elvinas's MacBook' run `krowk devices add` again for a new code. Nothing was kept.")
        #expect(registry.joined.count == 1, "never tried again")
    }

    @Test("R-E2E-3: no open pairing says to run krowk devices add first; the registry's words go to the log only")
    func sentences() async throws {
        #expect(PairingFailure.noPairing.sentence(laptop: nil) == "Nothing to pair with yet: run `krowk devices add` on your laptop first.")
        #expect(PairingFailure.codeUsedUp.sentence(laptop: nil).contains("on your laptop run `krowk devices add` again"))
        // Before the code was sent: it wasn't used.
        #expect(PairingFailure.other(detail: "x").sentence(laptop: "host").contains("the code wasn't used"))
        let logged = Box<[String]>([])
        let registry = StubPairing(open: .failure(.other(detail: "<the registry's words>")))
        let model = Self.model(registry, log: { logged.value.append($0) })
        model.enter("K7QF-9M3X")
        try await until { model.phase == .entering(.other(detail: "<the registry's words>")) }
        #expect(!(model.failureSentence ?? "").contains("registry's words"))
        #expect(logged.value == ["pairing: <the registry's words>"])
        #expect(!logged.value.joined().contains("K7QF"), "no code is ever logged")
    }

    @Test("R-E2E-3: Cancel ends the join and the pairing, and goes back to the code")
    func cancel() async throws {
        let registry = StubPairing(open: .success(Self.laptop))
        let model = Self.model(registry)
        model.enter("K7QF-9M3X")
        try await until { if case .pairing = model.phase { true } else { false } }
        model.cancel()
        #expect(model.phase == .entering(nil))
        #expect(registry.cancelled == 1)
        registry.finish(.failure(.cancelled))
        try await Task.sleep(for: .milliseconds(100))
        #expect(model.phase == .entering(nil))
    }

    @Test("R-E2E-3: a cancel too late to count (the keys were kept) still lands on paired")
    func lateCancel() async throws {
        let registry = StubPairing(open: .success(Self.laptop))
        let model = Self.model(registry)
        model.enter("K7QF-9M3X")
        try await until { if case .pairing = model.phase { true } else { false } }
        model.cancel()
        registry.finish(.success(()))
        try await until { model.phase == .paired }
    }

    @Test("R-E2E-3: a cancel while the pairing is being looked up never joins")
    func cancelBeforeJoin() async throws {
        let gate = Box(false)
        let registry = StubPairing(open: .success(Self.laptop), gate: gate)
        let model = Self.model(registry)
        model.enter("K7QF-9M3X")
        #expect(model.phase == .finding)
        model.cancel()
        gate.value = true
        try await Task.sleep(for: .milliseconds(200))
        #expect(registry.joined.isEmpty)
        #expect(model.phase == .entering(nil))
    }

    @Test("R-E2E-3: leaving the screen cancels the join")
    func leavingCancels() async throws {
        let registry = StubPairing(open: .success(Self.laptop))
        var model: PairingModel? = Self.model(registry)
        model?.enter("K7QF-9M3X")
        try await until { if case .pairing = model?.phase { true } else { false } }
        model = nil
        try await until { registry.cancelled == 1 }
        registry.finish(.failure(.cancelled))
    }

    // MARK: Helpers

    static func model(
        _ registry: StubPairing, scanner: Bool = false, camera: StubCamera = StubCamera(.allowed),
        log: @escaping @Sendable (String) -> Void = { _ in }, onPaired: @escaping @MainActor () -> Void = {},
        onJoined: @escaping @MainActor () -> Void = {}
    ) -> PairingModel {
        PairingModel(
            service: registry, camera: camera, scannerSupported: scanner, deviceName: "Test iPhone", log: log, onPaired: onPaired,
            onJoined: onJoined)
    }
}

/// The registry, stubbed: `parseCode` knows krowk's alphabet only well
/// enough for these tests (Rust's parser is AppTests'); a join blocks until
/// `finish`.
final class StubPairing: PairingService, PairingAttempt, @unchecked Sendable {
    private let lock = NSLock()
    private let open: Result<OpenedPairing, PairingFailure>
    private let gate: Box<Bool>?
    private var result: Result<Void, PairingFailure>?
    private var _opened = 0
    private var _joined: [PairingTicket] = []
    private var _cancelled = 0
    private var _parsedCodes: [String] = []

    var opened: Int { locked { _opened } }
    var joined: [PairingTicket] { locked { _joined } }
    var cancelled: Int { locked { _cancelled } }
    var parsedCodes: [String] { locked { _parsedCodes } }

    init(open: Result<OpenedPairing, PairingFailure>, gate: Box<Bool>? = nil) {
        self.open = open
        self.gate = gate
    }

    private func locked<T>(_ body: () -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return body()
    }

    func parseCode(_ text: String) -> String? {
        locked { _parsedCodes.append(text) }
        let symbols = text.uppercased().filter { $0 != " " && $0 != "-" }
        guard symbols.count == 8, symbols.allSatisfy({ "23456789ABCDEFGHJKMNPQRSTVWXYZ".contains($0) }) else { return nil }
        return "\(symbols.prefix(4))-\(symbols.suffix(4))"
    }

    func parseInvite(_ payload: String) -> PairingInvitation? {
        payload == PairingModelTests.qr ? PairingModelTests.invite : nil
    }

    func attempt() -> any PairingAttempt { self }

    func openPairing() throws(PairingFailure) -> OpenedPairing {
        while let gate, !gate.value { Thread.sleep(forTimeInterval: 0.01) }
        locked { _opened += 1 }
        return try open.get()
    }

    func join(_ ticket: PairingTicket) throws(PairingFailure) {
        locked { _joined.append(ticket) }
        while true {
            if let result = locked({ result }) { return try result.get() }
            Thread.sleep(forTimeInterval: 0.01)
        }
    }

    func cancel() {
        locked { _cancelled += 1 }
    }

    func finish(_ result: Result<Void, PairingFailure>) {
        locked { self.result = result }
    }
}

@MainActor
final class StubCamera: CameraAccess {
    var status: CameraStatus
    var answer = true
    private(set) var asked = 0

    init(_ status: CameraStatus) {
        self.status = status
    }

    func request() async -> Bool {
        asked += 1
        status = answer ? .allowed : .refused
        return answer
    }
}
