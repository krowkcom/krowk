import Observation
import UIKit

/// "New session" (R-HOST-4, ticket 134): UIKit, a navigation controller in
/// a sheet, opened by Sessions' `+`. A host, then one of its directories,
/// then a model one of its instances has ready, then the prompt and Start.
/// Start hands the request to `onStart` and closes the sheet at once: the
/// row in Sessions says how it goes (R-LAG-8). Nothing here offers a mode,
/// a budget, a toolset or an effort.
@MainActor
public enum NewSessionFlow {
    /// The flow in its navigation controller, ready to present. With
    /// `prefill` (Edit and retry), it opens on the prompt, filled in, with
    /// the host list under it.
    public static func make(source: any HostSource, prefill: StartRequest? = nil, onStart: @escaping @MainActor (StartRequest) -> Void) -> UINavigationController {
        let hosts = HostStepViewController(model: HostListModel(source: source), source: source, onStart: onStart)
        let navigation = UINavigationController(rootViewController: hosts)
        navigation.view.accessibilityIdentifier = "new-session"
        if let prefill {
            let draft = Draft(device: prefill.device, hostName: prefill.hostName, cwd: prefill.cwd, instance: prefill.instance, model: prefill.model)
            navigation.pushViewController(PromptStepViewController(draft: draft, text: prefill.prompt, source: source, onStart: onStart), animated: false)
        }
        return navigation
    }

    /// What the steps have chosen so far.
    struct Draft: Equatable {
        var device: String
        var hostName: String
        var cwd: String
        var instance: String
        var model: String
    }
}

/// A list of things to choose, in sections: one that isn't `enabled` is
/// drawn greyed and can't be chosen.
@MainActor
class ChoiceListViewController: UITableViewController {
    struct Choice: Equatable {
        var title: String
        var detail: String?
        var enabled: Bool
        var identifier: String
        /// What choosing it means, passed back to `chosen`.
        var value: String
    }

    struct Section: Equatable {
        var header: String?
        var footer: String?
        var choices: [Choice]
    }

    var sections: [Section] = [] {
        didSet {
            guard sections != oldValue, isViewLoaded else { return }
            tableView.reloadData()
        }
    }

    init() {
        super.init(style: .insetGrouped)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("built in code")
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        tableView.register(UITableViewCell.self, forCellReuseIdentifier: "choice")
    }

    /// The choice at `path` was tapped.
    func chosen(_ choice: Choice, in section: Int) {}

    override func numberOfSections(in tableView: UITableView) -> Int { sections.count }

    override func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int { sections[section].choices.count }

    override func tableView(_ tableView: UITableView, titleForHeaderInSection section: Int) -> String? { sections[section].header }

    override func tableView(_ tableView: UITableView, titleForFooterInSection section: Int) -> String? { sections[section].footer }

    override func tableView(_ tableView: UITableView, cellForRowAt path: IndexPath) -> UITableViewCell {
        let choice = sections[path.section].choices[path.row]
        let cell = tableView.dequeueReusableCell(withIdentifier: "choice", for: path)
        var content = UIListContentConfiguration.subtitleCell()
        content.text = choice.title
        content.secondaryText = choice.detail
        content.secondaryTextProperties.numberOfLines = 0
        content.secondaryTextProperties.color = .secondaryLabel
        if !choice.enabled {
            content.textProperties.color = .tertiaryLabel
            content.secondaryTextProperties.color = .tertiaryLabel
        }
        cell.contentConfiguration = content
        cell.accessoryType = choice.enabled ? .disclosureIndicator : .none
        cell.selectionStyle = choice.enabled ? .default : .none
        cell.accessibilityIdentifier = choice.identifier
        cell.accessibilityTraits = choice.enabled ? .button : [.staticText, .notEnabled]
        return cell
    }

    override func tableView(_ tableView: UITableView, shouldHighlightRowAt path: IndexPath) -> Bool {
        sections[path.section].choices[path.row].enabled
    }

    override func tableView(_ tableView: UITableView, willSelectRowAt path: IndexPath) -> IndexPath? {
        sections[path.section].choices[path.row].enabled ? path : nil
    }

    override func tableView(_ tableView: UITableView, didSelectRowAt path: IndexPath) {
        tableView.deselectRow(at: path, animated: true)
        let choice = sections[path.section].choices[path.row]
        guard choice.enabled else { return }
        chosen(choice, in: path.section)
    }

    /// Whether the choice titled `title` can be chosen; nil when none is
    /// shown. For the tests.
    func choosable(_ identifier: String) -> Bool? {
        sections.lazy.flatMap(\.choices).first { $0.identifier == identifier }?.enabled
    }
}

