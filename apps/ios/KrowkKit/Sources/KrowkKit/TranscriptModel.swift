import Foundation
import UIKit
import Observation

/// What a transcript row shows, one per item id (ticket 88): a thin
/// `@Observable` layer over the session's events, so a later change to one
/// item invalidates only its own cell, which observes it.
@MainActor
@Observable
public final class ItemState {
    public enum Kind: Equatable, Sendable {
        /// Your prompt (`userText`): a right-aligned bubble.
        case user
        /// The model's answer (`assistantText`): full-width text.
        case assistant
        /// The model's readable reasoning: "Thought", expanded on a tap.
        case reasoning
        /// A tool call and, once it comes, its result beneath it.
        case tool
        /// A thin separator: a turn's end, or "Now on <model>".
        case mark
        /// A prompt or steer typed here, shown at once until the host logs
        /// it (ticket 90): a bubble with how its send stands beneath.
        case pending
    }

    /// The row's id in the transcript: `item:<itemId>`, `turn:<turnId>` or
    /// `switch:<eventId>`.
    public nonisolated let id: String
    public nonisolated let kind: Kind
    /// A prompt's, an answer's or reasoning's text; a mark's line. As the
    /// session holds it: cells clean it (`SafeText`) as they draw it.
    public var text: String
    /// A tool call's name, and its input as JSON text.
    public var toolName: String
    public var toolInput: String
    /// A tool call's result, once it came.
    public var output: String?
    public var isError = false
    /// A mark that says a turn ended other than completed.
    public var alarming = false
    /// The item is whole. Always so from the log at rest, which holds only
    /// completed items; an item started live is not done until its logged
    /// completion (or its turn's end) comes, and its text grows meanwhile.
    public var done: Bool
    /// A tool call started live whose result hasn't come: "running…".
    public var running = false
    /// A reasoning or tool row, expanded by a tap.
    public var expanded = false
    /// A pending bubble's line beneath it: "sending…", "queued…", or why
    /// the host refused it; empty once the host took it.
    public var caption = ""
    /// A pending bubble the host refused (or that was never sent): its
    /// caption is the reason, and it offers "Edit and resend".
    public var refused = false
    /// A tool call waiting on an approval (ticket 91), until its result:
    /// "waiting for approval", "allowed — sending…", "allowed", "denied".
    public var verdict = ""

    @ObservationIgnored private var summarized: (input: String, summary: String)?
    /// The cell drawing it now, if one is: redrawn on each change (the
    /// one observation an item has, `TranscriptCell.follow`).
    @ObservationIgnored weak var drawer: TranscriptCell?
    /// Followed already: at most one observation per item, however often
    /// cells show it.
    @ObservationIgnored var followed = false

    public init(id: String, kind: Kind, text: String = "", toolName: String = "", toolInput: String = "", done: Bool = true) {
        self.id = id
        self.kind = kind
        self.text = text
        self.toolName = toolName
        self.toolInput = toolInput
        self.done = done
    }

    /// Reads every property a cell draws, so one observation covers them.
    func touch() {
        _ = (text, toolName, toolInput, output, isError, alarming, done, running, expanded, caption, refused, verdict)
    }

    /// `source` drawn as `role` at `size`, from the last drawing if neither
    /// changed.
    func drawing(_ role: String, source: String, font: UIFont, _ make: () -> NSAttributedString) -> NSAttributedString {
        let key = "\(role) \(kind) \(id) \(ObjectIdentifier(self).hashValue)" as NSString
        if let last = TranscriptDrawings.cache.object(forKey: key), last.font == font, last.source == source { return last.text }
        let text = make()
        TranscriptDrawings.cache.setObject(TranscriptDrawings.Drawing(source: source, font: font, text: text), forKey: key, cost: text.length * 2)
        return text
    }

    /// Forgets a drawing (a collapsed output isn't kept).
    func forget(_ role: String) {
        TranscriptDrawings.cache.removeObject(forKey: "\(role) \(kind) \(id) \(ObjectIdentifier(self).hashValue)" as NSString)
    }

    /// A tool call's one line: `<name> <summary of its input>`.
    public var toolLine: String {
        let name = SafeText.line(toolName[...], limit: 80)
        let summary = inputSummary
        return summary.isEmpty ? name : "\(name) \(summary)"
    }

    /// The input's `command`, `file_path`/`path`, `pattern` or `url` when it
    /// has one, else its JSON's first 80 characters: one line, cleaned.
    /// Worked out once per input.
    public var inputSummary: String {
        let input = toolInput
        if let summarized, summarized.input == input { return summarized.summary }
        let summary = Self.summary(of: input)
        summarized = (input, summary)
        return summary
    }

    /// Inputs larger than this aren't parsed for a field: the first 80
    /// characters say enough, and a large one would hold up the main thread.
    nonisolated static let parsedInputLimit = 256 * 1024

    public nonisolated static func summary(of input: String) -> String {
        if input.utf8.count <= parsedInputLimit,
           let object = try? JSONSerialization.jsonObject(with: Data(input.utf8)) as? [String: Any] {
            for key in ["command", "file_path", "path", "pattern", "url"] {
                if let value = object[key] as? String, !value.isEmpty {
                    return SafeText.line(value[...], limit: 200)
                }
            }
        }
        return SafeText.line(input.prefix(80), limit: 80)
    }
}

/// Text as cells last drew it, shared by every transcript and bounded: a
/// long session scrolled through keeps no more than this of it.
@MainActor
enum TranscriptDrawings {
    final class Drawing {
        let source: String
        let font: UIFont
        let text: NSAttributedString
        init(source: String, font: UIFont, text: NSAttributedString) {
            self.source = source
            self.font = font
            self.text = text
        }
    }

    static let cache: NSCache<NSString, Drawing> = {
        let cache = NSCache<NSString, Drawing>()
        cache.countLimit = 300
        cache.totalCostLimit = 8 << 20
        return cache
    }()
}

