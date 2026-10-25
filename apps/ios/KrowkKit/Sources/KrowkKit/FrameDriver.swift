import Foundation
import os
import QuartzCore

/// What drives the frames: a display link, paused while the session is
/// idle. A protocol so the driver's tests can count without a screen.
@MainActor
public protocol FrameTicker: AnyObject {
    var isPaused: Bool { get set }
}

#if DEBUG
/// Where a DEBUG build's main thread spends its frames, for StallBudgetTests
/// to say what its longest gap was made of (R-PERF-7): each frame's apply
/// and draw, the rows' snapshot, the list's layout passes and each text
/// row's sizing. Nil, as in every other run, it costs a check.
@MainActor
public enum FrameCosts {
    /// Called as each piece ends: its name, how long it took and when it
    /// ended, in seconds on `CACurrentMediaTime`'s clock.
    public static var record: (@MainActor (String, CFTimeInterval, CFTimeInterval) -> Void)?

    /// When a piece starts, if anything records.
    static func start() -> CFTimeInterval? {
        record == nil ? nil : CACurrentMediaTime()
    }

    /// The piece `what` that began at `start`, ended now.
    static func end(_ what: @autoclosure () -> String, _ start: CFTimeInterval?) {
        guard let start, let record else { return }
        let now = CACurrentMediaTime()
        record(what(), now - start, now)
    }
}
#endif

/// A `CADisplayLink` as a ticker.
@MainActor
final class DisplayLinkTicker: FrameTicker {
    let link: CADisplayLink
    init(_ link: CADisplayLink) { self.link = link }
    var isPaused: Bool {
        get { link.isPaused }
        set { link.isPaused = newValue }
    }
}

/// One batch per display frame (R-LAG-7, R-PERF-4): each frame takes what
/// the session brought since the last (`LiveSession.takeFrame`, a lock and
/// no FFI) and applies all of it to the transcript as one transaction, in
/// an `os_signpost` interval named `krowk.frame.apply`. A frame that brings
/// nothing costs a lock and returns.
///
/// When nothing has come for `idleAfter` and no item is under way, the
/// ticker is paused, so an idle session costs no frames; while an item is
/// under way it waits `quietAfter` first. The session's `onUpdates` wakes
/// it (`wake`) as soon as anything comes, so a pause loses nothing.
@MainActor
public final class FrameDriver {
    /// Idle this long with nothing under way: paused.
    public static let idleAfter: TimeInterval = 1
    /// Idle this long with an item under way (its host went quiet): paused
    /// all the same; the next update wakes it.
    public static let quietAfter: TimeInterval = 5

    nonisolated static let signposter = OSSignposter(subsystem: "com.krowk.app", category: "transcript")

    #if DEBUG
    /// A deliberate stall, for proving the stall budget bites (R-PERF-7,
    /// ticket 127): a DEBUG build launched with `KROWK_TEST_STALL_MS`
    /// sleeps that long on the main thread in every frame that applies
    /// something. CI runs StallBudgetTests with 250 and passes only if
    /// they fail. Not in a Release build (scripts/approvals.sh).
    nonisolated static let testStall: TimeInterval? = ProcessInfo.processInfo.environment["KROWK_TEST_STALL_MS"]
        .flatMap(Double.init).flatMap { $0 > 0 ? $0 / 1000 : nil }
    #endif

    let session: any LiveSession
    let model: TranscriptModel
    /// The display link, once the screen has one.
    public var ticker: (any FrameTicker)?
    /// Called inside the frame after each apply, with what it did: the
    /// screen draws the streaming cells' new text now.
    var applied: ((FrameApplied) -> Void)?

    /// Frames ticked, and those that applied anything (one transaction
    /// each), and the updates they took.
    public private(set) var frames = 0
    public private(set) var transactions = 0
    public private(set) var updates = 0
    /// When the run of frames that brought nothing began.
    private var quietSince: TimeInterval?

    public init(session: any LiveSession, model: TranscriptModel) {
        self.session = session
        self.model = model
    }

    /// A tick is under way, and one was asked for inside it (something it
    /// applied called back into it).
    private var ticking = false
    private var again = false

    /// One display frame at `now` (seconds, the display link's clock).
    /// Asked for inside another, it runs once that one is done, never
    /// inside it: what the inner one took would be applied before what
    /// the outer one already holds.
    public func tick(at now: TimeInterval) {
        guard !ticking else {
            again = true
            return
        }
        ticking = true
        defer { ticking = false }
        repeat {
            again = false
            tickOnce(at: now)
        } while again
    }

    private func tickOnce(at now: TimeInterval) {
        frames += 1
        let taken = session.takeFrame()
        guard !taken.isEmpty else {
            let since = quietSince ?? now
            quietSince = since
            if now - since >= (model.isStreaming ? Self.quietAfter : Self.idleAfter) {
                ticker?.isPaused = true
            }
            return
        }
        quietSince = nil
        let signposter = Self.signposter
        let interval = signposter.beginInterval("krowk.frame.apply", id: signposter.makeSignpostID(), "\(taken.count) updates")
        #if DEBUG
        if let stall = Self.testStall { Thread.sleep(forTimeInterval: stall) }
        let applying = FrameCosts.start()
        #endif
        let done = model.apply(frame: taken)
        #if DEBUG
        FrameCosts.end("apply", applying)
        let drawing = FrameCosts.start()
        #endif
        applied?(done)
        #if DEBUG
        FrameCosts.end("draw", drawing)
        #endif
        signposter.endInterval("krowk.frame.apply", interval)
        transactions += 1
        updates += taken.count
    }

    /// Updates came with none waiting: frames again, from now.
    public func wake() {
        quietSince = nil
        ticker?.isPaused = false
    }
}
