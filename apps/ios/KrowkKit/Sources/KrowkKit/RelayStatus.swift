import UIKit

/// How a session's live connection stands, as the bar under the navigation
/// bar says it (R-IOS-1: a bar, never a spinner over what's on screen).
/// The connection decides nothing here: every state follows from what
/// krowk-ffi's `Viewer` showed or did (`RelayStatusTracker`).
public enum RelayStatus: Equatable, Sendable {
    /// The first join is under way.
    case connecting
    /// Joined, and a host welcomed this phone.
    case live
    /// Joined, with no host on the session: read-only (R-HAND-4). The
    /// viewer waits for the relay's presence notice; nothing redials.
    case hostOffline
    /// A connection that had joined dropped, and the viewer joins again.
    case reconnecting
    /// The relay dropped a batch (`gap`): the host is asked for what it
    /// held, until its catch-up comes (`caughtUp`).
    case catchingUp
    /// The relay could not be joined: the viewer's note, verbatim. It
    /// keeps trying.
    case unreachable(String)
    /// Waiting to try again, until `at` on the supervisor's backoff
    /// (ticket 86): after a drop, or with `why` it couldn't connect (the
    /// relay unreachable, or the session not opened).
    case retrying(at: ContinuousClock.Instant, why: String?)
    /// The viewer stopped and won't go on, and why, when it said.
    case ended(String?)

    /// What the bar reads.
    public var text: String { text(at: .now) }

    /// What the bar reads at `now`: a wait counts down in whole seconds.
    public func text(at now: ContinuousClock.Instant) -> String {
        switch self {
        case .retrying(let at, let why):
            let left = now.duration(to: at)
            let seconds = Int((Double(left.components.seconds) + Double(left.components.attoseconds) / 1e18).rounded(.up))
            let when = seconds > 0 ? "in \(seconds)s" : "now"
            return why.map { "\($0) — trying again \(when)" } ?? (seconds > 0 ? "reconnecting \(when)" : "reconnecting")
        default:
            return plainText
        }
    }

    fileprivate var plainText: String {
        switch self {
        case .connecting: "connecting"
        case .live: "live"
        case .hostOffline: "host offline"
        case .reconnecting: "reconnecting"
        case .catchingUp: "catching up…"
        case .unreachable(let note): "can't reach the relay: \(note)"
        case .retrying: "reconnecting"
        case .ended(let detail?): "stopped: \(detail)"
        case .ended(nil): "stopped"
        }
    }
}

/// The status, from the viewer's updates and the connection's drops, in
/// the order they came.
public struct RelayStatusTracker: Equatable, Sendable {
    public private(set) var status: RelayStatus = .connecting

    public init() {}

    /// `Update` host: a host welcomed this phone, or none is on the session.
    public mutating func host(_ present: Bool) {
        guard !isEnded else { return }
        // A host there while the catch-up is under way: still catching up.
        if present, status == .catchingUp { return }
        status = present ? .live : .hostOffline
    }

    /// `Update` note: why the relay can't be joined. Stays until a join
    /// says where the host is. It comes after the wait it explains: a wait
    /// already counting down keeps its time and takes the reason.
    public mutating func note(_ text: String) {
        guard !isEnded else { return }
        let unreachable = RelayStatus.unreachable(text)
        if case .retrying(let at, _) = status {
            status = .retrying(at: at, why: unreachable.plainText)
        } else {
            status = unreachable
        }
    }

    /// The connection was dropped (the viewer's `Close`) or ended by
    /// itself. A note stays: the retry it announced is this.
    public mutating func dropped() {
        switch status {
        case .live, .hostOffline, .catchingUp: status = .reconnecting
        case .connecting, .reconnecting, .unreachable, .retrying, .ended: break
        }
    }

    /// The next try waits until `at` (the supervisor's backoff): the bar
    /// counts down to it, keeping why the last one failed.
    public mutating func retry(at: ContinuousClock.Instant) {
        switch status {
        case .reconnecting: status = .retrying(at: at, why: nil)
        case .unreachable: status = .retrying(at: at, why: status.plainText)
        case .retrying(_, let why): status = .retrying(at: at, why: why)
        case .connecting, .live, .hostOffline, .catchingUp, .ended: break
        }
    }

    /// The session opened, after a failure the bar may still say: it joins
    /// now.
    public mutating func opened() {
        if case .retrying = status { status = .connecting }
    }

    /// Opening the session failed, and is tried again at `at`.
    public mutating func openFailed(_ detail: String, retryAt at: ContinuousClock.Instant) {
        guard !isEnded else { return }
        status = .retrying(at: at, why: "couldn't open the session: \(detail)")
    }


    /// `Update` gap: the relay dropped a batch, and the core asks the host
    /// for what it held. Live only: before a join there's nothing to miss,
    /// and with no host there's no one to ask.
    public mutating func gap() {
        if status == .live { status = .catchingUp }
    }

    /// `Update` caughtUp: what the gap left out came. Live again; a catch-up
    /// that comes without a gap (as a join's welcome) changes nothing.
    public mutating func caughtUp() {
        if status == .catchingUp { status = .live }
    }

