import Foundation

/// A tool call waiting for a yes, or the agent asking which way to go
/// (ticket 91): the generated `ApprovalRequest` as KrowkAccount maps it.
/// What it says is the agent's (its tool's input, summary and reason come
/// from what the model wrote): the sheet draws all of it through
/// `SafeText`, never as markup.
public struct ApprovalPrompt: Equatable, Sendable {
    /// The session whose call it is: a subagent's request carries its own
    /// (child) session id, which the answer must name.
    public let sessionID: String
    public let turnID: String
    public let requestID: String
    public let tool: String
    /// The call's input, as JSON text.
    public let input: String
    public let summary: String
    public let reason: String
    /// The rules "Allow for this session" remembers; empty: no such choice.
    public let remember: [String]
    /// Non-empty: the agent asks these, rather than to run a call.
    public let questions: [ApprovalQuestion]

    public init(sessionID: String, turnID: String, requestID: String, tool: String, input: String, summary: String, reason: String, remember: [String] = [], questions: [ApprovalQuestion] = []) {
        self.sessionID = sessionID
        self.turnID = turnID
        self.requestID = requestID
        self.tool = tool
        self.input = input
        self.summary = summary
        self.reason = reason
        self.remember = remember
        self.questions = questions
    }

    /// The agent asks questions, rather than to run a call.
    public var asks: Bool { !questions.isEmpty }
}

public struct ApprovalQuestion: Equatable, Sendable {
    public let id: String
    public let header: String
    public let question: String
    public let options: [ApprovalOption]
    public let multiSelect: Bool
    /// The written answer is a secret: typed in a secure field, never
    /// echoed or logged.
    public let secret: Bool

    public init(id: String, header: String, question: String, options: [ApprovalOption], multiSelect: Bool = false, secret: Bool = false) {
        self.id = id
        self.header = header
        self.question = question
        self.options = options
        self.multiSelect = multiSelect
        self.secret = secret
    }
}

public struct ApprovalOption: Equatable, Sendable {
    public let label: String
    public let description: String

    public init(label: String, description: String = "") {
        self.label = label
        self.description = description
    }
}

/// What the phone may answer (R-IOS-11): a call allowed once or for the
/// session, or refused. A rule for the whole project is made on the host,
/// which refuses one from a viewer, so there is no such case here.
public enum ApprovalVerdict: Equatable, Sendable {
    case allow
    case allowSession
    case deny

    public var allows: Bool { self != .deny }
}

/// One question's answer: the options picked (their labels), and what was
/// written, if anything. A secret one never prints what was written.
public struct QuestionReply: Equatable, Sendable, CustomStringConvertible, CustomDebugStringConvertible, CustomReflectable {
    public let id: String
    public let picked: [String]
    public let text: String?
    public let secret: Bool

    public init(id: String, picked: [String], text: String?, secret: Bool = false) {
        self.id = id
        self.picked = picked
        self.text = text
        self.secret = secret
    }

    private var shownText: String {
        guard let text else { return "nil" }
        return secret ? "<secret>" : "\"\(text)\""
    }

    public var description: String { "QuestionReply(id: \"\(id)\", picked: \(picked), text: \(shownText))" }
    public var debugDescription: String { description }
    public var customMirror: Mirror {
        Mirror(self, children: ["id": id, "picked": picked, "text": shownText], displayStyle: .struct)
    }
}

/// The answer to one request, as the approve command carries it: for the
/// request's own session (a subagent's, for its request), never the
/// screen's.
public struct ApprovalReply: Equatable, Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    public let sessionID: String
    public let requestID: String
    public let verdict: ApprovalVerdict
    public let answers: [QuestionReply]

    public init(sessionID: String, requestID: String, verdict: ApprovalVerdict, answers: [QuestionReply] = []) {
        self.sessionID = sessionID
        self.requestID = requestID
        self.verdict = verdict
        self.answers = answers
    }

    public var description: String {
        "ApprovalReply(sessionID: \"\(sessionID)\", requestID: \"\(requestID)\", verdict: \(verdict), answers: [\(answers.map(\.description).joined(separator: ", "))])"
    }
    public var debugDescription: String { description }
}

/// Confirms the person holding the phone is its owner before a call is
/// allowed (R-IOS-11): Face ID, falling back to the passcode, in the app
/// (`LAContext`). False for a failed or cancelled check: nothing is sent.
public protocol OwnerCheck: Sendable {
    @MainActor func confirm(reason: String) async -> Bool
}

/// Which allows need the owner check first (R-IOS-11).
public enum ApprovalRules {
    /// Tools that only read: krowk's native ones, lower-case, and Claude
    /// Code's. Exact names: anything else, an unknown name included, is
    /// taken as able to change something.
    public static let readOnly: Set<String> = ["read", "grep", "glob", "Read", "Grep", "Glob", "LS"]

