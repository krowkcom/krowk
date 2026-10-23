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
///
/// Every listing first checks that the phone is still on the person's
/// devices (`Account.checkDeviceList`, ticket 117), which also takes up a
/// list that grew and a rotated user key: a phone removed, or left off a
/// list that was started over, lists nothing (`SessionListFailure.gone`),
/// and `onGone` hears it, as it does when opening a session is refused
/// for that reason.
public final class RegistrySessions: SessionSource {
    public let api: String
    private let storage: any KeyStorage
    /// Opened on first use, off the main actor: opening reads the Keychain.
    private let account = Mutex<Account?>(nil)
    private let onGone: @Sendable (DeviceGone) -> Void

    /// - Parameters:
    ///   - onGone: told, on whatever thread found it, each time a check
    ///     finds the phone gone; the app shows why, once.
    public init(api: String, storage: any KeyStorage, onGone: @escaping @Sendable (DeviceGone) -> Void = { _ in }) {
        self.api = api
        self.storage = storage
        self.onGone = onGone
    }

    /// Blocking: the device list's check, then a registry call per page
    /// of 100 and one per session.
    public func list() throws(SessionListFailure) -> SessionListing {
        // Never on the main thread: a long list takes a while.
        dispatchPrecondition(condition: .notOnQueue(.main))
        if let gone = standing() { throw .gone(gone) }
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

    /// Whether the phone is gone from the person's devices, as the
    /// registry's device list says now; nil while it is on it, and when
    /// the check couldn't say (the registry out of reach, a list this
    /// phone won't trust): the listing that follows says why. Told to
    /// `onGone` too. Blocking: a registry call or two.
    public func standing() -> DeviceGone? {
        dispatchPrecondition(condition: .notOnQueue(.main))
        let gone: DeviceGone?
        do {
            try opened().checkDeviceList()
            gone = nil
        } catch {
            gone = DeviceGone(error)
        }
        if let gone { onGone(gone) }
        return gone
    }

    /// Session `id`, opened to follow live (`RelayConnection`), held to at
    /// least `known`, the head its listing named. On the relay
    /// `RelayConnection.relayOverride` names, else the one Rust picks for
    /// the registry. A refusal is checked against the device list: a phone
    /// gone from it is refused as such, and `onGone` hears it. Blocking:
    /// registry calls.
    public func openSession(id: String, known: HeadRecord?) throws -> OpenedSession {
        dispatchPrecondition(condition: .notOnQueue(.main))
        do {
            return try opened().openSession(id: id, known: known, relayOverride: RelayConnection.relayOverride())
        } catch KrowkError.Refused(let code, let detail) {
            if let gone = standing() { throw KrowkError.Refused(code: gone.kind, detail: gone.title + ".") }
            throw KrowkError.Refused(code: code, detail: detail)
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

extension DeviceGone {
    /// `Account.checkDeviceList`'s refusals that mean the phone is gone
    /// from the person's devices; nil for anything else.
    init?(_ error: any Error) {
        switch error as? KrowkError {
        case .Refused(code: "device_removed", let detail)?: self = .removed(detail: detail)
        case .Refused(code: "device_list_reset", let detail)?: self = .listReset(detail: detail)
        default: return nil
        }
    }
}

extension SessionListFailure {
    /// The core's refusals, as `Account.listSessions` names them.
    init(_ error: any Error) {
        if let gone = DeviceGone(error) {
            self = .gone(gone)
            return
        }
        switch error as? KrowkError {
        case .Network(let detail)?: self = .network(detail: detail)
        case .Refused(code: "not_paired", _)?: self = .notPaired
        case .Refused(_, let detail)?, .Internal(let detail)?, .Decode(let detail)?, .InvalidInput(let detail)?:
            self = .other(detail: detail)
        case nil: self = .other(detail: String(describing: error))
        }
    }
}
