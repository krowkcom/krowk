import Observation
import UIKit

/// "Sessions", the paired phone's first screen (R-IOS-3, R-IOS-7): one row
/// per synced session, most recently written first, keyed by session id
/// in a diffable data source over a plain list. A session is `● live`
/// only while a host holds its lease; otherwise "last seen". Sessions
/// this phone can't open are counted in the footer, never shown. The list
/// never offers to take or host a session (R-HAND-4). With `newSessions`,
/// its `+` starts one on a host (R-HOST-4): the start shows at once as a
/// row at the top, `starting on <host>…`, until the host answers (R-LAG-8).
public final class SessionListViewController: UIViewController, UICollectionViewDelegate {
    /// What the screen shows, rebuilt from the model at most 10 times a
    /// second and applied only when it changed.
    struct Screen: Equatable {
        enum Status: Equatable {
            /// The phone's kept listing is being read (ticket 131): nothing
            /// is drawn yet, not even a spinner, so the first snapshot is
            /// the kept rows when there are any.
            case priming
            /// The first read is running.
            case loading
            /// No session: the empty sentence.
            case empty
            /// The first read failed: its sentence.
            case failed(String)
            case listed
        }

        var rows: [SessionRow]
        var status: Status
        /// The starts under way or refused, newest first, above the rows.
        var starting: [StartingRow] = []
        /// "2 sessions can't be opened on this iPhone.", or nil.
        var unreadable: String?
        /// A refresh failed after a listing: the rows are the last ones read.
        var stale: String?
    }

    static let emptySentence = "No synced sessions yet. Start one on your laptop with krowk; it syncs by itself once sync is on."

    let model: SessionListModel
    private let formatter = SessionRowFormatter()
    /// What tapping a row pushes: its transcript.
    private let open: @MainActor (SessionRow) -> UIViewController
    /// Starting a session on a host: none when nil.
    public let newSessions: NewSessions?

    private(set) var collectionView: UICollectionView!
    private(set) var dataSource: UICollectionViewDiffableDataSource<Int, String>!
    private(set) var throttle: SnapshotThrottle<Screen>!
    /// The rows applied, by id: what each cell shows.
    private var rows: [String: SessionRow] = [:]
    /// The starts applied, by row id.
    private var startingRows: [String: StartingRow] = [:]
    /// The footer's lines, as last applied.
    private var footerLines: [String] = []
    let emptyLabel = UILabel()
    let spinner = UIActivityIndicatorView(style: .medium)
    private var observers: [NSObjectProtocol] = []
    /// Routes from notifications taken: one finding its session gives way
    /// to a later one.
    private var routes = 0

