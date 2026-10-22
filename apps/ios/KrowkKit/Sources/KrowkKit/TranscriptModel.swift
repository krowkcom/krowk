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
        _ = (text, toolName, toolInput, output, isError, alarming, done, running, expanded)
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
    case userText(item: String, text: String, images: [UInt32])
    case assistantText(item: String, text: String)
    case reasoning(item: String, text: String)
    case toolCall(item: String, callID: String, name: String, input: String)
    case toolResult(item: String, callID: String, output: String, isError: Bool)
    case turnCompleted(turn: String, end: TurnEnd)
    case modelSwitched(model: String)
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
}

/// What a frame's apply did that the screen draws at once, inside the
/// frame, rather than on the observation's next turn.
public struct FrameApplied {
    /// Items whose text grew by deltas, each once, in the order they grew.
    public var grown: [ItemState] = []
    /// Turn ends a logged line brought (not the session read at rest, nor
    /// a catch-up): what VoiceOver is told.
    public var ended: [ItemState] = []
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
    /// The rows, top to bottom. Never holds an id twice: the diffable data
    /// source traps on one.
    public private(set) var ids: [String] = []
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
    /// Notes drawn so far, for their row ids, and the last one's text: the
    /// same note again, straight after, isn't drawn twice.
    @ObservationIgnored private var notices = 0
    @ObservationIgnored private var lastNotice: (row: String, text: String)?
    /// Turn ends a logged line brought in the frame being applied.
    @ObservationIgnored private var ended: [ItemState] = []
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
        items[id]
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
                }
            }
            flush(&deltas, into: &applied)
        }
        if updates.contains(where: { if case .attached = $0 { true } else { false } }), !attached { attached = true }
        applied.ended = ended
        ended = []
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
            items[id] = state
            working!.append(id)
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
            return .settled(nil)
        }
        if state.text != text { state.text = text }
        state.done = true
        guard unshown.remove(id) != nil else { return .settled(nil) }
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
        working = ids
        body()
        let rows = working ?? ids
        working = nil
        transactions += 1
        // Unchanged rows share `ids`' storage: this is a pointer check.
        guard rows != ids else { return }
        assert(Set(rows).count == rows.count, "a transcript row twice")
        ids = rows
    }

    private func place(_ events: [TranscriptEvent], placement: TranscriptPlacement) {
        placing = placement
        for event in events where event.sessionID == sessionID {
            guard seen.insert(event.id).inserted else { continue }
            var at = working!.endIndex
            if placement == .afterParent, let parent = event.parentID, let above = anchors[parent], above != working!.last {
                at = above.isEmpty ? working!.startIndex : (working!.firstIndex(of: above).map { $0 + 1 } ?? working!.endIndex)
            }
            if let change = event.change, let row = take(change, event: event.id) {
                working!.insert(row, at: at)
                anchors[event.id] = row
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
    private func take(_ change: TranscriptChange, event: String) -> String? {
        switch change {
        case .userText(let item, let text, let images):
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
        case .turnCompleted(let turn, let end):
            endLive(turn: turn)
            let state = ItemState(id: "turn:\(turn)", kind: .mark, text: end.line)
            state.alarming = end != .completed
            guard let row = add(state) else { return nil }
            if live { ended.append(state) }
            return row
        case .modelSwitched(let model):
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
