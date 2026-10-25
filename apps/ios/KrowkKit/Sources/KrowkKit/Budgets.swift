import Darwin
import Foundation
import os
import QuartzCore

/// The budgets only a phone can hold (R-PERF-7, R-IOS-PERF, ticket 128),
/// measured by the app itself in every build, Release included, so anyone
/// holding the phone can read them without Xcode: each is a signpost
/// interval and one `.notice` log line, `budget <name> <ms> ms`, under
/// subsystem `com.krowk.app`, category `budgets` (README.md → Reading the
/// budgets on a phone). Nothing else is logged: no session content, no id.
///
/// - `launch.cachedList`: the process's start (the kernel's, so the time
///   before `main` counts) to the session list's first listing applied on
///   screen: the one the cache kept, when it keeps one (ticket 131), else
///   the network's. Once per process, and only when Sessions is the first screen
///   and lists before the app leaves the foreground or a read fails. A
///   prewarmed process (iOS started it ahead of the tap) counts from
///   `willFinishLaunching` instead.
/// - `foreground.caughtUp`: `sceneWillEnterForeground` to the open
///   session's bar reading live again, or with none open, the list's first
///   read landing. Not after less than 10 s away: that's a wake. Given up
///   when the host is offline, the connection ends, the read fails, it
///   takes over 10 s, or a session screen opens or closes meanwhile.
/// - `tap.optimistic`: Send's touch-up to the next display frame after the
///   bubble's row was applied, so its cell is in the committed layer tree.
/// - `key.glyph`: `textViewDidChange` to the next display frame, for one
///   keystroke in ten.
///
/// Cheap: signposts cost almost nothing while nobody records, the log line
/// is one per measurement, and a keystroke not sampled costs a count.
@MainActor
public final class Budgets {
    public static let shared = Budgets()

    /// The four measurements.
    enum Measure: CaseIterable {
        case launchCachedList, foregroundCaughtUp, tapOptimistic, keyGlyph

        /// The signpost interval's name.
        var signpost: StaticString {
            switch self {
            case .launchCachedList: "launch.cachedList"
            case .foregroundCaughtUp: "foreground.caughtUp"
            case .tapOptimistic: "tap.optimistic"
            case .keyGlyph: "key.glyph"
            }
        }

        /// The same, for the log line: literals, so nothing is allocated.
        var name: String {
            switch self {
            case .launchCachedList: "launch.cachedList"
            case .foregroundCaughtUp: "foreground.caughtUp"
            case .tapOptimistic: "tap.optimistic"
            case .keyGlyph: "key.glyph"
            }
        }
    }

    nonisolated static let signposter = OSSignposter(subsystem: "com.krowk.app", category: "budgets")
    nonisolated static let logger = Logger(subsystem: "com.krowk.app", category: "budgets")

    /// Away less than this, a return is a wake, not a foreground (Canon →
    /// The iPhone app → Foreground).
    static let foregroundAfter: TimeInterval = 10
    /// A return not caught up in this long is given up, not logged: what
    /// held it (a host away, the network down) is no foreground's cost.
    static let foregroundCeiling: TimeInterval = 10
    /// One keystroke in this many is measured.
    static let keySample = 10

    /// An interval under way: its signpost and when it began, on `clock`.
    private struct Open {
        let state: OSSignpostIntervalState
        let start: TimeInterval
    }

    private var launch: Open?
    private var foreground: Open?
    private var tap: Open?
    private var key: Open?
    /// Measured or given up: a launch is measured once per process.
    private var launchSettled = false
    /// The bubble of the tap under way is applied: the next frame ends it.
    private var tapShown = false
    /// Since the foreground began, the open session's bar read something
    /// other than live: its next live is the catch-up.
    private var foregroundLeftLive = false
    /// What the open session's bar last read, live or not, foreground or
    /// not: one that dropped while the app was away publishes nothing
    /// more until it's live again.
    private var sessionLive: Bool?
    /// When the foreground under way began, by the wall clock: a list read
    /// that started before it (in flight as the app left) is not it.
    private var foregroundSince: Date?
    /// When the app went to the background, on `clock`.
    private var backgroundedAt: TimeInterval?
    private var keystrokes = 0

    /// Seconds, on a clock that runs on while the phone sleeps (Darwin's
    /// CLOCK_MONOTONIC), so the time away counts.
    private let clock: @MainActor () -> TimeInterval
    /// Told each measurement: the log line, but for tests.
    private let record: @MainActor (Measure, Double) -> Void
    /// The process's start, in seconds since 1970; nil if unknown.
    private let processStart: () -> TimeInterval?

