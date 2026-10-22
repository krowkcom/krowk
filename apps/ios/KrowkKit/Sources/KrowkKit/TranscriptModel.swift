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

    public init(id: String, kind: Kind, text: String = "", toolName: String = "", toolInput: String = "", done: Bool = true) {
        self.id = id
        self.kind = kind
        self.text = text
        self.toolName = toolName
        self.toolInput = toolInput
        self.done = done
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

/// One logged event as the transcript takes it: its id, its session, and
/// what it changes (nil: nothing to draw).
public struct TranscriptEvent: Equatable, Sendable {
    public let id: String
    public let sessionID: String
    public let change: TranscriptChange?

    public init(id: String, sessionID: String, change: TranscriptChange?) {
        self.id = id
        self.sessionID = sessionID
        self.change = change
    }
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
    /// A tool call's row, by its `callId`.
    @ObservationIgnored private var calls: [String: ItemState] = [:]

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

    /// `events`, in order, each at most once. `ids` changes once, however
    /// many rows they add.
    public func apply(_ events: [TranscriptEvent]) {
        var added: [String] = []
        for event in events where event.sessionID == sessionID {
            guard seen.insert(event.id).inserted, let change = event.change else { continue }
            if let row = take(change, event: event.id) { added.append(row) }
        }
        guard !added.isEmpty else { return }
        ids.append(contentsOf: added)
        assert(Set(ids).count == ids.count, "a transcript row twice")
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
            return add(ItemState(id: "item:\(item)", kind: .assistant, text: text))
        case .reasoning(let item, let text):
            // Withheld by the provider: no row.
            guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }
            return add(ItemState(id: "item:\(item)", kind: .reasoning, text: text))
        case .toolCall(let item, let callID, let name, let input):
            let state = ItemState(id: "item:\(item)", kind: .tool, toolName: name, toolInput: input)
            guard let row = add(state) else { return nil }
            // A first call keeps its id.
            if calls[callID] == nil { calls[callID] = state }
            return row
        case .toolResult(let item, let callID, let output, let isError):
            if let call = calls[callID] {
                guard call.output == nil else { return nil }
                call.output = output
                call.isError = isError
                return nil
            }
            // Its call is before the checkpoint the log was read from: the
            // result on a row of its own.
            let state = ItemState(id: "item:\(item)", kind: .tool, toolName: "result")
            state.output = output
            state.isError = isError
            return add(state)
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