    /// Whether answering `prompt` with `verdict` asks for Face ID first:
    /// any allow (an answer to questions too) of a tool off the read-only
    /// list. Deny and Decline never do.
    public static func needsOwner(_ prompt: ApprovalPrompt, _ verdict: ApprovalVerdict) -> Bool {
        verdict.allows && !readOnly.contains(prompt.tool)
    }

    /// What the Face ID prompt says: "Allow <tool>: <summary's first 60
    /// characters>", cleaned, and how many more lines it has, so a command
    /// whose first line is harmless never reads as all there is.
    public static func reason(_ prompt: ApprovalPrompt) -> String {
        let tool = SafeText.line(prompt.tool[...], limit: 60)
        let summary = SafeText.line(prompt.summary[...], limit: 60)
        let more = lines(prompt.summary) - 1
        let rest = more > 0 ? " (+\(more) more line\(more == 1 ? "" : "s"))" : ""
        return summary.isEmpty ? "Allow \(tool)" : "Allow \(tool): \(summary)\(rest)"
    }

    /// The lines `text` holds, once cleaned (a line or paragraph separator
    /// counts as a break), blank ones too.
    static func lines(_ text: String) -> Int {
        SafeText.clean(text).split(separator: "\n", omittingEmptySubsequences: false).count
    }

    /// The most of a request's input the sheet shows.
    public static let inputLimit = 16 * 1024

    /// The host's sentence when the request it was sent for isn't waiting
    /// any more (answered elsewhere, or its turn over): the answer is
    /// dropped, never offered again.
    static func saysGone(_ error: String) -> Bool {
        (error.contains("has no approval request") && error.contains("waiting")) || error.contains("is no longer waiting")
    }
}

/// How a request was settled elsewhere, as `approval.resolved` says it.
public enum ApprovalResolution: Equatable, Sendable {
    case allowed
    case denied

    public var word: String { self == .allowed ? "allowed" : "denied" }
}

/// The requests waiting for an answer from this phone, in arrival order
/// (ticket 91, R-PERM-2): the sheet shows the first ("1 of 3"), and
/// answering it shows the next. Pure bookkeeping on the main actor; the
/// screen presents the sheet and sends what `sender` is handed.
///
/// Each answer goes once. A request takes one answer: tapping again, a
/// second key press, a request re-sent in a welcome or the same one live
/// twice change nothing. While Face ID is up the request is locked; once
/// it says yes, the request is checked again (it may have been answered on
/// another device or its turn ended meanwhile) before anything is sent.
/// The answer then waits for its ack: an error brings the request back,
/// saying why, unless it was settled already (`approval.resolved`, the
/// turn's end, or the host saying it isn't waiting any more).
@MainActor
public final class ApprovalQueue {
    public enum Phase: Equatable, Sendable {
        /// Shown, or waiting its turn to be.
        case waiting
        /// Face ID is up for this answer: nothing else may answer it.
        case confirming(ApprovalVerdict)
        /// Sent as command `token`, waiting for its ack.
        case answered(token: UInt64, verdict: ApprovalVerdict)
    }

    public struct Held: Equatable, Sendable {
        public let prompt: ApprovalPrompt
        public internal(set) var phase: Phase
        /// The host refused the last answer: its sentence.
        public internal(set) var error: String?
        /// The last owner check said no (failed, cancelled).
        public internal(set) var unconfirmed = false
    }

    /// What happened to a request, for the screen: the tool row's line,
    /// the toast, VoiceOver.
    public enum Event: Equatable, Sendable {
        /// Arrived, and waits.
        case arrived(ApprovalPrompt)
        /// Answered here; the ack is to come.
        case answered(ApprovalPrompt, ApprovalVerdict)
        /// The host took the answer.
        case settled(ApprovalPrompt, ApprovalVerdict)
        /// The host refused the answer: the request is back, saying why.
        case returned(ApprovalPrompt, String)
        /// Answered on another device (or the terminal) first.
        case resolvedElsewhere(ApprovalPrompt, ApprovalResolution)
        /// Gone with nothing to say for it: its turn ended, the host said it
        /// waits no more, or the connection stopped.
        case dropped(ApprovalPrompt, String?)
    }

    /// The session the screen follows.
    public let sessionID: String
    let ownerCheck: any OwnerCheck

    /// In arrival order: those waiting (the first is shown), and those
    /// answered and not yet acked.
    public private(set) var held: [Held] = []
    /// Every request id taken in: one re-sent (a welcome, the live line
    /// again) is shown once.
    private var seen: Set<String> = []
    /// Resolved (here or elsewhere): an ack error after it brings nothing
    /// back.
    private var resolved: Set<String> = []
    /// Turns that ended, by session: a request of one is never shown.
    private var endedTurns: Set<String> = []

