import Observation
import UIKit

/// A transcript row's cell: configured from its `ItemState`, which is
/// observed with `withObservationTracking`, registered again on each
/// change, and nothing else, so a change to one item redraws its own cell
/// only. The observation is the item's, one however often it is shown
/// (`follow`): it redraws whichever cell shows the item now, so cells
/// shown and reused while scrolling never pile observations up. The
/// collection view's self-sizing invalidation follows the cell's height.
@MainActor
class TranscriptCell: UICollectionViewListCell {
    private(set) var state: ItemState?
    /// A link tapped: the screen asks before anything opens.
    var openLink: (@MainActor (URL) -> Void)?

    override init(frame: CGRect) {
        super.init(frame: frame)
        automaticallyUpdatesBackgroundConfiguration = false
        backgroundConfiguration = .clear()
        // Hostile text (a pile of combining marks) draws inside its row.
        contentView.clipsToBounds = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("transcript cells are built in code")
    }

    /// Shows `state`, and redraws it on each change while it shows it.
    func show(_ state: ItemState) {
        if let old = self.state, old !== state, old.drawer === self { old.drawer = nil }
        self.state = state
        state.drawer = self
        if state.followed {
            render(state)
        } else {
            state.followed = true
            Self.follow(state)
        }
    }

    override func prepareForReuse() {
        super.prepareForReuse()
        if state?.drawer === self { state?.drawer = nil }
        state = nil
    }

    /// The item's one observation: every property a cell draws read, the
    /// cell showing it (if any) redrawn, and the same again after the next
    /// change, for as long as the item lives.
    static func follow(_ state: ItemState) {
        withObservationTracking {
            state.touch()
            state.drawer?.render(state)
        } onChange: { [weak state] in
            Task { @MainActor [weak state] in
                guard let state else { return }
                follow(state)
            }
        }
    }

    /// Draws `state`.
    func render(_ state: ItemState) {}
}

/// A non-editable, non-scrolling TextKit 2 text view
/// (`UITextView(usingTextLayoutManager: true)`) whose links only ask: a
/// tap calls `openLink`, never opens; a long press shows no preview, which
/// would load the page. Selectable, so text can be copied and a link
/// tapped; no data detectors of UIKit's own.
@MainActor
final class TranscriptTextView: NSObject, UITextViewDelegate {
    let view = UITextView(usingTextLayoutManager: true)
    var openLink: (@MainActor (URL) -> Void)?

    override init() {
        super.init()
        view.isEditable = false
        view.isScrollEnabled = false
        view.isSelectable = true
        view.dataDetectorTypes = []
        view.backgroundColor = .clear
        view.textContainerInset = .zero
        view.textContainer.lineFragmentPadding = 0
        view.adjustsFontForContentSizeCategory = true
        view.delegate = self
        view.translatesAutoresizingMaskIntoConstraints = false
        view.setContentCompressionResistancePriority(.required, for: .vertical)
    }

    /// The text storage, which ticket 89 appends a streaming item's text to
    /// in place rather than rebuilding it.
    var storage: NSTextStorage { view.textStorage }

    var attributedText: NSAttributedString? {
        get { view.attributedText }
        set { view.attributedText = newValue }
    }

    /// `more`, drawn safe (`SafeText.attributed`), added at the end in place.
    func append(_ more: NSAttributedString) {
        storage.append(more)
    }

    func textView(_ textView: UITextView, primaryActionFor textItem: UITextItem, defaultAction: UIAction) -> UIAction? {
        guard case .link(let url) = textItem.content else { return nil }
        let open = openLink
        return UIAction { _ in open?(url) }
    }

    func textView(_ textView: UITextView, menuConfigurationFor textItem: UITextItem, defaultMenu: UIMenu) -> UITextItem.MenuConfiguration? {
        guard case .link = textItem.content else { return UITextItem.MenuConfiguration(menu: defaultMenu) }
        // No menu and no preview: a preview would fetch the page.
        return nil
    }
}

/// Fonts the transcript draws with, following Dynamic Type.
@MainActor
enum TranscriptFonts {
    static var body: UIFont { .preferredFont(forTextStyle: .body) }
    static var mono: UIFont {
        UIFontMetrics(forTextStyle: .footnote).scaledFont(for: .monospacedSystemFont(ofSize: 13, weight: .regular))
    }
    static var monoLine: UIFont {
        UIFontMetrics(forTextStyle: .subheadline).scaledFont(for: .monospacedSystemFont(ofSize: 14, weight: .medium))
    }
}