/// How a turn ended, as its separator says.
public enum TurnEnd: Equatable, Sendable {
    case completed
    case interrupted
    /// With the error's message, when it had one.
    case failed(String?)

    public var line: String {
        switch self {
        case .completed: "Turn completed"
        case .interrupted: "Turn interrupted"
        case .failed(let message?) where !message.isEmpty: "Turn failed: \(message)"
        case .failed: "Turn failed"
        }
    }
}

/// What one logged event does to the transcript, as the bindings' side
/// reads it from the generated `LogEvent` (KrowkAccount; R-PROTO-2): the
/// events a transcript draws, and nothing for the rest.
public enum TranscriptChange: Equatable, Sendable {
    /// A prompt or steer, with its turn: the first of a turn is its prompt,
    /// which a rate-limit offer sends again (ticket 135).
    case userText(item: String, text: String, images: [UInt32], turn: String? = nil)
    case assistantText(item: String, text: String)
    case reasoning(item: String, text: String)
    case toolCall(item: String, callID: String, name: String, input: String)
    case toolResult(item: String, callID: String, output: String, isError: Bool)
    /// `turn.started`: a turn runs until its `turn.completed` (the
    /// composer's Steer and Stop, ticket 90), on `model`: the session's
    /// current model from then (ticket 135). Draws nothing.
    case turnStarted(turn: String, model: ModelChoice? = nil)
    case turnCompleted(turn: String, end: TurnEnd)
    /// `model.switched`: "Now on <model>", and the session's current model
    /// from then is `to`. `failed`: a switch that couldn't run its turn,
    /// back to the model before (R-SWITCH-4), from `from`.
    case modelSwitched(model: String, from: ModelChoice? = nil, to: ModelChoice? = nil, failed: Bool = false)
}

/// One logged event as the transcript takes it: its id, the event before
/// it on its branch, its session, and what it changes (nil: nothing to
/// draw).
public struct TranscriptEvent: Equatable, Sendable {
    public let id: String
    public let parentID: String?
    public let sessionID: String
    public let change: TranscriptChange?
    /// When the host logged it (its clock, ms since 1970), if known.
    public let timeMs: Int64?

    public init(id: String, parentID: String? = nil, sessionID: String, change: TranscriptChange?, timeMs: Int64? = nil) {
        self.id = id
        self.parentID = parentID
        self.sessionID = sessionID
        self.change = change
        self.timeMs = timeMs
    }
}

/// Where the rows of applied events go.
public enum TranscriptPlacement: Sendable {
    /// At the end: the session read at rest, a live line.
    case end
    /// After the row of the event before them (`parentID`), when that was
    /// applied: a catch-up, which brings events a gap left out after later
    /// ones were drawn. At the end when it wasn't.
    case afterParent
}

/// What an item started live is, before any of it came (the generated
/// `ItemKind`, as KrowkAccount maps it).
public enum LiveItemKind: Equatable, Sendable {
    case user
    case assistant
    case reasoning
    case toolCall(callID: String, name: String)
    case toolResult(callID: String)
}

/// One of the session's updates as the transcript takes it (ticket 89):
/// what KrowkAccount maps the viewer's `Update`s to, a display frame's worth
/// at a time. Everything else the viewer says (the host, a gap, a note)
/// is the status bar's, and approvals are ticket 91's.
public enum TranscriptUpdate: Equatable, Sendable {
    /// The session as read at rest.
    case attached([TranscriptEvent])
    /// Logged lines, in order: a run of them, applied at once.
    case logged([TranscriptEvent])
    /// What a gap left out, from the host: placed after its parents.
    case caughtUp([TranscriptEvent])
    /// `item.started`: an item under way.
    case started(session: String, turn: String, item: String, kind: LiveItemKind)
    /// `item.delta` with text: more of an answer's or reasoning's text.
    case delta(session: String, item: String, text: String)
    /// `notice`: a one-line note.
    case notice(session: String, text: String)
    /// The core gave the command typed as `token` its id (ticket 90): what
    /// its `commandSent` and `commandAcked` name.
    case commandMinted(token: UInt64, id: String)
    /// The command typed as `token` never reached the core: why.
    case commandUnsent(token: UInt64, why: String)
    /// `Update.sent`: shown sent, or queued with no host (R-LAG-8).
    case commandSent(id: String, queued: Bool)
    /// `Update.acked`: the host took it, or refused it saying why.
    case commandAcked(id: String, error: String?)
    /// `result`: the run's end of turn `turn`, with the host's offer of
    /// another instance when its own hit its limit (R-INST-7, ticket 135).
    case result(session: String, turn: String, offer: SwitchOfferInfo?)
    /// `limits`: how near its limit `instance` is (R-INST-6).
    case limits(session: String, instance: String, limit: InstanceLimit)
}

/// What a frame's apply did that the screen draws at once, inside the
/// frame, rather than on the observation's next turn.
public struct FrameApplied {
    /// Items whose text grew by deltas, each once, in the order they grew.
    public var grown: [ItemState] = []
    /// Turn ends a logged line brought (not the session read at rest, nor
    /// a catch-up): what VoiceOver is told.
    public var ended: [ItemState] = []
    /// Bubbles the host refused in this frame: VoiceOver is told why.
    public var refused: [ItemState] = []
    /// The host refused an interrupt while the turn still runs: why.
    public var stopRefused: String?
    /// Approval answers the host acked (`error` nil) or refused, or that
    /// never reached the core, in order: for the approval queue.
    public var approvalAnswers: [(token: UInt64, error: String?)] = []
    /// The host refused the phone's model switch, or it never went: why
    /// (ticket 135).
    public var switchRefused: String?
    /// The switch an offer's Continue sent was taken: the limited turn's
    /// prompt, to send again now as a new prompt.
    public var promptAgain: String?
}