/// The host step: the hosts that announce themselves, online first. An
/// offline one is greyed with when it was last seen, and can't be chosen
/// (R-IOS-7). Read every 10 s while on screen, and on a pull. Which path a
/// session will take, direct or relay, is known only once it's open, so
/// none is shown here.
@MainActor
final class HostStepViewController: ChoiceListViewController {
    let model: HostListModel
    private let source: any HostSource
    private let onStart: @MainActor (StartRequest) -> Void

    init(model: HostListModel, source: any HostSource, onStart: @escaping @MainActor (StartRequest) -> Void) {
        self.model = model
        self.source = source
        self.onStart = onStart
        super.init()
        title = "New session"
        navigationItem.leftBarButtonItem = UIBarButtonItem(systemItem: .cancel, primaryAction: UIAction { [weak self] _ in
            self?.dismiss(animated: true)
        })
        navigationItem.leftBarButtonItem?.accessibilityIdentifier = "new-session-cancel"
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        tableView.accessibilityIdentifier = "new-session-hosts"
        let refresh = UIRefreshControl()
        refresh.addAction(UIAction { [weak self] _ in
            guard let self else { return }
            Task {
                await self.model.refresh()
                self.refreshControl?.endRefreshing()
            }
        }, for: .valueChanged)
        refreshControl = refresh
        observe()
    }

    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        model.appeared()
    }

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        model.disappeared()
    }

    private func observe() {
        let built = withObservationTracking {
            Self.sections(model)
        } onChange: { [weak self] in
            Task { @MainActor in self?.observe() }
        }
        sections = built
    }

    static func sections(_ model: HostListModel) -> [Section] {
        let rows = model.rows
        let choices = rows.map { Choice(title: $0.name, detail: $0.detail, enabled: $0.choosable, identifier: "host-\($0.name)", value: $0.device) }
        var footer: String?
        if model.failed {
            footer = "Your hosts couldn't be read just now, so none is shown online. Pull down to try again."
        } else if model.listing == nil {
            footer = "Looking for your hosts…"
        } else if rows.isEmpty {
            footer = "None of your computers announces itself yet. Turn sync on in krowk on one, and it shows here."
        }
        if let unreadable = model.listing?.unreadable, unreadable > 0 {
            let line = "\(unreadable) host\(unreadable == 1 ? "" : "s") can't be read on this iPhone."
            footer = footer.map { "\($0)\n\(line)" } ?? line
        }
        return [Section(header: rows.isEmpty ? nil : "Start on", footer: footer, choices: choices)]
    }

    override func chosen(_ choice: Choice, in section: Int) {
        guard let host = model.host(choice.value) else { return }
        navigationController?.pushViewController(DirectoryStepViewController(host: host, source: source, onStart: onStart), animated: true)
    }
}

/// The directory step: the directories the host announced, home as `~`.
@MainActor
final class DirectoryStepViewController: ChoiceListViewController {
    private let host: any AnnouncedHost
    private let source: any HostSource
    private let onStart: @MainActor (StartRequest) -> Void

    init(host: any AnnouncedHost, source: any HostSource, onStart: @escaping @MainActor (StartRequest) -> Void) {
        self.host = host
        self.source = source
        self.onStart = onStart
        super.init()
        title = "Directory"
        let choices = host.directories.enumerated().map { i, path in
            Choice(title: NewSessionText.homeAsTilde(SafeText.clean(path)), detail: nil, enabled: true, identifier: "directory-\(i)", value: path)
        }
        let footer = choices.isEmpty ? "\(SafeText.clean(host.name)) offers no directory yet: trust one in krowk there." : nil
        sections = [Section(header: "On \(SafeText.clean(host.name))", footer: footer, choices: choices)]
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        tableView.accessibilityIdentifier = "new-session-directories"
    }

    override func chosen(_ choice: Choice, in section: Int) {
        navigationController?.pushViewController(ModelStepViewController(host: host, cwd: choice.value, source: source, onStart: onStart), animated: true)
    }
}

/// The model step: each instance's ready models under its name and limit
/// (R-INST-6). One not ready is listed greyed with its fix, and can't be
/// chosen.
@MainActor
final class ModelStepViewController: ChoiceListViewController {
    private let host: any AnnouncedHost
    private let cwd: String
    private let source: any HostSource
    private let onStart: @MainActor (StartRequest) -> Void

    init(host: any AnnouncedHost, cwd: String, source: any HostSource, onStart: @escaping @MainActor (StartRequest) -> Void) {
        self.host = host
        self.cwd = cwd
        self.source = source
        self.onStart = onStart
        super.init()
        title = "Model"
        sections = Self.sections(host.announcedInstances)
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        tableView.accessibilityIdentifier = "new-session-models"
    }