/// Your prompt, in a right-aligned bubble, or an answer, full width.
final class TranscriptTextCell: TranscriptCell {
    let textView = TranscriptTextView()
    private let bubble = UIView()
    private var shown: NSAttributedString?
    private var userLayout: [NSLayoutConstraint] = []
    private var answerLayout: [NSLayoutConstraint] = []
    /// The text's width, set as the cell is sized: a non-scrolling text
    /// view's height follows the width it is given, never a stale one.
    private var textWidth: NSLayoutConstraint!
    private var user = false

    override init(frame: CGRect) {
        super.init(frame: frame)
        bubble.layer.cornerRadius = 16
        bubble.layer.cornerCurve = .continuous
        bubble.translatesAutoresizingMaskIntoConstraints = false
        textView.view.accessibilityIdentifier = "transcript-text"
        // A short prompt's bubble hugs it.
        textView.view.setContentHuggingPriority(.defaultHigh, for: .horizontal)
        contentView.addSubview(bubble)
        bubble.addSubview(textView.view)
        let margins = contentView.layoutMarginsGuide
        NSLayoutConstraint.activate([
            bubble.topAnchor.constraint(equalTo: margins.topAnchor),
            bubble.bottomAnchor.constraint(equalTo: margins.bottomAnchor),
            bubble.trailingAnchor.constraint(equalTo: margins.trailingAnchor),
        ])
        textWidth = textView.view.widthAnchor.constraint(equalToConstant: 200)
        textWidth.priority = .required - 1
        textWidth.isActive = true
        userLayout = [
            bubble.leadingAnchor.constraint(greaterThanOrEqualTo: margins.leadingAnchor, constant: 48),
            textView.view.topAnchor.constraint(equalTo: bubble.topAnchor, constant: 8),
            textView.view.bottomAnchor.constraint(equalTo: bubble.bottomAnchor, constant: -8),
            textView.view.leadingAnchor.constraint(equalTo: bubble.leadingAnchor, constant: 12),
            textView.view.trailingAnchor.constraint(equalTo: bubble.trailingAnchor, constant: -12),
        ]
        answerLayout = [
            bubble.leadingAnchor.constraint(equalTo: margins.leadingAnchor),
            textView.view.topAnchor.constraint(equalTo: bubble.topAnchor),
            textView.view.bottomAnchor.constraint(equalTo: bubble.bottomAnchor),
            textView.view.leadingAnchor.constraint(equalTo: bubble.leadingAnchor),
            textView.view.trailingAnchor.constraint(equalTo: bubble.trailingAnchor),
        ]
    }

    override func render(_ state: ItemState) {
        user = state.kind == .user
        textView.openLink = { [weak self] in self?.openLink?($0) }
        NSLayoutConstraint.deactivate(user ? answerLayout : userLayout)
        NSLayoutConstraint.activate(user ? userLayout : answerLayout)
        bubble.backgroundColor = user ? .secondarySystemBackground : .clear
        accessibilityIdentifier = user ? "transcript-user" : "transcript-assistant"
        textView.view.accessibilityLabel = user ? "Your prompt" : "Answer"
        let font = TranscriptFonts.body
        let text = state.drawing("text", source: state.text, size: font.pointSize) {
            SafeText.capped(state.text, font: font)
        }
        // Unchanged text keeps its layout.
        guard shown !== text else { return }
        shown = text
        textView.attributedText = text
        sizeText(toFit: bounds.width)
    }

    override func preferredLayoutAttributesFitting(_ attributes: UICollectionViewLayoutAttributes) -> UICollectionViewLayoutAttributes {
        sizeText(toFit: attributes.size.width)
        return super.preferredLayoutAttributesFitting(attributes)
    }

    /// The text view's width in a cell `width` wide: the margins' width for
    /// an answer; for a prompt, what its text needs, up to that less 48
    /// points and the bubble's padding.
    private func sizeText(toFit width: CGFloat) {
        guard width > 0 else { return }
        let margins = contentView.directionalLayoutMargins
        let room = max(0, width - margins.leading - margins.trailing)
        var target = room
        if user {
            let most = max(0, room - 48 - 24)
            let needed = textView.view.attributedText.boundingRect(
                with: CGSize(width: most, height: .greatestFiniteMagnitude),
                options: [.usesLineFragmentOrigin, .usesFontLeading], context: nil).width
            target = min(most, ceil(needed) + 1)
        }
        if abs(textWidth.constant - target) > 0.5 { textWidth.constant = target }
    }