/// A session's transcript (ticket 88): its rows in order, by id, and one
/// `ItemState` per row. Events are applied by id: the same event twice is
/// a no-op (ticket 89 relies on it where live lines and catch-up overlap),
/// and an event of another session (a subagent's, on the same log) is
/// skipped. A tool's result is drawn under its call, paired by `callId`.
@MainActor
@Observable
public final class TranscriptModel {
    public nonisolated let sessionID: String
    /// The rows, top to bottom: the session's, then the bubbles of what
    /// was typed here and not yet logged. Never holds an id twice: the
    /// diffable data source traps on one.
    public private(set) var ids: [String] = []
    /// The session's own rows, top to bottom (`ids` without the bubbles).
    @ObservationIgnored private var rows: [String] = []

    /// What the composer sent and the host hasn't logged yet (ticket 90).
    @ObservationIgnored public private(set) var commands = PendingCommands()
    /// Each bubble's state, by token.
    @ObservationIgnored private var bubbles: [UInt64: ItemState] = [:]
    /// The bubbles' rows, in the order typed, and whether they changed in
    /// the transaction under way.
    @ObservationIgnored private var bubbleRows: [String] = []
    @ObservationIgnored private var bubblesChanged = false
    /// No host on the session: a queued bubble says it waits for one.
    @ObservationIgnored private var hostOffline = false
    /// The turn running, from its `turn.started` (or an item started live
    /// in it) to its `turn.completed`: the composer offers Steer and Stop.
    @ObservationIgnored public private(set) var runningTurn: String?
    /// Turns whose end was applied: a start that comes after it (a
    /// catch-up's) doesn't set one running again.
    @ObservationIgnored private var endedTurns: Set<String> = []
    /// The session as read at rest was applied (empty or not).
    public private(set) var attached = false

    @ObservationIgnored private var items: [String: ItemState] = [:]
    /// Every event applied, by id.
    @ObservationIgnored private var seen: Set<String> = []
    /// A tool call's row, by its `callId`: the latest call with that id,
    /// which a result follows (a provider may reuse an id).
    @ObservationIgnored private var calls: [String: ItemState] = [:]
    /// Per event applied, the row at or above it ("" for the top): where an
    /// event that follows it goes, placed `afterParent`.
    @ObservationIgnored private var anchors: [String: String] = [:]
    /// A result whose call wasn't applied yet (before the checkpoint, or a
    /// gap's catch-up still to come), by `callId`: drawn on a row of its
    /// own until its call comes, which takes it in.
    @ObservationIgnored private var orphans: [String: ItemState] = [:]
    /// Rows a call took in while `apply` runs, to drop.
    @ObservationIgnored private var adopted: [(orphan: String, call: String)] = []
    /// How the events `apply` is taking are placed.
    @ObservationIgnored private var placing = TranscriptPlacement.end
    /// The rows as the transaction under way leaves them: `ids` is set
    /// once, at its end, and only if they changed.
    @ObservationIgnored private var working: [String]?
    /// How many transactions changed (or might have changed) the model:
    /// one per display frame that brought anything.
    @ObservationIgnored public private(set) var transactions = 0

    /// Items started live and not yet whole, by row id, and the turn of
    /// each: their text grows by deltas until their logged completion
    /// replaces it (if it differs) or their turn ends.
    @ObservationIgnored private var streaming: [String: (state: ItemState, turn: String)] = [:]
    /// Of those, prompts, answers and reasoning with no visible text yet:
    /// no row until some comes, so no empty row shows.
    @ObservationIgnored private var unshown: Set<String> = []
    /// Tool calls started live whose result hasn't come, and their turn.
    @ObservationIgnored private var running: [String: (state: ItemState, turn: String)] = [:]
    /// Rows started live whose logged completion hasn't come: one that
    /// comes after its turn's end settled them (a gap's catch-up, after
    /// the end came live) still sets their text, or a call's input.
    @ObservationIgnored private var unsettled: Set<String> = []
    /// The existing row a completion just settled (`take` adds none): the
    /// row an event after it goes under.
    @ObservationIgnored private var settledRow: String?
    /// Notes drawn so far, for their row ids, and the last one's text: the
    /// same note again, straight after, isn't drawn twice.
    @ObservationIgnored private var notices = 0
    @ObservationIgnored private var lastNotice: (row: String, text: String)?
    /// Turn ends a logged line brought in the frame being applied.
    @ObservationIgnored private var ended: [ItemState] = []
    /// Bubbles refused by a turn's end in the frame being applied.
    @ObservationIgnored private var refusedNow: [ItemState] = []
    /// What placing is applying: the session read at rest, a catch-up, or
    /// a live line.
    @ObservationIgnored private var live = false
    /// When the host logged the newest live line applied (its clock): how
    /// far behind the host the screen is, as LiveFollowTests measure it.
    @ObservationIgnored public private(set) var newestLiveMs: Int64?

    /// At most this much of an item's streamed text is kept while it
    /// streams: more than a cell lays out (64 KiB), and its logged
    /// completion brings the whole text anyway. A host that never ends an
    /// item can't grow the phone's memory without bound.
    public nonisolated static let streamLimit = 4 * SafeText.textLimit

    /// An item is under way: the display link keeps running a while longer
    /// when nothing comes, rather than pause.
    public var isStreaming: Bool { !streaming.isEmpty }

    public init(sessionID: String) {
        self.sessionID = sessionID
    }

    public func item(_ id: String) -> ItemState? {
        items[id] ?? token(ofBubble: id).flatMap { bubbles[$0] }
    }

    // MARK: Commands (ticket 90)

    /// A turn runs: the composer's button reads Steer, and Stop shows.
    public var isTurnRunning: Bool { runningTurn != nil }
    /// An interrupt is out: Stop reads "stopping…" until its ack or the
    /// turn's end.
    public var stopping: Bool { commands.stopping }
    /// Commands the core holds unanswered, and Continue's prompt waiting
    /// for its switch to be logged: a screen left meanwhile is parked, so
    /// the prompt still goes.
    public var commandsOut: Int { commands.out + (continuing == nil ? 0 : 1) }
    /// A bubble the host took whose logged item hasn't come yet.
    public var awaitingLog: Bool { commands.bubbles.contains { $0.state == .settled } }

