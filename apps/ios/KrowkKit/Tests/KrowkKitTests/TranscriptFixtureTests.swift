// R-PERF-7: the memory budget's fixture (ticket 127), the session a DEBUG
// build opens with KROWK_FIXTURE=tenk and BudgetUITests scrolls: 10,000
// rows in the model, the same every time, its tool outputs within 64 KiB.
import Testing
@testable import KrowkKit

@MainActor
@Suite("R-PERF-7: the 10,000-item fixture the memory budget scrolls")
struct TranscriptFixtureTests {
    @Test("R-PERF-7: the fixture is 10,000 rows of answers, reasoning, tool calls and turn ends, generated the same each time")
    func tenThousandRows() {
        let events = TranscriptFixture.tenThousand()
        #expect(events == TranscriptFixture.tenThousand(), "not deterministic")
        let model = TranscriptModel(sessionID: TranscriptFixture.sessionID)
        model.attach(events)
        #expect(model.ids.count == TranscriptFixture.rows)
        let items = model.ids.compactMap(model.item)
        for kind in [ItemState.Kind.user, .reasoning, .tool, .assistant, .mark] {
            #expect(items.filter { $0.kind == kind }.count == 2_000, "\(kind)")
        }
        let outputs = model.ids.compactMap(model.item).compactMap(\.output).map(\.utf8.count)
        #expect(outputs.count == 2_000)
        #expect(outputs.max()! <= TranscriptFixture.largestOutput && outputs.max()! > 32 * 1024, "largest output \(outputs.max()!)")
        print("krowk fixture tenk: \(model.ids.count) rows, \(outputs.reduce(0, +) / 1024) KiB of tool output")
    }
}
