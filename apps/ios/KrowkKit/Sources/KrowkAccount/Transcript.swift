import Krowk
import KrowkKit

/// The transcript from the generated protocol types (R-PROTO-2): the
/// session's `LogEvent`s as krowk-ffi decoded them, never a Swift copy of
/// their JSON. Each event becomes what it changes in KrowkKit's
/// `TranscriptModel`; the switches are exhaustive, so a new event or item
/// kind fails this build until it is placed.
extension TranscriptModel {
    /// One of the viewer's updates: the session read at rest (`attached`),
    /// a logged line, a catch-up. The rest — live frames (ticket 89), the
    /// host, commands, the path — draw nothing here.
    public func apply(_ update: ViewerUpdate) {
        switch update {
        case .attached(let events, _):
            attach(events.map(TranscriptEvent.init))
        case .caughtUp(let events):
            apply(events.map(TranscriptEvent.init))
        case .line(.log(let event)):
            apply([TranscriptEvent(event)])
        case .line(.live), .host, .sent, .acked, .path, .gap, .failed, .note, .handoffTaken, .handoffRefused:
            break
        }
    }
}

extension TranscriptEvent {
    /// `event` as the transcript takes it.
    public init(_ event: LogEvent) {
        self.init(id: event.id, sessionID: event.sessionId, change: Self.change(event))
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
        case .sessionStarted, .turnStarted, .responseCompleted, .backendSession, .subagentResponse, .subagentStarted,
             .todosUpdated, .runOpened, .backendHandoff, .sessionMoved:
            return nil
        }
    }
}