    /// Joined and welcomed by a host: an answer goes now (never queued for
    /// a later host, where the request may be long gone).
    public var connected = false {
        didSet { if connected != oldValue { onChange?() } }
    }

    /// Hands an answer to the session as a command: its token (the
    /// model's `issue`), or nil if it couldn't go.
    public var sender: (@MainActor (ApprovalReply) -> UInt64?)?
    /// Something changed: the sheet redraws.
    public var onChange: (@MainActor () -> Void)?
    /// What happened to a request.
    public var onEvent: (@MainActor (Event) -> Void)?

    public init(sessionID: String, ownerCheck: any OwnerCheck) {
        self.sessionID = sessionID
        self.ownerCheck = ownerCheck
    }

    /// The request the sheet shows: the first not answered.
    public var current: Held? {
        held.first { !$0.isAnswered }
    }

    /// Requests waiting for an answer here (the sheet's "1 of N").
    public var waiting: Int { held.filter { !$0.isAnswered }.count }

    /// Answers sent and not yet acked.
    public var answering: Int { held.filter(\.isAnswered).count }

    /// "1 of 3", or nil with one.
    public var position: String? {
        let count = waiting
        return count > 1 ? "1 of \(count)" : nil
    }

    // MARK: What the session says

    /// `approval.requested`, live or re-sent in a welcome: queued once.
    public func requested(_ prompt: ApprovalPrompt) {
        guard !endedTurns.contains(Self.turnKey(prompt.sessionID, prompt.turnID)), !resolved.contains(prompt.requestID),
              seen.insert(prompt.requestID).inserted else { return }
        held.append(Held(prompt: prompt, phase: .waiting))
        onEvent?(.arrived(prompt))
        onChange?()
    }

    /// `approval.resolved`: answered, here or elsewhere. One still waiting
    /// here goes, saying it was answered elsewhere. One answered here and
    /// not yet acked is settled as it was resolved: as this phone's answer
    /// when the outcome is the one it sent, and as answered elsewhere when
    /// it isn't (the terminal got there first; this phone's answer will be
    /// refused).
    public func resolved(requestID: String, resolution: ApprovalResolution) {
        resolved.insert(requestID)
        guard let at = held.firstIndex(where: { $0.prompt.requestID == requestID }) else { return }
        let gone = held.remove(at: at)
        if case .answered(_, let verdict) = gone.phase, verdict.allows == (resolution == .allowed) {
            onEvent?(.settled(gone.prompt, verdict))
        } else {
            onEvent?(.resolvedElsewhere(gone.prompt, resolution))
        }
        onChange?()
    }

    /// A turn ended (`turn.completed`, or the run's result): its requests
    /// go, answered or not. A subagent's turn ends on the same stream, by
    /// its own session id.
    public func turnEnded(sessionID session: String, turnID turn: String) {
        endedTurns.insert(Self.turnKey(session, turn))
        let ends = { (prompt: ApprovalPrompt) in prompt.sessionID == session && prompt.turnID == turn }
        let gone = held.filter { ends($0.prompt) }
        guard !gone.isEmpty else { return }
        held.removeAll { ends($0.prompt) }
        for g in gone {
            resolved.insert(g.prompt.requestID)
            onEvent?(.dropped(g.prompt, nil))
        }
        onChange?()
    }

    /// The ack of answer `token`: settled, or the request back with the
    /// host's sentence (unless it was settled meanwhile, or the host says
    /// it waits no more).
    public func acked(_ token: UInt64, error: String?) {
        guard let at = held.firstIndex(where: { $0.token == token }), case .answered(_, let verdict) = held[at].phase else { return }
        guard let error else {
            let done = held.remove(at: at)
            resolved.insert(done.prompt.requestID)
            onEvent?(.settled(done.prompt, verdict))
            onChange?()
            return
        }
        let said = PendingCommands.refusal(error)
        if resolved.contains(held[at].prompt.requestID) || ApprovalRules.saysGone(said) {
            let gone = held.remove(at: at)
            resolved.insert(gone.prompt.requestID)
            onEvent?(.dropped(gone.prompt, said))
        } else {
            // Back at the front: shown next, with the reason.
            var back = held.remove(at: at)
            back.phase = .waiting
            back.error = said
            held.insert(back, at: 0)
            onEvent?(.returned(back.prompt, said))
        }
        onChange?()
    }

