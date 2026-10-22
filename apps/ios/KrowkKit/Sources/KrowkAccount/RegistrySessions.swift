import Foundation
import Krowk
import KrowkKit
import Synchronization

/// The generated record is what a row reads (R-PROTO-2): no Swift mirror.
extension SessionSummary: SessionRecord {}

/// KrowkKit's `SessionSource` over krowk-ffi's `Account` (R-IOS-3,
/// R-CLIENT-1): Rust lists the person's synced sessions from their sealed
/// indexes, each record checked against the device list this phone pinned,
/// and counts those it can't open. It never reads a transcript to list.
/// The holder's name comes from that verified list (`Account.devices`,
/// read from the Keychain, no network).
public final class RegistrySessions: SessionSource {
    public let api: String
    private let storage: any KeyStorage
    /// Opened on first use, off the main actor: opening reads the Keychain.
    private let account = Mutex<Account?>(nil)

    public init(api: String, storage: any KeyStorage) {
        self.api = api
        self.storage = storage
    }

    /// Blocking: a registry call per page of 100 and one per session.
    public func list() throws(SessionListFailure) -> SessionListing {
        // Never on the main thread: a long list takes a while.
        dispatchPrecondition(condition: .notOnQueue(.main))
        do {
            let account = try opened()
            let list = try account.listSessions()
            let devices = (try? account.devices()) ?? []
            let names = Dictionary(devices.map { ($0.id, $0.name) }, uniquingKeysWith: { first, _ in first })
            return SessionListing(sessions: list.sessions, unreadable: Int(list.unreadable), deviceNames: names)
        } catch {
            throw SessionListFailure(error)
        }
    }

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

extension SessionListFailure {
    /// The core's refusals, as `Account.listSessions` names them.
    init(_ error: any Error) {
        switch error as? KrowkError {
        case .Network(let detail)?: self = .network(detail: detail)
        case .Refused(code: "not_paired", _)?: self = .notPaired
        case .Refused(_, let detail)?, .Internal(let detail)?, .Decode(let detail)?, .InvalidInput(let detail)?:
            self = .other(detail: detail)
        case nil: self = .other(detail: String(describing: error))
        }
    }
}
