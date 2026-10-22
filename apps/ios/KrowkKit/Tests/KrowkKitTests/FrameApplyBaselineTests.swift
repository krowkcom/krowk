// R-LAG-7, R-PERF-4: what one display frame's apply costs on the main
// thread while an answer streams at 200 tokens a second — the
// `krowk.frame.apply` signpost interval (the session's drain, the model's
// one transaction, the streaming cell's append), measured with
// XCTOSSignpostMetric and, frame by frame, with a clock. A baseline the PR
// records, not a gate: device budgets come with a device runner (ticket
// 107); a simulator on a CI runner is no phone.
import XCTest
@testable import KrowkKit

final class FrameApplyBaselineTests: XCTestCase {
    /// Tokens a second, and the display's frames a second.
    static let rate = 200
    static let hz = 60

    @MainActor
    func testFrameApplyBaselineAt200TokensASecond() throws {
        let options = XCTMeasureOptions()
        options.iterationCount = 5
        var perFrame: [Duration] = []
        var withLayout: [Duration] = []
        let signpost = XCTOSSignpostMetric(subsystem: "com.krowk.app", category: "transcript", name: "krowk.frame.apply")
        measure(metrics: [signpost, XCTClockMetric()], options: options) {
            MainActor.assumeIsolated {
                let (apply, laid) = Self.oneSecondOfStream()
                perFrame += apply
                withLayout += laid
            }
        }
        XCTAssertFalse(perFrame.isEmpty)
        let line = Self.summary("krowk.frame.apply baseline (the apply)", perFrame) + "\n"
            + Self.summary("krowk.frame.apply baseline (the apply and the frame's layout)", withLayout)
        print(line)
        let attachment = XCTAttachment(string: line)
        attachment.name = "krowk.frame.apply baseline"
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    static func summary(_ what: String, _ times: [Duration]) -> String {
        let sorted = times.sorted()
        let micros = { (d: Duration) in Double(d.components.attoseconds) / 1e12 + Double(d.components.seconds) * 1e6 }
        let mean = sorted.map(micros).reduce(0, +) / Double(sorted.count)
        return String(format: "%@: %d frames at %d tokens/s, %d Hz: mean %.0f µs, p50 %.0f µs, p95 %.0f µs, max %.0f µs",
                      what, sorted.count, rate, hz, mean, micros(sorted[sorted.count / 2]),
                      micros(sorted[min(sorted.count - 1, sorted.count * 95 / 100)]), micros(sorted[sorted.count - 1]))
    }

    /// A screen with a turn drawn and an answer started, then a second of
    /// it streaming: `hz` frames, each bringing that frame's share of
    /// `rate` word-sized deltas and laid out as the display would. The
    /// time each frame's apply took, and its apply and layout together.
    @MainActor
    static func oneSecondOfStream() -> (apply: [Duration], laid: [Duration]) {
        let session = StubSession()
        let row = SessionRow(id: TranscriptScreenTests.session, title: "t", detail: "", live: true, accessibilityLabel: "t")
        let screen = TranscriptViewController(row: row, session: session)
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        defer { window.isHidden = true }
        session.bring(TranscriptScreenTests.turn, attach: true)
        session.bring([.started(session: TranscriptScreenTests.session, turn: "t2", item: "a2", kind: .assistant)])
        session.bring([.delta(session: TranscriptScreenTests.session, item: "a2", text: "Streaming")])
        screen.frame()
        // The rows' snapshot and the cell, as the run loop would make them.
        RunLoop.main.run(until: Date.now.addingTimeInterval(0.05))
        window.layoutIfNeeded()

        let clock = ContinuousClock()
        var times: [Duration] = []
        var laid: [Duration] = []
        var sent = 0
        for frame in 1...hz {
            let due = rate * frame / hz
            session.bring((sent..<due).map { .delta(session: TranscriptScreenTests.session, item: "a2", text: " word\($0)") })
            sent = due
            let start = clock.now
            screen.frame()
            times.append(clock.now - start)
            window.layoutIfNeeded()
            laid.append(clock.now - start)
        }
        return (times, laid)
    }
}