    public init(model: SessionListModel, newSessions: NewSessions? = nil, open: (@MainActor (SessionRow) -> UIViewController)? = nil) {
        self.model = model
        self.newSessions = newSessions
        self.open = open ?? { TranscriptViewController(row: $0) }
        super.init(nibName: nil, bundle: nil)
        // Back in the foreground with no session open over this list, its
        // first read landing is the app caught up (ticket 128).
        model.landed = { [weak self] started, succeeded in
            guard let self, navigationController?.topViewController === self, viewIfLoaded?.window != nil else { return }
            Budgets.shared.listRead(startedAt: started, succeeded: succeeded)
        }
        title = "Sessions"
        navigationItem.largeTitleDisplayMode = .always
        if let newSessions {
            let add = UIBarButtonItem(systemItem: .add, primaryAction: UIAction { [weak self] _ in self?.newSession() })
            add.accessibilityIdentifier = "new-session-open"
            add.accessibilityLabel = "New session"
            navigationItem.rightBarButtonItem = add
            newSessions.starting.started = { [weak self] session, request in self?.started(session, request) }
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("SessionListViewController is built in code")
    }

    override public func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemBackground

        // A footer only while it has something to say.
        let layout = UICollectionViewCompositionalLayout { [weak self] _, environment in
            var list = UICollectionLayoutListConfiguration(appearance: .plain)
            list.footerMode = (self?.footerLines.isEmpty ?? true) ? .none : .supplementary
            return NSCollectionLayoutSection.list(using: list, layoutEnvironment: environment)
        }
        collectionView = UICollectionView(frame: view.bounds, collectionViewLayout: layout)
        collectionView.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        collectionView.accessibilityIdentifier = "sessions-list"
        collectionView.delegate = self
        collectionView.alwaysBounceVertical = true
        view.addSubview(collectionView)

        let refresh = UIRefreshControl()
        refresh.accessibilityIdentifier = "sessions-refresh"
        refresh.addAction(UIAction { [weak self] _ in self?.pulled() }, for: .valueChanged)
        collectionView.refreshControl = refresh

        let cell = UICollectionView.CellRegistration<UICollectionViewListCell, String> { [weak self] cell, _, id in
            if let start = self?.startingRows[id] {
                self?.configure(cell, starting: start)
                return
            }
            guard let row = self?.rows[id] else { return }
            Self.configure(cell, with: row)
        }
        dataSource = UICollectionViewDiffableDataSource<Int, String>(collectionView: collectionView) { view, path, id in
            view.dequeueConfiguredReusableCell(using: cell, for: path, item: id)
        }
        let footer = UICollectionView.SupplementaryRegistration<UICollectionViewListCell>(elementKind: UICollectionView.elementKindSectionFooter) { [weak self] view, _, _ in
            Self.configure(footer: view, lines: self?.footerLines ?? [])
        }
        dataSource.supplementaryViewProvider = { view, _, path in
            view.dequeueConfiguredReusableSupplementary(using: footer, for: path)
        }

        emptyLabel.font = .preferredFont(forTextStyle: .body)
        emptyLabel.adjustsFontForContentSizeCategory = true
        emptyLabel.textColor = .secondaryLabel
        emptyLabel.textAlignment = .center
        emptyLabel.numberOfLines = 0
        emptyLabel.accessibilityIdentifier = "sessions-empty"
        spinner.accessibilityIdentifier = "sessions-loading"
        spinner.accessibilityLabel = "Loading your sessions"
        let background = UIView()
        for v in [emptyLabel, spinner] as [UIView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            background.addSubview(v)
        }
        let guide = background.layoutMarginsGuide
        NSLayoutConstraint.activate([
            emptyLabel.leadingAnchor.constraint(equalTo: guide.leadingAnchor, constant: 16),
            emptyLabel.trailingAnchor.constraint(equalTo: guide.trailingAnchor, constant: -16),
            emptyLabel.centerYAnchor.constraint(equalTo: background.centerYAnchor),
            spinner.centerXAnchor.constraint(equalTo: background.centerXAnchor),
            spinner.centerYAnchor.constraint(equalTo: background.centerYAnchor),
        ])
        collectionView.backgroundView = background

        // Subscribed before the first read: the read is tracked, so a
        // change after it schedules the next rebuild.
        throttle = SnapshotThrottle(read: { [weak self] in self?.observedScreen() ?? Screen(rows: [], status: .loading) }, apply: { [weak self] previous, new in
            self?.apply(new, animated: previous.map { $0.status == .listed || $0.status == .empty } ?? false)
        })
        throttle.rebuild()
    }