    /// A pending bubble's row id.
    public nonisolated static func bubbleID(_ token: UInt64) -> String { "pending:\(token)" }

    /// The token of the bubble whose row is `id`.
    public func token(ofBubble id: String) -> UInt64? {
        guard id.hasPrefix("pending:") else { return nil }
        return UInt64(id.dropFirst("pending:".count))
    }

    /// A command typed (R-LAG-8): shown now, in this run-loop turn, before
    /// anything is sent — a prompt or steer as a bubble at the end, an
    /// interrupt as "stopping…". Its token, which the session's updates
    /// name it by until the core mints its id.
    @discardableResult
    public func issue(_ command: LiveCommand) -> UInt64 {
        let token: UInt64
        switch command {
        case .prompt(let text):
            token = commands.add(.prompt, text: text)
            // A prompt of the person's own answers an offer with a no, and
            // is the one a later offer sends again.
            lastPrompt = text
            if offer != nil { offer = nil }
        case .steer(let text): token = commands.add(.steer, text: text)
        case .interrupt: return commands.add(.interrupt)
        case .approve: return commands.add(.approve)
        case .switchModel(let choice):
            let token = commands.add(.switchModel, text: choice.name)
            switching = choice
            switchToken = token
            switchAcked = false
            switchSince = [newestLiveMs, currentAtMs].compactMap { $0 }.max()
            if switchRefusal != nil { switchRefusal = nil }
            if let continuing, continuing.token != token { self.continuing = nil }
            return token
        }
        let state = ItemState(id: Self.bubbleID(token), kind: .pending, text: commands.entry(token)?.text ?? "")
        bubbles[token] = state
        refresh(token)
        transact {
            bubbleRows.append(state.id)
            bubblesChanged = true
        }
        return token
    }

    /// A refused bubble taken back (edit and resend) or dismissed: its
    /// text, for the composer.
    @discardableResult
    public func takeBack(_ token: UInt64) -> String? {
        guard let entry = commands.entry(token), entry.isRefused else { return nil }
        commands.remove(token)
        transact { dropBubble(token) }
        return entry.text
    }

    /// The refused bubbles, in the order typed: what a screen let go keeps
    /// for the next screen of the session (`restore`).
    public var refusals: [PendingCommands.Entry] {
        commands.bubbles.filter(\.isRefused)
    }

    /// Refused bubbles of an earlier screen of this session, shown again
    /// below the rows, each with "Edit and resend".
    public func restore(_ refusals: [PendingCommands.Entry]) {
        guard !refusals.isEmpty else { return }
        transact {
            for entry in refusals {
                guard case .refused(let why) = entry.state else { continue }
                let token = commands.addRefused(entry.kind, text: entry.text, why: why)
                let state = ItemState(id: Self.bubbleID(token), kind: .pending, text: entry.text)
                bubbles[token] = state
                refresh(token)
                bubbleRows.append(state.id)
                bubblesChanged = true
            }
        }
    }

    /// Whether the session has a host: a queued bubble says what it waits
    /// for.
    public func setHostOffline(_ offline: Bool) {
        guard offline != hostOffline else { return }
        hostOffline = offline
        for entry in commands.bubbles where entry.state == .queued { refresh(entry.token) }
    }

    /// The connection stopped for good: what's still out is refused,
    /// saying so. The bubbles refused.
    @discardableResult
    public func connectionEnded() -> [ItemState] {
        // A switch still out goes with the connection: what the host took
        // is in its log, read when the session opens again.
        if switching != nil { endSwitch() }
        let changed = commands.connectionEnded()
        for token in changed { refresh(token) }
        return changed.compactMap { bubbles[$0] }
    }

    /// A steer's refusal that says no turn is running to take it (the host
    /// daemon's `no_running_turn` or `turn_ending` sentence).
    nonisolated static func saysNoTurn(_ error: String) -> Bool {
        error.contains("no turn running") || error.contains("is finishing and reads no more input")
    }

    /// Bubble `token`'s caption as its entry stands.
    private func refresh(_ token: UInt64) {
        guard let state = bubbles[token], let entry = commands.entry(token) else { return }
        let caption: String
        switch entry.state {
        case .sending: caption = "sending…"
        case .queued: caption = hostOffline ? "queued — sends when your laptop is back" : "queued…"
        case .settled: caption = ""
        case .refused(let why): caption = why
        }
        if state.caption != caption { state.caption = caption }
        if state.refused != entry.isRefused { state.refused = entry.isRefused }
    }

    private func dropBubble(_ token: UInt64) {
        guard let state = bubbles.removeValue(forKey: token) else { return }
        bubbleRows.removeAll { $0 == state.id }
        bubblesChanged = true
    }

    /// One of the session's updates about a command, applied.
    private func command(_ update: TranscriptUpdate, into applied: inout FrameApplied) {
        switch update {
        case .commandMinted(let token, let id):
            commands.minted(token, id: id)
        case .commandUnsent(let token, let why):
            let answer = commands.entry(token)?.kind == .approve
            commands.unsent(token, why: why)
            if answer { applied.approvalAnswers.append((token, why)) }
            if token == switchToken {
                endSwitch()
                switchRefusal = why
                applied.switchRefused = why
            }
            refresh(token)
            if let state = bubbles[token], state.refused { applied.refused.append(state) }
        case .commandSent(let id, let queued):
            if let token = commands.sent(id, queued: queued) { refresh(token) }
        case .commandAcked(let id, let error):
            switch commands.acked(id, error: error, turnRunning: runningTurn != nil) {
            case .bubble(let token)?:
                refresh(token)
                if let state = bubbles[token], state.refused {
                    applied.refused.append(state)
                    // A steer refused because no turn runs, while one looks
                    // running (its end never reached the log: a host that
                    // died mid-turn): the composer sends prompts again. A
                    // later item of that turn sets it running again.
                    if commands.entry(token)?.kind == .steer, let error, Self.saysNoTurn(error) {
                        runningTurn = nil
                    }
                }
            case .interrupt(_, let error?)? where runningTurn != nil:
                applied.stopRefused = error
            case .approval(let token, let error)?:
                applied.approvalAnswers.append((token, error))
            case .switched(let token, let error)?:
                answeredSwitch(token, error: error, into: &applied)
            case .interrupt?, nil:
                break
            }
        default:
            break
        }
    }

