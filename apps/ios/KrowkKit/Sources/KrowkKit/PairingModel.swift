import Foundation
import Observation
import Synchronization

/// Joining the person's devices, as the pairing screen needs it (R-E2E-3).
/// KrowkKit does not import the Rust bindings: KrowkAccount implements this
/// over krowk-ffi's `Account`, whose join runs krowk-client's joining side
/// (SPAKE2, the payload check, the device-list pin, the claim), and the
/// tests stub it. Swift never checks a code or reads a QR code itself.
public protocol PairingService: Sendable {
    /// The code in its canonical `XXXX-XXXX` form, or nil when it is not
    /// one (krowk-client's parser: case, spaces and dashes ignored). Never
    /// a network call.
    func parseCode(_ text: String) -> String?
    /// The invite a scanned QR code carries, or nil when it is not
    /// krowk's (krowk-client's strict parser). Never a network call.
    func parseInvite(_ payload: String) -> PairingInvitation?
    /// One join, from finding the open pairing to the claim.
    func attempt() -> any PairingAttempt
}

/// One join under way. Its calls block (registry calls): never on the main
/// actor.
public protocol PairingAttempt: Sendable {
    /// The pairing open now: which device shows the code. Sends nothing to
    /// the pairing, so it costs no code.
    func openPairing() throws(PairingFailure) -> OpenedPairing
    /// Joins with `ticket`: blocks until the other device has added this
    /// one (the user keys and the device list kept, the claim made), or
    /// the join failed and kept nothing. A scanned invite is checked
    /// against the account, then the open pairing, before its code is
    /// spent.
    func join(_ ticket: PairingTicket) throws(PairingFailure)
    /// Ends the join within a moment. Once its first message is out, it
    /// also ends the pairing on the registry, so the other device isn't
    /// left waiting; before that the code is unspent and the other
    /// device's pairing stays open. A join already past the other
    /// device's reply finishes as it would have.
    func cancel()
}

/// What a scanned invite says, read by krowk-client's parser. Its
/// description never shows the code.
public struct PairingInvitation: Sendable, Equatable, CustomStringConvertible {
    public let code: String
    public let userId: String
    /// The id of the device showing the code, 32 lowercase hex.
    public let device: String

    public init(code: String, userId: String, device: String) {
        self.code = code
        self.userId = userId
        self.device = device
    }

    public var description: String { "PairingInvitation(code: …, userId: \(userId), device: \(device))" }
}

/// The code a join spends: typed, or from a scanned invite.
public enum PairingTicket: Sendable, Equatable, CustomStringConvertible {
    case code(String)
    case invite(PairingInvitation)

    public var description: String {
        switch self {
        case .code: "code(…)"
        case .invite(let invite): "invite(\(invite))"
        }
    }
}

/// The pairing open on the person's account: the device showing the code.
public struct OpenedPairing: Sendable, Equatable {
    /// Its id, 32 lowercase hex.
    public let device: String
    /// Its name on the device list, cleaned by Rust; may be empty.
    public let deviceName: String

    public init(device: String, deviceName: String) {
        self.device = device
        self.deviceName = deviceName
    }
}

/// Why a join ended without the keys, as the screen tells it.
public enum PairingFailure: Error, Sendable, Equatable {
    /// What was typed is not a pairing code. Nothing was sent.
    case notACode
    /// `no_pairing`: no device has a pairing open.
    case noPairing
    /// The pairing ended and its code is spent (`pairing_failed`,
    /// `not_added`, `pairing_gone`).
    case codeUsedUp
    /// `invite_other_account`: the QR code is another person's.
    case otherAccount
    /// `invite_other_pairing`: the QR code was shown for a pairing other
    /// than the one open now.
    case otherDevice
    /// `device_removed`: this phone's keys were removed from the list.
    case removed
    /// `already_joined`: this phone is on the list with the keys already.
    case alreadyJoined
    /// The registry could not be reached before the code was spent.
    case network(detail: String)
    /// This app ended it; never shown.
    case cancelled
    /// Anything else, which ends a join before its code is sent (a Keychain
    /// that won't open, a reply the phone can't read): the code is unspent.
    /// The core's words go to the log, not the screen.
    case other(detail: String)

    /// One sentence for the screen, naming the device that showed the code
    /// when it is known.
    public func sentence(laptop: String?) -> String {
        switch self {
        case .notACode:
            "That isn't a pairing code: it's the 8 characters `krowk devices add` shows, like K7QF-9M3X. Nothing was sent."
        case .noPairing:
            "Nothing to pair with yet: run `krowk devices add` on your laptop first."
        case .codeUsedUp:
            if let laptop, !laptop.isEmpty {
                "The pairing failed, and a code works once — on '\(laptop)' run `krowk devices add` again for a new code. Nothing was kept."
            } else {
                "The pairing failed, and a code works once — on your laptop run `krowk devices add` again for a new code. Nothing was kept."
            }
        case .otherAccount:
            "This code is for another krowk account."
        case .otherDevice:
            "This code is from a different device's pairing — run `krowk devices add` again."
        case .removed:
            "This iPhone was removed from your devices, and a removed device can't join again with its keys."
        case .alreadyJoined:
            "This iPhone is on your devices."
        case .network:
            "krowk's registry couldn't be reached, so the code wasn't used. Check the connection and enter it again."
        case .other:
            "Pairing didn't start, so the code wasn't used. Enter it again."
        case .cancelled:
            "Pairing was cancelled."
        }
    }
}

