import Foundation
import Krowk
import KrowkKit
import Synchronization

/// KrowkKit's `PairingService` over krowk-ffi's `Account` (R-E2E-3,
/// R-CLIENT-1): Rust parses the code and the QR code, finds the open
/// pairing, and runs krowk-client's joining side — SPAKE2, the payload
/// check, the device-list pin, the claim — writing the user keys and the
/// device list into `storage`, the app's `KeychainKeyStorage`. Swift never
/// sees a key, and never retries a join: a code is spent once.
public final class RegistryPairing: PairingService {
    public let api: String
    private let storage: any KeyStorage
    private let name: String
    private let os: String
    /// Opened on first use, off the main actor: opening reads the Keychain.
    private let account = Mutex<Account?>(nil)

    /// - Parameters:
    ///   - name: what the other device's prompt calls this phone.
    ///   - os: what it runs, like `iOS 26.1`; Rust cuts it to 32 bytes.
    public init(api: String, storage: any KeyStorage, name: String, os: String) {
        self.api = api
        self.storage = storage
        self.name = name
        self.os = os
    }

    public func parseCode(_ text: String) -> String? {
        try? parsePairingCode(text: text)
    }

    public func parseInvite(_ payload: String) -> PairingInvitation? {
        parsePairingInvite(uri: payload).map { PairingInvitation(code: $0.code, userId: $0.userId, device: $0.device) }
    }

    public func attempt() -> any PairingAttempt {
        Attempt(service: self)
    }

    /// The signed-in account. Blocking.
    func opened() throws(PairingFailure) -> Account {
        if let account = account.withLock({ $0 }) { return account }
        do {
            let opened = try Account.open(api: api, storage: storage)
            return account.withLock { kept in
                if let kept { return kept }
                kept = opened
                return opened
            }
        } catch {
            throw PairingFailure(error)
        }
    }

    /// One join. A cancel before Rust has started the join is not kept by
    /// Rust (`cancel_pairing` does nothing then), so it is kept here: a
    /// join cancelled before it starts never starts, and one cancelled
    /// as it starts is told again once it has.
    private final class Attempt: PairingAttempt {
        let service: RegistryPairing
        private let state = Mutex(State())

        struct State {
            var cancelled = false
            var account: Account?
        }

        init(service: RegistryPairing) {
            self.service = service
        }

        func openPairing() throws(PairingFailure) -> OpenedPairing {
            let account = try service.opened()
            try checkCancelled()
            do {
                let open = try account.openPairing()
                return OpenedPairing(device: open.device, deviceName: open.deviceName)
            } catch {
                throw PairingFailure(error)
            }
        }

        func join(_ ticket: PairingTicket) throws(PairingFailure) {
            let account = try service.opened()
            let cancelled = state.withLock { s in
                s.account = account
                return s.cancelled
            }
            if cancelled { throw .cancelled }
            // A cancel that lands between the check above and Rust taking
            // the join's slot would be dropped; this one, a moment in, is not.
            let late = DispatchWorkItem { [self] in
                if state.withLock({ $0.cancelled }) { account.cancelPairing() }
            }
            DispatchQueue.global().asyncAfter(deadline: .now() + 0.25, execute: late)
            defer { late.cancel() }
            do {
                switch ticket {
                case .code(let code):
                    _ = try account.joinPairing(code: code, name: service.name, os: service.os)
                case .invite(let invite):
                    let scanned = PairingInvite(code: invite.code, userId: invite.userId, device: invite.device)
                    _ = try account.joinInvite(invite: scanned, name: service.name, os: service.os)
                }
            } catch {
                throw PairingFailure(error)
            }
        }

        func cancel() {
            let account = state.withLock { s in
                s.cancelled = true
                return s.account
            }
            account?.cancelPairing()
        }

        private func checkCancelled() throws(PairingFailure) {
            if state.withLock({ $0.cancelled }) { throw .cancelled }
        }
    }
}

extension PairingFailure {
    /// The core's refusal codes, as krowk-ffi's join names them. Once the
    /// code is sent only `pairing_failed`, `not_added` and `cancelled`
    /// come back, so whatever isn't named here ended the join before the
    /// code was spent.
    init(_ error: any Error) {
        guard let error = error as? KrowkError else {
            self = .other(detail: String(describing: error))
            return
        }
        switch error {
        case .Refused(code: "no_pairing", _): self = .noPairing
        case .Refused(code: "pairing_failed", _), .Refused(code: "not_added", _), .Refused(code: "pairing_gone", _):
            self = .codeUsedUp
        case .Refused(code: "invite_other_account", _): self = .otherAccount
        case .Refused(code: "invite_other_pairing", _): self = .otherDevice
        case .Refused(code: "device_removed", _): self = .removed
        case .Refused(code: "already_joined", _): self = .alreadyJoined
        case .Refused(code: "cancelled", _): self = .cancelled
        case .InvalidInput: self = .notACode
        case .Network(let detail): self = .network(detail: detail)
        case .Refused(_, let detail), .Internal(let detail), .Decode(let detail):
            self = .other(detail: detail)
        }
    }
}
