// R-PERF-7, R-LAG-7: a burst never stalls the main thread (ticket 127). A
// recorded stream (burst.stream, in the fake fleet's line shapes: 20 s of
// answer at 200 tokens a second in bursts every 100 ms, and two 1 MiB tool
// outputs) is replayed at real speed into a TranscriptViewController
// through the mailbox the live path uses: posted from a thread of its own,
// as RelayConnection's queue posts, drained once per display frame by
// FrameDriver. A CADisplayLink of the test's own records the longest gap
// between frames; over `stall.baselineMs × stall.factor` in
// apps/ios/budgets.json fails it. CI runs it once more with
// KROWK_TEST_STALL_MS=250, a DEBUG build's deliberate sleep in the apply,
// and passes only if that run fails: the gate bites.
//
// Hosted, with the app's bindings: the lines are decoded by krowk-ffi's
// decodeStreamLine before the replay starts, so what the main thread does
// meanwhile is only what it does live: drain the mailbox and apply.
import Foundation
import Krowk
import KrowkAccount
import KrowkKit
import QuartzCore
import os
import UIKit
import XCTest

final class StallBudgetTests: XCTestCase {
    static let session = "0192f0a0-0000-7000-8000-000000000127"

    @MainActor
    func testR_PERF_7_replayingABurstNeverStallsTheMainThread() async throws {
        let (baseline, factor) = try Self.budget()
        let limit = baseline * factor
        let lines = try Self.recording()
        XCTAssertGreaterThan(lines.count, 4_000)

        let replay = ReplaySession()
        let row = SessionRow(id: Self.session, title: "A burst", detail: "", live: true, accessibilityLabel: "A burst")
        let screen = TranscriptViewController(row: row, session: replay)
        let scene = try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene, "no window scene in the host app")
        let window = UIWindow(windowScene: scene)
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        defer { window.isHidden = true }

        // The display running steadily before the burst: 30 frames seen.
        let gaps = FrameGaps()
        defer { gaps.stop() }
        try await Self.wait(until: { gaps.frames >= 30 }, within: .seconds(10), "the display link never ran")
        gaps.reset()

        replay.play(lines)
        try await Self.wait(until: { replay.done && screen.model.ids.last == "turn:t1" }, within: .seconds(90),
                            "the replay never reached the turn's end: \(screen.model.ids.suffix(3))")
        let longest = gaps.longest * 1000
        let frames = gaps.frames
        let text = String(format: "krowk main-thread stall: the longest gap between frames %.1f ms over %d frames, replaying %d lines (budget %.1f ms: %.1f ms baseline × %.0f)",
                          longest, frames, lines.count, limit, baseline, factor)
        print(text)
        let attachment = XCTAttachment(string: text)
        attachment.name = "krowk main-thread stall"
        attachment.lifetime = .keepAlways
        add(attachment)

