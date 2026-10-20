// R-PROTO-2: the protocol's types reach Swift generated from the Rust
// declarations (krowk-ffi's src/protocol.rs), never written here. A line
// is decoded and a command encoded in Rust; Swift switches over the
// generated enums.
import Krowk
import Testing

@Suite("R-PROTO-2: the protocol's types are generated Swift enums and structs")
struct ProtocolTests {
    let session = "0192f0a0-0000-7000-8000-000000000001"
    let turn = "0192f0a0-0000-7000-8000-000000000002"

    @Test("R-PROTO-2: decodeStreamLine(json:) reads a logged turn.started into LogBody.turnStarted")
    func decodesALogLine() throws {
        let json = """
            {"id":"0192f0a0-0000-7000-8000-0000000000aa","sessionId":"\(session)","timeMs":5,\
            "type":"turn.started","turnId":"\(turn)","model":{"instance":"anthropic","model":"m"},\
            "provider":"anthropic","wireApi":"anthropic-messages","permissionMode":"plan","effort":"high"}
            """
        guard case .log(let ev) = try decodeStreamLine(json: json) else {
            Issue.record("not a log line")
            return
        }
        #expect(ev.sessionId == session)
        #expect(ev.parentId == nil)
        guard case .turnStarted(let turnId, let model, _, let wireApi, let mode, let effort) = ev.body else {
            Issue.record("not turn.started: \(ev.body)")
            return
        }
        #expect(turnId == turn)
        #expect(model == ModelRef(instance: "anthropic", model: "m"))
        #expect(wireApi == .anthropicMessages)
        #expect(mode == .plan)
        #expect(effort == .high)
    }

    @Test("R-PROTO-2: decodeStreamLine(json:) reads a live item.delta, and a tool's input as JSON text")
    func decodesLiveLines() throws {
        let delta = """
            {"type":"item.delta","sessionId":"\(session)","turnId":"\(turn)","itemId":"i1",\
            "delta":{"kind":"text","text":"he"}}
            """
        guard case .live(.itemDelta(let sessionId, _, let itemId, .text(let text))) = try decodeStreamLine(json: delta) else {
            Issue.record("not a text item.delta")
            return
        }
        #expect(sessionId == session)
        #expect(itemId == "i1")
        #expect(text == "he")

        let approval = """
            {"type":"approval.requested","sessionId":"\(session)","turnId":"\(turn)","requestId":"r1",\
            "tool":"bash","input":{"command":"ls"},"summary":"ls","reason":"runs a command"}
            """
        guard case .live(.approvalRequested(let request)) = try decodeStreamLine(json: approval) else {
            Issue.record("not approval.requested")
            return
        }
        #expect(request.input == #"{"command":"ls"}"#)
        #expect(request.questions.isEmpty)
    }

    @Test("R-PROTO-2: decodeStreamLine(json:) throws KrowkError.Decode for a type it does not know")
    func refusesAnUnknownType() {
        do {
            let line = try decodeStreamLine(json: #"{"type":"item.teleported","sessionId":"s"}"#)
            Issue.record("decoded \(line)")
        } catch KrowkError.Decode(let detail) {
            #expect(detail.contains("stream line"))
        } catch {
            Issue.record("threw \(error), not KrowkError.Decode")
        }
    }

    @Test("R-PROTO-1: encodeCommand(command:) writes the daemon's wire form")
    func encodesCommands() throws {
        #expect(try encodeCommand(command: .interrupt(sessionId: session))
            == #"{"type":"interrupt","sessionId":"\#(session)"}"#)
        let approve = try encodeCommand(command: .approve(
            sessionId: session, requestId: "r1", decision: .allowSession, answers: []))
        #expect(approve == #"{"type":"approve","sessionId":"\#(session)","requestId":"r1","decision":"allowSession"}"#)
    }
}