/// Whether the camera may be used, as the scanner needs it: the system's
/// answer in the app, a stub in the tests.
@MainActor
public protocol CameraAccess: AnyObject {
    var status: CameraStatus { get }
    /// Asks the person, once; true when allowed.
    func request() async -> Bool
}

public enum CameraStatus: Sendable {
    case allowed, notAsked, refused
    /// Turned off for this app by Screen Time or a profile: Settings can't help.
    case restricted
}

/// The pairing screen's state (R-E2E-3): take a code, typed or scanned,
/// find the open pairing off the main actor, name the device showing the
/// code, join, and say how it ended. A code is spent once: nothing here
/// ever joins with the same code twice, and a failure clears it.
@MainActor
@Observable
public final class PairingModel {
    public enum Phase: Equatable, Sendable {
        /// "Scan its QR code or type the code", with the last attempt's
        /// failure, if any.
        case entering(PairingFailure?)
        /// Looking for the open pairing.
        case finding
        /// "Pairing with <name>": the other device asks its person.
        case pairing(with: String)
        /// "This iPhone is on your devices."
        case paired
    }

    /// Where the scanner stands.
    public enum Scanner: Equatable, Sendable {
        case closed
        case open
        /// The camera may not be used: say so, with the way to Settings.
        case refused
        /// The camera is restricted, or the scanner couldn't start: type
        /// the code.
        case unavailable
    }

    public private(set) var phase: Phase = .entering(nil)
    public private(set) var scanner: Scanner = .closed
    /// The code field, upper-cased as typed (`enter`). Only Rust decides
    /// whether it is a code.
    public private(set) var codeText = ""

    /// Whether this phone can scan at all (VisionKit's
    /// `DataScannerViewController.isSupported`): false in the simulator,
    /// where the button is not shown.
    public let scannerSupported: Bool
    /// This phone's name, as the other device will ask about it.
    public let deviceName: String

    @ObservationIgnored private let service: any PairingService
    @ObservationIgnored private let camera: any CameraAccess
    @ObservationIgnored private let onPaired: @MainActor () -> Void
    @ObservationIgnored private let log: @Sendable (String) -> Void
    /// The attempt under way, outside the actor so `deinit` can cancel it.
    @ObservationIgnored private let current = CurrentAttempt()
    /// The name of the device the last attempt found, for its failure.
    @ObservationIgnored private var laptop: String?
    /// Bumped by every start and cancel: an answer for an older run is dropped.
    @ObservationIgnored private var run = 0

    /// - Parameters:
    ///   - deviceName: the name the other device will ask about, as
    ///     `SignInRules.deviceName` cleaned it.
    ///   - onPaired: moves the app on, once "Continue" is tapped.
    ///   - log: where a failure's detail goes, never the screen. No code
    ///     is ever handed to it.
    public init(
        service: any PairingService,
        camera: any CameraAccess,
        scannerSupported: Bool,
        deviceName: String,
        log: @escaping @Sendable (String) -> Void = { _ in },
        onPaired: @escaping @MainActor () -> Void = {}
    ) {
        self.service = service
        self.camera = camera
        self.scannerSupported = scannerSupported
        self.deviceName = SignInRules.deviceName(deviceName)
        self.log = log
        self.onPaired = onPaired
    }

    deinit {
        current.take()?.cancel()
    }

    /// The failure the screen shows now, if any.
    public var failure: PairingFailure? {
        if case .entering(let failure) = phase { failure } else { nil }
    }

    /// The failure's sentence, naming the device that showed the code.
    public var failureSentence: String? {
        failure.map { $0.sentence(laptop: laptop) }
    }

    /// The name of the device the last attempt found, if it named one.
    public var laptopName: String? {
        laptop.flatMap { $0.isEmpty ? nil : $0 }
    }

    /// The field changed. As soon as it holds a whole code (Rust's parser:
    /// case, spaces and dashes ignored) the pairing starts, and the field
    /// is cleared: a code is spent once.
    public func enter(_ text: String) {
        codeText = text.uppercased()
        guard case .entering = phase, let code = service.parseCode(codeText) else { return }
        codeText = ""
        start(.code(code))
    }

    /// "Pair": what the field holds, checked by Rust's parser before
    /// anything is sent. Not a code: say so, send nothing.
    public func submitCode() {
        guard case .entering = phase else { return }
        guard let code = service.parseCode(codeText) else {
            phase = .entering(.notACode)
            return
        }
        codeText = ""
        start(.code(code))
    }