        XCTAssertEqual(screen.model.item("item:a2")?.done, true, "the second answer never completed")
        XCTAssertEqual(screen.model.item("item:c2")?.output?.utf8.count, 1 << 20, "the second 1 MiB output is not whole")
        XCTAssertLessThanOrEqual(longest, limit, "R-PERF-7: the main thread stalled \(Int(longest)) ms between two frames, over the budget's \(Int(limit)) ms")
    }

    /// `stall` from apps/ios/budgets.json: the baseline in ms, and the factor.
    static func budget(_ file: String = #filePath) throws -> (Double, Double) {
        let url = URL(fileURLWithPath: file).deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent("budgets.json")
        let json = try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any]
        let stall = try XCTUnwrap(json?["stall"] as? [String: Any], "no stall in \(url.path)")
        return (try XCTUnwrap(stall["baselineMs"] as? Double), try XCTUnwrap(stall["factor"] as? Double))
    }

    /// burst.stream decoded: when each line comes (ms from the start), its
    /// tool outputs filled to 1 MiB.
    static func recording(_ file: String = #filePath) throws -> [(at: Int, line: StreamLine)] {
        let url = Bundle(for: StallBudgetTests.self).url(forResource: "burst", withExtension: "stream")
            ?? URL(fileURLWithPath: file).deletingLastPathComponent().appendingPathComponent("burst.stream")
        let text = try String(contentsOf: url, encoding: .utf8)
        // 1 MiB once decoded: numbered lines, their newlines escaped for JSON.
        var big = ""
        var n = 0
        while big.utf8.count < 1 << 20 {
            n += 1
            big += "output line \(n): the quick brown fox jumps over the lazy dog\n"
        }
        let filled = String(decoding: big.utf8.prefix(1 << 20), as: UTF8.self).replacingOccurrences(of: "\n", with: "\\n")
        return try text.split(separator: "\n").map { entry in
            let parts = entry.split(separator: "\t", maxSplits: 1)
            let json = String(parts[1]).replacingOccurrences(of: "@1MiB@", with: filled)
            return (Int(parts[0])!, try decodeStreamLine(json: json))
        }
    }

    /// Polls `done` on the main actor every 20 ms until it holds, or fails
    /// after `limit`, saying `why`.
    @MainActor
    static func wait(until done: () -> Bool, within limit: Duration, _ why: @autoclosure () -> String) async throws {
        let deadline = ContinuousClock.now + limit
        while !done() && ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(20))
        }
        guard done() else {
            XCTFail(why())
            throw Failure(description: why())
        }
    }

    struct Failure: Error, CustomStringConvertible {
        let description: String
    }
}

/// The longest gap between two display frames since `reset`, on the main
/// run loop beside the screen's own link.
@MainActor
final class FrameGaps: NSObject {
    private var link: CADisplayLink?
    private var last: CFTimeInterval?
    private(set) var longest: CFTimeInterval = 0
    private(set) var frames = 0

    override init() {
        super.init()
        let link = CADisplayLink(target: self, selector: #selector(frame(_:)))
        link.add(to: .main, forMode: .common)
        self.link = link
    }

    @objc private func frame(_ link: CADisplayLink) {
        if let last { longest = max(longest, link.timestamp - last) }
        last = link.timestamp
        frames += 1
    }

    func reset() {
        longest = 0
        frames = 0
    }

    func stop() {
        link?.invalidate()
        link = nil
    }
}

/// A session that replays recorded lines into an `UpdateMailbox`, as
/// RelayConnection posts what its viewer shows: the same drain once per
/// display frame (`takeFrame`), the same wake when lines arrive with none
/// waiting.
final class ReplaySession: LiveSession {
    let mailbox = UpdateMailbox()
    private let finished = OSAllocatedUnfairLock(initialState: false)

    var done: Bool { finished.withLock { $0 } }

    func follow(status: @escaping @MainActor @Sendable (RelayStatus) -> Void) {
        mailbox.post([.viewer(.attached(events: [], head: nil))])
        DispatchQueue.main.async { MainActor.assumeIsolated { status(.live) } }
    }

    func stop() {}

    func onUpdates(_ wake: @escaping @MainActor @Sendable () -> Void) {
        mailbox.onPost {
            DispatchQueue.main.async { MainActor.assumeIsolated { wake() } }
        }
    }

    @MainActor func takeFrame() -> [TranscriptUpdate] {
        TranscriptUpdate.from(posted: mailbox.drainAll(), approvals: NoApprovals())
    }

    func send(_ command: LiveCommand, token: UInt64) {}

    /// `lines` posted at real speed, from a thread of their own: those due
    /// at the same moment in one post, a burst.
    func play(_ lines: [(at: Int, line: StreamLine)]) {
        let mailbox = mailbox
        let finished = finished
        Thread.detachNewThread {
            let start = Date.now
            var i = 0
            while i < lines.count {
                let at = lines[i].at
                var burst: [MailboxItem] = []
                while i < lines.count, lines[i].at == at {
                    burst.append(.viewer(.line(line: lines[i].line)))
                    i += 1
                }
                Thread.sleep(until: start.addingTimeInterval(Double(at) / 1000))
                mailbox.post(burst)
            }
            finished.withLock { $0 = true }
        }
    }
}
