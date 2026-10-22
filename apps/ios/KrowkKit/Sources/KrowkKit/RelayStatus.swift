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
    /// The relay could not be joined: the viewer's note, verbatim. It
    /// keeps trying.
    case unreachable(String)
    /// The viewer stopped and won't go on, and why, when it said.
    case ended(String?)

    /// What the bar reads.
    public var text: String {
        switch self {
        case .connecting: "connecting"
        case .live: "live"
        case .hostOffline: "host offline"
        case .reconnecting: "reconnecting"
        case .unreachable(let note): "can't reach the relay: \(note)"
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
        status = present ? .live : .hostOffline
    }

    /// `Update` note: why the relay can't be joined. Stays until a join
    /// says where the host is.
    public mutating func note(_ text: String) {
        guard !isEnded else { return }
        status = .unreachable(text)
    }

    /// The connection was dropped (the viewer's `Close`) or ended by
    /// itself. A note stays: the retry it announced is this.
    public mutating func dropped() {
        switch status {
        case .live, .hostOffline: status = .reconnecting
        case .connecting, .reconnecting, .unreachable, .ended: break
        }
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
    /// Once per display frame, on the main thread: takes what arrived
    /// since the last and applies its logged events to `transcript` (the
    /// session read at rest, a line, a catch-up), and says how many updates
    /// it took. Live deltas are ticket 89's.
    @MainActor func takeFrame(into transcript: TranscriptModel) -> Int
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
        label.text = status.text
        accessibilityLabel = status.text
        dot.backgroundColor = switch status {
        case .live: .systemGreen
        case .hostOffline: .systemGray
        case .connecting, .reconnecting: .systemYellow
        case .unreachable, .ended: .systemRed
        }
    }
}