    /// The connection stopped for good: nothing more can be answered, and
    /// an answer not acked may or may not have reached the host.
    public func connectionEnded() {
        guard !held.isEmpty else { return }
        let gone = held
        held = []
        for g in gone {
            onEvent?(.dropped(g.prompt, g.isAnswered ? "No answer from the host before the connection stopped: it may have been taken. Open the session again to see." : nil))
        }
        onChange?()
    }

    // MARK: The person's answer

    /// The shown request answered with `verdict` (and `answers`, for
    /// questions): Face ID first for an allow of a tool off the read-only
    /// list (R-IOS-11), then the command, once. Nothing goes when the
    /// check fails or is cancelled, when no live link is up, when the
    /// request isn't the one shown and waiting, or when it was settled
    /// while Face ID was up. Whether an answer went.
    @discardableResult
    public func answer(_ requestID: String, _ verdict: ApprovalVerdict, answers: [QuestionReply] = []) async -> Bool {
        guard connected, let shown = current, shown.prompt.requestID == requestID, shown.phase == .waiting else { return false }
        let prompt = shown.prompt
        // Only the choices the sheet offers.
        if verdict == .allowSession && (prompt.remember.isEmpty || prompt.asks) { return false }
        if !prompt.asks && !answers.isEmpty { return false }
        if prompt.asks && verdict == .allow && !Self.complete(answers, for: prompt) { return false }
        let sent = verdict.allows ? answers : []
        if ApprovalRules.needsOwner(prompt, verdict) {
            set(requestID) { $0.phase = .confirming(verdict) }
            let yes = await ownerCheck.confirm(reason: ApprovalRules.reason(prompt))
            // Settled meanwhile (answered elsewhere, the turn ended): gone,
            // and nothing is sent.
            guard let now = held.first(where: { $0.prompt.requestID == requestID }), now.phase == .confirming(verdict) else { return false }
            guard yes, connected else {
                set(requestID) {
                    $0.phase = .waiting
                    $0.unconfirmed = !yes
                }
                return false
            }
        }
        guard let token = sender?(ApprovalReply(sessionID: prompt.sessionID, requestID: requestID, verdict: verdict, answers: sent)) else {
            set(requestID) { $0.phase = .waiting }
            return false
        }
        set(requestID) {
            $0.phase = .answered(token: token, verdict: verdict)
            $0.error = nil
            $0.unconfirmed = false
        }
        onEvent?(.answered(prompt, verdict))
        return true
    }

    /// Every question has an option picked or something written.
    public static func complete(_ answers: [QuestionReply], for prompt: ApprovalPrompt) -> Bool {
        prompt.questions.allSatisfy { question in
            answers.contains { $0.id == question.id && (!$0.picked.isEmpty || !($0.text ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty) }
        }
    }

    private func set(_ requestID: String, _ change: (inout Held) -> Void) {
        guard let at = held.firstIndex(where: { $0.prompt.requestID == requestID }) else { return }
        change(&held[at])
        onChange?()
    }

    private static func turnKey(_ session: String, _ turn: String) -> String {
        "\(session)\u{0}\(turn)"
    }
}

extension ApprovalQueue.Held {
    public var isAnswered: Bool {
        if case .answered = phase { true } else { false }
    }

    var token: UInt64? {
        if case .answered(let token, _) = phase { token } else { nil }
    }

    /// Face ID is up for it.
    public var isConfirming: Bool {
        if case .confirming = phase { true } else { false }
    }
}

/// The answers to a request's questions as the sheet collects them: one
/// pick per question (several when `multiSelect`) and what was written.
public struct QuestionDraft: Equatable, Sendable {
    public private(set) var picked: [String: [String]] = [:]
    public var written: [String: String] = [:]

    public init() {}

    /// Option `label` of `question` tapped: picked (the only one, unless
    /// several may be), or unpicked when it was.
    public mutating func toggle(_ label: String, of question: ApprovalQuestion) {
        var now = picked[question.id] ?? []
        if let at = now.firstIndex(of: label) {
            now.remove(at: at)
        } else if question.multiSelect {
            now.append(label)
        } else {
            now = [label]
        }
        picked[question.id] = now
    }

    public func isPicked(_ label: String, of question: ApprovalQuestion) -> Bool {
        picked[question.id]?.contains(label) ?? false
    }

    /// The `QuestionAnswer`s, in the questions' order: picked in the
    /// options' order; what was written, trimmed, or nil.
    public func replies(for prompt: ApprovalPrompt) -> [QuestionReply] {
        prompt.questions.map { question in
            let chosen = picked[question.id] ?? []
            let ordered = question.options.map(\.label).filter { chosen.contains($0) }
            let text = written[question.id]?.trimmingCharacters(in: .whitespacesAndNewlines)
            return QuestionReply(id: question.id, picked: ordered, text: (text?.isEmpty ?? true) ? nil : text, secret: question.secret)
        }
    }
}
