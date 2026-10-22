import Krowk
import KrowkKit

/// Where approvals asked and answered live go: ticket 91's prompt
/// implements it. Until then, nothing (`NoApprovals`).
public protocol ApprovalSink: AnyObject, Sendable {
    /// `approval.requested`: a tool call waits for a yes.
    @MainActor func requested(_ request: ApprovalRequest)
    /// `approval.resolved`: it was answered, here or elsewhere.
    @MainActor func resolved(sessionId: String, requestId: String, decision: ApprovalDecision)
}

/// Approvals go nowhere yet (ticket 91).
public final class NoApprovals: ApprovalSink {
    public init() {}
    @MainActor public func requested(_ request: ApprovalRequest) {}
    @MainActor public func resolved(sessionId: String, requestId: String, decision: ApprovalDecision) {}
}

/// The transcript from the generated protocol types (R-PROTO-2): the
/// session's `LogEvent`s and live lines as krowk-ffi decoded them, never a
/// Swift copy of their JSON. Each becomes what it changes in KrowkKit's
/// `TranscriptModel`; the switches are exhaustive, so a new event, line or
/// item kind fails this build until it is placed.
extension TranscriptUpdate {
    /// The viewer's updates, in order, as the transcript takes them.
    @MainActor
    public static func from(_ updates: [ViewerUpdate], approvals: any ApprovalSink) -> [TranscriptUpdate] {
        from(posted: updates.map(MailboxItem.viewer), approvals: approvals)
    }

    /// What the mailbox held, in order, as the transcript takes it: a run
    /// of logged lines as one `logged`, the live lines a transcript draws
    /// (an item started, its text deltas, a notice), approvals handed to
    /// `approvals`, and the composer's commands as the core reports them
    /// (their ids, sent or queued, acked). The rest — the host, a gap, a
    /// note, a failure — are the status bar's (`RelayConnection`), and the
    /// path and handoffs draw nothing here.
    @MainActor
    public static func from(posted: [MailboxItem], approvals: any ApprovalSink) -> [TranscriptUpdate] {
        var out: [TranscriptUpdate] = []
        var logged: [TranscriptEvent] = []
        func flush() {
            if !logged.isEmpty { out.append(.logged(logged)) }
            logged = []
        }
        for item in posted {
            let update: ViewerUpdate
            switch item {
            case .viewer(let viewer): update = viewer
            case .minted(let token, let id):
                flush()
                out.append(.commandMinted(token: token, id: id))
                continue
            case .unsent(let token, let why):
                flush()
                out.append(.commandUnsent(token: token, why: why))
                continue
            }
            if case .line(.log(let event)) = update {
                logged.append(TranscriptEvent(event))
                continue
            }
            flush()
            switch update {
            case .attached(let events, _):
                out.append(.attached(events.map(TranscriptEvent.init)))
            case .caughtUp(let events):
                out.append(.caughtUp(events.map(TranscriptEvent.init)))
            case .line(.live(let line)):
                if let mapped = live(line, approvals: approvals) { out.append(mapped) }
            case .sent(let id, let queued):
                out.append(.commandSent(id: id, queued: queued))
            case .acked(let id, let error):
                out.append(.commandAcked(id: id, error: error))
            case .line(.log), .host, .path, .gap, .failed, .note, .handoffTaken, .handoffRefused:
                break
            }
        }
        flush()
        return out
    }

    /// One live line: what it draws, if anything. A tool call's input
    /// deltas draw nothing (its summary comes from the completed call);
    /// cost, limits, background work and the run's result are M1's to
    /// ignore; subagents' streams are out of scope.
    @MainActor
    static func live(_ line: LiveEvent, approvals: any ApprovalSink) -> TranscriptUpdate? {
        switch line {
        case .itemStarted(let session, let turn, let item, let kind):
            let mapped: LiveItemKind = switch kind {
            case .userText: .user
            case .assistantText: .assistant
            case .reasoning: .reasoning
            case .toolCall(let callId, let name): .toolCall(callID: callId, name: name)
            case .toolResult(let callId): .toolResult(callID: callId)
            }
            return .started(session: session, turn: turn, item: item, kind: mapped)
        case .itemDelta(let session, _, let item, let delta):
            switch delta {
            case .text(let text): return .delta(session: session, item: item, text: text)
            case .toolInput: return nil
            }
        case .notice(let session, _, let text):
            return .notice(session: session, text: text)
        case .approvalRequested(let request):
            approvals.requested(request)
            return nil
        case .approvalResolved(let session, _, let request, let decision):
            approvals.resolved(sessionId: session, requestId: request, decision: decision)
            return nil
        case .cost, .limits, .result, .backendAgents, .background, .subagentStatus, .turnUnprompted:
            return nil
        }
    }
}

/// What the connection hands the screen, in order: the viewer's updates,
/// and for each command the composer sent, the id the core gave it (posted
/// before its `sent`) or why it never reached the core.
public enum MailboxItem: Sendable {
    case viewer(ViewerUpdate)
    case minted(token: UInt64, id: String)
    case unsent(token: UInt64, why: String)
}

extension LiveCommand {
    /// The generated `Command` for session `session` (R-CLIENT-1: never
    /// its JSON): a prompt with no images, model, toolset, effort or
    /// budget and the default permission mode, which the host's bridge
    /// sets from the session anyway.
    func command(session: String) -> Command {
        switch self {
        case .prompt(let text):
            .prompt(sessionId: session, text: text, images: [], model: nil, permissionMode: .default, toolset: nil, effort: nil, budget: nil)
        case .steer(let text):
            .steer(sessionId: session, text: text, images: [])
        case .interrupt:
            .interrupt(sessionId: session)
        }
    }
}

extension TranscriptModel {
    /// One of the viewer's updates applied: as a frame of one.
    public func apply(_ update: ViewerUpdate) {
        apply(frame: TranscriptUpdate.from([update], approvals: NoApprovals()))
    }
}

extension TranscriptEvent {
    /// `event` as the transcript takes it.
    public init(_ event: LogEvent) {
        self.init(id: event.id, parentID: event.parentId, sessionID: event.sessionId, change: Self.change(event), timeMs: event.timeMs)
    }

    static func change(_ event: LogEvent) -> TranscriptChange? {
        switch event.body {
        case .itemCompleted(_, let itemId, let item):
            switch item {
            case .userText(let text, let images):
                return .userText(item: itemId, text: text, images: images.map(\.number))
            case .assistantText(let text):
                return .assistantText(item: itemId, text: text)
            case .reasoning(let text, _):
                return .reasoning(item: itemId, text: text)
            case .toolCall(let callId, let name, let input):
                return .toolCall(item: itemId, callID: callId, name: name, input: input)
            case .toolResult(let callId, let output, let isError):
                return .toolResult(item: itemId, callID: callId, output: output, isError: isError)
            }
        case .turnCompleted(let turnId, let status, _, _, _, _, let error):
            let end: TurnEnd = switch status {
            case .completed: .completed
            case .interrupted: .interrupted
            case .failed: .failed(error?.message)
            }
            return .turnCompleted(turn: turnId, end: end)
        case .modelSwitched(_, _, let to, _, _):
            return .modelSwitched(model: to.model)
        case .turnStarted(let turnId, _, _, _, _, _):
            return .turnStarted(turn: turnId)
        case .sessionStarted, .responseCompleted, .backendSession, .subagentResponse, .subagentStarted,
             .todosUpdated, .runOpened, .backendHandoff, .sessionMoved:
            return nil
        }
    }
}
