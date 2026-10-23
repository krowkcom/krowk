import UIKit

/// The approval sheet (ticket 91, R-PERM-2, R-IOS-11): the request the
/// queue shows, over the transcript, in a UIKit sheet with a medium and a
/// large detent. A call shows its tool, its summary in monospace, why it
/// asks, and its input under "Show input" (at most 16 KiB); Allow once,
/// Allow for this session (only when the request remembers rules, which
/// it lists) and Deny. Questions show each one's header, question and
/// options (one, or several when `multiSelect`), and a field to write an
/// answer, secure when `secret`; Answer and Decline.
///
/// What a request says is the agent's, and hostile (R-IOS-10): it's drawn
/// as plain text through `SafeText`, inside boxes under labels the app
/// writes, never as a link or markup, and the buttons sit in a bar of
/// their own below it, so nothing it says can pass for one. A request's
/// buttons take a tap only `armDelay` after it shows, so a tap meant for
/// what was there before (a double tap on the last answer, the sheet
/// rising under a finger) never answers it. Allowing a tool off the
/// read-only list asks for Face ID first (the queue's `OwnerCheck`).
public final class ApprovalSheetController: UIViewController, UITextFieldDelegate {
    public let queue: ApprovalQueue

    /// How long a request's buttons wait after it shows before they take a
    /// tap.
    public var armDelay: TimeInterval = 0.6
    /// The request shown.
    public private(set) var shownID: String?
    /// Its buttons take taps.
    public private(set) var armed = false
    private var arming: DispatchWorkItem?
    /// The answers to its questions, as picked and written so far.
    public private(set) var draft = QuestionDraft()
    /// The drafts of requests shown before, by request id.
    private var drafts: [String: QuestionDraft] = [:]
    /// When ⌘↵ last fired: a held key repeats, and never answers twice.
    private var lastKeyAllow: TimeInterval = 0
    private var inputShown = false

    let positionLabel = UILabel()
    let titleLabel = UILabel()
    let toolLabel = UILabel()
    let summaryLabel = UILabel()
    /// "12 lines" over a summary of more than one.
    let summaryLines = UILabel()
    let reasonLabel = UILabel()
    let rememberLabel = UILabel()
    let inputButton = UIButton(configuration: .plain())
    let inputLabel = UILabel()
    let questionsStack = UIStackView()
    public let noteLabel = UILabel()
    public let allowButton = UIButton(configuration: .filled())
    public let allowSessionButton = UIButton(configuration: .tinted())
    public let denyButton = UIButton(configuration: .gray())
    public let answerButton = UIButton(configuration: .filled())
    public let declineButton = UIButton(configuration: .gray())
    private let summaryBox = UIView()
    private let inputBox = UIView()
    private let scroll = UIScrollView()
    private let content = UIStackView()
    private let buttons = UIStackView()
    /// The buttons' bar, which scrolls when capped.
    private var bar: UIScrollView?
    /// The written-answer fields, by question id.
    public private(set) var fields: [String: UITextField] = [:]
    /// The option rows, by question id, in order.
    public private(set) var options: [String: [UIButton]] = [:]

