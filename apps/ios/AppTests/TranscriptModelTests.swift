// R-PROTO-2: the transcript reads the session's events through the types
// generated from the Rust declarations — each line below decoded by
// krowk-ffi's decodeLogEvent(json:), then placed by KrowkAccount's
// exhaustive switch over LogBody and Item — never a Swift copy of their
// JSON. TranscriptModel keeps the log's order, draws a tool's result under
// its call (by callId), applies an event once however often it comes,
// skips another session's events (a subagent's, on the same log), and
// ends each turn with its status.
import Krowk
import KrowkAccount
import KrowkKit
import Testing

@MainActor
@Suite("R-PROTO-2: TranscriptModel over the generated LogEvent")
struct TranscriptModelTests {
    static let session = "0192f0a0-0000-7000-8000-000000000001"
    static let child = "0192f0a0-0000-7000-8000-0000000000c1"

    /// A recorded session's events, as its log holds them: two turns, the
    /// second failing; a subagent's lines between them; a model switch.
    static let fixture: [String] = [
        line("e00", #""type":"session.started","cwd":"/repo","krowkVersion":"0.1.0","protocolVersion":1"#),
        line("e01", #""type":"turn.started","turnId":"t1","model":{"instance":"anthropic","model":"claude-sonnet-4-6"},"provider":"anthropic","wireApi":"anthropic-messages","permissionMode":"default""#),
        line("e02", #""type":"item.completed","turnId":"t1","itemId":"i1","item":{"kind":"userText","text":"Look at [Image #1] and read the README","images":[{"number":1,"mediaType":"image/png","file":"1.png"}]}"#),
        line("e03", #""type":"item.completed","turnId":"t1","itemId":"i2","item":{"kind":"reasoning","text":"","blob":{"provider":"anthropic","wireApi":"anthropic-messages","data":{"signature":"x"}}}"#),
        line("e04", #""type":"item.completed","turnId":"t1","itemId":"i3","item":{"kind":"toolCall","callId":"call-a","name":"read","input":{"path":"README.md"}}"#),
        line("e05", #""type":"item.completed","turnId":"t1","itemId":"i4","item":{"kind":"toolCall","callId":"call-b","name":"bash","input":{"command":"ls -la","timeout_ms":1000}}"#),
        line("e06", #""type":"item.completed","turnId":"t1","itemId":"i5","item":{"kind":"toolResult","callId":"call-b","output":"total 0\n.","isError":false}"#),
        line("e07", #""type":"item.completed","turnId":"t1","itemId":"i6","item":{"kind":"toolResult","callId":"call-a","output":"no such file","isError":true}"#),
        line("e08", #""type":"response.completed","turnId":"t1","model":"claude-sonnet-4-6","usage":\#(usage),"itemIds":["i3","i4"]"#),
        line("e09", #""type":"item.completed","turnId":"t1","itemId":"i7","item":{"kind":"assistantText","text":"There is no README."}"#),
        line("c01", #""type":"item.completed","turnId":"ct1","itemId":"ci1","item":{"kind":"assistantText","text":"a subagent's answer"}"#, session: child),
        line("e10", #""type":"turn.completed","turnId":"t1","status":"completed","usage":\#(usage),"durationMs":1200"#),
        line("e11", #""type":"model.switched","turnId":"t1","to":{"instance":"openai","model":"gpt-5"},"reason":"requested""#),
        line("e12", #""type":"turn.started","turnId":"t2","model":{"instance":"openai","model":"gpt-5"},"provider":"openai","wireApi":"openai-responses","permissionMode":"default""#),
        line("e13", #""type":"item.completed","turnId":"t2","itemId":"i8","item":{"kind":"userText","text":"Again"}"#),
        line("e14", #""type":"item.completed","turnId":"t2","itemId":"i9","item":{"kind":"reasoning","text":"Trying once more."}"#),
        line("e15", #""type":"turn.completed","turnId":"t2","status":"failed","usage":\#(usage),"durationMs":5,"error":{"code":"overloaded","message":"the provider is overloaded"}"#),
        line("e16", #""type":"turn.started","turnId":"t3","model":{"instance":"openai","model":"gpt-5"},"provider":"openai","wireApi":"openai-responses","permissionMode":"default""#),
        line("e17", #""type":"turn.completed","turnId":"t3","status":"interrupted","usage":\#(usage),"durationMs":5"#),
    ]

    static let usage = #"{"inputTokens":1,"outputTokens":2,"cacheReadTokens":0,"cacheWriteTokens":0,"reasoningTokens":0}"#

    static func line(_ id: String, _ body: String, session: String = TranscriptModelTests.session) -> String {
        #"{"id":"0192f0a0-0000-7000-8000-0000000\#(id)","sessionId":"\#(session)","timeMs":5,\#(body)}"#
    }

    /// The fixture, decoded by the generated decoder.
    static func events() throws -> [LogEvent] {
        try fixture.map { try decodeLogEvent(json: $0) }
    }

    static func model(_ events: [LogEvent]) -> TranscriptModel {
        let model = TranscriptModel(sessionID: session)
        model.apply(.attached(events: events, head: nil))
        return model
    }

    @Test("R-PROTO-2: the fixture decodes through decodeLogEvent and draws in the log's order")
    func order() throws {
        let model = Self.model(try Self.events())
        #expect(model.attached)
        #expect(model.ids == [
            "item:i1", "item:i3", "item:i4", "item:i7", "turn:t1",
            "switch:0192f0a0-0000-7000-8000-0000000e11",
            "item:i8", "item:i9", "turn:t2", "turn:t3",
        ])
        #expect(model.item("item:i1")?.kind == .user)
        #expect(model.item("item:i1")?.text == "Look at [Image #1] and read the README")
        #expect(model.item("item:i7")?.kind == .assistant)
        #expect(model.item("item:i9")?.kind == .reasoning)
        #expect(model.item("switch:0192f0a0-0000-7000-8000-0000000e11")?.text == "Now on gpt-5")
        // Reasoning the provider withheld (empty text) has no row.
        #expect(model.item("item:i2") == nil)
    }

    @Test("R-PROTO-2: a result is drawn under its call, paired by callId whatever the order they came in")
    func pairing() throws {
        let model = Self.model(try Self.events())
        let read = try #require(model.item("item:i3"))
        #expect(read.toolLine == "read README.md")
        #expect(read.output == "no such file")
        #expect(read.isError)
        let bash = try #require(model.item("item:i4"))
        #expect(bash.toolLine == "bash ls -la")
        #expect(bash.output == "total 0\n.")
        #expect(!bash.isError)
        // The results have no rows of their own.
        #expect(model.item("item:i5") == nil && model.item("item:i6") == nil)
    }

    @Test("R-PROTO-2: the same event applied twice draws once — as the session read at rest, a line and a catch-up")
    func idempotent() throws {
        let events = try Self.events()
        let model = Self.model(events)
        let before = model.ids
        model.apply(.attached(events: events, head: nil))
        model.apply(.caughtUp(events: events))
        for event in events { model.apply(.line(line: .log(event))) }
        #expect(model.ids == before)
        #expect(Set(model.ids).count == model.ids.count)
        // A result seen again leaves its call as it was.
        #expect(model.item("item:i3")?.output == "no such file")

        // Something new after them still lands, at the end.
        let next = try decodeLogEvent(json: Self.line("e18", #""type":"item.completed","turnId":"t4","itemId":"i10","item":{"kind":"assistantText","text":"Later."}"#))
        model.apply(.caughtUp(events: [next, next]))
        #expect(model.ids == before + ["item:i10"])
    }

    @Test("R-PROTO-2: what a gap left out, caught up after later lines were drawn, lands where its parent chain puts it, not at the end")
    func catchUpAfterAGap() throws {
        func event(_ id: String, parent: String?, _ body: String) throws -> LogEvent {
            let parentJSON = parent.map { #","parentId":"0192f0a0-0000-7000-8000-0000000\#($0)""# } ?? ""
            return try decodeLogEvent(json: #"{"id":"0192f0a0-0000-7000-8000-0000000\#(id)"\#(parentJSON),"sessionId":"\#(Self.session)","timeMs":5,\#(body)}"#)
        }
        let a1 = try event("a01", parent: nil, #""type":"item.completed","turnId":"t1","itemId":"i1","item":{"kind":"userText","text":"one"}"#)
        let a2 = try event("a02", parent: "a01", #""type":"item.completed","turnId":"t1","itemId":"i2","item":{"kind":"assistantText","text":"two"}"#)
        let a3 = try event("a03", parent: "a02", #""type":"item.completed","turnId":"t1","itemId":"i3","item":{"kind":"toolCall","callId":"c","name":"bash","input":{"command":"ls"}}"#)
        let a4 = try event("a04", parent: "a03", #""type":"item.completed","turnId":"t1","itemId":"i4","item":{"kind":"toolResult","callId":"c","output":"x","isError":false}"#)
        let a5 = try event("a05", parent: "a04", #""type":"item.completed","turnId":"t1","itemId":"i5","item":{"kind":"assistantText","text":"five"}"#)
        let a6 = try event("a06", parent: "a05", #""type":"turn.completed","turnId":"t1","status":"completed","usage":\#(Self.usage),"durationMs":1"#)
        let model = TranscriptModel(sessionID: Self.session)
        model.apply(.attached(events: [a1, a2], head: nil))
        // The relay dropped a3…a5; a6 came live, then the catch-up.
        model.apply(.line(line: .log(a6)))
        model.apply(.caughtUp(events: [a3, a4, a5]))
        #expect(model.ids == ["item:i1", "item:i2", "item:i3", "item:i5", "turn:t1"])
        #expect(model.item("item:i3")?.output == "x")
        // A line after that still goes at the end.
        let a7 = try event("a07", parent: "a06", #""type":"item.completed","turnId":"t2","itemId":"i7","item":{"kind":"userText","text":"seven"}"#)
        model.apply(.line(line: .log(a7)))
        #expect(model.ids.last == "item:i7")
    }

    @Test("R-PROTO-2: a call id used twice pairs each result with the call before it; an empty answer has no row")
    func reusedCallID() throws {
        let lines = [
            Self.line("b01", #""type":"item.completed","turnId":"t1","itemId":"i1","item":{"kind":"toolCall","callId":"call_0","name":"bash","input":{"command":"one"}}"#),
            Self.line("b02", #""type":"item.completed","turnId":"t1","itemId":"i2","item":{"kind":"toolResult","callId":"call_0","output":"first","isError":false}"#),
            Self.line("b03", #""type":"item.completed","turnId":"t2","itemId":"i3","item":{"kind":"assistantText","text":"  "}"#),
            Self.line("b04", #""type":"item.completed","turnId":"t2","itemId":"i4","item":{"kind":"toolCall","callId":"call_0","name":"bash","input":{"command":"two"}}"#),
            Self.line("b05", #""type":"item.completed","turnId":"t2","itemId":"i5","item":{"kind":"toolResult","callId":"call_0","output":"second","isError":true}"#),
        ]
        let model = Self.model(try lines.map { try decodeLogEvent(json: $0) })
        #expect(model.ids == ["item:i1", "item:i4"])
        #expect(model.item("item:i1")?.output == "first")
        #expect(model.item("item:i4")?.output == "second")
        #expect(model.item("item:i4")?.isError == true)
    }

    @Test("R-PROTO-2: another session's events on the same log (a subagent's) are skipped")
    func otherSession() throws {
        let model = Self.model(try Self.events())
        #expect(model.item("item:ci1") == nil)
        #expect(!model.ids.contains("item:ci1"))
        let line = try decodeLogEvent(json: Self.line("c02", #""type":"turn.completed","turnId":"ct1","status":"completed","usage":\#(Self.usage),"durationMs":1"#, session: Self.child))
        model.apply(.line(line: .log(line)))
        #expect(!model.ids.contains("turn:ct1"))
    }

    @Test("R-PROTO-2: a turn's end says its status: completed, failed with the error's message, interrupted")
    func statuses() throws {
        let model = Self.model(try Self.events())
        #expect(model.item("turn:t1")?.text == "Turn completed")
        #expect(model.item("turn:t1")?.alarming == false)
        #expect(model.item("turn:t2")?.text == "Turn failed: the provider is overloaded")
        #expect(model.item("turn:t2")?.alarming == true)
        #expect(model.item("turn:t3")?.text == "Turn interrupted")
        #expect(model.item("turn:t3")?.alarming == true)
    }

    @Test("A prompt's image is its [Image #N] text, never its bytes; one the text doesn't name is added")
    func images() throws {
        let model = TranscriptModel(sessionID: Self.session)
        let event = try decodeLogEvent(json: Self.line("e20", #""type":"item.completed","turnId":"t1","itemId":"i1","item":{"kind":"userText","text":"What is this?","images":[{"number":2,"mediaType":"image/png","file":"2.png"}]}"#))
        model.apply([TranscriptEvent(event)])
        #expect(model.item("item:i1")?.text == "What is this? [Image #2]")
    }

    @Test("A tool's one line: its command, path, pattern or url, else its input's first 80 characters")
    func summaries() {
        #expect(ItemState.summary(of: #"{"file_path":"/a/b.swift","content":"x"}"#) == "/a/b.swift")
        #expect(ItemState.summary(of: #"{"pattern":"TODO","path":"src"}"#) == "src")
        #expect(ItemState.summary(of: #"{"pattern":"TODO"}"#) == "TODO")
        #expect(ItemState.summary(of: #"{"url":"https://example.com"}"#) == "https://example.com")
        #expect(ItemState.summary(of: #"{"command":"echo\u202e hi\nthere"}"#) == "echo hi")
        let long = #"{"query":""# + String(repeating: "q", count: 200) + #""}"#
        #expect(ItemState.summary(of: long) == String(long.prefix(80)))
    }
}