    // MARK: Approvals on tool rows (ticket 91)

    /// The tool row each approval's line is on, by request id.
    @ObservationIgnored private var approvalRows: [String: String] = [:]
    /// Lines for requests whose row isn't drawn yet, in the order they
    /// came: placed when it is.
    @ObservationIgnored private var unplacedMarks: [(request: String, tool: String, input: String, line: String)] = []

    /// Request `requestID`'s line on its tool call's row (R-LAG-8), in this
    /// session (a subagent's call has no row here). Nil clears it. The
    /// request names no call, so the row is found among the calls of
    /// `tool` since the last turn's end with no result and no other
    /// request's line: the one whose input is `input`, else the oldest
    /// (requests come in the order their calls were made). A row found
    /// once keeps the line.
    public func markApproval(_ requestID: String, session: String, tool: String, input: String, line: String?) {
        guard session == sessionID else { return }
        unplacedMarks.removeAll { $0.request == requestID }
        guard let line else {
            if let row = approvalRows.removeValue(forKey: requestID), let state = items[row], !state.verdict.isEmpty { state.verdict = "" }
            return
        }
        guard let row = approvalRows[requestID] ?? toolRow(for: tool, input: input), let state = items[row] else {
            unplacedMarks.append((requestID, tool, input, line))
            return
        }
        approvalRows[requestID] = row
        if state.verdict != line { state.verdict = line }
    }

    /// The row for a request of `tool` with `input`, as `markApproval`
    /// says.
    private func toolRow(for tool: String, input: String) -> String? {
        let taken = Set(approvalRows.values)
        var open: [ItemState] = []
        for id in rows.reversed() {
            if id.hasPrefix("turn:") { break }
            guard let state = items[id], state.kind == .tool, state.output == nil, state.toolName == tool, !taken.contains(id) else { continue }
            open.append(state)
        }
        let wanted = Self.json(input)
        return (open.last { wanted != nil && Self.json($0.toolInput) == wanted } ?? open.last)?.id
    }

    /// `text` parsed as JSON, to compare two inputs whatever their key
    /// order or spacing; nil when it doesn't parse or is too large to.
    nonisolated static func json(_ text: String) -> NSObject? {
        guard !text.isEmpty, text.utf8.count <= ItemState.parsedInputLimit else { return nil }
        return (try? JSONSerialization.jsonObject(with: Data(text.utf8), options: [.fragmentsAllowed])) as? NSObject
    }

    /// Lines waiting for their rows, tried again after rows came.
    private func placeMarks() {
        guard !unplacedMarks.isEmpty else { return }
        let waiting = unplacedMarks
        unplacedMarks = []
        for mark in waiting {
            markApproval(mark.request, session: sessionID, tool: mark.tool, input: mark.input, line: mark.line)
        }
    }

    // MARK: The model (ticket 135)

    /// The model the session runs on: the newest of its log's
    /// `turn.started` models and `model.switched` targets, as applied. Nil
    /// until one is.
    public private(set) var currentModel: ModelChoice?
    /// When the host logged what set it: a catch-up's older line doesn't
    /// move it back.
    @ObservationIgnored private var currentAtMs: Int64?
    /// A switch this phone asked for (R-LAG-8): shown at once, until the
    /// host refuses it or its `model.switched` comes. During a running turn
    /// the host keeps it for after the turn, so it stays until then.
    public private(set) var switching: ModelChoice?
    @ObservationIgnored private var switchToken: UInt64?
    /// The host took the switch: the next `model.switched` or turn start
    /// it logs is the switch, or what replaced it (the host keeps one
    /// switch for after a running turn, the last asked for).
    @ObservationIgnored private var switchAcked = false
    /// The host's clock when the switch was asked for, as far as the
    /// screen knows it: a catch-up's line from before then is not its
    /// answer.
    @ObservationIgnored private var switchSince: Int64?
    /// The host's refusal of the last switch (R-SWITCH-4): its sentence,
    /// ending "— the session stays on <model>", until the next switch.
    public private(set) var switchRefusal: String?
    /// The host's offer, when the session's last turn hit its instance's
    /// limit (R-INST-7): withdrawn by another turn or a prompt typed here.
    public private(set) var offer: HeldOffer?
    /// Continue's switch, and the prompt it sends again once the host
    /// takes the switch.
    @ObservationIgnored private var continuing: (token: UInt64, text: String)?
    /// Continue's prompt, its switch settled in the frame being applied.
    @ObservationIgnored private var againNow: String?
    /// The last prompt this phone sent, and the latest turns' prompts: the
    /// limited turn was this phone's when its prompt is that one.
    @ObservationIgnored private var lastPrompt: String?
    @ObservationIgnored private var asked: [(turn: String, text: String)] = []
    /// Turns whose prompts `asked` keeps: enough for a catch-up that comes
    /// after the result of the turn it brings the start of.
    static let askedKept = 8
    /// Each instance's limit, as the session's `limits` lines last said
    /// (R-INST-6).
    public private(set) var limits: [String: InstanceLimit] = [:]
    @ObservationIgnored private var turnModels: [String: ModelChoice] = [:]