    public init(queue: ApprovalQueue) {
        self.queue = queue
        super.init(nibName: nil, bundle: nil)
        modalPresentationStyle = .pageSheet
        if let sheet = sheetPresentationController {
            sheet.detents = [.medium(), .large()]
            sheet.prefersGrabberVisible = true
            sheet.prefersScrollingExpandsWhenScrolledToEdge = true
            // The transcript above stays readable, and its back button
            // reachable.
            sheet.largestUndimmedDetentIdentifier = .medium
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("ApprovalSheetController is built in code")
    }

    isolated deinit {
        arming?.cancel()
    }

    override public func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemBackground
        view.accessibilityIdentifier = "approval-sheet"

        style(positionLabel, .footnote, color: .secondaryLabel, id: "approval-position")
        style(titleLabel, .title3, id: "approval-title")
        titleLabel.font = UIFontMetrics(forTextStyle: .title3).scaledFont(for: .systemFont(ofSize: 20, weight: .semibold))
        titleLabel.accessibilityTraits = .header
        style(toolLabel, .subheadline, color: .secondaryLabel, id: "approval-tool")
        toolLabel.font = TranscriptFonts.monoLine
        style(summaryLines, .caption1, color: .secondaryLabel, id: "approval-summary-lines")
        style(summaryLabel, .body, id: "approval-summary")
        summaryLabel.font = UIFontMetrics(forTextStyle: .body).scaledFont(for: .monospacedSystemFont(ofSize: 15, weight: .regular))
        style(reasonLabel, .subheadline, color: .secondaryLabel, id: "approval-reason")
        style(rememberLabel, .footnote, color: .secondaryLabel, id: "approval-remember")
        style(inputLabel, .footnote, id: "approval-input")
        inputLabel.font = TranscriptFonts.mono
        style(noteLabel, .footnote, color: .systemRed, id: "approval-note")

        for (box, label) in [(summaryBox, summaryLabel), (inputBox, inputLabel)] {
            box.backgroundColor = .secondarySystemBackground
            box.layer.cornerRadius = 8
            box.layer.cornerCurve = .continuous
            label.translatesAutoresizingMaskIntoConstraints = false
            box.addSubview(label)
            NSLayoutConstraint.activate([
                label.topAnchor.constraint(equalTo: box.topAnchor, constant: 10),
                label.bottomAnchor.constraint(equalTo: box.bottomAnchor, constant: -10),
                label.leadingAnchor.constraint(equalTo: box.leadingAnchor, constant: 12),
                label.trailingAnchor.constraint(equalTo: box.trailingAnchor, constant: -12),
            ])
        }

        var show = UIButton.Configuration.plain()
        show.contentInsets = NSDirectionalEdgeInsets(top: 4, leading: 0, bottom: 4, trailing: 0)
        inputButton.configuration = show
        inputButton.contentHorizontalAlignment = .leading
        inputButton.accessibilityIdentifier = "approval-show-input"
        inputButton.addAction(UIAction { [weak self] _ in self?.toggleInput() }, for: .primaryActionTriggered)

        questionsStack.axis = .vertical
        questionsStack.spacing = 16

        content.axis = .vertical
        content.spacing = 10
        content.alignment = .fill
        for v in [positionLabel, titleLabel, noteLabel, toolLabel, summaryLines, summaryBox, reasonLabel, rememberLabel, inputButton, inputBox, questionsStack] as [UIView] {
            content.addArrangedSubview(v)
        }
        content.setCustomSpacing(4, after: positionLabel)
        content.setCustomSpacing(4, after: titleLabel)
        content.translatesAutoresizingMaskIntoConstraints = false
        scroll.translatesAutoresizingMaskIntoConstraints = false
        scroll.alwaysBounceVertical = true
        scroll.keyboardDismissMode = .interactive
        scroll.addSubview(content)

        // The buttons: one under another, whatever the text size, in a bar
        // of their own under what the request says.
        configure(allowButton, "Allow once", id: "approval-allow", hint: nil) { [weak self] in self?.choose(.allow) }
        configure(allowSessionButton, "Allow for this session", id: "approval-allow-session", hint: nil) { [weak self] in self?.choose(.allowSession) }
        configure(denyButton, "Deny", id: "approval-deny", hint: "Refuses the call; the agent is told") { [weak self] in self?.choose(.deny) }
        configure(answerButton, "Answer", id: "approval-answer", hint: nil) { [weak self] in self?.choose(.allow) }
        configure(declineButton, "Decline", id: "approval-decline", hint: "Answers nothing; the agent is told") { [weak self] in self?.choose(.deny) }
        buttons.axis = .vertical
        buttons.spacing = 8
        for b in [allowButton, allowSessionButton, answerButton, denyButton, declineButton] { buttons.addArrangedSubview(b) }
        // The bar scrolls once it would take more than 45% of the sheet
        // (large text, landscape), so what the request says always has
        // room above it.
        let bar = UIScrollView()
        bar.translatesAutoresizingMaskIntoConstraints = false
        bar.alwaysBounceVertical = false
        buttons.translatesAutoresizingMaskIntoConstraints = false
        bar.addSubview(buttons)
        // Below the buttons' own resistance: past the cap, the bar
        // scrolls rather than squash a button.
        let fit = bar.heightAnchor.constraint(equalTo: buttons.heightAnchor)
        fit.priority = .defaultHigh - 1
        for b in [allowButton, allowSessionButton, answerButton, denyButton, declineButton] {
            b.setContentCompressionResistancePriority(.required, for: .vertical)
        }
        // The room above the keyboard, which the cap is a share of.
        let room = UILayoutGuide()
        let rule = UIView()
        rule.backgroundColor = .separator
        rule.translatesAutoresizingMaskIntoConstraints = false

        view.addSubview(scroll)
        view.addSubview(rule)
        view.addSubview(bar)
        view.addLayoutGuide(room)
        self.bar = bar
        let margins = view.layoutMarginsGuide
        let safe = view.safeAreaLayoutGuide
        NSLayoutConstraint.activate([
            scroll.topAnchor.constraint(equalTo: view.topAnchor, constant: 20),
            scroll.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: rule.topAnchor),
            content.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor, constant: 8),
            content.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor, constant: -12),
            content.leadingAnchor.constraint(equalTo: margins.leadingAnchor),
            content.trailingAnchor.constraint(equalTo: margins.trailingAnchor),
            rule.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            rule.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            rule.heightAnchor.constraint(equalToConstant: 0.5),
            bar.topAnchor.constraint(equalTo: rule.bottomAnchor, constant: 10),
            bar.leadingAnchor.constraint(equalTo: margins.leadingAnchor),
            bar.trailingAnchor.constraint(equalTo: margins.trailingAnchor),
            bar.bottomAnchor.constraint(equalTo: view.keyboardLayoutGuide.topAnchor, constant: -8),
            bar.bottomAnchor.constraint(lessThanOrEqualTo: safe.bottomAnchor, constant: -8),
            buttons.topAnchor.constraint(equalTo: bar.contentLayoutGuide.topAnchor),
            buttons.bottomAnchor.constraint(equalTo: bar.contentLayoutGuide.bottomAnchor),
            buttons.leadingAnchor.constraint(equalTo: bar.contentLayoutGuide.leadingAnchor),
            buttons.trailingAnchor.constraint(equalTo: bar.contentLayoutGuide.trailingAnchor),
            buttons.widthAnchor.constraint(equalTo: bar.frameLayoutGuide.widthAnchor),
            fit,
            room.topAnchor.constraint(equalTo: view.topAnchor),
            room.bottomAnchor.constraint(equalTo: view.keyboardLayoutGuide.topAnchor),
            bar.heightAnchor.constraint(lessThanOrEqualTo: room.heightAnchor, multiplier: 0.45),
        ])
        refresh()
    }

    /// The sheet takes the keyboard's commands (⌘↵, Escape) once up, and
    /// a request's buttons are armed from when it's on screen.
    override public var canBecomeFirstResponder: Bool { true }

    override public func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        // At an accessibility text size, the whole height: the buttons'
        // bar would leave the medium detent no room for what runs.
        if traitCollection.preferredContentSizeCategory.isAccessibilityCategory {
            sheetPresentationController?.selectedDetentIdentifier = .large
        }
    }

    override public func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        if fields.values.allSatisfy({ !$0.isFirstResponder }) { becomeFirstResponder() }
        // Capped, the bar shows it scrolls: Deny may be below the fold.
        bar?.flashScrollIndicators()
        // The delay counts from when the request is on screen, not from
        // when the sheet began to rise.
        arm()
    }

    // MARK: Drawing

    /// Shows what the queue holds now: its first request (a new one drawn
    /// afresh and armed after `armDelay`), how many wait, and whether its
    /// buttons take taps. The screen dismisses the sheet when nothing waits.
    public func refresh() {
        guard isViewLoaded, let held = queue.current else { return }
        let prompt = held.prompt
        if prompt.requestID != shownID {
            // What was picked and written for the request shown before is
            // kept for when it shows again (it may come back first).
            if let shownID { drafts[shownID] = draft }
            // Only drafts of requests still held: what was written for one
            // gone (a secret too) is not kept.
            drafts = drafts.filter { id, _ in queue.held.contains { $0.prompt.requestID == id } }
            shownID = prompt.requestID
            draft = drafts[prompt.requestID] ?? QuestionDraft()
            inputShown = false
            draw(prompt)
            for (id, text) in draft.written { fields[id]?.text = text }
            bar?.flashScrollIndicators()
            arm()
            UIAccessibility.post(notification: .screenChanged, argument: titleLabel)
        }
        positionLabel.text = queue.position
        positionLabel.isHidden = queue.position == nil
        let note: String? = if let error = held.error {
            "Your answer wasn't taken: \(SafeText.line(error[...], limit: 300))"
        } else if held.unconfirmed {
            "Not confirmed: nothing was sent."
        } else if !queue.connected {
            "Answer once the link to your laptop is live."
        } else {
            nil
        }
        noteLabel.text = note
        noteLabel.isHidden = note == nil
        let open = armed && held.phase == .waiting && queue.connected
        allowButton.isEnabled = open
        allowSessionButton.isEnabled = open
        denyButton.isEnabled = open
        declineButton.isEnabled = open
        answerButton.isEnabled = open && ApprovalQueue.complete(draft.replies(for: prompt), for: prompt)
        for (question, rows) in zip(prompt.questions, prompt.questions.map { options[$0.id] ?? [] }) {
            for (option, row) in zip(question.options, rows) {
                mark(row, picked: draft.isPicked(option.label, of: question), multi: question.multiSelect)
            }
        }
    }

    /// A request drawn afresh: its words cleaned, its buttons the ones it
    /// offers.
    private func draw(_ prompt: ApprovalPrompt) {
        let asks = prompt.asks
        titleLabel.text = asks ? "The agent asks" : "Allow this call?"
        toolLabel.text = SafeText.line(prompt.tool[...], limit: 80)
        toolLabel.accessibilityLabel = "Tool: \(toolLabel.text ?? "")"
        summaryLabel.text = ApprovalRules.shownSummary(prompt.summary)
        summaryBox.isHidden = summaryLabel.text?.isEmpty ?? true
        let lines = ApprovalRules.lines(prompt.summary)
        summaryLines.text = lines > 1 ? "\(lines) lines" : nil
        summaryLines.isHidden = lines <= 1 || summaryBox.isHidden
        summaryLabel.accessibilityLabel = "What it runs\(lines > 1 ? ", \(lines) lines" : ""): \(summaryLabel.text ?? "")"
        let reason = SafeText.line(prompt.reason[...], limit: 500)
        reasonLabel.text = reason.isEmpty ? nil : "Why it asks: \(reason)"
        reasonLabel.isHidden = reason.isEmpty
        let rules = prompt.remember.map { SafeText.line($0[...], limit: 200) }.filter { !$0.isEmpty }
        rememberLabel.text = rules.isEmpty ? nil : "Allow for this session remembers: \(rules.joined(separator: ", "))"
        rememberLabel.isHidden = rules.isEmpty || asks
        inputLabel.text = Self.plain(Self.pretty(prompt.input), limit: ApprovalRules.inputLimit)
        inputButton.isHidden = asks || prompt.input.isEmpty
        drawInput()

        for v in questionsStack.arrangedSubviews { v.removeFromSuperview() }
        fields = [:]
        options = [:]
        for (index, question) in prompt.questions.enumerated() {
            questionsStack.addArrangedSubview(questionView(question, index: index))
        }
        questionsStack.isHidden = !asks

        let owner = ApprovalRules.needsOwner(prompt, .allow)
        allowButton.isHidden = asks
        allowSessionButton.isHidden = asks || prompt.remember.isEmpty
        denyButton.isHidden = asks
        answerButton.isHidden = !asks
        declineButton.isHidden = !asks
        for b in [allowButton, allowSessionButton, answerButton] {
            var c = b.configuration
            c?.image = owner ? UIImage(systemName: "faceid") : nil
            c?.imagePadding = 6
            b.configuration = c
        }
        let runs = asks ? "Sends your answers" : "Runs the call on your computer"
        allowButton.accessibilityHint = owner ? "\(runs). Asks for Face ID first" : runs
        allowSessionButton.accessibilityHint = owner ? "Runs it, and calls like it this session, without asking. Asks for Face ID first" : "Runs it, and calls like it this session, without asking"
        answerButton.accessibilityHint = owner ? "\(runs). Asks for Face ID first" : runs
        scroll.setContentOffset(.zero, animated: false)
    }

    private func questionView(_ question: ApprovalQuestion, index: Int) -> UIView {
        let stack = UIStackView()
        stack.axis = .vertical
        stack.spacing = 6
        stack.accessibilityIdentifier = "approval-question-\(index)"
        let header = UILabel()
        style(header, .footnote, color: .secondaryLabel, id: "approval-question-header")
        header.text = SafeText.line(question.header[...], limit: 200)
        header.isHidden = header.text?.isEmpty ?? true
        header.accessibilityTraits = .header
        let text = UILabel()
        style(text, .body, id: "approval-question-text")
        text.text = Self.plain(question.question, limit: 4 * 1024)
        stack.addArrangedSubview(header)
        stack.addArrangedSubview(text)
        var rows: [UIButton] = []
        for (n, option) in question.options.enumerated() {
            // The whole label, as it's sent (cleaned), never just its first
            // line: what's picked is what's shown.
            var c = UIButton.Configuration.plain()
            c.title = Self.plain(option.label, limit: 1024)
            let about = Self.plain(option.description, limit: 2 * 1024)
            c.subtitle = about.isEmpty ? nil : about
            c.titleAlignment = .leading
            c.titleLineBreakMode = .byWordWrapping
            c.subtitleLineBreakMode = .byWordWrapping
            c.imagePadding = 10
            c.contentInsets = NSDirectionalEdgeInsets(top: 6, leading: 0, bottom: 6, trailing: 0)
            let row = UIButton(configuration: c)
            row.contentHorizontalAlignment = .leading
            row.accessibilityIdentifier = "approval-option-\(index)-\(n)"
            row.addAction(UIAction { [weak self] _ in self?.pick(option.label, of: question) }, for: .primaryActionTriggered)
            mark(row, picked: false, multi: question.multiSelect)
            rows.append(row)
            stack.addArrangedSubview(row)
        }
        options[question.id] = rows
        let field = UITextField()
        field.borderStyle = .roundedRect
        field.placeholder = question.options.isEmpty ? "Write your answer" : "or write your own"
        field.font = .preferredFont(forTextStyle: .body)
        field.adjustsFontForContentSizeCategory = true
        field.autocorrectionType = question.secret ? .no : .default
        field.autocapitalizationType = question.secret ? .none : .sentences
        field.spellCheckingType = question.secret ? .no : .default
        // Never shown, kept by the keyboard, or offered to autofill.
        field.isSecureTextEntry = question.secret
        field.returnKeyType = .done
        field.delegate = self
        field.accessibilityIdentifier = "approval-written-\(index)"
        field.accessibilityLabel = "Your answer to: \(SafeText.line(question.question[...], limit: 200))"
        field.accessibilityHint = question.secret ? "A secret: not shown as you type" : nil
        field.addAction(UIAction { [weak self, weak field] _ in
            guard let self, let field else { return }
            self.draft.written[question.id] = field.text ?? ""
            self.refresh()
        }, for: .editingChanged)
        fields[question.id] = field
        stack.addArrangedSubview(field)
        return stack
    }

    public func textFieldShouldReturn(_ textField: UITextField) -> Bool {
        textField.resignFirstResponder()
        return true
    }

    private func mark(_ row: UIButton, picked: Bool, multi: Bool) {
        var c = row.configuration
        let name = multi ? (picked ? "checkmark.square.fill" : "square") : (picked ? "checkmark.circle.fill" : "circle")
        c?.image = UIImage(systemName: name)
        row.configuration = c
        row.accessibilityTraits = picked ? [.button, .selected] : .button
    }

    private func drawInput() {
        var c = inputButton.configuration
        c?.title = inputShown ? "Hide input" : "Show input"
        inputButton.configuration = c
        inputBox.isHidden = !inputShown
    }

    // MARK: The person's part

    /// "Show input": the call's JSON input, at most 16 KiB of it.
    func toggleInput() {
        inputShown.toggle()
        drawInput()
        if inputShown { UIAccessibility.post(notification: .layoutChanged, argument: inputLabel) }
    }

    /// An option tapped: picked, or unpicked.
    func pick(_ label: String, of question: ApprovalQuestion) {
        draft.toggle(label, of: question)
        refresh()
    }

    /// What was written for question `id` (the tests' typing).
    public func write(_ text: String, for id: String) {
        fields[id]?.text = text
        draft.written[id] = text
        refresh()
    }

    /// A button: the shown request answered, once it's armed. The queue
    /// asks for Face ID when the answer needs it and sends the command.
    public func choose(_ verdict: ApprovalVerdict) {
        guard armed, let held = queue.current, held.phase == .waiting, held.prompt.requestID == shownID else { return }
        let prompt = held.prompt
        let answers = prompt.asks && verdict.allows ? draft.replies(for: prompt) : []
        view.endEditing(true)
        // The first tap decides: the buttons are off from now, before the
        // answer runs, so a second tap in the same burst does nothing.
        armed = false
        refresh()
        Task { [weak self] in
            let sent = await self?.queue.answer(prompt.requestID, verdict, answers: answers) ?? true
            // Not sent (Face ID said no, the link went): the same request
            // takes taps again.
            guard let self, !sent, self.shownID == prompt.requestID else { return }
            self.armed = true
            self.refresh()
        }
    }

    /// ⌘↵ allows once (answers, for questions), through the same checks
    /// as the button; Escape puts the sheet away for later.
    override public var keyCommands: [UIKeyCommand]? {
        let allow = UIKeyCommand(title: "Allow once", action: #selector(keyAllow), input: "\r", modifierFlags: .command)
        allow.wantsPriorityOverSystemBehavior = true
        allow.repeatBehavior = .nonRepeatable
        let later = UIKeyCommand(title: "Later", action: #selector(keyLater), input: UIKeyCommand.inputEscape)
        return [allow, later]
    }

    @objc private func keyAllow() {
        // Held, ⌘↵ never repeats (`nonRepeatable`), and only a press half a
        // second after the last counts: holding it never allows the next
        // request too.
        let now = CACurrentMediaTime()
        defer { lastKeyAllow = now }
        guard now - lastKeyAllow > 0.5, let held = queue.current else { return }
        if held.prompt.asks, !answerButton.isEnabled { return }
        choose(.allow)
    }

    @objc private func keyLater() {
        dismiss(animated: true) { [weak self] in
            guard let self, let controller = self.presentationController else { return }
            controller.delegate?.presentationControllerDidDismiss?(controller)
        }
    }

    /// The buttons take taps `armDelay` after a request shows.
    private func arm() {
        arming?.cancel()
        armed = armDelay <= 0
        guard !armed else { return }
        let work = DispatchWorkItem { [weak self] in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.armed = true
                self.arming = nil
                self.refresh()
            }
        }
        arming = work
        DispatchQueue.main.asyncAfter(deadline: .now() + armDelay, execute: work)
    }

    // MARK: Helpers

    private func style(_ label: UILabel, _ style: UIFont.TextStyle, color: UIColor = .label, id: String) {
        label.font = .preferredFont(forTextStyle: style)
        label.adjustsFontForContentSizeCategory = true
        label.textColor = color
        label.numberOfLines = 0
        label.accessibilityIdentifier = id
    }

    private func configure(_ button: UIButton, _ title: String, id: String, hint: String?, _ action: @escaping @MainActor () -> Void) {
        var c = button.configuration ?? .filled()
        c.title = title
        c.buttonSize = .large
        c.cornerStyle = .large
        c.titleLineBreakMode = .byWordWrapping
        button.configuration = c
        button.accessibilityIdentifier = id
        button.accessibilityHint = hint
        button.addAction(UIAction { _ in action() }, for: .primaryActionTriggered)
    }

    /// `text` as the sheet draws it: cleaned, at most `limit` bytes, then
    /// how many more; never a link.
    static func plain(_ text: String, limit: Int) -> String {
        let (shown, more) = SafeText.truncated(text, limit: limit)
        let clean = SafeText.clean(shown)
        return more > 0 ? clean + "\n" + SafeText.more(more) : clean
    }

    /// A call's JSON input, laid out with its keys sorted when it parses
    /// (and isn't huge); as it came otherwise.
    static func pretty(_ input: String) -> String {
        guard input.utf8.count <= 4 * ApprovalRules.inputLimit,
              let object = try? JSONSerialization.jsonObject(with: Data(input.utf8), options: [.fragmentsAllowed]),
              let data = try? JSONSerialization.data(withJSONObject: object, options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes, .fragmentsAllowed])
        else { return input }
        return String(decoding: data, as: UTF8.self)
    }
}
