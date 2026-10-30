import Krowk
import KrowkKit

/// Where approvals asked and answered live go, in the order the session
/// says them, as the main thread takes its frame: the screen's
/// `ApprovalQueue` (ticket 91), or nowhere (`NoApprovals`).
public protocol ApprovalSink: AnyObject, Sendable {
    /// `approval.requested`, live or re-sent in a host's welcome (the core
    /// shows each once until it's resolved): a tool call waits for a yes.
    @MainActor func requested(_ request: ApprovalRequest)
    /// `approval.resolved`: it was answered, here or elsewhere.
    @MainActor func resolved(sessionId: String, requestId: String, decision: ApprovalDecision)
    /// `approvalGone`: answered elsewhere while the phone was away, how
    /// isn't known (ticket 145).
    @MainActor func gone(requestId: String)
    /// A turn ended (its logged `turn.completed`, or the run's result): what
    /// it still asked is over.
    @MainActor func turnEnded(sessionId: String, turnId: String)
}

/// Approvals go nowhere: a screen with no sheet.
public final class NoApprovals: ApprovalSink {
    public init() {}
    @MainActor public func requested(_ request: ApprovalRequest) {}
    @MainActor public func resolved(sessionId: String, requestId: String, decision: ApprovalDecision) {}
    @MainActor public func gone(requestId: String) {}
    @MainActor public func turnEnded(sessionId: String, turnId: String) {}
}

/// The screen's queue takes the session's approvals (R-PERM-2): each
/// request as KrowkKit's `ApprovalPrompt`, a resolution as allowed or
/// denied, and turn ends.
extension ApprovalQueue: ApprovalSink {
    public func requested(_ request: ApprovalRequest) {
        requested(ApprovalPrompt(request))
    }

    public func resolved(sessionId: String, requestId: String, decision: ApprovalDecision) {
        let resolution: ApprovalResolution = switch decision {
        case .allow: .allowed
        case .allowSession: .allowedForSession
        case .deny: .denied
        // A rule the host made for the project: the phone never makes
        // one, only hears of it.
        default: .allowedByRule
        }
        resolved(requestID: requestId, resolution: resolution)
    }

    public func turnEnded(sessionId: String, turnId: String) {
        turnEnded(sessionID: sessionId, turnID: turnId)
    }

    public func gone(requestId: String) {
        gone(requestID: requestId)
    }
}

extension ApprovalPrompt {
    /// The generated request as the sheet takes it: its own session (a
    /// subagent's, for its request), its input as JSON text.
    public init(_ request: ApprovalRequest) {
        self.init(
            sessionID: request.sessionId, turnID: request.turnId, requestID: request.requestId, tool: request.tool,
            input: request.input, summary: request.summary, reason: request.reason, remember: request.remember,
            questions: request.questions.map {
                ApprovalQuestion(id: $0.id, header: $0.header, question: $0.question,
                                 options: $0.options.map { ApprovalOption(label: $0.label, description: $0.description) },
                                 multiSelect: $0.multiSelect, secret: $0.secret)
            })
    }
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
            case .restarted:
                flush()
                out.append(.restarted)
                continue
            }
            if case .line(.log(let event)) = update {
                ended(event, approvals: approvals)
                logged.append(TranscriptEvent(event))
                continue
            }
            flush()
            switch update {
            case .attached(let events, _):
                for event in events { ended(event, approvals: approvals) }
                out.append(.attached(events.map(TranscriptEvent.init)))
            // Empty, it only says a catch-up ended: the bar's business.
            case .caughtUp(let events):
                guard !events.isEmpty else { continue }
                for event in events { ended(event, approvals: approvals) }
                out.append(.caughtUp(events.map(TranscriptEvent.init)))
            case .approvalGone(let request):
                approvals.gone(requestId: request)
            case .line(.live(let line)):
                if let mapped = live(line, approvals: approvals) { out.append(mapped) }
            case .sent(let id, let queued):
                out.append(.commandSent(id: id, queued: queued))
            case .acked(let id, let error):
                out.append(.commandAcked(id: id, error: error))
            // A place to save, the cache's (`TranscriptCacheWriter`):
            // nothing on screen changes.
            case .line(.log), .host, .path, .gap, .failed, .note, .handoffTaken, .handoffRefused, .saved:
                break
            }
        }
        flush()
        return out
    }

    /// A logged turn's end, told to the approvals: what it asked is over.
    @MainActor
    static func ended(_ event: LogEvent, approvals: any ApprovalSink) {
        if case .turnCompleted(let turn, _, _, _, _, _, _) = event.body {
            approvals.turnEnded(sessionId: event.sessionId, turnId: turn)
        }
    }

    /// One live line: what it draws, if anything. A tool call's input
    /// deltas draw nothing (its summary comes from the completed call);
    /// cost and background work are M1's to ignore; subagents' streams are
    /// out of scope. The run's result carries a rate-limit offer, and
    /// `limits` each instance's limit (ticket 135).
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
        case .result(let result):
            // The run's end: its turn asks nothing more, and the host may
            // offer another instance for it (ticket 135).
            approvals.turnEnded(sessionId: result.sessionId, turnId: result.turnId)
            let offer = result.switchOffer.map {
                SwitchOfferInfo(from: ModelChoice($0.from), to: ModelChoice($0.to), resetsAtMs: $0.resetsAtMs)
            }
            return .result(session: result.sessionId, turn: result.turnId, offer: offer)
        case .limits(let session, _, let instance, let limit):
            let state = switch limit.status {
            case .allowed: "allowed"
            case .warning: "warning"
            case .limited: "limited"
            }
            return .limits(session: session, instance: instance, limit: InstanceLimit(state: state, usedPercent: limit.usedPercent, resetsAtMs: limit.resetsAtMs))
        case .cost, .backendAgents, .background, .subagentStatus, .turnUnprompted:
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
    /// The session was read afresh, what the phone kept of it replaced
    /// (ticket 131): the screen's rows start over.
    case restarted
}