    /// What ends a tap or a keystroke: the display link, made on first use
    /// and paused between, or a test's.
    var ticker: (any FrameTicker)?

    #if DEBUG
    /// Intervals begun and ended (given up included), by measurement: the
    /// tests' count.
    private(set) var begun: [Measure: Int] = [:]
    private(set) var ended: [Measure: Int] = [:]
    #endif

    init(
        clock: @escaping @MainActor () -> TimeInterval = { Double(clock_gettime_nsec_np(CLOCK_MONOTONIC)) / 1e9 },
        processStart: @escaping () -> TimeInterval? = Budgets.kernelProcessStart,
        record: @escaping @MainActor (Measure, Double) -> Void = Budgets.line
    ) {
        self.clock = clock
        self.processStart = processStart
        self.record = record
    }

    // MARK: launch.cachedList

    /// `application(_:willFinishLaunchingWithOptions:)`: the interval starts
    /// here, and its number from the kernel's start of the process.
    public func launchBegan() {
        guard !launchSettled, launch == nil else { return }
        let now = clock()
        let age = processStart().map { max(0, Date().timeIntervalSince1970 - $0) } ?? 0
        launch = begin(.launchCachedList, at: now - age)
    }

    /// The first listing (from the cache or the network) applied on the
    /// session list.
    func listShown() {
        guard let open = launch else { return }
        launch = nil
        launchSettled = true
        end(.launchCachedList, open)
    }

    /// The first screen isn't Sessions (signing in, pairing, keys
    /// unavailable): no launch to the cached list to measure.
    public func launchAbandoned() {
        launchSettled = true
        guard let open = launch else { return }
        launch = nil
        abandon(.launchCachedList, open)
    }

    // MARK: foreground.caughtUp

    /// `sceneDidEnterBackground`: whatever is under way is given up, a
    /// launch not yet listed included.
    public func wentToBackground() {
        backgroundedAt = clock()
        launchAbandoned()
        giveUpForeground()
        if let open = tap { abandon(.tapOptimistic, open) }
        tap = nil
        if let open = key { abandon(.keyGlyph, open) }
        key = nil
        ticker?.isPaused = true
    }

    /// `sceneWillEnterForeground`, before anything else: measured after
    /// 10 s away or more. A launch's first has no time away.
    public func cameForward() {
        defer { backgroundedAt = nil }
        guard let away = backgroundedAt, foreground == nil else { return }
        let now = clock()
        guard now - away >= Self.foregroundAfter else { return }
        // A bar that dropped while away reads live next, once caught up.
        foregroundLeftLive = sessionLive == false
        foregroundSince = Date()
        foreground = begin(.foregroundCaughtUp, at: now)
    }

    /// The open session's bar, on screen: live again after it wasn't
    /// (connecting, reconnecting, catching up…) is caught up. A host
    /// offline or a connection ended can't catch up: given up.
    func sessionStatus(_ status: RelayStatus) {
        let live = status == .live
        sessionLive = live
        guard let open = foreground else { return }
        switch status {
        case .live:
            guard foregroundLeftLive else { return }
            foreground = nil
            endForeground(open)
        case .hostOffline, .ended:
            foreground = nil
            abandon(.foregroundCaughtUp, open)
        case .connecting, .reconnecting, .catchingUp, .unreachable, .retrying:
            foregroundLeftLive = true
        }
    }

    /// A session's screen comes into view (pushed, or uncovered), its bar
    /// reading `status`. A return under way was another screen's: the
    /// time spent getting here is no catch-up, so it's given up.
    func sessionShown(_ status: RelayStatus) {
        sessionLive = status == .live
        giveUpForeground()
    }

    /// The open session's screen closed or parked: what its bar read says
    /// nothing about the next one's, and a return under way was its own.
    func sessionClosed() {
        sessionLive = nil
        giveUpForeground()
    }

    private func giveUpForeground() {
        guard let open = foreground else { return }
        foreground = nil
        abandon(.foregroundCaughtUp, open)
    }

    /// The session list, with no session open over it, read by a read
    /// that started at `startedAt`; one that failed gives the return up.
    func listRead(startedAt: Date, succeeded: Bool = true) {
        guard let open = foreground, let since = foregroundSince, startedAt >= since else { return }
        foreground = nil
        if succeeded { endForeground(open) } else { abandon(.foregroundCaughtUp, open) }
    }

