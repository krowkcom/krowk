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

    /// The text storage, which a streaming item's new text is appended to
    /// in place rather than rebuilt (`append`).
    var storage: NSTextStorage { view.textStorage }

    var attributedText: NSAttributedString? {
        get { view.attributedText }
        set { view.attributedText = newValue }
    }

    /// `more`, already cleaned, added at the end in place, in one editing
    /// transaction: TextKit 2 lays out only what changed, and
    /// `attributedText` is never set again.
    func append(_ more: NSAttributedString) {
        storage.beginEditing()
        storage.append(more)
        storage.endEditing()
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

/// Text that streams (R-LAG-7): TranscriptTextViews stacked top to bottom,
/// of which only the last, the live one, is ever edited. New text is
/// appended to its storage in place; once it holds more than `blockLimit`
/// characters, what's before its last line break (or, in one long
/// paragraph, its last space) is moved into a block of its own above it,
/// which is never touched again. So a frame lays out at most one block's
/// text however long the answer has grown, and the list re-lays out the
/// one cell. A cut is always at whitespace, which no link holds. Once the
/// item is whole, `end` takes the blocks away and the live view is drawn
/// whole, with its links.
@MainActor
final class TranscriptStreamingText {
    /// The blocks and the live view, top to bottom.
    let view = UIStackView()
    /// The last block, the one edited; the whole text when not streaming.
    let live = TranscriptTextView()
    private(set) var blocks: [TranscriptTextView] = []
    /// What each block's cut left out between it and the next: a line
    /// break or a space.
    private var cuts: [String] = []
    /// While an item streams: which item's text is drawn here, in which
    /// font, and how many of its UTF-8 bytes. A frame's new text is the
    /// rest, appended.
    private(set) var streamed: (item: ObjectIdentifier, bytes: Int, font: UIFont)?
    /// Appends made while streaming, for the tests: text grows in place.
    private(set) var appends = 0
    var openLink: (@MainActor (URL) -> Void)? {
        didSet {
            live.openLink = openLink
            for block in blocks { block.openLink = openLink }
        }
    }
    /// Characters the live view holds before what's above its last line
    /// break becomes a block.
    static let blockLimit = 1_024

    init() {
        view.axis = .vertical
        view.alignment = .fill
        view.spacing = 0
        view.translatesAutoresizingMaskIntoConstraints = false
        view.addArrangedSubview(live.view)
    }

    /// The text as drawn, blocks and all.
    var drawnText: String {
        var out = ""
        for (block, cut) in zip(blocks, cuts) { out += (block.view.text ?? "") + cut }
        return out + (live.view.text ?? "")
    }

    /// `state`'s text while it streams: what it gained since this was last
    /// drawn, cleaned (`SafeText.clean`, scalar by scalar, so a control
    /// character is stripped wherever a delta boundary falls), appended to
    /// the live view in one editing transaction. The first sight of it (or
    /// a new font) draws what it has, once. At most 64 KiB while it
    /// streams. Whether anything changed.
    @discardableResult
    func stream(_ state: ItemState, font: UIFont, color: UIColor = .label) -> Bool {
        let id = ObjectIdentifier(state)
        let utf8 = state.text.utf8
        let total = utf8.count
        let attributes: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: color]
        let more: String
        if let streamed, streamed.item == id, streamed.font == font, streamed.bytes <= total {
            let limit = min(total, SafeText.textLimit)
            guard streamed.bytes < limit else { return false }
            let from = utf8.index(utf8.startIndex, offsetBy: streamed.bytes)
            var end = utf8.index(utf8.startIndex, offsetBy: limit)
            // Off a continuation byte, to the start of its scalar.
            while end > from, end != utf8.endIndex, utf8[end] & 0xC0 == 0x80 {
                end = utf8.index(before: end)
            }
            guard end > from else { return false }
            more = SafeText.clean(String(decoding: utf8[from..<end], as: UTF8.self))
            self.streamed = (id, utf8.distance(from: utf8.startIndex, to: end), font)
            guard !more.isEmpty else { return false }
            appends += 1
        } else {
            end()
            live.attributedText = nil
            let head = SafeText.truncated(state.text, limit: SafeText.textLimit).shown
            more = SafeText.clean(head)
            streamed = (id, head.utf8.count, font)
        }
        live.append(NSAttributedString(string: more, attributes: attributes))
        split()
        live.view.invalidateIntrinsicContentSize()
        return true
    }

    /// While the live view holds more than `blockLimit`: what's before its
    /// last line break (else its last space) within that becomes a block.
    private func split() {
        let limit = Self.blockLimit
        while live.storage.length > limit {
            let text = live.storage.string as NSString
            let within = NSRange(location: 0, length: limit)
            var cut = text.rangeOfCharacter(from: .newlines, options: .backwards, range: within)
            if cut.location == NSNotFound || cut.location == 0 {
                cut = text.rangeOfCharacter(from: .whitespaces, options: .backwards, range: within)
            }
            // One unbroken run: left whole in the live view.
            guard cut.location != NSNotFound, cut.location > 0 else { return }
            let block = TranscriptTextView()
            block.attributedText = live.storage.attributedSubstring(from: NSRange(location: 0, length: cut.location))
            block.openLink = openLink
            block.view.accessibilityLabel = live.view.accessibilityLabel
            view.insertArrangedSubview(block.view, at: blocks.count)
            blocks.append(block)
            cuts.append(text.substring(with: cut))
            live.storage.beginEditing()
            live.storage.deleteCharacters(in: NSRange(location: 0, length: cut.location + cut.length))
            live.storage.endEditing()
        }
    }

    /// Done streaming, or showing something else: the blocks go, and the
    /// next stream draws whole.
    func end() {
        for block in blocks { block.view.removeFromSuperview() }
        blocks = []
        cuts = []
        streamed = nil
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
    let text = TranscriptStreamingText()
    /// The live text view: the whole text once the item is whole.
    var textView: TranscriptTextView { text.live }
    private let bubble = UIView()
    private var shown: NSAttributedString?
    private var userLayout: [NSLayoutConstraint] = []
    private var answerLayout: [NSLayoutConstraint] = []
    /// The text's width, set as the cell is sized: a non-scrolling text
    /// view's height follows the width it is given, never a stale one.
    private var textWidth: NSLayoutConstraint!
    private var user = false
    /// The layout last configured: a prompt's, an answer's, or none yet.
    private var configuredUser: Bool?
    /// The streaming state (for the tests).
    var streamed: (item: ObjectIdentifier, bytes: Int, font: UIFont)? { text.streamed }
    var appends: Int { text.appends }

    override init(frame: CGRect) {
        super.init(frame: frame)
        bubble.layer.cornerRadius = 16
        bubble.layer.cornerCurve = .continuous
        bubble.translatesAutoresizingMaskIntoConstraints = false
        textView.view.accessibilityIdentifier = "transcript-text"
        // A short prompt's bubble hugs it.
        text.view.setContentHuggingPriority(.defaultHigh, for: .horizontal)
        contentView.addSubview(bubble)
        bubble.addSubview(text.view)
        let margins = contentView.layoutMarginsGuide
        NSLayoutConstraint.activate([
            bubble.topAnchor.constraint(equalTo: margins.topAnchor),
            bubble.bottomAnchor.constraint(equalTo: margins.bottomAnchor),
            bubble.trailingAnchor.constraint(equalTo: margins.trailingAnchor),
        ])
        textWidth = text.view.widthAnchor.constraint(equalToConstant: 200)
        textWidth.priority = .required - 1
        textWidth.isActive = true
        userLayout = [
            bubble.leadingAnchor.constraint(greaterThanOrEqualTo: margins.leadingAnchor, constant: 48),
            text.view.topAnchor.constraint(equalTo: bubble.topAnchor, constant: 8),
            text.view.bottomAnchor.constraint(equalTo: bubble.bottomAnchor, constant: -8),
            text.view.leadingAnchor.constraint(equalTo: bubble.leadingAnchor, constant: 12),
            text.view.trailingAnchor.constraint(equalTo: bubble.trailingAnchor, constant: -12),
        ]
        answerLayout = [
            bubble.leadingAnchor.constraint(equalTo: margins.leadingAnchor),
            text.view.topAnchor.constraint(equalTo: bubble.topAnchor),
            text.view.bottomAnchor.constraint(equalTo: bubble.bottomAnchor),
            text.view.leadingAnchor.constraint(equalTo: bubble.leadingAnchor),
            text.view.trailingAnchor.constraint(equalTo: bubble.trailingAnchor),
        ]
    }

    override func render(_ state: ItemState) {
        let font = TranscriptFonts.body
        // Streaming, with nothing new since this cell drew it (the frame
        // drew it already; this is the observation's turn): nothing to do.
        if !state.done, let streamed, streamed.item == ObjectIdentifier(state), streamed.font == font,
           streamed.bytes == min(state.text.utf8.count, SafeText.textLimit) {
            return
        }
        let wasUser = configuredUser
        user = state.kind == .user
        configuredUser = user
        if wasUser != user {
            NSLayoutConstraint.deactivate(user ? answerLayout : userLayout)
            NSLayoutConstraint.activate(user ? userLayout : answerLayout)
            bubble.backgroundColor = user ? .secondarySystemBackground : .clear
            accessibilityIdentifier = user ? "transcript-user" : "transcript-assistant"
            textView.view.accessibilityLabel = user ? "Your prompt" : "Answer"
        }
        if text.openLink == nil { text.openLink = { [weak self] in self?.openLink?($0) } }
        guard state.done else { return stream(state, font: font) }
        let drawn = state.drawing("text", source: state.text, font: font) {
            SafeText.capped(state.text, font: font)
        }
        // Unchanged text keeps its layout.
        guard shown !== drawn else { return }
        // Whole: drawn once more, in one view, with its links (a URL may
        // have been cut between two deltas, so none is tappable while it
        // streams).
        text.end()
        shown = drawn
        textView.attributedText = drawn
        sizeText(toFit: bounds.width)
    }

    /// An item under way (R-LAG-7): its new text appended in place, only
    /// its last block laid out (`TranscriptStreamingText`), and this
    /// cell's height invalidated, so the list re-lays out this cell alone.
    private func stream(_ state: ItemState, font: UIFont) {
        guard text.stream(state, font: font) else { return }
        shown = nil
        if user { sizeText(toFit: bounds.width) }
        invalidateIntrinsicContentSize()
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
            // A long prompt fills the bubble's room: only a short one is
            // measured.
            if let shown, shown.length < 2_000 {
                let needed = shown.boundingRect(
                    with: CGSize(width: most, height: .greatestFiniteMagnitude),
                    options: [.usesLineFragmentOrigin, .usesFontLeading], context: nil).width
                target = min(most, ceil(needed) + 1)
            } else {
                target = most
            }
        }
        if abs(textWidth.constant - target) > 0.5 { textWidth.constant = target }
    }

    override func prepareForReuse() {
        super.prepareForReuse()
        shown = nil
        text.end()
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
    /// The full text, expanded: a reasoning's streams as an answer's does.
    let output = TranscriptStreamingText()
    var body: TranscriptTextView { output.live }
    private let stack = UIStackView()
    private var shownBody: NSAttributedString?
    private var shownExpanded: (id: String, expanded: Bool)?
    /// Tapped on the header (VoiceOver's activate too): expand or collapse.
    var toggle: (@MainActor () -> Void)?
    private static let down = UIImage(systemName: "chevron.down")
    private static let right = UIImage(systemName: "chevron.right")

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
        stack.addArrangedSubview(output.view)
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
        if output.openLink == nil { output.openLink = { [weak self] in self?.openLink?($0) } }
        chevron.image = expanded ? Self.down : Self.right
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
            } else if state.running {
                // Started live, its result still to come.
                result.text = "running…"
                result.textColor = .secondaryLabel
                result.isHidden = false
                what = "\(state.toolLine). Running"
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
            output.view.isHidden = true
            state.forget("output")
            output.end()
            if shownBody != nil {
                body.attributedText = nil
                shownBody = nil
            }
            return
        }
        output.view.isHidden = false
        if state.kind == .reasoning, !state.done {
            // Expanded while it streams: appended in place, as an answer.
            shownBody = nil
            if output.stream(state, font: TranscriptFonts.body, color: .secondaryLabel) { invalidateIntrinsicContentSize() }
            if opened { UIAccessibility.post(notification: .layoutChanged, argument: body.view) }
            return
        }
        output.end()
        let text: NSAttributedString
        if state.kind == .reasoning {
            let font = TranscriptFonts.body
            text = state.drawing("output", source: state.text, font: font) {
                SafeText.capped(state.text, font: font, color: .secondaryLabel)
            }
        } else if let output = state.output {
            let font = TranscriptFonts.mono
            text = state.drawing("output", source: output, font: font) {
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
        output.end()
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
        accessibilityIdentifier = state.id.hasPrefix("turn:") ? "transcript-turn-end"
            : state.id.hasPrefix("notice:") ? "transcript-notice" : "transcript-model-switch"
        label.text = SafeText.line(state.text[...], limit: 500)
        label.textColor = state.alarming ? .systemOrange : .secondaryLabel
        label.accessibilityIdentifier = "transcript-mark"
    }
}
