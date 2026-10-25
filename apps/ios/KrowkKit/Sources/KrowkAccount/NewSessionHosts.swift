import Foundation
import Krowk
import KrowkKit

// Starting a session from the phone (R-HOST-4, ticket 134), and moving one
// to another host (R-HAND-4, ticket 136), over krowk-ffi:
// the hosts from their sealed announcements (`Account.listHosts`, R-HOST-3),
// a start sealed to the chosen host and its answer opened
// (`Account.startSession`), under a command id Rust mints
// (`newCommandId`). The start names the place, the model and the words; no
// mode, budget, toolset or effort is sent, since the command has no field
// for one. A move (`Account.takeSession`) names the session and the
// directory only.

/// The generated records are what the flow reads (R-PROTO-2): no Swift
/// mirror.
extension HostRecord: AnnouncedHost {
    public var announcedInstances: [any AnnouncedInstance] { instances }
}

extension InstanceRecord: AnnouncedInstance {
    public var limitState: String? {
        switch limit?.status {
        case .allowed?: "allowed"
        case .warning?: "warning"
        case .limited?: "limited"
        case nil: nil
        }
    }

    public var limitUsedPercent: Double? { limit?.usedPercent }
    public var limitResetsAtMs: Int64? { limit?.resetsAtMs }
}

extension RegistrySessions: HostSource {
    /// Blocking: one registry call. Never on the main thread.
    public func hosts() throws -> HostListing {
        dispatchPrecondition(condition: .notOnQueue(.main))
        let list = try opened().listHosts()
        return HostListing(hosts: list.hosts, unreadable: Int(list.unreadable))
    }

    /// Blocking, for up to 30 s and the sends: never on the main thread.
    /// What fails is said in the row's words: the host's own refusal, or
    /// why it couldn't be asked.
    public func start(_ r: StartRequest) -> StartOutcome {
        dispatchPrecondition(condition: .notOnQueue(.main))
        do {
            let model = ModelRef(instance: r.instance, model: r.model)
            switch try opened().startSession(device: r.device, cwd: r.cwd, model: model, prompt: r.prompt, id: r.id) {
            case .started(let session): return .started(sessionID: session)
            case .refused(let code, let message): return .refused(code: code, message: message)
            }
        } catch KrowkError.Network {
            return .refused(code: "network", message: "krowk's registry couldn't be reached. The start may still reach \(r.hostName) and run.")
        } catch KrowkError.InvalidInput(let detail) {
            return .refused(code: "invalid", message: detail)
        } catch KrowkError.Refused(let code, let detail) {
            return .refused(code: code, message: detail)
        } catch {
            return .refused(code: "failed", message: "The session couldn't be started on \(r.hostName).")
        }
    }

    public func newCommandID() -> String {
        newCommandId()
    }

    /// Blocking, for up to six minutes and the sends, or until `cancelTake`:
    /// never on the main thread. What fails is said in the screen's words:
    /// the host's own refusal, or why it couldn't be asked.
    public func take(_ r: MoveRequest) -> MoveOutcome {
        dispatchPrecondition(condition: .notOnQueue(.main))
        do {
            switch try opened().takeSession(device: r.device, sessionId: r.sessionID, cwd: r.cwd, id: r.id) {
            case .taken: return .taken
            case .refused(let code, let message): return .refused(code: code, message: message)
            }
        } catch KrowkError.Network {
            return .refused(code: "network", message: "krowk's registry couldn't be reached. The move may still reach \(r.hostName).")
        } catch KrowkError.InvalidInput(let detail) {
            return .refused(code: "invalid", message: detail)
        } catch KrowkError.Refused(let code, let detail) {
            return .refused(code: code, message: detail)
        } catch {
            return .refused(code: "failed", message: "The session couldn't be moved to \(r.hostName).")
        }
    }

    /// Never blocks: an account not opened yet has no take to wait on.
    public func cancelTake() {
        openedAlready?.cancelTake()
    }

    /// Session `id`, just started on a host, opened to follow live:
    /// automatic sync takes it up to the registry at its first turn, so
    /// "not found" is tried again for up to 10 s. Blocking.
    public func openStartedSession(id: String) throws -> OpenedSession {
        dispatchPrecondition(condition: .notOnQueue(.main))
        let deadline = Date.now.addingTimeInterval(10)
        while true {
            do {
                return try openSession(id: id, known: nil)
            } catch KrowkError.Refused(code: "not_found", _) where Date.now < deadline {
                Thread.sleep(forTimeInterval: 0.5)
            }
        }
    }
}
