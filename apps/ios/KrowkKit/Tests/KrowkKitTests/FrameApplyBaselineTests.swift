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
        // At 120 Hz too, as a ProMotion iPhone asks: about 1.7 tokens a
        // frame, half the time for each.
        var fast: (apply: [Duration], laid: [Duration]) = ([], [])
        for _ in 0..<3 {
            let run = Self.oneSecondOfStream(hz: 120)
            fast.apply += run.apply
            fast.laid += run.laid
        }
        // Late in a long answer: 3,000 words (about 20 KB) already drawn.
        // What a frame costs should not grow with what's above it.
        var late: (apply: [Duration], laid: [Duration]) = ([], [])
        for _ in 0..<3 {
            let run = Self.oneSecondOfStream(hz: 120, after: 3_000)
            late.apply += run.apply
            late.laid += run.laid
        }
        // The frame a 3,000-word answer completes in: drawn whole once, in
        // one view, with its links.
        var completing: [Duration] = []
        for _ in 0..<3 { completing.append(Self.completion(after: 3_000)) }
        let line = Self.summary("krowk.frame.apply baseline (the apply)", perFrame, hz: Self.hz) + "\n"
            + Self.summary("krowk.frame.apply baseline (the apply and the frame's layout)", withLayout, hz: Self.hz) + "\n"
            + Self.summary("krowk.frame.apply baseline (the apply)", fast.apply, hz: 120) + "\n"
            + Self.summary("krowk.frame.apply baseline (the apply and the frame's layout)", fast.laid, hz: 120) + "\n"
            + Self.summary("krowk.frame.apply baseline after 3,000 words (the apply)", late.apply, hz: 120) + "\n"
            + Self.summary("krowk.frame.apply baseline after 3,000 words (the apply and the frame's layout)", late.laid, hz: 120) + "\n"
            + Self.summary("krowk.frame.apply baseline, the frame a 3,000-word answer completes in (the apply and the frame's layout)", completing, hz: 120)
        print(line)
        let attachment = XCTAttachment(string: line)
        attachment.name = "krowk.frame.apply baseline"
        attachment.lifetime = .keepAlways
        add(attachment)
    }

    static func summary(_ what: String, _ times: [Duration], hz: Int) -> String {
        let sorted = times.sorted()
        let micros = { (d: Duration) in Double(d.components.attoseconds) / 1e12 + Double(d.components.seconds) * 1e6 }
        let mean = sorted.map(micros).reduce(0, +) / Double(sorted.count)
        return String(format: "%@: %d frames at %d tokens/s, %d Hz: mean %.0f µs, p50 %.0f µs, p95 %.0f µs, max %.0f µs",
                      what, sorted.count, rate, hz, mean, micros(sorted[sorted.count / 2]),
                      micros(sorted[min(sorted.count - 1, sorted.count * 95 / 100)]), micros(sorted[sorted.count - 1]))
    }

    /// A screen with an answer of `words` words streamed, then its logged
    /// completion: that frame's apply and layout.
    @MainActor
    static func completion(after words: Int) -> Duration {
        let session = StubSession()
        let row = SessionRow(id: TranscriptScreenTests.session, title: "t", detail: "", live: true, accessibilityLabel: "t")
        let screen = TranscriptViewController(row: row, session: session)
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        defer { window.isHidden = true }
        let text = (0..<words).map { $0 % 12 == 11 ? "w\($0)\n" : "w\($0) " }.joined()
        session.bring(TranscriptScreenTests.turn, attach: true)
        session.bring([.started(session: TranscriptScreenTests.session, turn: "t2", item: "a2", kind: .assistant)])
        session.bring([.delta(session: TranscriptScreenTests.session, item: "a2", text: text)])
        screen.frame()
        RunLoop.main.run(until: Date.now.addingTimeInterval(0.05))
        window.layoutIfNeeded()
        session.bring([.logged([TranscriptEvent(id: "e99", sessionID: TranscriptScreenTests.session, change: .assistantText(item: "a2", text: text))])])
        let clock = ContinuousClock()
        let start = clock.now
        screen.frame()
        // The completion is drawn by the item's observation, a turn later.
        RunLoop.main.run(until: Date.now.addingTimeInterval(0.001))
        window.layoutIfNeeded()
        return clock.now - start
    }

    /// A screen with a turn drawn and an answer started, then a second of
    /// it streaming: `hz` frames, each bringing that frame's share of
    /// `rate` word-sized deltas and laid out as the display would. The
    /// time each frame's apply took, and its apply and layout together.
    @MainActor
    static func oneSecondOfStream(hz: Int = FrameApplyBaselineTests.hz, after words: Int = 0) -> (apply: [Duration], laid: [Duration]) {
        let session = StubSession()
        let row = SessionRow(id: TranscriptScreenTests.session, title: "t", detail: "", live: true, accessibilityLabel: "t")
        let screen = TranscriptViewController(row: row, session: session)
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        defer { window.isHidden = true }
        session.bring(TranscriptScreenTests.turn, attach: true)
        session.bring([.started(session: TranscriptScreenTests.session, turn: "t2", item: "a2", kind: .assistant)])
        let prefix = (0..<words).map { $0 % 12 == 11 ? "w\($0)\n" : "w\($0) " }.joined()
        session.bring([.delta(session: TranscriptScreenTests.session, item: "a2", text: "Streaming " + prefix)])
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
