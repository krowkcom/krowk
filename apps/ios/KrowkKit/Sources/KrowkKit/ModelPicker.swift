import Observation
import UIKit

/// The session's model picker (ticket 135): a sheet over the transcript,
/// opened by the model in the navigation bar's subtitle. It lists the
/// ready instances of the session's host (its holder, as the session list
/// names it, matched to `listHosts`) and their models, each instance with
/// its limit (R-INST-6): the session's own `limits` lines when it has seen
/// one, else the host's announcement. One not ready is greyed with its
/// fix. With the host offline it says so and offers nothing (R-HAND-4).
/// A pick needs the live link, as Steer and Stop do: it is never queued
/// for a later host. Nothing here offers a mode, toolset, effort or budget.
@MainActor
final class ModelPickerViewController: ChoiceListViewController {
    /// What the sheet shows besides the host's listing: the session's
    /// model, whether the link is live, its limits.
    struct Session: Equatable {
        var current: ModelChoice?
        var live: Bool
        var hostOffline: Bool
        var limits: [String: InstanceLimit]
    }

    let hosts: HostListModel
    let device: String?
    var session: Session {
        didSet { if session != oldValue { rebuild() } }
    }
    /// The tests' seam: the instances shown, from the host's.
    private let adjust: (@MainActor ([any AnnouncedInstance]) -> [any AnnouncedInstance])?
    private let onPick: @MainActor (ModelChoice) -> Void
    /// The instance of each section, in order.
    private var instances: [String] = []

    init(hosts: HostListModel, device: String?, session: Session,
         adjust: (@MainActor ([any AnnouncedInstance]) -> [any AnnouncedInstance])?, onPick: @escaping @MainActor (ModelChoice) -> Void) {
        self.hosts = hosts
        self.device = device
        self.session = session
        self.adjust = adjust
        self.onPick = onPick
        super.init()
        title = "Model"
        navigationItem.rightBarButtonItem = UIBarButtonItem(systemItem: .done, primaryAction: UIAction { [weak self] _ in
            self?.dismiss(animated: true)
        })
        navigationItem.rightBarButtonItem?.accessibilityIdentifier = "model-picker-done"
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        tableView.accessibilityIdentifier = "model-picker"
        observe()
    }

    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        hosts.appeared()
    }

    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        hosts.disappeared()
    }

    private func observe() {
        withObservationTracking {
            rebuild()
        } onChange: { [weak self] in
            Task { @MainActor in self?.observe() }
        }
    }

    private func rebuild() {
        let host = device.flatMap(hosts.host)
        let shown = host.map { adjust?($0.announcedInstances) ?? $0.announcedInstances } ?? []
        let built = Self.sections(host: host, listed: hosts.listing != nil, failed: hosts.failed, instances: shown, session: session)
        instances = built.instances
        sections = built.sections
    }

    /// The sheet's sections, and the instance each lists.
    static func sections(host: (any AnnouncedHost)?, listed: Bool, failed: Bool, instances: [any AnnouncedInstance], session: Session) -> (sections: [Section], instances: [String]) {
        guard listed || failed else {
            return ([Section(header: nil, footer: "Looking for this session's host…", choices: [])], [])
        }
        let name = host.map { SafeText.clean($0.name) } ?? "This session's host"
        guard let host, host.online, !failed, !session.hostOffline else {
            let why = failed && host?.online == true
                ? "Your hosts couldn't be read just now: the model can be switched once they are."
                : "\(name) is offline: the model can be switched once it's back."
            return ([Section(header: nil, footer: why, choices: [])], [])
        }
        var sections: [Section] = []
        var names: [String] = []
        for i in instances {
            let instance = SafeText.clean(i.name)
            let seen = session.limits[i.name]
            let limit = seen.map { NewSessionText.limit(state: $0.state, usedPercent: $0.usedPercent, resetsAtMs: $0.resetsAtMs) } ?? NewSessionText.limit(i)
            let header = limit.map { "\(instance) · \($0)" } ?? instance
            let choices: [Choice]
            if i.ready && !i.models.isEmpty {
                choices = i.models.map { m in
                    let here = session.current == ModelChoice(instance: i.name, model: m)
                    return Choice(title: SafeText.clean(m), detail: here ? "current" : nil, enabled: session.live && !here,
                                  identifier: "switch-model-\(i.name)/\(m)", value: m)
                }
            } else if i.ready {
                choices = [Choice(title: "No model announced", detail: nil, enabled: false, identifier: "switch-model-\(i.name)-none", value: "")]
            } else {
                choices = [Choice(title: "Not ready", detail: i.fix.map(SafeText.clean) ?? "not ready", enabled: false, identifier: "switch-model-\(i.name)-unready", value: "")]
            }
            sections.append(Section(header: header, footer: nil, choices: choices))
            names.append(i.name)
        }
        if sections.isEmpty {
            sections = [Section(header: nil, footer: "\(name) announces no instance.", choices: [])]
        } else if !session.live {
            sections[sections.count - 1].footer = "Switching waits for the link to \(name)."
        }
        return (sections, names)
    }

    override func chosen(_ choice: Choice, in section: Int) {
        guard section < instances.count else { return }
        let pick = ModelChoice(instance: instances[section], model: choice.value)
        onPick(pick)
        dismiss(animated: true)
    }
}
