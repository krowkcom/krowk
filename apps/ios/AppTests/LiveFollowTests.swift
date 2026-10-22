// R-LAG-7, R-PERF-4, R-CLIENT-1, R-PROTO-2: a running turn followed live
// on the transcript screen, through the fake fleet's relay, on TestFleet's
// paired phone. The fleet hosts a session of the test's own and runs a
// turn in it on demand (POST <sessions_url>/<id>/turn): the model makes a
// `read` call, then streams 120 numbered lines at 200 tokens a second. The
// answer streams into one cell — the same cell object from item.started to
// item.completed, its text grown by appends to its storage — the call
// shows running… then its result, and the turn's separator appears. How
// far the screen runs behind the host (each live logged line's host time
// to the frame that applied it, one clock: the simulator's is the Mac's)
// is printed as a recorded number, not a gate. Then the host goes away
// mid-turn: the bar says host offline, and what was drawn stays.
//
// Live lines decoded by krowk-ffi (`decodeStreamLine`) map onto the
// transcript's updates exhaustively (R-PROTO-2): LiveLineMappingTests.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its URLs to this
// process. Hosted in the app, as the Keychain needs.
import Foundation
import Krowk
import KrowkAccount
import KrowkKit
import Testing
import UIKit

extension HostedKeychain {
    @MainActor
    @Suite("R-LAG-7, R-PERF-4: a running turn followed live through the fleet")
    struct LiveFollowTests {
        /// The fleet's long answer: 120 numbered lines.
        static let answer = (1...120).map { String(format: "line %05d: the quick brown fox jumps over the lazy dog again\n", $0) }.joined()

        @Test("R-LAG-7, R-PERF-4, R-CLIENT-1: a turn streams into one cell, the same cell instance from item.started to item.completed, its text grown by appends; its tool call runs then settles; the turn's separator appears")
        func turnStreamsIntoOneCell() async throws {
            let (screen, window, session, _) = try await Self.open("A streaming session")
            defer { window.isHidden = true }
            let model = screen.model
            let before = Set(model.ids)
            let snapshots = screen.snapshotApplies

            #expect(try await SessionListTests.control("/\(session)/turn", body: "Please stream a long answer") == "started")

            // Followed frame by frame: the read running, then the answer's
            // cell growing, until it's whole.
            var sawRunning = false
            var answerID: String?
            var cellID: ObjectIdentifier?
            var drawn: [String] = []
            var latencies: [Int64] = []
            var lastNewest = model.newestLiveMs
            let deadline = ContinuousClock.now + .seconds(60)
            while ContinuousClock.now < deadline {
                try await Task.sleep(for: .milliseconds(4))
                if let newest = model.newestLiveMs, newest != lastNewest {
                    latencies.append(Int64(Date.now.timeIntervalSince1970 * 1000) - newest)
                    lastNewest = newest
                }
                let fresh = model.ids.filter { !before.contains($0) }.compactMap(model.item)
                if fresh.contains(where: { $0.kind == .tool && $0.running && $0.output == nil }) { sawRunning = true }
                guard let answer = fresh.first(where: { $0.kind == .assistant }) else { continue }
                answerID = answer.id
                if answer.done, model.ids.last?.hasPrefix("turn:") == true { break }
                window.layoutIfNeeded()
                guard let path = screen.dataSource.indexPath(for: answer.id),
                      let cell = screen.collectionView.cellForItem(at: path) else { continue }
                if let cellID {
                    #expect(ObjectIdentifier(cell) == cellID, "the answer moved to another cell mid-stream")
                } else {
                    cellID = ObjectIdentifier(cell)
                }
                let text = screen.drawnText(of: answer.id) ?? ""
                if drawn.last != text { drawn.append(text) }
            }
            let id = try #require(answerID, "no answer streamed: \(TranscriptTests.rows(of: model))")
            let state = try #require(model.item(id))
            try #require(state.done, "the answer never completed: \(drawn.last?.count ?? 0) characters drawn")
            #expect(sawRunning, "the read never showed running…")

            // Followed to the end, the turn's separator in view.
            window.layoutIfNeeded()
            #expect(screen.following && screen.isAtBottom, "not at the end after the turn")
            // Nothing under way once the turn ended: the display link pauses
            // within a second or so of the last update.
            #expect(!model.isStreaming, "an item still under way after the turn")
            try await Self.wait(until: { screen.driver?.ticker?.isPaused == true }, within: .seconds(4), "the display link still runs after the turn")

            // One cell, start to end, grown by appends: each text drawn
            // begins with the one before it. (Scrolled to only if it went
            // out of view, which stops the following; checked above.)
            let path = try #require(screen.dataSource.indexPath(for: id))
            if screen.collectionView.cellForItem(at: path) == nil {
                screen.collectionView.scrollToItem(at: path, at: .bottom, animated: false)
                window.layoutIfNeeded()
            }
            let cell = try #require(screen.collectionView.cellForItem(at: path))
            #expect(ObjectIdentifier(cell) == cellID)
            #expect(drawn.count >= 5, "\(drawn.count) texts drawn: not streamed")
            for (a, b) in zip(drawn, drawn.dropFirst()) {
                #expect(b.hasPrefix(a), "text replaced, not appended: \(a.suffix(40)) → \(b.suffix(40))")
            }
            #expect(state.text == Self.answer)
            #expect(screen.drawnText(of: id) == Self.answer)

            // The read settled with its result; the turn's separator is last.
            let tool = try #require(model.ids.filter { !before.contains($0) }.compactMap(model.item).first { $0.kind == .tool })
            #expect(tool.toolLine == "read README.md")
            #expect(!tool.running && tool.output?.contains("# The demo project") == true)
            let end = try #require(model.ids.last.flatMap(model.item))
            #expect(end.text == "Turn completed")
            // Snapshots only for new rows (the call, the answer, the end,
            // the prompt typed at the host): never one per delta.
            #expect(screen.snapshotApplies - snapshots <= 6, "\(screen.snapshotApplies - snapshots) snapshots for one turn")
            #expect(screen.driver?.transactions ?? 0 > 0)

            let sorted = latencies.sorted()
            if !sorted.isEmpty {
                print("krowk host→screen latency (live logged lines, \(sorted.count) samples, ±4 ms polling): p50 \(sorted[sorted.count / 2]) ms, p95 \(sorted[min(sorted.count - 1, sorted.count * 95 / 100)]) ms, max \(sorted.last!) ms")
            }
        }

