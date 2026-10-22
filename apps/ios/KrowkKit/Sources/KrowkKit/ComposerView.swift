import UIKit

/// How the composer stands, from the session's state: what its buttons
/// read and whether they work. Worked out in one place so the screen and
/// its tests agree.
public struct ComposerState: Equatable, Sendable {
    /// The primary button: "Send" (a new turn) or "Steer" (into the
    /// running one).
    public var steers: Bool
    public var canSend: Bool
    /// Stop shows while a turn runs; it reads "stopping…" while an
    /// interrupt is out.
    public var showsStop: Bool
    public var canStop: Bool
    public var stopping: Bool
    /// The line under the text: "host offline", why a text can't go, or
    /// why a stop was refused. Empty for none.
    public var note: String

    /// `text` typed; a turn `running`; `stopping`; the bar's `status`;
    /// `stopRefused`, the host's last word on a stop.
    public init(text: String, running: Bool, stopping: Bool, status: RelayStatus, stopRefused: String? = nil) {
        let offline = status == .hostOffline
        let ended: Bool = if case .ended = status { true } else { false }
        // Joined and welcomed by a host: a stop goes now, not at some later
        // welcome where it could reach a turn started meanwhile.
        let connected = status == .live || status == .catchingUp
        let problem = ComposerRules.problem(text)
        // With no host a prompt still goes (queued for the next welcome,
        // R-HAND-4), even past a turn the log shows unfinished (a host that
        // went mid-turn); a steer has no turn to reach, nor a stop. While
        // the link joins again a steer waits too: held for a later welcome
        // it could reach a turn started meanwhile.
        steers = running && !offline
        canSend = problem == nil && !ended && (!steers || connected)
        showsStop = running || stopping
        self.stopping = stopping
        canStop = running && !stopping && connected
        if ended {
            note = "stopped: open the session again to send"
        } else if let problem, !problem.isEmpty {
            note = problem
        } else if running && !offline && !connected {
            note = "Steer and Stop wait for the link to the host"
        } else if offline && running {
            note = "host offline: Steer and Stop wait for it; a prompt is queued until your laptop is back"
        } else if offline {
            note = "host offline: a prompt is queued until your laptop is back"
        } else if let stopRefused, !stopRefused.isEmpty {
            note = "Couldn't stop: \(SafeText.line(stopRefused[...], limit: 200))"
        } else {
            note = ""
        }
    }
}

/// The composer under the transcript (ticket 90): a UIKit `UITextView`
/// that grows to six lines and then scrolls, Send (or Steer while a turn
/// runs) and Stop beside it, and a line beneath for what stops them. Return
/// inserts a newline; ⌘↵ sends and ⌘. stops (the screen's key commands).
/// Typing is never held up: the text view stays editable whatever is in
/// flight (R-LAG-8).
public final class ComposerView: UIView, UITextViewDelegate {
    public let textView = UITextView(usingTextLayoutManager: true)
    public let sendButton = UIButton(configuration: .filled())
    public let stopButton = UIButton(configuration: .tinted())
    public let noteLabel = UILabel()
    private let placeholder = UILabel()
    private let row = UIStackView()
    private let buttons = UIStackView()
    private var height: NSLayoutConstraint!
    /// The text's length and width when last measured.
    private var measured: (length: Int, width: CGFloat)?
    /// Typed, or the text set: the screen works the buttons out again.
    var changed: (@MainActor () -> Void)?
    var send: (@MainActor () -> Void)?
    var stop: (@MainActor () -> Void)?

    /// How many lines it grows to before it scrolls.
    static let maxLines: CGFloat = 6

    override public init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .secondarySystemBackground
        accessibilityIdentifier = "composer"

        textView.font = .preferredFont(forTextStyle: .body)
        textView.adjustsFontForContentSizeCategory = true
        textView.backgroundColor = .systemBackground
        textView.layer.cornerRadius = 18
        textView.layer.cornerCurve = .continuous
        textView.textContainerInset = UIEdgeInsets(top: 8, left: 8, bottom: 8, right: 8)
        textView.isScrollEnabled = false
        textView.delegate = self
        textView.accessibilityIdentifier = "composer-text"
        textView.accessibilityLabel = "Message"
        // No smart quotes or dashes: a prompt names code and paths.
        textView.smartQuotesType = .no
        textView.smartDashesType = .no
        textView.translatesAutoresizingMaskIntoConstraints = false

        placeholder.text = "Message"
        placeholder.font = .preferredFont(forTextStyle: .body)
        placeholder.adjustsFontForContentSizeCategory = true
        placeholder.textColor = .placeholderText
        placeholder.isAccessibilityElement = false
        placeholder.translatesAutoresizingMaskIntoConstraints = false
        textView.addSubview(placeholder)

        var send = UIButton.Configuration.filled()
        send.title = "Send"
        send.cornerStyle = .capsule
        sendButton.configuration = send
        sendButton.accessibilityIdentifier = "composer-send"
        sendButton.addAction(UIAction { [weak self] _ in self?.send?() }, for: .primaryActionTriggered)