    /// The viewer's `End`, with the `Failed` before it.
    public mutating func ended(_ detail: String?) {
        guard !isEnded else { return }
        status = .ended(detail)
    }

    private var isEnded: Bool {
        if case .ended = status { true } else { false }
    }
}

/// A session followed live, as a screen drives it. The bindings' side
/// (KrowkAccount's `RelayConnection`) conforms; KrowkKit never sees the
/// generated updates themselves, only what they do to a transcript.
public protocol LiveSession: AnyObject, Sendable {
    /// Joins, and keeps the session followed until `stop`. `status` is
    /// called on the main actor with every change, the first at once
    /// (nothing, once stopped). Once only: a later call does nothing.
    func follow(status: @escaping @MainActor @Sendable (RelayStatus) -> Void)
    /// Drops the connection and every timer; nothing more is called.
    func stop()
    /// `wake` is called on the main actor whenever updates arrive with
    /// none waiting: the screen's display link, paused while the session
    /// is idle, runs again (`FrameDriver.wake`).
    func onUpdates(_ wake: @escaping @MainActor @Sendable () -> Void)
    /// Once per display frame, on the main thread: everything that arrived
    /// since the last, oldest first, as the transcript takes it. Takes a
    /// lock and maps what it holds: never blocks on the network or calls
    /// into Rust.
    @MainActor func takeFrame() -> [TranscriptUpdate]
    /// The composer's command (ticket 90), which the model shows already
    /// as `token` (`TranscriptModel.issue`): handed to the core off the
    /// main thread, which mints its id (a `commandMinted` frame update),
    /// then reports it sent or queued and acked. Never kept or resent here.
    func send(_ command: LiveCommand, token: UInt64)
    /// The scene is coming forward (ticket 86, R-IOS-2): the connection is
    /// checked before any view appears. Never blocks.
    func foreground()
    /// The scene went to the background. Never blocks.
    func background()
    /// The scene became active, or the person pulled to refresh. Never
    /// blocks.
    func wake()
}

/// A session that follows no lifecycle (a test's stand-in) ignores it.
public extension LiveSession {
    func foreground() {}
    func background() {}
    func wake() {}
}

/// The bar under the navigation bar: the status in one line of footnote
/// text on a tinted strip, with a dot whose colour says it at a glance.
public final class RelayStatusBar: UIView {
    let label = UILabel()
    let dot = UIView()

    public private(set) var status: RelayStatus = .connecting

    override public init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .secondarySystemBackground
        dot.layer.cornerRadius = 4
        dot.translatesAutoresizingMaskIntoConstraints = false
        label.font = .preferredFont(forTextStyle: .footnote)
        label.adjustsFontForContentSizeCategory = true
        label.textColor = .secondaryLabel
        label.numberOfLines = 0
        label.translatesAutoresizingMaskIntoConstraints = false
        addSubview(dot)
        addSubview(label)
        isAccessibilityElement = true
        accessibilityIdentifier = "relay-status"
        accessibilityTraits = [.staticText, .updatesFrequently]
        let guide = layoutMarginsGuide
        NSLayoutConstraint.activate([
            dot.widthAnchor.constraint(equalToConstant: 8),
            dot.heightAnchor.constraint(equalToConstant: 8),
            dot.leadingAnchor.constraint(equalTo: guide.leadingAnchor),
            dot.centerYAnchor.constraint(equalTo: label.firstBaselineAnchor, constant: -4),
            label.leadingAnchor.constraint(equalTo: dot.trailingAnchor, constant: 8),
            label.trailingAnchor.constraint(equalTo: guide.trailingAnchor),
            label.topAnchor.constraint(equalTo: topAnchor, constant: 6),
            label.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -6),
        ])
        show(.connecting)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("RelayStatusBar is built in code")
    }

    public func show(_ status: RelayStatus) {
        self.status = status
        draw()
        dot.backgroundColor = switch status {
        case .live: .systemGreen
        case .hostOffline: .systemGray
        case .connecting, .reconnecting, .catchingUp, .retrying(_, nil): .systemYellow
        case .unreachable, .retrying, .ended: .systemRed
        }
        // A wait counts down once a second, and only while it lasts: no
        // timer runs for any other status.
        countdown?.invalidate()
        countdown = nil
        if case .retrying(let at, _) = status, ContinuousClock.now < at {
            let timer = Timer(timeInterval: 1, repeats: true) { [weak self] _ in
                MainActor.assumeIsolated { self?.counted() }
            }
            timer.tolerance = 0.2
            RunLoop.main.add(timer, forMode: .common)
            countdown = timer
        }
    }

    /// The countdown's timer, while a wait lasts.
    private var countdown: Timer?

    /// A second of the wait gone: drawn, and the timer stopped once over.
    private func counted() {
        draw()
        if case .retrying(let at, _) = status, ContinuousClock.now < at { return }
        countdown?.invalidate()
        countdown = nil
    }

    isolated deinit {
        countdown?.invalidate()
    }

    private func draw() {
        let text = status.text
        label.text = text
        accessibilityLabel = text
    }
}