        @Test("R-LAG-7, R-PERF-4: a very long answer (300 lines, about 16 s at 200 tokens a second) keeps up: its turn's separator is drawn within seconds of the host's end, and the display link then pauses")
        func longAnswerKeepsUp() async throws {
            let (screen, window, session, _) = try await Self.open("A very long answer")
            defer { window.isHidden = true }
            let model = screen.model
            let started = ContinuousClock.now
            #expect(try await SessionListTests.control("/\(session)/turn", body: "Please stream a very long answer") == "started")
            let frames = screen.driver?.frames ?? 0
            try await Self.wait(until: {
                model.ids.last.flatMap(model.item).map { $0.kind == .mark && $0.id.hasPrefix("turn:") && model.ids.count > 4 } ?? false
            }, within: .seconds(60), "the turn never ended on screen: \(TranscriptTests.rows(of: model).suffix(3))")
            let took = ContinuousClock.now - started
            let behind = Int64(Date.now.timeIntervalSince1970 * 1000) - (model.newestLiveMs ?? 0)
            window.layoutIfNeeded()
            print("krowk host→screen latency (a 300-line answer): ended on screen \(took) after the prompt, \(behind) ms after the host logged its end; \((screen.driver?.frames ?? 0) - frames) frames, \(screen.driver?.transactions ?? 0) transactions")
            #expect(took < .seconds(40), "\(took) for a 16 s answer")
            #expect(screen.following && screen.isAtBottom)
            try await Self.wait(until: { screen.driver?.ticker?.isPaused == true }, within: .seconds(4), "the display link still runs after the turn")
        }

        @Test("R-LAG-7, R-HAND-4: when the host goes away mid-turn, the bar says host offline and what was drawn stays")
        func hostGoesMidTurn() async throws {
            let (screen, window, id, _) = try await Self.open("Host goes mid-turn")
            defer { window.isHidden = true }
            let model = screen.model
            let before = Set(model.ids)
            #expect(try await SessionListTests.control("/\(id)/turn", body: "Please stream a long answer") == "started")
            // Some of the answer drawn, then the host let go.
            try await Self.wait(until: {
                model.ids.filter { !before.contains($0) }.compactMap(model.item).contains { $0.kind == .assistant && $0.text.count > 200 }
            }, within: .seconds(30), "no answer streaming")
            #expect(try await SessionListTests.control("/\(id)/release") == "released")
            try await Self.wait(until: { screen.statusBar.status == .hostOffline }, within: .seconds(15), "not host offline: \(screen.statusBar.status.text)")
            #expect(screen.statusBar.accessibilityLabel == "host offline")
            let rows = model.ids
            let answer = try #require(rows.compactMap(model.item).first { $0.kind == .assistant && !before.contains($0.id) })
            let text = answer.text
            #expect(text.count > 200)
            try await Task.sleep(for: .seconds(2))
            #expect(Array(model.ids.prefix(rows.count)) == rows, "rows went")
            #expect(answer.text.hasPrefix(text), "what was drawn went")
            window.layoutIfNeeded()
            #expect(screen.dataSource.snapshot().itemIdentifiers.count >= rows.count)
        }