        var stop = UIButton.Configuration.tinted()
        stop.title = "Stop"
        stop.baseForegroundColor = .systemRed
        stop.baseBackgroundColor = .systemRed
        stop.cornerStyle = .capsule
        stopButton.configuration = stop
        stopButton.accessibilityIdentifier = "composer-stop"
        stopButton.accessibilityHint = "Stops the running turn, keeping what it wrote"
        stopButton.isHidden = true
        stopButton.addAction(UIAction { [weak self] _ in self?.stop?() }, for: .primaryActionTriggered)

        for button in [sendButton, stopButton] {
            button.setContentHuggingPriority(.required, for: .horizontal)
            button.setContentCompressionResistancePriority(.required, for: .horizontal)
        }
        buttons.addArrangedSubview(stopButton)
        buttons.addArrangedSubview(sendButton)
        buttons.spacing = 8
        buttons.alignment = .center

        noteLabel.font = .preferredFont(forTextStyle: .footnote)
        noteLabel.adjustsFontForContentSizeCategory = true
        noteLabel.textColor = .secondaryLabel
        noteLabel.numberOfLines = 0
        noteLabel.accessibilityIdentifier = "composer-note"
        noteLabel.isHidden = true

        row.addArrangedSubview(textView)
        row.addArrangedSubview(buttons)
        row.spacing = 8
        let column = UIStackView(arrangedSubviews: [row, noteLabel])
        column.axis = .vertical
        column.spacing = 4
        column.translatesAutoresizingMaskIntoConstraints = false
        addSubview(column)

        height = textView.heightAnchor.constraint(equalToConstant: 36)
        let margins = layoutMarginsGuide
        NSLayoutConstraint.activate([
            column.topAnchor.constraint(equalTo: topAnchor, constant: 8),
            column.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -8),
            column.leadingAnchor.constraint(equalTo: margins.leadingAnchor),
            column.trailingAnchor.constraint(equalTo: margins.trailingAnchor),
            height,
            placeholder.leadingAnchor.constraint(equalTo: textView.leadingAnchor, constant: 12),
            placeholder.topAnchor.constraint(equalTo: textView.topAnchor, constant: 8),
        ])
        arrange()
        registerForTraitChanges([UITraitPreferredContentSizeCategory.self]) { (self: Self, _: UITraitCollection) in
            self.arrange()
            self.measured = nil
            self.fit()
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("ComposerView is built in code")
    }

    /// The text typed.
    public var text: String {
        get { textView.text ?? "" }
        set {
            textView.text = newValue
            textChanged()
        }
    }

    /// Shows `state`: the buttons' titles, whether they work, and the note.
    public func show(_ state: ComposerState) {
        let title = state.steers ? "Steer" : "Send"
        if sendButton.configuration?.title != title { sendButton.configuration?.title = title }
        sendButton.isEnabled = state.canSend
        sendButton.accessibilityHint = state.steers
            ? "Adds your message to the running turn, which goes on"
            : "Starts a turn with your message"
        stopButton.isHidden = !state.showsStop
        let stopTitle = state.stopping ? "stopping…" : "Stop"
        if stopButton.configuration?.title != stopTitle { stopButton.configuration?.title = stopTitle }
        stopButton.isEnabled = state.canStop
        noteLabel.text = state.note
        noteLabel.isHidden = state.note.isEmpty
    }

    public func textViewDidChange(_ textView: UITextView) {
        textChanged()
    }

    private func textChanged() {
        placeholder.isHidden = !(textView.text ?? "").isEmpty
        fit()
        // Keeps the caret in view as it types; a layout alone never moves
        // a long draft the person scrolled.
        if textView.isScrollEnabled { textView.scrollRangeToVisible(textView.selectedRange) }
        changed?()
    }

    /// Grows with the text up to six lines, then scrolls inside them.
    private func fit() {
        let font = textView.font ?? .preferredFont(forTextStyle: .body)
        let inset = textView.textContainerInset
        let most = ceil(font.lineHeight * Self.maxLines + inset.top + inset.bottom)
        let width = textView.bounds.width > 0 ? textView.bounds.width : 280
        // Already at six lines and scrolling, and the text only grew at the
        // same width: no need to measure it all again (a long paste would
        // make every keystroke lay out the whole draft).
        let length = (textView.text ?? "").utf16.count
        defer { measured = (length, width) }
        if textView.isScrollEnabled, let measured, length >= measured.length, measured.width == width, abs(height.constant - most) < 0.5 {
            return
        }
        let fits = ceil(textView.sizeThatFits(CGSize(width: width, height: .greatestFiniteMagnitude)).height)
        let target = min(max(fits, ceil(font.lineHeight + inset.top + inset.bottom)), most)
        let scrolls = fits > most
        if textView.isScrollEnabled != scrolls { textView.isScrollEnabled = scrolls }
        if abs(height.constant - target) > 0.5 { height.constant = target }
    }

    /// At accessibility text sizes the buttons go under the text, side by
    /// side, rather than squeeze it.
    private func arrange() {
        let large = traitCollection.preferredContentSizeCategory.isAccessibilityCategory
        row.axis = large ? .vertical : .horizontal
        row.alignment = large ? .fill : .bottom
        buttons.distribution = large ? .fillEqually : .fill
    }

    override public func layoutSubviews() {
        super.layoutSubviews()
        fit()
    }
}