    /// Caught up: measured, unless it took longer than any foreground
    /// should (`foregroundCeiling`).
    private func endForeground(_ open: Open) {
        if clock() - open.start > Self.foregroundCeiling {
            abandon(.foregroundCaughtUp, open)
        } else {
            end(.foregroundCaughtUp, open)
        }
    }

    // MARK: tap.optimistic and key.glyph

    /// Send's touch-up.
    func tapped() {
        if let open = tap { abandon(.tapOptimistic, open) }
        tapShown = false
        tap = begin(.tapOptimistic, at: clock())
    }

    /// The tap's bubble is applied: the next display frame ends the tap.
    func bubbleShown() {
        guard tap != nil else { return }
        tapShown = true
        nextFrame()
    }

    /// The tap's action is done: one that showed no bubble (Send refused)
    /// is given up.
    func tapSettled() {
        guard !tapShown, let open = tap else { return }
        tap = nil
        abandon(.tapOptimistic, open)
    }

    /// `textViewDidChange`: one keystroke in ten is measured to the next
    /// display frame. The others cost this count.
    func keystroke() {
        keystrokes &+= 1
        guard keystrokes % Self.keySample == 1, key == nil else { return }
        key = begin(.keyGlyph, at: clock())
        nextFrame()
    }

    /// A display frame: what the last run-loop turn applied is committed.
    func frame() {
        ticker?.isPaused = true
        if tapShown, let open = tap {
            tap = nil
            tapShown = false
            end(.tapOptimistic, open)
        }
        if let open = key {
            key = nil
            end(.keyGlyph, open)
        }
    }

    private func nextFrame() {
        if ticker == nil {
            let link = CADisplayLink(target: BudgetsFrameTarget(self), selector: #selector(BudgetsFrameTarget.frame(_:)))
            link.add(to: .main, forMode: .common)
            ticker = DisplayLinkTicker(link)
        }
        ticker?.isPaused = false
    }

    // MARK: Intervals

    private func begin(_ measure: Measure, at start: TimeInterval) -> Open {
        #if DEBUG
        begun[measure, default: 0] += 1
        #endif
        let signposter = Self.signposter
        return Open(state: signposter.beginInterval(measure.signpost, id: signposter.makeSignpostID()), start: start)
    }

    private func end(_ measure: Measure, _ open: Open) {
        #if DEBUG
        ended[measure, default: 0] += 1
        #endif
        Self.signposter.endInterval(measure.signpost, open.state)
        record(measure, (clock() - open.start) * 1000)
    }

    /// Ended with no number: nothing is logged. Instruments still shows
    /// the interval, its end marked "given up".
    private func abandon(_ measure: Measure, _ open: Open) {
        #if DEBUG
        ended[measure, default: 0] += 1
        #endif
        Self.signposter.endInterval(measure.signpost, open.state, "given up")
    }

    /// The log line: `budget tap.optimistic 4.2 ms`, the number public, at
    /// `.notice`, so a phone with no profile installed shows it.
    nonisolated static func line(_ measure: Measure, _ ms: Double) {
        logger.notice("budget \(measure.name, privacy: .public) \(ms, format: .fixed(precision: 1), privacy: .public) ms")
    }

    /// When the kernel started this process (`kinfo_proc.kp_proc.p_starttime`),
    /// in seconds since 1970. Nil for a prewarmed one: iOS may start it
    /// minutes before the person taps, so its start says nothing about
    /// the launch.
    nonisolated static func kernelProcessStart() -> TimeInterval? {
        guard ProcessInfo.processInfo.environment["ActivePrewarm"] != "1" else { return nil }
        var info = kinfo_proc()
        var size = MemoryLayout<kinfo_proc>.stride
        var mib: [Int32] = [CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid()]
        guard sysctl(&mib, u_int(mib.count), &info, &size, nil, 0) == 0, size > 0 else { return nil }
        let start = info.kp_proc.p_starttime
        return Double(start.tv_sec) + Double(start.tv_usec) / 1e6
    }
}

/// The budgets' display link's target, holding them weakly.
@MainActor
private final class BudgetsFrameTarget: NSObject {
    weak var budgets: Budgets?

    init(_ budgets: Budgets) {
        self.budgets = budgets
    }

    @objc func frame(_ link: CADisplayLink) {
        guard let budgets else { return link.invalidate() }
        budgets.frame()
    }
}