    override func prepareForReuse() {
        super.prepareForReuse()
        shown = nil
    }
}

/// A tool call (`<name> <summary>`, then `✓`/`✗` and its result's first
/// line), or reasoning ("Thought"): one line until tapped, then the full
/// text beneath. A tool's output is laid out only expanded, and at most
/// 64 KiB of it.
final class TranscriptToolCell: TranscriptCell {
    let header = TranscriptRowHeader()
    let title = UILabel()
    let chevron = UIImageView()
    let result = UILabel()
    let body = TranscriptTextView()
    private let stack = UIStackView()
    private var shownBody: NSAttributedString?
    private var shownExpanded: (id: String, expanded: Bool)?
    /// Tapped on the header (VoiceOver's activate too): expand or collapse.
    var toggle: (@MainActor () -> Void)?

    override init(frame: CGRect) {
        super.init(frame: frame)
        title.font = TranscriptFonts.monoLine
        title.adjustsFontForContentSizeCategory = true
        title.numberOfLines = 1
        title.lineBreakMode = .byTruncatingTail
        title.accessibilityIdentifier = "transcript-tool-line"
        chevron.tintColor = .tertiaryLabel
        chevron.setContentHuggingPriority(.required, for: .horizontal)
        chevron.setContentCompressionResistancePriority(.required, for: .horizontal)
        result.font = TranscriptFonts.mono
        result.adjustsFontForContentSizeCategory = true
        result.numberOfLines = 1
        result.lineBreakMode = .byTruncatingTail
        result.accessibilityIdentifier = "transcript-tool-result"
        body.view.accessibilityIdentifier = "transcript-tool-output"

        let line = UIStackView(arrangedSubviews: [title, chevron])
        line.spacing = 6
        line.alignment = .center
        let lines = UIStackView(arrangedSubviews: [line, result])
        lines.axis = .vertical
        lines.spacing = 2
        lines.isUserInteractionEnabled = false
        lines.translatesAutoresizingMaskIntoConstraints = false
        header.addSubview(lines)
        NSLayoutConstraint.activate([
            lines.topAnchor.constraint(equalTo: header.topAnchor),
            lines.bottomAnchor.constraint(equalTo: header.bottomAnchor),
            lines.leadingAnchor.constraint(equalTo: header.leadingAnchor),
            lines.trailingAnchor.constraint(equalTo: header.trailingAnchor),
        ])
        header.activate = { [weak self] in self?.toggle?() }

        stack.axis = .vertical
        stack.spacing = 6
        stack.addArrangedSubview(header)
        stack.addArrangedSubview(body.view)
        stack.translatesAutoresizingMaskIntoConstraints = false
        contentView.addSubview(stack)
        let margins = contentView.layoutMarginsGuide
        NSLayoutConstraint.activate([
            stack.topAnchor.constraint(equalTo: margins.topAnchor),
            stack.bottomAnchor.constraint(equalTo: margins.bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: margins.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: margins.trailingAnchor),
        ])
    }