    static func sections(_ instances: [any AnnouncedInstance]) -> [Section] {
        instances.map { i in
            let name = SafeText.clean(i.name)
            let header = NewSessionText.limit(i).map { "\(name) · \($0)" } ?? name
            if i.ready && !i.models.isEmpty {
                return Section(header: header, footer: nil, choices: i.models.map { m in
                    Choice(title: SafeText.clean(m), detail: nil, enabled: true, identifier: "model-\(i.name)/\(m)", value: m)
                })
            }
            if i.ready {
                return Section(header: header, footer: nil, choices: [Choice(title: "No model announced", detail: nil, enabled: false, identifier: "model-\(i.name)-none", value: "")])
            }
            let fix = i.fix.map(SafeText.clean) ?? "not ready"
            return Section(header: header, footer: nil, choices: [Choice(title: "Not ready", detail: fix, enabled: false, identifier: "model-\(i.name)-unready", value: "")])
        }
    }

    override func chosen(_ choice: Choice, in section: Int) {
        let instance = host.announcedInstances[section].name
        let draft = NewSessionFlow.Draft(device: host.device, hostName: host.name, cwd: cwd, instance: instance, model: choice.value)
        navigationController?.pushViewController(PromptStepViewController(draft: draft, text: "", source: source, onStart: onStart), animated: true)
    }
}

/// The prompt step: where it will run, a text view, and Start, which hands
/// the request over under a fresh command id and closes the sheet at once.
@MainActor
final class PromptStepViewController: UIViewController, UITextViewDelegate {
    let draft: NewSessionFlow.Draft
    let textView = UITextView()
    private let summary = UILabel()
    private let source: any HostSource
    private let onStart: @MainActor (StartRequest) -> Void
    /// The longest prompt a host takes.
    static let maxBytes = 256 * 1024

    init(draft: NewSessionFlow.Draft, text: String, source: any HostSource, onStart: @escaping @MainActor (StartRequest) -> Void) {
        self.draft = draft
        self.source = source
        self.onStart = onStart
        super.init(nibName: nil, bundle: nil)
        title = "Prompt"
        textView.text = text
        let start = UIBarButtonItem(title: "Start", primaryAction: UIAction { [weak self] _ in self?.start() })
        start.style = .prominent
        start.accessibilityIdentifier = "new-session-start"
        navigationItem.rightBarButtonItem = start
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("built in code")
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemGroupedBackground
        summary.text = "On \(SafeText.clean(draft.hostName)) · \(NewSessionText.homeAsTilde(SafeText.clean(draft.cwd))) · \(SafeText.clean(draft.instance))/\(SafeText.clean(draft.model))"
        summary.font = .preferredFont(forTextStyle: .footnote)
        summary.adjustsFontForContentSizeCategory = true
        summary.textColor = .secondaryLabel
        summary.numberOfLines = 0
        summary.accessibilityIdentifier = "new-session-summary"
        textView.font = .preferredFont(forTextStyle: .body)
        textView.adjustsFontForContentSizeCategory = true
        textView.backgroundColor = .secondarySystemGroupedBackground
        textView.layer.cornerRadius = 10
        textView.textContainerInset = UIEdgeInsets(top: 10, left: 6, bottom: 10, right: 6)
        textView.accessibilityIdentifier = "new-session-prompt"
        textView.accessibilityLabel = "Prompt"
        textView.delegate = self
        for v in [summary, textView] as [UIView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            view.addSubview(v)
        }
        let guide = view.layoutMarginsGuide
        NSLayoutConstraint.activate([
            summary.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 12),
            summary.leadingAnchor.constraint(equalTo: guide.leadingAnchor),
            summary.trailingAnchor.constraint(equalTo: guide.trailingAnchor),
            textView.topAnchor.constraint(equalTo: summary.bottomAnchor, constant: 12),
            textView.leadingAnchor.constraint(equalTo: guide.leadingAnchor),
            textView.trailingAnchor.constraint(equalTo: guide.trailingAnchor),
            textView.bottomAnchor.constraint(equalTo: view.keyboardLayoutGuide.topAnchor, constant: -12),
        ])
        updateStart()
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        textView.becomeFirstResponder()
    }

    func textViewDidChange(_ textView: UITextView) {
        updateStart()
    }

    private var prompt: String { textView.text.trimmingCharacters(in: .whitespacesAndNewlines) }

    private func updateStart() {
        navigationItem.rightBarButtonItem?.isEnabled = !prompt.isEmpty && textView.text.utf8.count <= Self.maxBytes
    }

    /// The request, under a fresh command id: each Start is a new command.
    func request() -> StartRequest {
        StartRequest(id: source.newCommandID(), device: draft.device, hostName: draft.hostName, cwd: draft.cwd, instance: draft.instance, model: draft.model, prompt: textView.text)
    }

    func start() {
        guard !prompt.isEmpty else { return }
        onStart(request())
        (navigationController ?? self).dismiss(animated: true)
    }
}