    /// The navigation bar's subtitle: `switching to <model>…`, or the
    /// current model.
    public var modelLine: String { ModelLine.text(current: currentModel, switching: switching) }

    /// The model turn `turn` started on, if its start was applied.
    public func model(ofTurn turn: String) -> ModelChoice? { turnModels[turn] }

    /// The offer taken (Continue) or put away: it goes.
    @discardableResult
    public func takeOffer() -> HeldOffer? {
        defer { if offer != nil { offer = nil } }
        return offer
    }

    /// Once the switch `token` is logged, `text` goes again as a new
    /// prompt (`FrameApplied.promptAgain`): the offer's Continue. Not with
    /// the switch, nor at its ack: the host's bridge runs each command a
    /// viewer sends on a task of its own, so a prompt sent beside the
    /// switch can start its turn before the switch is logged, on the
    /// limited instance again.
    public func prompt(_ text: String, afterSwitch token: UInt64) {
        continuing = (token, text)
    }

    /// The refusal under the subtitle, put away.
    public func dismissSwitchRefusal() {
        if switchRefusal != nil { switchRefusal = nil }
    }

    private func current(_ model: ModelChoice, at time: Int64?) {
        // A catch-up brings what a gap left out, after later lines: one
        // the host logged before what set the model doesn't move it.
        if placing == .afterParent, let time, let currentAtMs, time < currentAtMs { return }
        if currentModel != model { currentModel = model }
        if let time { currentAtMs = max(currentAtMs ?? time, time) }
    }

    /// The switch out ends: `settled` by its `model.switched` (Continue's
    /// prompt goes now), or refused or lost (it never goes).
    private func endSwitch(settled: Bool = false) {
        if let continuing, continuing.token == switchToken || switchToken == nil {
            if settled { againNow = continuing.text }
            self.continuing = nil
        }
        if switching != nil { switching = nil }
        switchToken = nil
        switchAcked = false
    }

    /// Whether a line logged at `time` can answer the switch out: not a
    /// catch-up's from before it was asked for.
    private func answersSwitch(at time: Int64?) -> Bool {
        guard placing == .afterParent, let time, let switchSince else { return true }
        return time > switchSince
    }

    /// The host answered switch `token`: refused, it goes back, saying the
    /// host's sentence; taken, it waits for its `model.switched` (none
    /// comes for the model it is on already, so that one settles now).
    private func answeredSwitch(_ token: UInt64, error: String?, into applied: inout FrameApplied) {
        guard token == switchToken else { return }
        if let error {
            endSwitch()
            let said = PendingCommands.refusal(error)
            switchRefusal = said
            applied.switchRefused = said
            return
        }
        switchAcked = true
        if switching == currentModel, runningTurn == nil { endSwitch(settled: true) }
    }

    /// The first text of turn `turn`, its prompt, kept.
    private func keepPrompt(_ turn: String, _ text: String) {
        guard !asked.contains(where: { $0.turn == turn }) else { return }
        if asked.count == Self.askedKept { asked.removeFirst() }
        asked.append((turn, text))
        // The limited turn's start caught up after its result: Continue
        // now, if it was this phone's.
        if let offer, offer.turn == turn, offer.again == nil, let again = prompt(of: turn) {
            self.offer?.again = again
        }
    }

    /// Turn `turn`'s prompt, when it is the last one this phone sent.
    private func prompt(of turn: String) -> String? {
        guard let lastPrompt, let text = asked.first(where: { $0.turn == turn })?.text,
              PendingCommands.trimmed(text) == PendingCommands.trimmed(lastPrompt) else { return nil }
        return lastPrompt
    }

    /// The session as read at rest: its events applied, and `attached` set.
    public func attach(_ events: [TranscriptEvent]) {
        transact {
            place(events, placement: .end)
            // A result read at rest without its call: its call is before the
            // checkpoint, and never comes. A later call reusing the id is
            // another call.
            orphans = [:]
        }
        if !attached { attached = true }
    }

    /// `events`, in order, each at most once, their rows placed by
    /// `placement`. `ids` changes once, however many rows they add.
    public func apply(_ events: [TranscriptEvent], placement: TranscriptPlacement = .end) {
        transact { place(events, placement: placement) }
    }

    /// One display frame's updates, in order, as one transaction (R-LAG-7):
    /// `ids` changes at most once, and only when a row is added (a new
    /// id) or taken in; a delta only grows its item's text, which its cell
    /// observes, so no snapshot is applied for it. Deltas in a row for the
    /// same item are joined first, so 200 of them are one append.
    @discardableResult
    public func apply(frame updates: [TranscriptUpdate]) -> FrameApplied {
        var applied = FrameApplied()
        transact {
            var deltas = Deltas()
            for update in updates {
                if case .delta(let session, let item, let text) = update {
                    if session == sessionID { deltas.add(text, to: "item:\(item)") }
                    continue
                }
                flush(&deltas, into: &applied)
                switch update {
                case .attached(let events):
                    place(events, placement: .end)
                    orphans = [:]
                case .logged(let events):
                    if let newest = events.compactMap(\.timeMs).max() { newestLiveMs = max(newestLiveMs ?? newest, newest) }
                    live = true
                    place(events, placement: .end)
                    live = false
                case .caughtUp(let events):
                    place(events, placement: .afterParent)
                case .started(let session, let turn, let item, let kind):
                    if session == sessionID { start("item:\(item)", turn: turn, kind: kind) }
                case .notice(let session, let text):
                    guard session == sessionID else { continue }
                    if let lastNotice, lastNotice.text == text, working!.last == lastNotice.row { continue }
                    notices += 1
                    let state = ItemState(id: "notice:\(notices)", kind: .mark, text: text)
                    items[state.id] = state
                    working!.append(state.id)
                    lastNotice = (state.id, text)
                case .delta:
                    break
                case .commandMinted, .commandUnsent, .commandSent, .commandAcked:
                    command(update, into: &applied)
                case .result(let session, let turn, let offer):
                    guard session == sessionID else { continue }
                    // A result of the session's own turn replaces the offer.
                    let held = offer.map { HeldOffer(turn: turn, info: $0, again: prompt(of: turn)) }
                    if self.offer != held { self.offer = held }
                case .limits(let session, let instance, let limit):
                    guard session == sessionID, limits[instance] != limit else { continue }
                    limits[instance] = limit
                }
            }
            flush(&deltas, into: &applied)
        }
        if updates.contains(where: { if case .attached = $0 { true } else { false } }), !attached { attached = true }
        applied.ended = ended
        ended = []
        applied.refused += refusedNow
        refusedNow = []
        applied.promptAgain = againNow
        againNow = nil
        placeMarks()
        return applied
    }