        /// Polls `done` on the main actor every 20 ms until it holds, or
        /// fails after `limit`, saying `why`.
        static func wait(until done: () -> Bool, within limit: Duration, _ why: @autoclosure () -> String) async throws {
            let deadline = ContinuousClock.now + limit
            while !done() && ContinuousClock.now < deadline {
                try await Task.sleep(for: .milliseconds(20))
            }
            try #require(done(), Comment(rawValue: why()))
        }

        /// The text a cell draws (its text view's).
        static func text(in view: UIView) -> String? {
            if let text = view as? UITextView { return text.text }
            for sub in view.subviews { if let text = text(in: sub) { return text } }
            return nil
        }

        /// A session of the test's own, hosted by the fleet, opened on the
        /// transcript screen in a window and followed live until its first
        /// turn is drawn and the bar reads live.
        static func open(_ title: String) async throws -> (TranscriptViewController, UIWindow, String, RelayConnection) {
            let phone = try await TestFleet.shared.paired()
            let id = try await SessionListTests.control("", body: title)
            let opened = try await RelayConnectionTests.open(id, phone: phone)
            let live = RelayConnection { opened }
            let row = SessionRow(id: id, title: title, detail: "", live: true, accessibilityLabel: title)
            let screen = TranscriptViewController(row: row, session: live)
            let scene = try #require(UIApplication.shared.connectedScenes.first as? UIWindowScene, "no window scene in the host app")
            let window = UIWindow(windowScene: scene)
            window.rootViewController = UINavigationController(rootViewController: screen)
            window.makeKeyAndVisible()
            try await Self.wait(until: {
                screen.statusBar.status == .live && screen.model.ids.last?.hasPrefix("turn:") == true
            }, within: .seconds(20), "not live with the first turn drawn: \(screen.statusBar.status.text), \(TranscriptTests.rows(of: screen.model))")
            return (screen, window, id, live)
        }
    }

    /// R-PROTO-2: live lines as krowk-ffi decodes them (`decodeStreamLine`,
    /// the generated `StreamLine`) map onto the transcript's updates by an
    /// exhaustive switch: item.started and text deltas draw, a tool input
    /// delta, cost and the result don't, an approval goes to the sink.
    @MainActor
    @Suite("R-PROTO-2: live lines through the generated StreamLine")
    struct LiveLineMappingTests {
        static let session = "0192f0a0-0000-7000-8000-000000000001"

        static func live(_ body: String) throws -> ViewerUpdate {
            .line(line: try decodeStreamLine(json: #"{"sessionId":"\#(session)",\#(body)}"#))
        }

        @Test("R-PROTO-2, R-LAG-7: a decoded burst of 200 text deltas is one frame's one transaction, and its completion leaves the text as it streamed")
        func decodedBurst() throws {
            let approvals = CountingApprovals()
            var updates: [ViewerUpdate] = [
                try Self.live(#""type":"item.started","turnId":"t1","itemId":"a1","item":{"kind":"assistantText"}"#),
                try Self.live(#""type":"item.delta","turnId":"t1","itemId":"a1","delta":{"kind":"toolInput","partialJson":"{\"pa"}"#),
                try Self.live(#""type":"cost","turnId":"t1","generatedTokens":3"#),
            ]
            for n in 0..<200 {
                updates.append(try Self.live(#""type":"item.delta","turnId":"t1","itemId":"a1","delta":{"kind":"text","text":"w\#(n) "}"#))
            }
            let mapped = TranscriptUpdate.from(updates, approvals: approvals)
            #expect(mapped.count == 201, "the started line and 200 deltas: \(mapped.count)")
            let model = TranscriptModel(sessionID: Self.session)
            let before = model.transactions
            model.apply(frame: mapped)
            #expect(model.transactions == before + 1)
            let whole = (0..<200).map { "w\($0) " }.joined()
            #expect(model.item("item:a1")?.text == whole)
            #expect(model.item("item:a1")?.done == false)

            let completed = try decodeLogEvent(json: #"{"id":"0192f0a0-0000-7000-8000-0000000000e1","sessionId":"\#(Self.session)","timeMs":5,"type":"item.completed","turnId":"t1","itemId":"a1","item":{"kind":"assistantText","text":"\#(whole)"}}"#)
            model.apply(frame: TranscriptUpdate.from([.line(line: .log(completed))], approvals: approvals))
            #expect(model.item("item:a1")?.done == true)
            #expect(model.item("item:a1")?.text == whole)
            #expect(model.ids == ["item:a1"])
            #expect(model.newestLiveMs == 5)
        }
    }
}

/// Counts what it is handed: ticket 91's sink stands in here.
@MainActor
final class CountingApprovals: ApprovalSink {
    var requested = 0
    var resolved = 0
    func requested(_ request: ApprovalRequest) { requested += 1 }
    func resolved(sessionId: String, requestId: String, decision: ApprovalDecision) { resolved += 1 }
}