extension LiveCommand {
    /// The generated `Command` for session `session` (R-CLIENT-1: never
    /// its JSON): a prompt with its images named by sha256 (no bytes) and no model, toolset, effort or
    /// budget and the default permission mode, which the host's bridge
    /// sets from the session anyway.
    public func command(session: String) -> Command {
        switch self {
        case .prompt(let text, let images):
            .prompt(sessionId: session, text: text, images: images.map(\.input), model: nil, permissionMode: .default, toolset: nil, effort: nil, budget: nil)
        case .steer(let text, let images):
            .steer(sessionId: session, text: text, images: images.map(\.input))
        case .interrupt:
            .interrupt(sessionId: session)
        case .approve(let reply):
            // The request's own session, never the screen's: a subagent's
            // request is answered for the subagent.
            .approve(sessionId: reply.sessionID, requestId: reply.requestID, decision: reply.verdict.decision,
                     answers: reply.answers.map { QuestionAnswer(id: $0.id, picked: $0.picked, text: $0.text) })
        case .switchModel(let choice):
            // The model only: a remote client never switches the mode,
            // toolset, effort or budget, and the command has no field for one.
            .switchModel(sessionId: session, model: ModelRef(instance: choice.instance, model: choice.model))
        }
    }
}

extension SentImage {
    /// The generated `ImageInput` naming the image by its sha256, with no
    /// bytes: they went ahead in a `putImage`.
    var input: ImageInput {
        ImageInput(number: UInt32(number), mediaType: mediaType, data: "", sha256: sha256)
    }
}

extension PreparedImage {
    /// The generated `putImage` for session `session`.
    func command(session: String) -> Command {
        .putImage(sessionId: session, sha256: sha256, mediaType: mediaType, data: data.base64EncodedString())
    }
}

extension ModelChoice {
    /// The generated `ModelRef`, as KrowkKit takes it.
    public init(_ ref: ModelRef) {
        self.init(instance: ref.instance, model: ref.model)
    }
}

extension ApprovalVerdict {
    /// The generated decision: once, for the session, or no. A viewer never
    /// makes a rule for the project (R-IOS-11): the host makes those.
    public var decision: ApprovalDecision {
        switch self {
        case .allow: .allow
        case .allowSession: .allowSession
        case .deny: .deny
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
        case .itemCompleted(let turnId, let itemId, let item):
            switch item {
            case .userText(let text, let images):
                var hashes: [UInt32: String] = [:]
                for image in images { if let sha = image.sha256, ImageCache.isHash(sha) { hashes[image.number] = sha } }
                return .userText(item: itemId, text: text, images: images.map(\.number), turn: turnId, hashes: hashes)
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
        case .modelSwitched(_, let from, let to, let reason, _):
            return .modelSwitched(model: to.model, from: from.map(ModelChoice.init), to: ModelChoice(to), failed: reason == .switchFailed)
        case .turnStarted(let turnId, let model, _, _, _, _):
            return .turnStarted(turn: turnId, model: ModelChoice(model))
        case .sessionStarted, .responseCompleted, .backendSession, .subagentResponse, .subagentStarted,
             .todosUpdated, .runOpened, .backendHandoff, .sessionMoved:
            return nil
        }
    }
}