    override public func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        model.appeared()
        if observers.isEmpty {
            let center = NotificationCenter.default
            observers = [
                // sceneWillEnterForeground: read again at once, and drop a
                // live mark older than a lease until it has been.
                center.addObserver(forName: UIScene.willEnterForegroundNotification, object: nil, queue: .main) { [weak self] _ in
                    MainActor.assumeIsolated { self?.model.appeared() }
                },
                center.addObserver(forName: UIScene.didEnterBackgroundNotification, object: nil, queue: .main) { [weak self] _ in
                    MainActor.assumeIsolated { self?.model.disappeared() }
                },
            ]
        }
    }

    override public func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        model.disappeared()
        observers.forEach(NotificationCenter.default.removeObserver)
        observers = []
    }

    public func collectionView(_ collectionView: UICollectionView, didSelectItemAt indexPath: IndexPath) {
        collectionView.deselectItem(at: indexPath, animated: true)
        guard let id = dataSource.itemIdentifier(for: indexPath) else { return }
        if let start = startingRows[id] {
            switch start.state {
            case .refused: editAndRetry(id)
            case .started(let session):
                if let open = openScreen(of: session.lowercased()) {
                    _ = reveal(open)
                } else if let newSessions {
                    navigationController?.pushViewController(newSessions.open(session, start.request), animated: true)
                }
            case .starting: break
            }
            return
        }
        guard let row = rows[id] else { return }
        navigationController?.pushViewController(opened(row), animated: true)
    }

    /// Session `row`'s screen, as `open` makes it: a transcript reads its
    /// "last seen" from this list, whose clock ticks each minute (R-IOS-7).
    private func opened(_ row: SessionRow) -> UIViewController {
        let screen = open(row)
        if let transcript = screen as? TranscriptViewController, transcript.lastSeen == nil {
            let id = row.id.lowercased()
            transcript.lastSeen = { [weak self] in self?.row(id)?.lastSeen }
        }
        return screen
    }

    // MARK: Building and applying

    /// The screen, read under observation: the model's next change
    /// triggers the next rebuild.
    private func observedScreen() -> Screen {
        withObservationTracking {
            screen()
        } onChange: { [weak self] in
            Task { @MainActor in
                guard let self else { return }
                // The first snapshot as soon as the kept listing is read,
                // not a throttle's wait after it.
                if self.throttle.current?.status == .priming { self.throttle.rebuild() } else { self.throttle.trigger() }
            }
        }
    }

    /// The screen the model calls for now.
    func screen() -> Screen {
        guard model.primed || model.listing != nil else { return Screen(rows: [], status: .priming) }
        let listed = Set(model.listing?.sessions.map(\.id) ?? [])
        // A started session's row gives way to the session's own once listed.
        let starting = (newSessions?.starting.rows ?? []).filter { if case .started(let id) = $0.state { !listed.contains(id) } else { true } }
        guard let listing = model.listing else {
            return Screen(rows: [], status: model.failure.map { .failed($0.sentence + " Pull down to try again.") } ?? .loading, starting: starting)
        }
        let trusted = model.liveTrusted
        let rows = listing.sessions.map { session in
            formatter.row(session, holderName: session.hostedBy.flatMap { listing.deviceNames[$0] }, liveTrusted: trusted, now: model.now)
        }
        let unreadable = listing.unreadable > 0
            ? "\(listing.unreadable) session\(listing.unreadable == 1 ? "" : "s") can't be opened on this iPhone."
            : nil
        let stale = model.failure.map { "\($0.sentence) This is the list as last read; no session shows as live until it's read again." }
        return Screen(rows: rows, status: rows.isEmpty && unreadable == nil && starting.isEmpty ? .empty : .listed, starting: starting, unreadable: unreadable, stale: stale)
    }

    /// Applies `screen`: the first listing without animation (the kept
    /// one, R-IOS-1, else the first read), later ones animated in under
    /// 200 ms without holding up a touch.
    private func apply(_ screen: Screen, animated: Bool) {
        if screen.status == .priming { return }
        let previous = rows
        let previousStarts = startingRows
        rows = Dictionary(screen.rows.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        startingRows = Dictionary(screen.starting.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        // Set before the layout reads it: whether there is a footer.
        let lines = [screen.stale, screen.unreadable].compactMap { $0 }
        let footerChanged = lines != footerLines
        footerLines = lines

        var snapshot = NSDiffableDataSourceSnapshot<Int, String>()
        snapshot.appendSections([0])
        snapshot.appendItems(screen.starting.map(\.id) + screen.rows.map(\.id))
        snapshot.reconfigureItems(screen.rows.filter { row in previous[row.id].map { $0 != row } ?? false }.map(\.id))
        snapshot.reconfigureItems(screen.starting.filter { start in previousStarts[start.id].map { $0 != start } ?? false }.map(\.id))
        if animated {
            UIView.animate(withDuration: 0.15, delay: 0, options: [.allowUserInteraction, .beginFromCurrentState]) {
                self.dataSource.apply(snapshot, animatingDifferences: true)
            }
        } else {
            dataSource.apply(snapshot, animatingDifferences: false)
        }

        if footerChanged {
            let kind = UICollectionView.elementKindSectionFooter
            for path in collectionView.indexPathsForVisibleSupplementaryElements(ofKind: kind) {
                if let footer = collectionView.supplementaryView(forElementKind: kind, at: path) as? UICollectionViewListCell {
                    Self.configure(footer: footer, lines: lines)
                }
            }
            // Shown, gone or a line longer: the footer's height follows.
            collectionView.collectionViewLayout.invalidateLayout()
        }

        switch screen.status {
        case .priming:
            break
        case .loading:
            spinner.startAnimating()
            emptyLabel.isHidden = true
        case .empty:
            spinner.stopAnimating()
            emptyLabel.text = Self.emptySentence
            emptyLabel.isHidden = false
            Budgets.shared.listShown()
        case .failed(let sentence):
            spinner.stopAnimating()
            emptyLabel.text = sentence
            emptyLabel.isHidden = false
            // A launch whose first read failed lists on a later one: not
            // a launch to the cached list (ticket 128).
            Budgets.shared.launchAbandoned()
        case .listed:
            spinner.stopAnimating()
            emptyLabel.isHidden = true
            // The launch's budget ends at the first listing applied, the
            // kept one when the phone has one (tickets 128, 131); later
            // ones change nothing.
            Budgets.shared.listShown()
        }
    }

    /// The list read again, and every session still followed (a screen
    /// parked with commands out) woken: a connection lost is joined now.
    private func pulled() {
        LiveSessions.shared.wake()
        let model = model
        Task { [weak self] in
            await model.refresh()
            self?.collectionView.refreshControl?.endRefreshing()
        }
    }

    // MARK: Starting a session (ticket 134)

    /// The new-session flow, in a sheet; for Edit and retry, filled in
    /// from the refused row `retrying`, which its Start replaces.
    func newSession(retrying: StartingRow? = nil) {
        guard let newSessions, presentedViewController == nil else { return }
        let starting = newSessions.starting
        let flow = NewSessionFlow.make(source: newSessions.source, prefill: retrying?.request) { request in
            starting.start(request, replacing: retrying?.id)
        }
        present(flow, animated: true)
    }

    /// The host started `session`: its row gives way to the session's, and
    /// its transcript opens live, unless the person has gone elsewhere.
    private func started(_ session: String, _ request: StartRequest) {
        // Automatic sync takes it up at its first turn: read until it's
        // listed, for up to 10 s, rather than wait for the next timed read.
        let model = model
        let starting = newSessions?.starting
        Task {
            let deadline = Date.now.addingTimeInterval(10)
            repeat {
                await model.refresh()
                if model.listing?.sessions.contains(where: { $0.id == session }) == true { break }
                try? await Task.sleep(for: .milliseconds(500))
            } while Date.now < deadline
            starting?.listed(session)
        }
        guard let newSessions, let navigation = navigationController, navigation.topViewController === self, presentedViewController == nil else { return }
        navigation.pushViewController(newSessions.open(session, request), animated: true)
    }

    /// The refused start `id`'s flow opened again, filled in; Start sends
    /// it as a new command, under a new id.
    func editAndRetry(_ id: String) {
        guard let newSessions, presentedViewController == nil, let request = newSessions.starting.refused(id) else { return }
        newSession(retrying: StartingRow(request: request, state: .refused("")))
    }

    private func configure(_ cell: UICollectionViewListCell, starting start: StartingRow) {
        var content = UIListContentConfiguration.subtitleCell()
        let host = SafeText.clean(start.request.hostName)
        let place = NewSessionText.homeAsTilde(SafeText.clean(start.request.cwd))
        content.secondaryTextProperties.color = .secondaryLabel
        switch start.state {
        case .starting:
            content.text = "starting on \(host)…"
            content.secondaryText = place
            let spinner = UIActivityIndicatorView(style: .medium)
            spinner.startAnimating()
            cell.accessories = [.customView(configuration: .init(customView: spinner, placement: .trailing()))]
            cell.accessibilityLabel = "starting on \(host), \(place)"
            cell.accessibilityCustomActions = nil
        case .started:
            content.text = "started on \(host)"
            content.secondaryText = place
            cell.accessories = [.disclosureIndicator()]
            cell.accessibilityLabel = "started on \(host), \(place)"
            cell.accessibilityCustomActions = nil
        case .refused(let message):
            content.text = message
            content.textProperties.numberOfLines = 0
            content.secondaryText = "\(host) · \(place) · didn't start"
            let retry = UIButton(configuration: .borderless(), primaryAction: UIAction(title: "Edit and retry") { [weak self] _ in
                self?.editAndRetry(start.id)
            })
            retry.accessibilityIdentifier = "starting-retry"
            cell.accessories = [.customView(configuration: .init(customView: retry, placement: .trailing()))]
            cell.accessibilityLabel = "\(message), \(host), didn't start"
            cell.accessibilityCustomActions = [UIAccessibilityCustomAction(name: "Edit and retry") { [weak self] _ in
                self?.editAndRetry(start.id)
                return true
            }]
        }
        cell.contentConfiguration = content
        cell.accessibilityIdentifier = "starting-\(start.request.id)"
        cell.isAccessibilityElement = true
        cell.accessibilityTraits = .button
    }

    private static func configure(_ cell: UICollectionViewListCell, with row: SessionRow) {
        var content = UIListContentConfiguration.subtitleCell()
        content.text = row.title
        content.textProperties.numberOfLines = 1
        content.secondaryTextProperties.color = .secondaryLabel
        if row.live, let dot = row.detail.range(of: "●") {
            // Only the dot's colour: the font and the rest's colour are the
            // configuration's, so they follow Dynamic Type.
            let detail = NSMutableAttributedString(string: row.detail)
            detail.addAttribute(.foregroundColor, value: UIColor.systemGreen, range: NSRange(dot, in: row.detail))
            content.secondaryAttributedText = detail
        } else {
            content.secondaryText = row.detail
        }
        cell.contentConfiguration = content
        cell.accessories = [.disclosureIndicator()]
        cell.accessibilityCustomActions = nil
        cell.accessibilityIdentifier = "session-\(row.id)"
        // One element for VoiceOver: the title, the place, and live as a word.
        cell.isAccessibilityElement = true
        cell.accessibilityLabel = row.accessibilityLabel
        cell.accessibilityTraits = .button
    }

    private static func configure(footer: UICollectionViewListCell, lines: [String]) {
        var content = UIListContentConfiguration.footer()
        content.text = lines.joined(separator: "\n")
        content.textProperties.numberOfLines = 0
        footer.contentConfiguration = content
        footer.accessibilityIdentifier = "sessions-footer"
    }

    /// The footer's text, as applied.
    var footerText: String { footerLines.joined(separator: "\n") }
}

// MARK: Opened from a notification (ticket 106)

extension SessionListViewController: RouteNavigator {
    /// Back to this list and on to session `id`'s transcript, as a tap on
    /// its row would push it: the screen already open for it when there is
    /// one (on top, or under a sheet), never a second; one parked with
    /// commands out (ticket 90) is taken back as a tap would. Only a
    /// session in the person's listing opens: one that isn't there after a
    /// fresh read (another account's, deleted, sealed under a key this
    /// phone lacks) leaves the app as it is. Without animation: the person
    /// tapped the alert to be there.
    public func show(session id: String) async -> (any RoutedScreen)? {
        routes += 1
        let this = routes
        if let open = openScreen(of: id) { return reveal(open) }
        await model.settled()
        if row(id) == nil, model.listing != nil { await model.refresh() }
        // A later route came meanwhile, or this list is no longer the
        // window's (signed out, the scene gone): nothing opens.
        guard this == routes, let row = row(id), let navigation = navigationController, navigation.view.window != nil else { return nil }
        if let open = openScreen(of: id) { return reveal(open) }
        await Self.dismissPresented(over: navigation)
        guard this == routes, navigation.view.window != nil else { return nil }
        navigation.popToViewController(self, animated: false)
        let screen = opened(row)
        navigation.pushViewController(screen, animated: false)
        return screen as? any RoutedScreen
    }

    public var shownSession: ShownSession? {
        guard let screen = navigationController?.topViewController as? TranscriptViewController else { return nil }
        return ShownSession(id: screen.row.id, live: screen.isLive)
    }

    /// Whatever is presented over `navigation` taken down: once it has
    /// finished rising, if it still is (UIKit ignores a dismissal asked
    /// mid-transition), and given at most a second, so a dismissal UIKit
    /// drops never holds the route up.
    private static func dismissPresented(over navigation: UINavigationController) async {
        guard let presented = navigation.presentedViewController else { return }
        if presented.isBeingPresented || presented.isBeingDismissed, let moving = presented.transitionCoordinator {
            await RouteWait.once { done in moving.animate(alongsideTransition: nil) { _ in MainActor.assumeIsolated { done.fire() } } }
        }
        guard navigation.presentedViewController != nil else { return }
        await RouteWait.once { done in navigation.dismiss(animated: false) { MainActor.assumeIsolated { done.fire() } } }
    }

    /// The row the list shows for session `id`, by its id in any case.
    private func row(_ id: String) -> SessionRow? {
        screen().rows.first { $0.id.lowercased() == id }
    }

    /// Session `id`'s screen in this list's navigation stack, if one is.
    private func openScreen(of id: String) -> TranscriptViewController? {
        navigationController?.viewControllers.lazy.compactMap { $0 as? TranscriptViewController }.first { $0.row.id.lowercased() == id }
    }

    /// `screen`, already open, on top: what was pushed over it goes, and a
    /// sheet of its own (the approval sheet) stays.
    private func reveal(_ screen: TranscriptViewController) -> TranscriptViewController {
        if navigationController?.topViewController !== screen {
            navigationController?.popToViewController(screen, animated: false)
        }
        return screen
    }
}

/// One wait of a route's: over when its callback fires, or after a second,
/// whichever comes first.
@MainActor
private final class RouteWait {
    private var continuation: CheckedContinuation<Void, Never>?

    private init(_ continuation: CheckedContinuation<Void, Never>) {
        self.continuation = continuation
    }

    func fire() {
        continuation?.resume()
        continuation = nil
    }

    static func once(_ start: (RouteWait) -> Void) async {
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            let wait = RouteWait(continuation)
            start(wait)
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) { MainActor.assumeIsolated { wait.fire() } }
        }
    }
}