    /// Text deltas waiting to be appended, joined per item, in order.
    private struct Deltas {
        var order: [String] = []
        var text: [String: String] = [:]
        mutating func add(_ more: String, to id: String) {
            if text[id] == nil { order.append(id) }
            text[id, default: ""] += more
        }
    }

    private func flush(_ deltas: inout Deltas, into applied: inout FrameApplied) {
        for id in deltas.order {
            if let state = grow(id, by: deltas.text[id] ?? ""), !applied.grown.contains(where: { $0 === state }) {
                applied.grown.append(state)
            }
        }
        deltas = Deltas()
    }

    /// `item.started`: its state, under way. A tool call gets its row at
    /// once ("running…"); a prompt, an answer or reasoning when it has
    /// visible text. An item already here (its completion came first, by a
    /// catch-up) is left as it is; a tool result waits for its logged line,
    /// which goes under its call.
    private func start(_ id: String, turn: String, kind: LiveItemKind) {
        if !endedTurns.contains(turn) { runningTurn = turn }
        guard items[id] == nil, streaming[id] == nil else { return }
        let state: ItemState
        switch kind {
        case .user: state = ItemState(id: id, kind: .user, done: false)
        case .assistant: state = ItemState(id: id, kind: .assistant, done: false)
        case .reasoning: state = ItemState(id: id, kind: .reasoning, done: false)
        case .toolCall(let callID, let name):
            state = ItemState(id: id, kind: .tool, toolName: name, done: false)
            state.running = true
            items[id] = state
            working!.append(id)
            calls[callID] = state
            streaming[id] = (state, turn)
            unsettled.insert(id)
            return
        case .toolResult:
            return
        }
        streaming[id] = (state, turn)
        unshown.insert(id)
        unsettled.insert(id)
    }

    /// `text` appended to item `id`, if it is under way: the item, if its
    /// text changed. Its row appears with its first visible text.
    private func grow(_ id: String, by more: String) -> ItemState? {
        guard let state = streaming[id]?.state, state.kind != .tool, !more.isEmpty else { return nil }
        let room = Self.streamLimit - state.text.utf8.count
        guard room > 0 else { return nil }
        state.text += more.utf8.count <= room ? more : SafeText.truncated(more, limit: room).shown
        if unshown.contains(id), state.text.contains(where: { !$0.isWhitespace }) {
            unshown.remove(id)
            // A row of that id already (a host reusing an item id): never twice.
            if items[id] == nil {
                items[id] = state
                working!.append(id)
            }
        }
        return state
    }

    /// An item's logged completion, if it was started live: `.notLive`
    /// when it wasn't; else its text set (only if it differs from what
    /// streamed), the item whole, and the row it adds if it had none.
    private enum Settled { case notLive, settled(String?) }

    private func settle(_ id: String, text: String) -> Settled {
        guard unsettled.remove(id) != nil else { return .notLive }
        guard let state = streaming.removeValue(forKey: id)?.state else {
            // Its turn ended first: a row if it had one, its text now whole.
            guard let state = items[id] else { return .notLive }
            if state.text != text { state.text = text }
            settledRow = id
            return .settled(nil)
        }
        if state.text != text { state.text = text }
        state.done = true
        guard unshown.remove(id) != nil else {
            settledRow = id
            return .settled(nil)
        }
        guard items[id] == nil else { return .settled(nil) }
        // Never shown: a row now, unless there's nothing to show (an empty
        // answer beside a tool call, withheld reasoning).
        guard state.kind == .user || text.contains(where: { !$0.isWhitespace }) else { return .settled(nil) }
        items[id] = state
        return .settled(id)
    }

    /// Turn `turn` ended: whatever of it was still under way is whole as
    /// it stands (a gap can leave an item without its completion).
    private func endLive(turn: String) {
        for (id, entry) in streaming where entry.turn == turn {
            entry.state.done = true
            entry.state.running = false
            streaming[id] = nil
            unshown.remove(id)
        }
        for (id, entry) in running where entry.turn == turn {
            entry.state.running = false
            running[id] = nil
        }
    }

    /// Runs `body` as one transaction: the rows it leaves set once.
    private func transact(_ body: () -> Void) {
        guard working == nil else { return body() }
        working = rows
        body()
        let transcript = working ?? rows
        working = nil
        transactions += 1
        // Unchanged rows share `rows`' storage: this is a pointer check.
        let changed = transcript != rows
        rows = transcript
        guard changed || bubblesChanged else { return }
        bubblesChanged = false
        // The bubbles always below the session's rows.
        let all = bubbleRows.isEmpty ? rows : rows + bubbleRows
        guard all != ids else { return }
        assert(Set(all).count == all.count, "a transcript row twice")
        ids = all
    }

