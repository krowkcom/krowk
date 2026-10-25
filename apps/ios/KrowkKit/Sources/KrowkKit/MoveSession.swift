import Observation
import UIKit

// Moving a session to another of the person's hosts from the phone
// (R-HAND-4, R-HAND-1, ticket 136): the phone never hosts, so it asks the
// host the session should go to, and that host runs `krowk sync take`
// itself, in a clone of the session's repository. Its take checks the
// session can run there before anything moves (R-CRED-1), and its refusal
// is the sentence shown. The session goes on over the relay meanwhile: the
// old host stops only once it hands the lease over.

/// A move as the person asked for it: the session, the host it goes to and
/// one of that host's directories, under `id`, a command id minted once per
/// pick.
public struct MoveRequest: Sendable, Equatable {
    public var id: String
    public var device: String
    public var hostName: String
    public var sessionID: String
    public var cwd: String

    public init(id: String, device: String, hostName: String, sessionID: String, cwd: String) {
        self.id = id
        self.device = device
        self.hostName = hostName
        self.sessionID = sessionID
        self.cwd = cwd
    }
}

/// How a move came out.
public enum MoveOutcome: Sendable, Equatable {
    /// The host holds the session now.
    case taken
    /// Nothing moved; `message` is the host's sentence, or why it couldn't
    /// be asked. As `cancelled`, this phone stopped waiting.
    case refused(code: String, message: String)
}

/// A session's move (R-HAND-4): `moving to <host>…` at once (R-LAG-8),
/// then `moved to <host>`, or the host's refusal in place and the session
/// where it was. One at a time.
@MainActor
@Observable
public final class SessionMove {
    public enum State: Equatable, Sendable {
        case moving(host: String)
        case moved(host: String)
        /// The host's sentence, until dismissed or the next move.
        case refused(String)
    }

    public private(set) var state: State?
    /// The device holding the session, as last known: the list's, then the
    /// host it moved to.
    public private(set) var holder: String?

    private let source: any HostSource

    public init(source: any HostSource, holder: String?) {
        self.source = source
        self.holder = holder
    }

    /// What the bar adds while a move is under way or done.
    public var barLine: String? {
        switch state {
        case .moving(let host)?: "moving to \(SafeText.clean(host))…"
        case .moved(let host)?: "moved to \(SafeText.clean(host))"
        case .refused?, nil: nil
        }
    }

    public var refusal: String? {
        if case .refused(let message)? = state { message } else { nil }
    }

    public var isMoving: Bool {
        if case .moving? = state { true } else { false }
    }

    /// Asks `request`'s host to take the session up, off the main thread.
    /// Whether it went: not while another move is waited on.
    @discardableResult
    public func move(_ request: MoveRequest) -> Bool {
        guard !isMoving, request.device != holder else { return false }
        state = .moving(host: request.hostName)
        let source = source
        Task {
            let outcome = await blocking { source.take(request) }
            switch outcome {
            case .taken:
                state = .moved(host: request.hostName)
                holder = request.device
            case .refused(code: "cancelled", _):
                state = nil
            case .refused(_, let message):
                state = .refused(message)
            }
        }
        return true
    }

    public func dismissRefusal() {
        if refusal != nil { state = nil }
    }

    /// The screen went: the wait ends, and the move goes on on the host.
    public func stopWaiting() {
        guard isMoving else { return }
        source.cancelTake()
    }
}

/// The Move to… sheet (R-HAND-4): the person's online hosts but the one
/// holding the session, each with the directories it announced. A pick is
/// a host and a directory, which should be a clone of the session's
/// repository: the phone can't tell, and the host's take refuses one that
/// isn't, saying so.
@MainActor
final class MovePickerViewController: ChoiceListViewController {
    let hosts: HostListModel
    let holder: String?
    private let onPick: @MainActor (_ device: String, _ hostName: String, _ cwd: String) -> Void
    /// The host of each section, in order: device and name.
    private var shown: [(device: String, name: String)] = []

    init(hosts: HostListModel, holder: String?, onPick: @escaping @MainActor (String, String, String) -> Void) {
        self.hosts = hosts
        self.holder = holder
        self.onPick = onPick
        super.init()
        title = "Move to"
        navigationItem.leftBarButtonItem = UIBarButtonItem(systemItem: .cancel, primaryAction: UIAction { [weak self] _ in
            self?.dismiss(animated: true)
        })
        navigationItem.leftBarButtonItem?.accessibilityIdentifier = "move-picker-cancel"
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        tableView.accessibilityIdentifier = "move-picker"
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
        let built = Self.sections(hosts: hosts.listing?.hosts ?? [], listed: hosts.listing != nil, failed: hosts.failed, holder: holder)
        shown = built.hosts
        sections = built.sections
    }

    /// The sheet's sections, and the host each lists: online hosts only,
    /// never the holder, each with its directories.
    static func sections(hosts: [any AnnouncedHost], listed: Bool, failed: Bool, holder: String?)
        -> (sections: [Section], hosts: [(device: String, name: String)])
    {
        guard listed || failed else {
            return ([Section(header: nil, footer: "Looking for your hosts…", choices: [])], [])
        }
        guard !failed else {
            return ([Section(header: nil, footer: "Your hosts couldn't be read just now: try again in a moment.", choices: [])], [])
        }
        var sections: [Section] = []
        var shown: [(device: String, name: String)] = []
        for host in hosts where host.online && host.device != holder {
            let name = SafeText.clean(host.name)
            let choices = host.directories.map { dir in
                Choice(title: NewSessionText.homeAsTilde(SafeText.clean(dir)), detail: nil, enabled: true, identifier: "move-\(host.name)-\(dir)", value: dir)
            }
            let footer = choices.isEmpty ? "\(name) offers no directory yet: run krowk there once in a clone of this session's repository, and trust it." : nil
            sections.append(Section(header: name, footer: footer, choices: choices))
            shown.append((host.device, host.name))
        }
        if sections.isEmpty {
            return ([Section(header: nil, footer: "No other host is online. Start krowk on another of your computers to move this session there.", choices: [])], [])
        }
        sections[sections.count - 1].footer = (sections[sections.count - 1].footer.map { $0 + "\n\n" } ?? "")
            + "Pick a clone of this session's repository. The host checks it can run the session there before anything moves."
        return (sections, shown)
    }

    override func chosen(_ choice: Choice, in section: Int) {
        guard section < shown.count else { return }
        let host = shown[section]
        dismiss(animated: true)
        onPick(host.device, host.name, choice.value)
    }
}