    override func render(_ state: ItemState) {
        let expanded = state.expanded
        body.openLink = { [weak self] in self?.openLink?($0) }
        chevron.image = UIImage(systemName: expanded ? "chevron.down" : "chevron.right")
        let what: String
        if state.kind == .reasoning {
            accessibilityIdentifier = "transcript-reasoning"
            title.text = "Thought"
            title.textColor = .secondaryLabel
            result.isHidden = true
            what = "Thought"
        } else {
            accessibilityIdentifier = "transcript-tool"
            title.text = state.toolLine
            title.textColor = .label
            if let output = state.output {
                let first = SafeText.line(output[...])
                result.text = "\(state.isError ? "✗" : "✓") \(first)"
                result.textColor = state.isError ? .systemRed : .secondaryLabel
                result.isHidden = false
                what = "\(state.toolLine). \(state.isError ? "Failed" : "Done"): \(first)"
            } else {
                result.text = nil
                result.isHidden = true
                what = state.toolLine
            }
        }
        header.accessibilityLabel = what
        header.accessibilityValue = expanded ? "expanded" : "collapsed"
        header.accessibilityHint = expanded ? "Hides the full text" : "Shows the full text"
        header.accessibilityIdentifier = "transcript-row-header"

        // VoiceOver moves to the output a tap just showed.
        let opened = expanded && shownExpanded?.id == state.id && shownExpanded?.expanded == false
        shownExpanded = (state.id, expanded)

        guard expanded else {
            // Collapsed: the full text isn't laid out, or kept.
            body.view.isHidden = true
            state.forget("output")
            if shownBody != nil {
                body.attributedText = nil
                shownBody = nil
            }
            return
        }
        body.view.isHidden = false
        let text: NSAttributedString
        if state.kind == .reasoning {
            let font = TranscriptFonts.body
            text = state.drawing("output", source: state.text, size: font.pointSize) {
                SafeText.capped(state.text, font: font, color: .secondaryLabel)
            }
        } else if let output = state.output {
            let font = TranscriptFonts.mono
            text = state.drawing("output", source: output, size: font.pointSize) {
                SafeText.capped(output, font: font, limit: SafeText.outputLimit)
            }
        } else {
            text = NSAttributedString(string: "No result yet.", attributes: [.font: TranscriptFonts.mono, .foregroundColor: UIColor.secondaryLabel])
        }
        if shownBody !== text {
            shownBody = text
            body.attributedText = text
        }
        if opened { UIAccessibility.post(notification: .layoutChanged, argument: body.view) }
    }

    override func prepareForReuse() {
        super.prepareForReuse()
        shownBody = nil
        shownExpanded = nil
        body.attributedText = nil
    }
}

/// The tappable top of a tool or reasoning row: one element for VoiceOver,
/// a button whose activation expands or collapses the row.
final class TranscriptRowHeader: UIView {
    var activate: (@MainActor () -> Void)?

    override init(frame: CGRect) {
        super.init(frame: frame)
        isAccessibilityElement = true
        accessibilityTraits = .button
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("built in code")
    }

    override func accessibilityActivate() -> Bool {
        activate?()
        return true
    }
}

/// A thin separator with a line on it: a turn's end and its status, or a
/// model switch.
final class TranscriptMarkCell: TranscriptCell {
    let label = UILabel()

    override init(frame: CGRect) {
        super.init(frame: frame)
        label.font = .preferredFont(forTextStyle: .caption1)
        label.adjustsFontForContentSizeCategory = true
        label.numberOfLines = 0
        label.textAlignment = .center
        label.translatesAutoresizingMaskIntoConstraints = false
        let left = UIView(), right = UIView()
        for rule in [left, right] {
            rule.backgroundColor = .separator
            rule.translatesAutoresizingMaskIntoConstraints = false
            contentView.addSubview(rule)
        }
        contentView.addSubview(label)
        let margins = contentView.layoutMarginsGuide
        let hairline: CGFloat = 0.5
        NSLayoutConstraint.activate([
            label.topAnchor.constraint(equalTo: margins.topAnchor),
            label.bottomAnchor.constraint(equalTo: margins.bottomAnchor),
            label.centerXAnchor.constraint(equalTo: margins.centerXAnchor),
            label.widthAnchor.constraint(lessThanOrEqualTo: margins.widthAnchor, multiplier: 0.8),
            left.leadingAnchor.constraint(equalTo: margins.leadingAnchor),
            left.trailingAnchor.constraint(equalTo: label.leadingAnchor, constant: -8),
            right.leadingAnchor.constraint(equalTo: label.trailingAnchor, constant: 8),
            right.trailingAnchor.constraint(equalTo: margins.trailingAnchor),
            left.heightAnchor.constraint(equalToConstant: hairline),
            right.heightAnchor.constraint(equalToConstant: hairline),
            left.centerYAnchor.constraint(equalTo: label.centerYAnchor),
            right.centerYAnchor.constraint(equalTo: label.centerYAnchor),
        ])
    }

    override func render(_ state: ItemState) {
        accessibilityIdentifier = state.id.hasPrefix("turn:") ? "transcript-turn-end" : "transcript-model-switch"
        label.text = SafeText.line(state.text[...], limit: 500)
        label.textColor = state.alarming ? .systemOrange : .secondaryLabel
        label.accessibilityIdentifier = "transcript-mark"
    }
}
