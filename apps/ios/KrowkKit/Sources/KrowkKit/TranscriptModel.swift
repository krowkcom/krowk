import Foundation
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
    /// completed items; ticket 89's streaming items start not done.
    public var done: Bool
    /// A reasoning or tool row, expanded by a tap.
    public var expanded = false

    @ObservationIgnored private var summarized: (input: String, summary: String)?
    /// The cell drawing it now, if one is: redrawn on each change (the
    /// one observation an item has, `TranscriptCell.follow`).
    @ObservationIgnored weak var drawer: TranscriptCell?
    /// Followed already: at most one observation per item, however often
    /// cells show it.
    @ObservationIgnored var followed = false
    /// Text as last drawn (`TranscriptDrawings`), by what it is ("text",
    /// "output"): a cell shown
    /// again draws it without laying it out or finding its links again.

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
        _ = (text, toolName, toolInput, output, isError, alarming, done, expanded)
    }

    /// `source` drawn as `role` at `size`, from the last drawing if neither
    /// changed.
    func drawing(_ role: String, source: String, size: CGFloat, _ make: () -> NSAttributedString) -> NSAttributedString {
        let key = "\(role) \(id)" as NSString
        if let last = TranscriptDrawings.cache.object(forKey: key), last.size == size, last.source == source { return last.text }
        let text = make()
        TranscriptDrawings.cache.setObject(TranscriptDrawings.Drawing(source: source, size: size, text: text), forKey: key, cost: source.utf8.count)
        return text
    }

    /// Forgets a drawing (a collapsed output isn't kept).
    func forget(_ role: String) {
        TranscriptDrawings.cache.removeObject(forKey: "\(role) \(id)" as NSString)
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
        let size: CGFloat
        let text: NSAttributedString
        init(source: String, size: CGFloat, text: NSAttributedString) {
            self.source = source
            self.size = size
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

    public init(id: String, parentID: String? = nil, sessionID: String, change: TranscriptChange?) {
        self.id = id
        self.parentID = parentID
        self.sessionID = sessionID
        self.change = change
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

    public init(sessionID: String) {
        self.sessionID = sessionID
    }

    public func item(_ id: String) -> ItemState? {
        items[id]
    }

    /// The session as read at rest: its events applied, and `attached` set.
    public func attach(_ events: [TranscriptEvent]) {
        apply(events)
        if !attached { attached = true }
    }

    /// `events`, in order, each at most once, their rows placed by
    /// `placement`. `ids` changes once, however many rows they add.
    public func apply(_ events: [TranscriptEvent], placement: TranscriptPlacement = .end) {
        var rows = ids
        var changed = false
        for event in events where event.sessionID == sessionID {
            guard seen.insert(event.id).inserted else { continue }
            var at = rows.endIndex
            if placement == .afterParent, let parent = event.parentID, let above = anchors[parent], above != rows.last {
                at = above.isEmpty ? rows.startIndex : (rows.firstIndex(of: above).map { $0 + 1 } ?? rows.endIndex)
            }
            if let change = event.change, let row = take(change, event: event.id) {
                rows.insert(row, at: at)
                anchors[event.id] = row
                changed = true
            } else {
                anchors[event.id] = at == rows.startIndex ? "" : rows[at - 1]
            }
            for (orphan, call) in adopted {
                rows.removeAll { $0 == orphan }
                for (event, row) in anchors where row == orphan { anchors[event] = call }
                changed = true
            }
            adopted = []
        }
        guard changed else { return }
        assert(Set(rows).count == rows.count, "a transcript row twice")
        ids = rows
    }

    /// `change` applied: the id of the row it adds, if it adds one.
    private func take(_ change: TranscriptChange, event: String) -> String? {
        switch change {
        case .userText(let item, let text, let images):
            // An image's bytes are never drawn: "[Image #N]", which the text
            // names it by; one the text doesn't name is added.
            let missing = images.filter { !text.contains("[Image #\($0)]") }.map { "[Image #\($0)]" }
            let shown = missing.isEmpty ? text : ([text] + missing).filter { !$0.isEmpty }.joined(separator: " ")
            return add(ItemState(id: "item:\(item)", kind: .user, text: shown))
        case .assistantText(let item, let text):
            // An empty answer beside a tool call: no row.
            guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }
            return add(ItemState(id: "item:\(item)", kind: .assistant, text: text))
        case .reasoning(let item, let text):
            // Withheld by the provider: no row.
            guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }
            return add(ItemState(id: "item:\(item)", kind: .reasoning, text: text))
        case .toolCall(let item, let callID, let name, let input):
            let state = ItemState(id: "item:\(item)", kind: .tool, toolName: name, toolInput: input)
            guard let row = add(state) else { return nil }
            calls[callID] = state
            if let orphan = orphans.removeValue(forKey: callID) {
                state.output = orphan.output
                state.isError = orphan.isError
                items[orphan.id] = nil
                adopted.append((orphan.id, row))
            }
            return row
        case .toolResult(let item, let callID, let output, let isError):
            if let call = calls[callID], call.output == nil {
                call.output = output
                call.isError = isError
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
            let state = ItemState(id: "turn:\(turn)", kind: .mark, text: end.line)
            state.alarming = end != .completed
            return add(state)
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
