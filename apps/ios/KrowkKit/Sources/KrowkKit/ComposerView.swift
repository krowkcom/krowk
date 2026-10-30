import UIKit
import UniformTypeIdentifiers

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
    /// Whether the photo and paste buttons work: not while images go.
    public var canAttach: Bool

    /// `text` typed; a turn `running`; `stopping`; the bar's `status`;
    /// `stopRefused`, the host's last word on a stop; `hostAway`, the
    /// screen's view of the host (R-IOS-7: none on the session, or none
    /// known with the relay out of reach), host offline by default.
    /// `images` are attached (R-IMG-14): a message of images alone goes,
    /// but not to an absent host, which cannot keep them. `sending` is the
    /// image going now, 1-based, `Sending image 2 of 3…`; `imageNote` why
    /// an image was left out or didn't go.
    public init(text: String, running: Bool, stopping: Bool, status: RelayStatus, stopRefused: String? = nil, hostAway: Bool? = nil, images: Int = 0, sending: Int? = nil, imageNote: String? = nil) {
        let offline = hostAway ?? (status == .hostOffline)
        let ended: Bool = if case .ended = status { true } else { false }
        // Joined and welcomed by a host: a stop goes now, not at some later
        // welcome where it could reach a turn started meanwhile.
        let connected = status == .live || status == .catchingUp
        // Images alone are a message: their tokens are its text.
        let problem = images > 0 && (text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty) ? nil : ComposerRules.problem(text)
        // With no host a prompt still goes (queued for the next welcome,
        // R-HAND-4), even past a turn the log shows unfinished (a host that
        // went mid-turn); a steer has no turn to reach, nor a stop. While
        // the link joins again a steer waits too: held for a later welcome
        // it could reach a turn started meanwhile.
        steers = running && !offline
        canSend = problem == nil && !ended && (!steers || connected) && sending == nil && !(images > 0 && offline)
        canAttach = sending == nil
        showsStop = running || stopping
        self.stopping = stopping
        canStop = running && !stopping && connected
        if ended {
            note = "stopped: open the session again to send"
        } else if let sending {
            note = "Sending image \(sending) of \(images)…"
        } else if let problem, !problem.isEmpty {
            note = problem
        } else if let imageNote, !imageNote.isEmpty {
            note = imageNote
        } else if images > 0 && offline {
            note = "host offline: images send when your laptop is back"
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
    public let textView = PasteTextView()
    /// The images attached to the message (R-IMG-14), shown above the
    /// field.
    public let attachments = ComposerImages()
    public let photoButton = UIButton(configuration: .plain())
    /// The thumbnails above the field, each with its remove button.
    public let strip = UIScrollView()
    private let thumbnails = UIStackView()
    /// Why an image was left out or didn't go, until the next change.
    public var imageNote: String? {
        didSet { if imageNote != oldValue { changed?() } }
    }
    /// The image going to the host now, 1-based; nil when none is.
    public var sending: Int? {
        didSet {
            guard sending != oldValue else { return }
            for button in removeButtons { button.isEnabled = sending == nil }
            changed?()
        }
    }
    private var removeButtons: [UIButton] { thumbnails.arrangedSubviews.compactMap { $0.subviews.compactMap { $0 as? UIButton }.first } }
    /// The photo button: the screen presents the picker.
    var addPhoto: (@MainActor () -> Void)?
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
    /// Sends what's typed; whether a bubble showed for it.
    var send: (@MainActor () -> Bool)?
    var stop: (@MainActor () -> Void)?

    /// How many lines it grows to before it scrolls.
    static let maxLines: CGFloat = 6
    static let thumbnailSide: CGFloat = 64

    /// Attaches images read from the clipboard or the picker, and says
    /// what went wrong with any of them.
    public func attach(_ sources: [Data]) async {
        // Images go as they were when Send was tapped: no more join them.
        guard sending == nil else { return }
        let added = await attachments.add(sources)
        imageNote = added.problem
    }

    /// The thumbnails again, one per attached image.
    private func reloadStrip() {
        for view in thumbnails.arrangedSubviews { view.removeFromSuperview() }
        for (at, image) in attachments.images.enumerated() {
            let side = Self.thumbnailSide
            let holder = UIView()
            let picture = UIImageView(image: UIImage(data: image.thumbnail))
            picture.contentMode = .scaleAspectFill
            picture.clipsToBounds = true
            picture.layer.cornerRadius = 8
            picture.layer.cornerCurve = .continuous
            picture.isAccessibilityElement = true
            picture.accessibilityLabel = "Image \(at + 1)"
            picture.accessibilityIdentifier = "composer-image"
            picture.translatesAutoresizingMaskIntoConstraints = false
            let remove = UIButton(type: .system)
            remove.setImage(UIImage(systemName: "xmark.circle.fill"), for: .normal)
            remove.tintColor = .white
            remove.layer.shadowColor = UIColor.black.cgColor
            remove.layer.shadowOpacity = 0.6
            remove.layer.shadowRadius = 2
            remove.layer.shadowOffset = .zero
            remove.contentHorizontalAlignment = .trailing
            remove.contentVerticalAlignment = .top
            remove.accessibilityLabel = "Remove Image"
            remove.accessibilityIdentifier = "composer-remove-image"
            remove.isEnabled = sending == nil
            remove.translatesAutoresizingMaskIntoConstraints = false
            let id = image.id
            remove.addAction(UIAction { [weak self] _ in
                self?.imageNote = nil
                self?.attachments.remove(id)
            }, for: .primaryActionTriggered)
            holder.addSubview(picture)
            holder.addSubview(remove)
            NSLayoutConstraint.activate([
                holder.widthAnchor.constraint(equalToConstant: side),
                holder.heightAnchor.constraint(equalToConstant: side),
                picture.leadingAnchor.constraint(equalTo: holder.leadingAnchor),
                picture.trailingAnchor.constraint(equalTo: holder.trailingAnchor),
                picture.topAnchor.constraint(equalTo: holder.topAnchor),
                picture.bottomAnchor.constraint(equalTo: holder.bottomAnchor),
                remove.topAnchor.constraint(equalTo: holder.topAnchor),
                remove.trailingAnchor.constraint(equalTo: holder.trailingAnchor),
                remove.widthAnchor.constraint(equalToConstant: 44),
                remove.heightAnchor.constraint(equalToConstant: 44),
            ])
            thumbnails.addArrangedSubview(holder)
        }
        strip.isHidden = attachments.images.isEmpty
    }

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
        textView.pastedImages = { [weak self] images in
            Task { @MainActor in await self?.attach(images) }
        }
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
        // The tap's budget (ticket 128): from the touch-up to the frame
        // after its bubble was applied; given up if none was.
        sendButton.addAction(UIAction { [weak self] _ in
            let budgets = Budgets.shared
            budgets.tapped()
            if self?.send?() == true { budgets.bubbleShown() }
            budgets.tapSettled()
        }, for: .primaryActionTriggered)

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

        var photo = UIButton.Configuration.plain()
        photo.image = UIImage(systemName: "photo")
        photo.contentInsets = NSDirectionalEdgeInsets(top: 8, leading: 4, bottom: 8, trailing: 4)
        photoButton.configuration = photo
        photoButton.accessibilityLabel = "Add Photo"
        photoButton.accessibilityIdentifier = "composer-add-photo"
        photoButton.setContentHuggingPriority(.required, for: .horizontal)
        photoButton.setContentCompressionResistancePriority(.required, for: .horizontal)
        photoButton.addAction(UIAction { [weak self] _ in self?.addPhoto?() }, for: .primaryActionTriggered)

        thumbnails.spacing = 8
        thumbnails.translatesAutoresizingMaskIntoConstraints = false
        strip.addSubview(thumbnails)
        strip.showsHorizontalScrollIndicator = false
        strip.isHidden = true
        strip.accessibilityIdentifier = "composer-images"
        NSLayoutConstraint.activate([
            thumbnails.leadingAnchor.constraint(equalTo: strip.contentLayoutGuide.leadingAnchor),
            thumbnails.trailingAnchor.constraint(equalTo: strip.contentLayoutGuide.trailingAnchor),
            thumbnails.topAnchor.constraint(equalTo: strip.contentLayoutGuide.topAnchor),
            thumbnails.bottomAnchor.constraint(equalTo: strip.contentLayoutGuide.bottomAnchor),
            thumbnails.heightAnchor.constraint(equalTo: strip.frameLayoutGuide.heightAnchor),
            strip.heightAnchor.constraint(equalToConstant: Self.thumbnailSide),
        ])
        attachments.changed = { [weak self] in
            self?.reloadStrip()
            self?.changed?()
        }

        row.addArrangedSubview(photoButton)
        row.addArrangedSubview(textView)
        row.addArrangedSubview(buttons)
        row.spacing = 8
        let column = UIStackView(arrangedSubviews: [strip, row, noteLabel])
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
            measured = nil
            textChanged()
        }
    }

    /// Shows `state`: the buttons' titles, whether they work, and the note.
    public func show(_ state: ComposerState) {
        let title = state.steers ? "Steer" : "Send"
        if sendButton.configuration?.title != title { sendButton.configuration?.title = title }
        sendButton.isEnabled = state.canSend
        photoButton.isEnabled = state.canAttach
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

    /// Text replaced (a selection typed or pasted over) may be shorter in
    /// lines though longer in characters: measured again.
    public func textView(_ textView: UITextView, shouldChangeTextIn range: NSRange, replacementText text: String) -> Bool {
        if range.length > 0 { measured = nil }
        return true
    }

    public func textViewDidChange(_ textView: UITextView) {
        // One keystroke in ten, to the next display frame (ticket 128).
        Budgets.shared.keystroke()
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
        photoButton.contentHorizontalAlignment = large ? .leading : .center
    }

    override public func layoutSubviews() {
        super.layoutSubviews()
        fit()
    }
}

