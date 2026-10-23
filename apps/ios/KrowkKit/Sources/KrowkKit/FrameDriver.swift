import Foundation
import os
import QuartzCore

/// What drives the frames: a display link, paused while the session is
/// idle. A protocol so the driver's tests can count without a screen.
@MainActor
public protocol FrameTicker: AnyObject {
    var isPaused: Bool { get set }
}

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

    /// One display frame at `now` (seconds, the display link's clock).
    /// A tick is under way (something it applied called back into it).
    private var ticking = false

    public func tick(at now: TimeInterval) {
        // Never inside another: what the inner one took would be applied
        // before what the outer one already holds. Left for the next.
        guard !ticking else { return }
        ticking = true
        defer { ticking = false }
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
        let done = model.apply(frame: taken)
        applied?(done)
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