    private func place(_ events: [TranscriptEvent], placement: TranscriptPlacement) {
        placing = placement
        for event in events where event.sessionID == sessionID {
            guard seen.insert(event.id).inserted else { continue }
            var at = working!.endIndex
            if placement == .afterParent, let parent = event.parentID, let above = anchors[parent], above != working!.last {
                at = above.isEmpty ? working!.startIndex : (working!.firstIndex(of: above).map { $0 + 1 } ?? working!.endIndex)
            }
            settledRow = nil
            if let change = event.change, let row = take(change, event: event.id, at: event.timeMs) {
                working!.insert(row, at: at)
                anchors[event.id] = row
            } else if let settledRow, working!.contains(settledRow) {
                // A completion of a row drawn live: what follows it goes
                // under that row, wherever the stream had put it.
                anchors[event.id] = settledRow
            } else {
                anchors[event.id] = at == working!.startIndex ? "" : working![at - 1]
            }
            for (orphan, call) in adopted {
                working!.removeAll { $0 == orphan }
                for (event, row) in anchors where row == orphan { anchors[event] = call }
            }
            adopted = []
        }
    }

    /// `change` applied: the id of the row it adds, if it adds one.
    private func take(_ change: TranscriptChange, event: String, at time: Int64?) -> String? {
        switch change {
        case .userText(let item, let text, let images, let turn):
            if let turn { keepPrompt(turn, text) }
            // Logged live or caught up (never read at rest): the oldest
            // bubble typed with this text gives way to it (ticket 90).
            if live || placing == .afterParent, let token = commands.logged(text) { dropBubble(token) }
            // An image's bytes are never drawn: "[Image #N]", which the text
            // names it by; one the text doesn't name is added.
            let missing = images.filter { !text.contains("[Image #\($0)]") }.map { "[Image #\($0)]" }
            let shown = missing.isEmpty ? text : ([text] + missing).filter { !$0.isEmpty }.joined(separator: " ")
            if case .settled(let row) = settle("item:\(item)", text: shown) { return row }
            return add(ItemState(id: "item:\(item)", kind: .user, text: shown))
        case .assistantText(let item, let text):
            if case .settled(let row) = settle("item:\(item)", text: text) { return row }
            // An empty answer beside a tool call: no row.
            guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }
            return add(ItemState(id: "item:\(item)", kind: .assistant, text: text))
        case .reasoning(let item, let text):
            if case .settled(let row) = settle("item:\(item)", text: text) { return row }
            // Withheld by the provider: no row.
            guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }
            return add(ItemState(id: "item:\(item)", kind: .reasoning, text: text))
        case .toolCall(let item, let callID, let name, let input):
            // Started live: its row is there, running; now it has its
            // input, and runs until its result.
            if unsettled.remove("item:\(item)") != nil, let live = items["item:\(item)"] {
                let entry = streaming.removeValue(forKey: live.id)
                live.toolName = name
                live.toolInput = input
                live.done = true
                calls[callID] = live
                // Its turn may have ended already (a catch-up): not running.
                if live.output == nil, let entry { running[live.id] = (live, entry.turn) } else { live.running = false }
                settledRow = live.id
                return nil
            }
            let state = ItemState(id: "item:\(item)", kind: .tool, toolName: name, toolInput: input)
            guard let row = add(state) else { return nil }
            calls[callID] = state
            // Only a catch-up brings a call after its result (a gap).
            if placing == .afterParent, let orphan = orphans.removeValue(forKey: callID) {
                state.output = orphan.output
                state.isError = orphan.isError
                state.expanded = orphan.expanded
                items[orphan.id] = nil
                adopted.append((orphan.id, row))
            }
            return row
        case .toolResult(let item, let callID, let output, let isError):
            if let call = calls[callID], call.output == nil {
                call.output = output
                call.isError = isError
                call.running = false
                running[call.id] = nil
                return nil
            }
            // Its call isn't here (yet), or has its result already: the
            // result on a row of its own, never dropped.
            let state = ItemState(id: "item:\(item)", kind: .tool, toolName: "result")
            state.output = output
            state.isError = isError
            guard let row = add(state) else { return nil }
            if calls[callID] == nil { orphans[callID] = state }
            return row
        case .turnStarted(let turn, let model):
            if !endedTurns.contains(turn) { runningTurn = turn }
            if let model {
                turnModels[turn] = model
                current(model, at: time)
                // On the model asked for, or, once the host took the switch,
                // on what replaced it: either way it is over (Continue's
                // prompt is not sent into a turn that runs).
                if let switching, answersSwitch(at: time), switching == model || switchAcked { endSwitch() }
            }
            // Another turn starting withdraws the offer, as it happens.
            if live, let offer, offer.turn != turn { self.offer = nil }
            return nil
        case .turnCompleted(let turn, let end):
            endedTurns.insert(turn)
            // The running turn's end (or one's while none was known to
            // run), not an older turn's a catch-up brings late.
            let current = runningTurn == nil || runningTurn == turn
            if runningTurn == turn { runningTurn = nil }
            // "stopping…" ends with the turn, if its ack hasn't come; a steer
            // it never read is refused.
            if current {
                for token in commands.turnEnded() {
                    refresh(token)
                    if let state = bubbles[token] { refusedNow.append(state) }
                }
            }
            endLive(turn: turn)
            let state = ItemState(id: "turn:\(turn)", kind: .mark, text: end.line)
            state.alarming = end != .completed
            guard let row = add(state) else { return nil }
            if live { ended.append(state) }
            return row
        case .modelSwitched(let model, let from, let to, let failed):
            if let to {
                current(to, at: time)
                // The switch asked for here, logged; or, failed once it
                // ran, logged back (R-SWITCH-4).
                if let switching, answersSwitch(at: time) {
                    if switching == to || (failed && from == switching) {
                        endSwitch(settled: !failed)
                    } else if switchAcked {
                        // Another device's switch replaced it at the host.
                        endSwitch()
                    }
                }
            }
            return add(ItemState(id: "switch:\(event)", kind: .mark, text: "Now on \(model)"))
        }
    }

    /// `state` as a new row, unless a row has its id.
    private func add(_ state: ItemState) -> String? {
        guard items[state.id] == nil else { return nil }
        items[state.id] = state
        return state.id
    }
}