/// The composer's text view, which takes an image from the clipboard where
/// a paste would otherwise find no text (R-IMG-14): a screenshot copied
/// from Photos or another app. Text on the clipboard, with or without an
/// image beside it, pastes as text.
public final class PasteTextView: UITextView {
    /// Images pasted, as the clipboard held them.
    var pastedImages: (@MainActor ([Data]) -> Void)?

    public init() {
        super.init(frame: .zero, textContainer: nil)
    }

    override public init(frame: CGRect, textContainer: NSTextContainer?) {
        super.init(frame: frame, textContainer: textContainer)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("PasteTextView is built in code")
    }

    private static var holdsOnlyImages: Bool {
        let board = UIPasteboard.general
        return board.hasImages && !board.hasStrings
    }

    override public func canPerformAction(_ action: Selector, withSender sender: Any?) -> Bool {
        if action == #selector(paste(_:)), pastedImages != nil, isEditable, Self.holdsOnlyImages { return true }
        return super.canPerformAction(action, withSender: sender)
    }

    override public func paste(_ sender: Any?) {
        guard let pastedImages, isEditable, Self.holdsOnlyImages else { return super.paste(sender) }
        let images = Self.clipboardImages()
        if images.isEmpty { return super.paste(sender) }
        pastedImages(images)
    }

    /// What the clipboard holds as images, one per item, in the format it
    /// was copied in (HEIC too): the normalizer reads it from there.
    static func clipboardImages(_ board: UIPasteboard = .general) -> [Data] {
        let kinds = [UTType.png, .jpeg, .heic, .gif, .webP, .tiff].map(\.identifier)
        var found: [Data] = []
        for item in board.items {
            if let data = kinds.lazy.compactMap({ item[$0] as? Data }).first {
                found.append(data)
            } else if let image = item[UTType.image.identifier] as? UIImage, let data = image.pngData() {
                found.append(data)
            }
        }
        if found.isEmpty, let images = board.images { found = images.compactMap { $0.pngData() } }
        return found
    }
}