    /// "Scan": the scanner, once the camera may be used.
    public func scan() {
        guard scannerSupported, case .entering = phase else { return }
        switch camera.status {
        case .allowed:
            scanner = .open
        case .refused:
            scanner = .refused
        case .restricted:
            scanner = .unavailable
        case .notAsked:
            let run = run
            Task { [weak self] in
                guard let self else { return }
                let allowed = await self.camera.request()
                guard run == self.run, case .entering = self.phase else { return }
                self.scanner = allowed ? .open : .refused
            }
        }
    }

    /// The scanner closed, or the camera notice was read.
    public func closeScanner() {
        scanner = .closed
    }

    /// The scanner could not start the camera.
    public func scannerFailed() {
        guard scanner == .open else { return }
        scanner = .unavailable
    }

    /// What the scanner read: krowk's invite starts the join; anything
    /// else is ignored and the scanner keeps looking.
    public func scanned(_ payload: String) {
        guard case .entering = phase, let invite = service.parseInvite(payload) else { return }
        scanner = .closed
        start(.invite(invite))
    }

    /// "Cancel": ends the join, and the pairing on the registry once the
    /// code has been sent, and goes back to the code. The screen going
    /// away does too, through `deinit`.
    public func cancel() {
        run &+= 1
        current.take()?.cancel()
        switch phase {
        case .finding, .pairing: phase = .entering(nil)
        case .entering, .paired: break
        }
    }

    /// "Continue", once paired.
    public func finish() {
        guard phase == .paired else { return }
        onPaired()
    }

    private func start(_ ticket: PairingTicket) {
        run &+= 1
        let run = run
        laptop = nil
        phase = .finding
        let attempt = service.attempt()
        current.set(attempt)
        // The calls block (ureq): a detached task, which hands each to a
        // thread of its own (`blocking`), for up to the ten minutes the
        // other device's person has to answer.
        Task.detached(priority: .userInitiated) { [weak self] in
            let found = await blocking { () -> Result<OpenedPairing, PairingFailure> in
                do throws(PairingFailure) { return .success(try attempt.openPairing()) } catch { return .failure(error) }
            }
            // The device to name: none for a scanned code whose pairing
            // isn't the open one, or with none open. Rust's join refuses
            // those before the code is spent, the account checked first,
            // so another person's code says so whatever is open.
            let named: OpenedPairing?
            switch (found, ticket) {
            case (.success(let open), .code): named = open
            case (.success(let open), .invite(let invite)): named = invite.device == open.device ? open : nil
            case (.failure(.noPairing), .invite): named = nil
            case (.failure(let failure), _):
                await MainActor.run { self?.end(run, failure) }
                return
            }
            let go = await MainActor.run { self?.found(named, run) ?? false }
            guard go else { return }
            let joined = await blocking { () -> Result<Void, PairingFailure> in
                do throws(PairingFailure) { return .success(try attempt.join(ticket)) } catch { return .failure(error) }
            }
            switch joined {
            case .success: await MainActor.run { self?.joined(run) }
            case .failure(let failure): await MainActor.run { self?.end(run, failure) }
            }
        }
    }

    /// The open pairing was found: name its device, if it is the one,
    /// unless this run was cancelled meanwhile, which then never joins.
    private func found(_ open: OpenedPairing?, _ run: Int) -> Bool {
        guard run == self.run, phase == .finding else { return false }
        if let open {
            laptop = open.deviceName
            phase = .pairing(with: open.deviceName.isEmpty ? "your laptop" : open.deviceName)
        }
        return true
    }

    /// The keys are kept whichever run kept them, even one the screen had
    /// moved on from (a cancel too late to count): the screen follows them.
    private func joined(_ run: Int) {
        if run != self.run {
            self.run &+= 1
            current.take()?.cancel()
        } else {
            _ = current.take()
        }
        scanner = .closed
        phase = .paired
    }

    private func end(_ run: Int, _ failure: PairingFailure) {
        guard run == self.run else { return }
        _ = current.take()
        switch failure {
        case .cancelled:
            phase = .entering(nil)
        case .alreadyJoined:
            phase = .paired
        case .network(let detail), .other(let detail):
            log("pairing: \(detail)")
            phase = .entering(failure)
        default:
            phase = .entering(failure)
        }
    }
}

/// The attempt under way, behind a lock: set and taken on the main actor,
/// taken once more by `deinit`, which is not.
private final class CurrentAttempt: Sendable {
    private let attempt = Mutex<(any PairingAttempt)?>(nil)

    func set(_ new: any PairingAttempt) {
        attempt.withLock { $0 = new }
    }

    func take() -> (any PairingAttempt)? {
        attempt.withLock { a in
            defer { a = nil }
            return a
        }
    }
}
