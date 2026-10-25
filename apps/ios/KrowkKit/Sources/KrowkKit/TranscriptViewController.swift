import Observation
import UIKit

/// A session's transcript (ticket 88, R-IOS-10, R-IOS-1): its turns top to
/// bottom, scrolled to the end, under the bar that says how its live
/// connection stands. Your prompts are right-aligned bubbles, answers full
/// width, reasoning and tool calls one line each that a tap expands, and a
/// thin separator ends each turn.
///
/// UIKit: a compositional list with estimated heights, a diffable data
/// source keyed by row id (`TranscriptModel.ids`), self-sizing cells that
/// observe their own `ItemState` and draw text with TextKit 2. Everything
/// an agent wrote is hostile: cells draw it through `SafeText`, and a link
/// opens only from a sheet naming its whole URL.
///
/// The session is followed while the screen is in the navigation stack,
/// and stopped as it leaves. What it brings is applied once per display
/// frame (`FrameDriver`, R-LAG-7): a `CADisplayLink` at up to 120 Hz,
/// paused while the session is idle. A streaming answer grows in its own
/// cell, which appends just the new text to its TextKit 2 storage and
/// re-lays out alone; the snapshot changes only when a row is added. At
/// the bottom, the list follows what comes; scrolled up, it stays put and
/// a "↓ New" pill jumps back. The blocking reads behind it run on threads
/// of their own (`LiveSession`).
///
/// Cache first (R-IOS-1, R-IOS-3, ticket 131): with what the phone kept of
/// the session (`kept`), its newest page is drawn before anything the
/// session brings is applied — at once when it was prefetched, else as
/// soon as it is read off the main thread — and older pages as the person
/// scrolls up. Events the session then reads, catches up or logs live are
/// merged by id, each drawn once. The connection bar is the only progress
/// shown: no spinner or empty sentence while kept rows are on screen.
///
/// Under the transcript sits the composer (ticket 90): Send starts a turn,
/// Steer adds to the running one, Stop interrupts it. What's typed shows
/// at once as a bubble at the end (R-LAG-8), settled by the host's ack and
/// replaced by the item the host logs; with no host it's queued in the
/// core for the next one (R-HAND-4). The core owns the ids, the queue and
/// the resending; this screen only shows what it reports.
///
/// A tool call waiting for a yes, or a question the agent asks, rises in
/// the approval sheet over the transcript (ticket 91, R-PERM-2), one at a
/// time in arrival order; put away, it waits behind the hand in the
/// navigation bar. An answer shows at once on the call's row ("allowed —
/// sending…", R-LAG-8) and settles on the host's ack; one answered on
/// another device first goes with a toast saying so.
///
/// The navigation bar's subtitle is the session's model (ticket 135): a tap
/// opens the picker of its host's instances, and a pick shows `switching
/// to <model>…` at once (R-LAG-8) until the host logs the switch, or puts
/// the host's refusal under it and goes back (R-SWITCH-4). A turn that hit
/// its instance's limit raises a banner over the composer, whose Continue
/// switches to the instance the host offers and sends the turn's prompt
/// again, when this phone sent it.
///
/// The navigation bar's menu has Move to… (ticket 136, R-HAND-4): the
/// person's other online hosts and their directories. A pick asks that
/// host to take the session up; the bar reads `moving to <host>…` at once
/// (R-LAG-8) while the session goes on, then `moved to <host>`, or the
/// host's refusal shows under it and the session stays.
public final class TranscriptViewController: UIViewController, UICollectionViewDelegate, UIAdaptivePresentationControllerDelegate {
    public let row: SessionRow
    public let model: TranscriptModel
    public let statusBar = RelayStatusBar()
    /// The session followed live, if the app gave one.
    let session: (any LiveSession)?
    /// What the phone kept of the session, if the app gave it (ticket 131).
    public let kept: (any KeptTranscript)?
    /// The kept newest page is being read: what the session brings waits
    /// in its mailbox, so it is merged under the kept rows, not above
    /// them; and no spinner shows yet.
    public private(set) var readingKept = false
    /// Where the next older kept page ends, while there is one.
    private var olderKept: String?
    /// An older kept page is being read.
    private var pagingOlder = false
    /// Bumped when the session is read afresh (`restarted`): a kept page
    /// read before it is no longer the session's, and is dropped.
    private var keptGeneration = 0
    /// How long the session's updates wait for the kept page at most.
    static let keptWait: Duration = .milliseconds(500)
    /// Opens a URL the person confirmed: `UIApplication.shared.open`.
    private let opener: @MainActor (URL) -> Void

    public private(set) var collectionView: UICollectionView!
    public private(set) var dataSource: UICollectionViewDiffableDataSource<Int, String>!
    let spinner = UIActivityIndicatorView(style: .medium)
    let emptyLabel = UILabel()
    private var frames: CADisplayLink?
    /// Applies what the session brings, one batch per display frame.
    public let driver: FrameDriver?
    /// Snapshots applied to the list: only when rows are added or go, or
    /// rows are drawn again (Dynamic Type, an expanded row).
    public private(set) var snapshotApplies = 0
    /// The list follows what comes: it was at the bottom (within 8 pt)
    /// and the person hasn't scrolled away since.
    public private(set) var following = true
    /// "↓ New": shown when something came while scrolled up; a tap jumps
    /// back to the bottom, and following again.
    public let newPill = UIButton(configuration: .filled())
    /// The rows when the last frame applied: whether a frame added any.
    private var rowsSeen = 0
    /// The list is being scrolled by this screen (pinning, the first load,
    /// "↓ New"), not by the person.
    private var scrollingItself = false
    /// What the list had laid out when it was last pinned: it's pinned
    /// again only when that changes, so a scroll alone is left be.
    private var pinnedFor: (content: CGSize, bounds: CGSize)?
    /// The content's height when the person's drag began: what streamed in
    /// meanwhile doesn't count against reaching the end.
    private var dragFrom: CGFloat?
    /// An assistive scroll down, animated: decided once it lands.
    private var assistiveDown = false
    /// VoiceOver's focus observer, removed as the screen goes.
    private var focusObserver: (any NSObjectProtocol)?
    /// Rows have been drawn: no spinner shows again.
    public private(set) var drewRows = false
    /// The rows as last applied.
    private var applied: [String] = []
    /// The session stopped before it was read: why, as the bar says it.
    private var stoppedEarly = false

    /// The composer, when the screen follows a session live.
    public let composer = ComposerView()
    /// What the buttons last showed.
    public private(set) var composerState = ComposerState(text: "", running: false, stopping: false, status: .connecting)
    /// The host's last refusal of a stop, until the next stop or turn.
    private var stopRefused: String?
    /// Where the screen goes when it's left with commands still out.
    public weak var parking: ParkedScreens?
    /// Left with commands out: kept by `parking`, its session still
    /// followed, what comes applied as it comes (no display link).
    public private(set) var parked = false

    /// The requests waiting for an answer from this phone (ticket 91), when
    /// the app gave a queue.
    public let approvals: ApprovalQueue?
    /// The approval sheet, while it's up.
    public private(set) var approvalSheet: ApprovalSheetController?
    /// The person put the sheet away: it rises again for a new request,
    /// or from the hand in the navigation bar.
    public private(set) var approvalsPutAway = false
    /// A line over the transcript for a moment: an approval answered on
    /// another device, or one the host no longer waits on.
    public let toast = UILabel()
    private var toastHide: DispatchWorkItem?
    /// A try again at raising the sheet, while another sheet is up.
    private var approvalsRetry: DispatchWorkItem?
    /// The app's lifecycle, which this screen's session hears (ticket 86).
    private let lifecycle: LiveSessions
    /// A notification's route that brought this screen up, until the
    /// connection says what it lands on (ticket 106).
    public private(set) var landing: NotificationRoute?
    /// A line under the bar that stays until it no longer holds: nothing
    /// waits here any more, or the host is away (ticket 106). Tapped, it
    /// goes.
    public let notice = UILabel()
    /// What the notice says, while it's up.
    public private(set) var noticeShown: RouteLanding?
    /// The route came with the session already live: its welcome was
    /// before the tap, and a request since may still be on its way (read
    /// as the app comes back), so nothing waiting is said only after
    /// `landingGrace`, unless the connection joins again first.
    private var landedLive = false
    private var graceOver = false
    /// The route landed on the sheet, and nothing has been answered here
    /// since: should what it showed be settled elsewhere (the turn's
    /// catch-up replaying a request and its answer, say), the notice says
    /// so rather than a passing toast.
    private var routedSheet = false
    private var landingWait: DispatchWorkItem?
    static let landingGrace: TimeInterval = 2

    /// The person's hosts (ticket 135): the model picker lists the
    /// session's host's instances from them.
    private let hosts: (any HostSource)?
    /// The subtitle: the session's model, or `switching to <model>…`. A
    /// tap opens the picker.
    public let modelButton = UIButton(configuration: .plain())
    /// The host's refusal of a switch, under the subtitle until the next
    /// switch or a tap.
    public let switchNote = UILabel()
    /// The rate-limit offer over the composer: its line, and Continue
    /// when this phone sent the limited turn's prompt.
    public let offerBanner = UIView()
    public let offerLabel = UILabel()
    public let offerContinue = UIButton(configuration: .filled())
    private let offerClose = UIButton(configuration: .plain())
    /// The model picker, while it's up.
    public private(set) var modelPicker: UITableViewController?
    /// The tests' seam: the instances the picker shows, from the host's
    /// (an instance the host lacks, to see the host refuse it).
    public var pickerInstances: (@MainActor ([any AnnouncedInstance]) -> [any AnnouncedInstance])?

    /// The session's move to another host (ticket 136), when the app gave
    /// the hosts and a live session.
    public let mover: SessionMove?
    /// The navigation bar's menu: Move to….
    public private(set) var menuItem: UIBarButtonItem?
    /// The host's refusal of a move, under the bar until the next move or
    /// a tap.
    public let moveNote = UILabel()
    /// The Move to… sheet, while it's up.
    public private(set) var movePicker: UITableViewController?
    /// What VoiceOver was last told of the move: each line once.
    private var moveSaid: String?

    public init(
        row: SessionRow, session: (any LiveSession)? = nil, model: TranscriptModel? = nil, approvals: ApprovalQueue? = nil,
        kept: (any KeptTranscript)? = nil, hosts: (any HostSource)? = nil, open: (@MainActor (URL) -> Void)? = nil, lifecycle: LiveSessions = .shared
    ) {
        self.row = row
        self.kept = kept
        self.lifecycle = lifecycle
        self.hosts = hosts
        self.session = session
        self.mover = hosts.flatMap { session == nil ? nil : SessionMove(source: $0, holder: row.hostDevice) }
        self.approvals = session == nil ? nil : approvals
        let model = model ?? TranscriptModel(sessionID: row.id)
        self.model = model
        self.driver = session.map { FrameDriver(session: $0, model: model) }
        self.opener = open ?? { UIApplication.shared.open($0) }
        super.init(nibName: nil, bundle: nil)
        title = row.title
        navigationItem.largeTitleDisplayMode = .never
        // From now, before the view loads: the scene's foreground reaches
        // the connection before any view appears (R-IOS-2).
        if let session {
            lifecycle.add(session) { [weak self] in
                // Loaded only: before its view, nothing is drawn to apply to.
                guard let self, self.isViewLoaded else { return }
                self.frame()
            }
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("TranscriptViewController is built in code")
    }

    /// Freed with its link still on the run loop (paused, so it never
    /// fires to find its screen gone): invalidated here.
    isolated deinit {
        frames?.invalidate()
        if let focusObserver { NotificationCenter.default.removeObserver(focusObserver) }
        session?.stop()
    }

    override public func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemBackground

        let layout = UICollectionViewCompositionalLayout { _, environment in
            var list = UICollectionLayoutListConfiguration(appearance: .plain)
            list.showsSeparators = false
            list.backgroundColor = .clear
            return NSCollectionLayoutSection.list(using: list, layoutEnvironment: environment)
        }
        let list = TranscriptCollectionView(frame: .zero, collectionViewLayout: layout)
        list.afterLayout = { [weak self] in self?.pin() }
        list.accessibilityScrolled = { [weak self] in self?.personScrolled($0) }
        collectionView = list
        collectionView.translatesAutoresizingMaskIntoConstraints = false
        collectionView.selfSizingInvalidation = .enabledIncludingConstraints
        collectionView.accessibilityIdentifier = "transcript"
        collectionView.delegate = self
        collectionView.alwaysBounceVertical = true
        collectionView.backgroundColor = .systemBackground
        view.addSubview(collectionView)

        let open: @MainActor (URL) -> Void = { [weak self] in self?.confirm($0) }
        let text = UICollectionView.CellRegistration<TranscriptTextCell, String> { [weak self] cell, _, id in
            guard let state = self?.model.item(id) else { return }
            #if DEBUG
            let configuring = FrameCosts.start()
            defer { FrameCosts.end("configure text", configuring) }
            #endif
            cell.openLink = open
            cell.show(state)
        }
        let tool = UICollectionView.CellRegistration<TranscriptToolCell, String> { [weak self] cell, _, id in
            guard let state = self?.model.item(id) else { return }
            #if DEBUG
            let configuring = FrameCosts.start()
            defer { FrameCosts.end("configure tool", configuring) }
            #endif
            cell.openLink = open
            cell.toggle = { [weak self] in self?.toggle(id) }
            cell.show(state)
        }
        let mark = UICollectionView.CellRegistration<TranscriptMarkCell, String> { [weak self] cell, _, id in
            guard let state = self?.model.item(id) else { return }
            cell.show(state)
        }
        let pending = UICollectionView.CellRegistration<TranscriptPendingCell, String> { [weak self] cell, _, id in
            guard let self, let state = model.item(id) else { return }
            cell.takeBack = { [weak self] in self?.takeBack(id) }
            cell.show(state)
        }
        dataSource = UICollectionViewDiffableDataSource<Int, String>(collectionView: collectionView) { [weak self] view, path, id in
            switch self?.model.item(id)?.kind {
            case .user?, .assistant?: view.dequeueConfiguredReusableCell(using: text, for: path, item: id)
            case .reasoning?, .tool?: view.dequeueConfiguredReusableCell(using: tool, for: path, item: id)
            case .pending?: view.dequeueConfiguredReusableCell(using: pending, for: path, item: id)
            case .mark?, nil: view.dequeueConfiguredReusableCell(using: mark, for: path, item: id)
            }
        }

        // The empty body: the title above, and an activity indicator until
        // the session is read. Once a row is drawn, it never shows again
        // (R-IOS-1).
        spinner.accessibilityIdentifier = "transcript-loading"
        spinner.accessibilityLabel = "Reading the session"
        spinner.hidesWhenStopped = true
        emptyLabel.font = .preferredFont(forTextStyle: .body)
        emptyLabel.adjustsFontForContentSizeCategory = true
        emptyLabel.textColor = .secondaryLabel
        emptyLabel.textAlignment = .center
        emptyLabel.numberOfLines = 0
        emptyLabel.isHidden = true
        emptyLabel.accessibilityIdentifier = "transcript-empty"
        let background = UIView()
        for v in [spinner, emptyLabel] as [UIView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            background.addSubview(v)
        }
        NSLayoutConstraint.activate([
            spinner.centerXAnchor.constraint(equalTo: background.centerXAnchor),
            spinner.centerYAnchor.constraint(equalTo: background.centerYAnchor),
            emptyLabel.leadingAnchor.constraint(equalTo: background.layoutMarginsGuide.leadingAnchor, constant: 16),
            emptyLabel.trailingAnchor.constraint(equalTo: background.layoutMarginsGuide.trailingAnchor, constant: -16),
            emptyLabel.centerYAnchor.constraint(equalTo: background.centerYAnchor),
        ])
        collectionView.backgroundView = background

        let safe = view.safeAreaLayoutGuide
        let top: NSLayoutYAxisAnchor
        if session != nil {
            // A strip under the navigation bar, above the transcript: what's
            // drawn stays as it was whatever the connection does.
            statusBar.translatesAutoresizingMaskIntoConstraints = false
            view.addSubview(statusBar)
            NSLayoutConstraint.activate([
                statusBar.topAnchor.constraint(equalTo: safe.topAnchor),
                statusBar.leadingAnchor.constraint(equalTo: view.leadingAnchor),
                statusBar.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            ])
            top = statusBar.bottomAnchor
        } else {
            top = safe.topAnchor
        }
        // The composer under the list, on the keyboard when it's up.
        let bottom: NSLayoutYAxisAnchor
        if session != nil {
            composer.translatesAutoresizingMaskIntoConstraints = false
            let under = UIView()
            under.backgroundColor = composer.backgroundColor
            under.translatesAutoresizingMaskIntoConstraints = false
            view.addSubview(under)
            view.addSubview(composer)
            NSLayoutConstraint.activate([
                composer.leadingAnchor.constraint(equalTo: view.leadingAnchor),
                composer.trailingAnchor.constraint(equalTo: view.trailingAnchor),
                composer.bottomAnchor.constraint(equalTo: view.keyboardLayoutGuide.topAnchor),
                under.topAnchor.constraint(equalTo: composer.bottomAnchor),
                under.leadingAnchor.constraint(equalTo: view.leadingAnchor),
                under.trailingAnchor.constraint(equalTo: view.trailingAnchor),
                under.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            ])
            bottom = composer.topAnchor
            composer.changed = { [weak self] in self?.updateComposer() }
            composer.send = { [weak self] in self?.sendTyped() ?? false }
            composer.stop = { [weak self] in self?.stopTurn() }
        } else {
            bottom = view.bottomAnchor
        }
        NSLayoutConstraint.activate([
            collectionView.topAnchor.constraint(equalTo: top),
            collectionView.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            collectionView.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            collectionView.bottomAnchor.constraint(equalTo: bottom),
        ])

        var pill = UIButton.Configuration.filled()
        pill.title = "↓ New"
        pill.cornerStyle = .capsule
        pill.buttonSize = .small
        newPill.configuration = pill
        newPill.accessibilityIdentifier = "transcript-new"
        newPill.accessibilityLabel = "New below"
        newPill.accessibilityHint = "Scrolls to the end, and follows it again"
        newPill.isHidden = true
        newPill.translatesAutoresizingMaskIntoConstraints = false
        newPill.addAction(UIAction { [weak self] _ in self?.jumpToEnd() }, for: .primaryActionTriggered)
        view.addSubview(newPill)
        NSLayoutConstraint.activate([
            newPill.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            newPill.bottomAnchor.constraint(equalTo: session != nil ? composer.topAnchor : safe.bottomAnchor, constant: -12),
        ])

        // Dynamic Type: the drawn text's fonts are fixed in its attributes,
        // so every row is drawn again at the new size.
        registerForTraitChanges([UITraitPreferredContentSizeCategory.self, UITraitLegibilityWeight.self]) { (self: Self, _: UITraitCollection) in
            // The model's rows first: none that's gone is drawn again.
            self.apply(rows: self.model.ids, attached: self.model.attached)
            var snapshot = self.dataSource.snapshot()
            snapshot.reconfigureItems(snapshot.itemIdentifiers.filter { self.model.item($0) != nil })
            self.dataSource.apply(snapshot, animatingDifferences: false)
            self.snapshotApplies += 1
        }

        setUpApprovals()
        setUpModel()
        setUpMove()
        if noticeShown != nil { addNotice() }
        observeFocus()
        drawKept()
        observeModel()
        driver?.applied = { [weak self] done in self?.drawFrame(done) }
        session?.onPath { [weak self] path in self?.statusBar.show(path: path) }
        session?.follow { [weak self] status in self?.relay(status) }
        session?.onUpdates { [weak self] in self?.woken() }
        updateComposer()
    }

    /// VoiceOver moving to a row above the last is reading back: not
    /// following, or the end would pull the row away every frame.
    private func observeFocus() {
        guard focusObserver == nil else { return }
        focusObserver = NotificationCenter.default.addObserver(forName: UIAccessibility.elementFocusedNotification, object: nil, queue: .main) { [weak self] note in
            // Delivered on the main queue, where it was posted: the element
            // never leaves it.
            nonisolated(unsafe) let element = note.userInfo?[UIAccessibility.focusedElementUserInfoKey] as AnyObject?
            MainActor.assumeIsolated { self?.focused(element) }
        }
    }

    /// Updates came with none waiting: the display link runs again, or,
    /// parked or in the background (where the link doesn't fire), they're
    /// applied now, so none pile up in the mailbox until iOS suspends the
    /// app (ticket 86). Each post after a drain wakes once: no busy loop.
    private func woken() {
        if parked || lifecycle.inBackground {
            frame()
        } else {
            driver?.wake()
        }
    }

    override public func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        // The foreground's budget follows the screen the app came back to,
        // and this one's bar from here (ticket 128).
        if session != nil { Budgets.shared.sessionShown(statusBar.status) }
        left = false
        unpark()
        observeFocus()
        guard let driver, frames == nil else { return }
        let link = CADisplayLink(target: TranscriptFrameTarget(self), selector: #selector(TranscriptFrameTarget.frame(_:)))
        // ProMotion: up to 120 Hz, never under 30 (R-PERF-4's 60 fps on
        // other iPhones, which can't go above it).
        link.preferredFrameRateRange = CAFrameRateRange(minimum: 30, maximum: 120, preferred: 120)
        link.add(to: .main, forMode: .common)
        frames = link
        driver.ticker = DisplayLinkTicker(link)
    }

    /// Covered, the screen keeps draining what the session brings, so
    /// nothing piles up; leaving, it stops the session and the drain.
    override public func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        syncApprovals()
    }

    override public func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        if isMovingFromParent || isBeingDismissed || navigationController?.isBeingDismissed == true {
            putSheetDown()
        }
    }

    override public func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        if isMovingFromParent || isBeingDismissed || navigationController?.isBeingDismissed == true {
            leave()
        }
    }

    /// Taken out of its navigation stack however it went (popped from
    /// beneath another screen, the stack replaced): its link, paused or
    /// not, never fires again to invalidate itself, so it's done here.
    override public func didMove(toParent parent: UIViewController?) {
        super.didMove(toParent: parent)
        if parent == nil, frames != nil { leave() }
    }

    private func leave() {
        // Once per time on screen: UIKit may say it left more than one way.
        guard !left else { return }
        left = true
        Budgets.shared.sessionClosed()
        if let focusObserver { NotificationCenter.default.removeObserver(focusObserver) }
        focusObserver = nil
        frames?.invalidate()
        frames = nil
        driver?.ticker = nil
        putSheetDown()
        // A move waited on goes on on the host; this screen stops waiting.
        mover?.stopWaiting()
        // A route lands on a screen in view, never on one parked, and
        // what it said goes with it.
        if noticeShown == .nothingWaiting { hideNotice() }
        landing = nil
        landingWait?.cancel()
        landingWait = nil
        routedSheet = false
        // Commands the core still holds go on: a queued prompt survives
        // leaving the screen (R-HAND-4), until the host answers it.
        if model.commandsOut > 0, let parking, !isEnded {
            parked = true
            parking.park(self)
            // What waits in the mailbox now: a later post wakes this only
            // once it has been drained.
            frame()
            return
        }
        parking?.keep(model.refusals, of: row.id)
        stopSession()
    }

    /// Shown again (`ParkedScreens.take`, or pushed back): followed by its
    /// display link from here, not let go.
    func unpark() {
        guard parked else { return }
        parked = false
        grace?.cancel()
        grace = nil
        parking?.release(self)
    }

    /// Parked, and nothing it holds needs the connection any more: every
    /// command answered and its logged item here (or the connection
    /// stopped): let go. One whose logged item never comes (the host logged
    /// other text) is let go `parkedGrace` after the last answer.
    private func settleParked() {
        guard parked else { return }
        if isEnded {
            // What's still out is refused (kept for the next screen).
            model.connectionEnded()
            return letGo()
        }
        if model.commandsOut == 0 && !model.awaitingLog { return letGo() }
        guard model.commandsOut == 0, grace == nil else { return }
        let grace = DispatchWorkItem { [weak self] in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.grace = nil
                if self.parked, self.model.commandsOut == 0 { self.letGo() }
            }
        }
        self.grace = grace
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.parkedGrace, execute: grace)
    }

    /// How long a parked screen waits for the logged items of commands the
    /// host took.
    static let parkedGrace: TimeInterval = 30
    /// The wait for them, while parked.
    private var grace: DispatchWorkItem?
    /// Left (stopped or parked) since it last appeared.
    private var left = false

    /// Stopped and forgotten by its parking; its refused bubbles kept for
    /// the session's next screen.
    func letGo() {
        guard parked else { return }
        parked = false
        grace?.cancel()
        grace = nil
        parking?.release(self)
        parking?.keep(model.refusals, of: row.id)
        stopSession()
    }

    /// The session stopped, and no longer told of the app's lifecycle.
    private func stopSession() {
        guard let session else { return }
        lifecycle.remove(session)
        session.stop()
    }

    /// Refused bubbles an earlier screen of this session left
    /// (`ParkedScreens.takeRefusals`): shown again, with "Edit and resend".
    public func restore(_ refusals: [PendingCommands.Entry]) {
        model.restore(refusals)
    }

    private var isEnded: Bool {
        if case .ended = statusBar.status { true } else { false }
    }

    /// One display frame: what the session brought, into the model; held
    /// while the kept newest page is read.
    func frame(at now: TimeInterval = CACurrentMediaTime()) {
        guard !readingKept else { return }
        driver?.tick(at: now)
    }

    // MARK: What the phone kept (ticket 131)

    /// The kept newest page: drawn now when it is in memory, else read
    /// off the main thread with the session's updates held for it (at
    /// most `keptWait`: one that comes later is merged as history). One in
    /// memory is read again all the same, in case the cache took more
    /// since it was read ahead: what that adds is merged.
    private func drawKept() {
        guard let kept else { return }
        let generation = keptGeneration
        if let page = kept.ready() {
            draw(page)
            Task { [weak self] in
                guard let page = await kept.newest(), let self, generation == keptGeneration else { return }
                draw(page, older: false)
            }
            return
        }
        readingKept = true
        Task { [weak self] in
            let page = await kept.newest()
            guard let self else { return }
            keptRead(generation == keptGeneration ? page : nil)
        }
        Task { [weak self] in
            try? await Task.sleep(for: Self.keptWait)
            self?.keptRead(nil, late: true)
        }
    }

    private func keptRead(_ page: KeptPage?, late: Bool = false) {
        if !late, let page { draw(page) }
        guard readingKept else { return }
        readingKept = false
        updateBody(attached: model.attached)
        // What the session brought meanwhile, under the kept rows.
        frame()
        driver?.wake()
    }

    private func draw(_ page: KeptPage, older: Bool = true) {
        if older { olderKept = page.older }
        model.drawKept(page.events)
        // The rows now, not on the observation's next turn.
        if isViewLoaded, dataSource != nil { apply(rows: model.ids, attached: model.attached) }
        pageOlderIfNeeded()
    }

    /// Within a screen of the top, an older kept page is read and drawn
    /// above (R-IOS-3); the rows on screen stay where they are.
    private func pageOlderIfNeeded() {
        guard let kept, let older = olderKept, !pagingOlder, let collectionView, drewRows else { return }
        // Only as the person scrolls (not the list settling at the end as
        // it opens), or while what is laid out doesn't fill the screen.
        let scrolling = collectionView.isTracking || collectionView.isDragging || collectionView.isDecelerating
        let content = collectionView.contentSize.height
        guard scrolling || (content > 0 && content <= collectionView.bounds.height) else { return }
        let fromTop = collectionView.contentOffset.y + collectionView.adjustedContentInset.top
        guard fromTop < collectionView.bounds.height else { return }
        pagingOlder = true
        let generation = keptGeneration
        Task { [weak self] in
            let page = await kept.page(before: older)
            guard let self, generation == keptGeneration else { return }
            pagingOlder = false
            guard let page else {
                olderKept = nil
                return
            }
            draw(page)
        }
    }

    /// Inside the frame, after its apply: the streaming cells on screen
    /// append their new text now, so the frame's layout has it; the pill
    /// shows if something came while scrolled up; VoiceOver hears a turn
    /// end (never each word).
    private func drawFrame(_ done: FrameApplied) {
        // The session was read afresh: what the phone kept is not it, and
        // a read of it still out is dropped.
        if done.restarted {
            olderKept = nil
            pagingOlder = false
            keptGeneration += 1
        }
        // A collapsed "Thought" draws none of its text.
        let drawn = done.grown.filter { $0.kind != .reasoning || $0.expanded }
        for state in drawn {
            state.drawer?.render(state)
        }
        let rows = model.ids.count
        if !following, !drawn.isEmpty || rows != rowsSeen {
            newPill.isHidden = false
        }
        rowsSeen = rows
        if UIAccessibility.isVoiceOverRunning, let end = done.ended.last {
            UIAccessibility.post(notification: .announcement, argument: SafeText.line(end.text[...], limit: 200))
        }
        if UIAccessibility.isVoiceOverRunning, let refused = done.refused.last {
            UIAccessibility.post(notification: .announcement, argument: "Your message wasn't taken: \(SafeText.line(refused.caption[...], limit: 200))")
        }
        if let why = done.stopRefused { stopRefused = why }
        if !done.ended.isEmpty { stopRefused = nil }
        for answer in done.approvalAnswers { approvals?.acked(answer.token, error: answer.error) }
        // The offer's Continue: its switch taken, the prompt goes again.
        if let again = done.promptAgain { sendBubble(.prompt(again)) }
        if UIAccessibility.isVoiceOverRunning, let why = done.switchRefused {
            UIAccessibility.post(notification: .announcement, argument: "The model wasn't switched: \(SafeText.line(why[...], limit: 200))")
        }
        updateModel()
        // A sheet that couldn't rise (another sheet was up) tries again.
        if approvalSheet == nil, approvals?.current != nil { syncApprovals() }
        updateComposer()
        settleParked()
    }

    // MARK: The composer (ticket 90)

    /// The composer's buttons and note, from the text, the turn, the stop
    /// and the bar.
    func updateComposer() {
        guard session != nil else { return }
        let state = ComposerState(text: composer.text, running: model.isTurnRunning, stopping: model.stopping, status: statusBar.status, stopRefused: stopRefused)
        composerState = state
        composer.show(state)
    }

    /// Send (or Steer, while a turn runs), the button or ⌘↵: the bubble
    /// shows now, in this run-loop turn, before the command crosses into
    /// the core off the main thread (R-LAG-8). The text field empties and
    /// keeps its focus; nothing waits on the send. Whether a bubble showed
    /// (the tap's budget, ticket 128).
    @discardableResult
    public func sendTyped() -> Bool {
        updateComposer()
        guard session != nil, composerState.canSend else { return false }
        let text = composer.text
        let command: LiveCommand = composerState.steers ? .steer(text) : .prompt(text)
        composer.text = ""
        sendBubble(command)
        if UIAccessibility.isVoiceOverRunning, statusBar.status == .hostOffline {
            UIAccessibility.post(notification: .announcement, argument: "Queued: it sends when your laptop is back")
        }
        return true
    }

    /// A prompt or steer shown as a bubble now and handed to the core.
    private func sendBubble(_ command: LiveCommand) {
        guard let session else { return }
        let token = model.issue(command)
        // The bubble's row now, not on the observation's next turn.
        apply(rows: model.ids, attached: model.attached)
        follow(true)
        pinnedFor = nil
        scrollToEnd()
        session.send(command, token: token)
        updateComposer()
        // A prompt answers an offer with a no.
        updateModel()
    }

    /// Stop, the button or ⌘.: "stopping…" now, the interrupt to the core;
    /// it stays until the interrupt's ack or the turn's end.
    public func stopTurn() {
        updateComposer()
        guard let session, composerState.canStop else { return }
        stopRefused = nil
        let token = model.issue(.interrupt)
        session.send(.interrupt, token: token)
        updateComposer()
        if UIAccessibility.isVoiceOverRunning {
            UIAccessibility.post(notification: .announcement, argument: "stopping")
        }
    }

    /// "Edit and resend" on a refused bubble: it goes, and its text is back
    /// in the composer (after what's there), to send as a new command.
    func takeBack(_ id: String) {
        guard let token = model.token(ofBubble: id), let text = model.takeBack(token) else { return }
        let typed = composer.text
        composer.text = typed.isEmpty ? text : typed + "\n" + text
        apply(rows: model.ids, attached: model.attached)
        composer.textView.becomeFirstResponder()
        updateComposer()
    }

    /// ⌘↵ sends, ⌘. stops: a hardware keyboard's (Return alone is a new
    /// line).
    override public var keyCommands: [UIKeyCommand]? {
        guard session != nil else { return super.keyCommands }
        let send = UIKeyCommand(title: "Send", action: #selector(keySend), input: "\r", modifierFlags: .command)
        send.wantsPriorityOverSystemBehavior = true
        let stop = UIKeyCommand(title: "Stop", action: #selector(keyStop), input: ".", modifierFlags: .command)
        stop.wantsPriorityOverSystemBehavior = true
        return [send, stop]
    }

    @objc private func keySend() { sendTyped() }
    @objc private func keyStop() { stopTurn() }

    // MARK: The model (ticket 135)

    private func setUpModel() {
        guard session != nil else { return }
        var button = UIButton.Configuration.plain()
        button.contentInsets = .zero
        button.titleLineBreakMode = .byTruncatingMiddle
        button.titleTextAttributesTransformer = UIConfigurationTextAttributesTransformer {
            var out = $0
            out.font = .preferredFont(forTextStyle: .footnote)
            return out
        }
        modelButton.configuration = button
        modelButton.accessibilityIdentifier = "session-model"
        modelButton.accessibilityHint = "Shows the models this session can switch to"
        modelButton.addAction(UIAction { [weak self] _ in self?.showModelPicker() }, for: .primaryActionTriggered)
        navigationItem.subtitleView = modelButton

        switchNote.font = .preferredFont(forTextStyle: .footnote)
        switchNote.adjustsFontForContentSizeCategory = true
        switchNote.numberOfLines = 0
        switchNote.textAlignment = .center
        switchNote.textColor = .label
        switchNote.backgroundColor = .secondarySystemBackground
        switchNote.layer.cornerRadius = 10
        switchNote.layer.masksToBounds = true
        switchNote.isHidden = true
        switchNote.accessibilityIdentifier = "switch-refusal"
        switchNote.accessibilityHint = "Hides this line"
        switchNote.isUserInteractionEnabled = true
        switchNote.addGestureRecognizer(UITapGestureRecognizer(target: self, action: #selector(switchNoteTapped)))
        switchNote.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(switchNote)

        offerBanner.backgroundColor = .secondarySystemBackground
        offerBanner.layer.cornerRadius = 12
        offerBanner.layer.borderColor = UIColor.separator.cgColor
        offerBanner.layer.borderWidth = 0.5
        offerBanner.isHidden = true
        offerBanner.accessibilityIdentifier = "switch-offer"
        offerLabel.font = .preferredFont(forTextStyle: .footnote)
        offerLabel.adjustsFontForContentSizeCategory = true
        offerLabel.numberOfLines = 2
        offerLabel.accessibilityIdentifier = "switch-offer-line"
        var go = UIButton.Configuration.filled()
        go.title = "Continue"
        go.cornerStyle = .capsule
        go.buttonSize = .small
        offerContinue.configuration = go
        offerContinue.accessibilityIdentifier = "switch-offer-continue"
        offerContinue.addAction(UIAction { [weak self] _ in self?.continueOffer() }, for: .primaryActionTriggered)
        var close = UIButton.Configuration.plain()
        close.image = UIImage(systemName: "xmark")
        close.buttonSize = .small
        offerClose.configuration = close
        offerClose.accessibilityLabel = "Dismiss"
        offerClose.accessibilityIdentifier = "switch-offer-dismiss"
        offerClose.addAction(UIAction { [weak self] _ in
            self?.model.takeOffer()
            self?.updateModel()
        }, for: .primaryActionTriggered)
        let row = UIStackView(arrangedSubviews: [offerLabel, offerContinue, offerClose])
        row.axis = .horizontal
        row.spacing = 8
        row.alignment = .center
        row.translatesAutoresizingMaskIntoConstraints = false
        offerLabel.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        offerBanner.addSubview(row)
        offerBanner.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(offerBanner)
        NSLayoutConstraint.activate([
            switchNote.topAnchor.constraint(equalTo: statusBar.bottomAnchor, constant: 8),
            switchNote.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            switchNote.widthAnchor.constraint(lessThanOrEqualTo: view.layoutMarginsGuide.widthAnchor),
            row.topAnchor.constraint(equalTo: offerBanner.topAnchor, constant: 8),
            row.bottomAnchor.constraint(equalTo: offerBanner.bottomAnchor, constant: -8),
            row.leadingAnchor.constraint(equalTo: offerBanner.leadingAnchor, constant: 12),
            row.trailingAnchor.constraint(equalTo: offerBanner.trailingAnchor, constant: -4),
            offerBanner.leadingAnchor.constraint(equalTo: view.layoutMarginsGuide.leadingAnchor),
            offerBanner.trailingAnchor.constraint(equalTo: view.layoutMarginsGuide.trailingAnchor),
            offerBanner.bottomAnchor.constraint(equalTo: composer.topAnchor, constant: -8),
        ])
        updateModel()
    }

    /// The subtitle, the refusal under it, the offer's banner and the
    /// picker, as the model and the link stand.
    func updateModel() {
        guard session != nil, isViewLoaded else { return }
        let line = model.modelLine
        if modelButton.configuration?.title != line {
            modelButton.configuration?.title = line
            modelButton.accessibilityLabel = line.isEmpty ? "Model" : "Model: \(line)"
        }
        modelButton.isHidden = line.isEmpty
        let refusal = model.switchRefusal.map { " \(SafeText.line($0[...], limit: 300)) " }
        if switchNote.text != refusal { switchNote.text = refusal }
        switchNote.isHidden = refusal == nil
        if refusal != nil { view.bringSubviewToFront(switchNote) }
        if let offer = model.offer {
            offerLabel.text = offer.line
            offerContinue.isHidden = offer.again == nil
            offerContinue.isEnabled = isLive && model.switching == nil
            offerBanner.isHidden = false
            view.bringSubviewToFront(offerBanner)
        } else {
            offerBanner.isHidden = true
        }
        (modelPicker as? ModelPickerViewController)?.session = pickerSession
    }

    private var pickerSession: ModelPickerViewController.Session {
        .init(current: model.currentModel, live: isLive, switching: model.switching, hostOffline: statusBar.status == .hostOffline, limits: model.limits)
    }

    /// The subtitle tapped: the picker of the host's instances, in a sheet.
    public func showModelPicker() {
        guard let hosts, session != nil, modelPicker == nil || modelPicker?.presentingViewController == nil else { return }
        let picker = ModelPickerViewController(
            hosts: HostListModel(source: hosts), device: mover?.holder ?? row.hostDevice, session: pickerSession, adjust: pickerInstances
        ) { [weak self] pick in
            self?.switchModel(to: pick)
        }
        modelPicker = picker
        let sheet = UINavigationController(rootViewController: picker)
        sheet.sheetPresentationController?.detents = [.medium(), .large()]
        present(sheet, animated: true)
    }

    /// A model picked (R-LAG-8): `switching to <model>…` now, the generated
    /// `switchModel` to the core off the main thread, as the composer's
    /// commands go. Only over a live link: never queued for a later host.
    /// Whether it went.
    @discardableResult
    public func switchModel(to choice: ModelChoice) -> Bool {
        guard let session, isLive, model.switching == nil, choice != model.currentModel else { return false }
        let token = model.issue(.switchModel(choice))
        session.send(.switchModel(choice), token: token)
        updateModel()
        if UIAccessibility.isVoiceOverRunning {
            UIAccessibility.post(notification: .announcement, argument: "switching to \(choice.shown)")
        }
        return true
    }

    /// The offer's Continue: the switch to the instance offered, and, once
    /// the host takes it, the limited turn's prompt again as a new prompt
    /// with no model (as the terminal's `/switch` does).
    public func continueOffer() {
        guard let session, isLive, model.switching == nil, let offer = model.offer, let again = offer.again else { return }
        model.takeOffer()
        let token = model.issue(.switchModel(offer.info.to))
        model.prompt(again, afterSwitch: token)
        session.send(.switchModel(offer.info.to), token: token)
        updateModel()
    }

    @objc private func switchNoteTapped() {
        model.dismissSwitchRefusal()
        updateModel()
    }

    // MARK: Moving the session (ticket 136)

    private func setUpMove() {
        guard let mover else { return }
        let move = UIAction(title: "Move to…", image: UIImage(systemName: "arrow.right.circle")) { [weak self] _ in self?.showMovePicker() }
        move.accessibilityIdentifier = "session-move"
        let item = UIBarButtonItem(title: "More", image: UIImage(systemName: "ellipsis.circle"), menu: UIMenu(children: [move]))
        item.accessibilityIdentifier = "session-menu"
        menuItem = item
        showApprovalsButton()

        moveNote.font = .preferredFont(forTextStyle: .footnote)
        moveNote.adjustsFontForContentSizeCategory = true
        moveNote.numberOfLines = 0
        moveNote.textAlignment = .center
        moveNote.textColor = .label
        moveNote.backgroundColor = .secondarySystemBackground
        moveNote.layer.cornerRadius = 10
        moveNote.layer.masksToBounds = true
        moveNote.isHidden = true
        moveNote.accessibilityIdentifier = "move-refusal"
        moveNote.accessibilityHint = "Hides this line"
        moveNote.isUserInteractionEnabled = true
        moveNote.addGestureRecognizer(UITapGestureRecognizer(target: self, action: #selector(moveNoteTapped)))
        moveNote.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(moveNote)
        NSLayoutConstraint.activate([
            moveNote.topAnchor.constraint(equalTo: statusBar.bottomAnchor, constant: 8),
            moveNote.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            moveNote.widthAnchor.constraint(lessThanOrEqualTo: view.layoutMarginsGuide.widthAnchor),
        ])
        observeMove(mover)
    }

    private func observeMove(_ mover: SessionMove) {
        withObservationTracking {
            updateMove(mover)
        } onChange: { [weak self, weak mover] in
            Task { @MainActor in
                guard let self, let mover else { return }
                self.observeMove(mover)
            }
        }
    }

    /// The bar's move line and the refusal under it, as the move stands.
    private func updateMove(_ mover: SessionMove) {
        let line = mover.barLine
        statusBar.show(move: line)
        let refusal = mover.refusal.map { " \(SafeText.line($0[...], limit: 300)) " }
        if moveNote.text != refusal { moveNote.text = refusal }
        moveNote.isHidden = refusal == nil
        if refusal != nil { view.bringSubviewToFront(moveNote) }
        let said = line ?? mover.refusal
        if UIAccessibility.isVoiceOverRunning, let said, said != moveSaid {
            UIAccessibility.post(notification: .announcement, argument: said)
        }
        moveSaid = said
    }

    /// Move to… chosen: the sheet of the person's other online hosts.
    public func showMovePicker() {
        guard let hosts, let mover, !mover.isMoving, movePicker == nil || movePicker?.presentingViewController == nil else { return }
        let picker = MovePickerViewController(hosts: HostListModel(source: hosts), holder: mover.holder) { [weak self] device, name, cwd in
            self?.move(to: device, hostName: name, cwd: cwd)
        }
        movePicker = picker
        let sheet = UINavigationController(rootViewController: picker)
        sheet.sheetPresentationController?.detents = [.medium(), .large()]
        present(sheet, animated: true)
    }

    /// A host and a directory picked (R-LAG-8): `moving to <host>…` now,
    /// and the take asked of the host off the main thread. Whether it went.
    @discardableResult
    public func move(to device: String, hostName: String, cwd: String) -> Bool {
        guard let hosts, let mover else { return false }
        guard mover.move(MoveRequest(id: hosts.newCommandID(), device: device, hostName: hostName, sessionID: row.id, cwd: cwd)) else { return false }
        // Now, not at the next turn of the main actor (R-LAG-8).
        updateMove(mover)
        return true
    }

    @objc private func moveNoteTapped() {
        mover?.dismissRefusal()
    }

    // MARK: Approvals (ticket 91)

    private func setUpApprovals() {
        guard let approvals, session != nil else { return }
        // The answer goes as the composer's commands do: shown at once,
        // handed to the core off the main thread, acked by the host. Held
        // weakly: the connection holds the queue.
        approvals.sender = { [weak self] reply in
            guard let self, let session = self.session else { return nil }
            let token = self.model.issue(.approve(reply))
            session.send(.approve(reply), token: token)
            return token
        }
        approvals.onChange = { [weak self] in self?.syncApprovals() }
        approvals.onEvent = { [weak self] in self?.approvalEvent($0) }

        toast.font = .preferredFont(forTextStyle: .footnote)
        toast.adjustsFontForContentSizeCategory = true
        toast.numberOfLines = 0
        toast.textAlignment = .center
        toast.textColor = .label
        toast.backgroundColor = .tertiarySystemBackground
        toast.layer.cornerRadius = 10
        toast.layer.masksToBounds = true
        toast.layer.borderColor = UIColor.separator.cgColor
        toast.layer.borderWidth = 0.5
        toast.alpha = 0
        toast.accessibilityIdentifier = "approval-toast"
        toast.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(toast)
        NSLayoutConstraint.activate([
            toast.topAnchor.constraint(equalTo: statusBar.bottomAnchor, constant: 8),
            toast.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            toast.widthAnchor.constraint(lessThanOrEqualTo: view.layoutMarginsGuide.widthAnchor),
        ])
        approvals.connected = statusBar.status == .live || statusBar.status == .catchingUp
        syncApprovals()
    }

    /// The sheet up with the first request waiting, its content current,
    /// or down with none; the hand in the navigation bar while it's put
    /// away. Never over a screen that left or isn't in a window.
    func syncApprovals() {
        guard let approvals else { return }
        defer { showApprovalsButton() }
        if let sheet = approvalSheet {
            if approvals.current == nil {
                approvalSheet = nil
                takeDown(sheet, animated: true)
            } else {
                sheet.refresh()
            }
            return
        }
        guard approvals.current != nil, !approvalsPutAway, !parked, !left, isViewLoaded, view.window != nil else { return }
        if let other = presentedViewController {
            if let stale = other as? ApprovalSheetController, !stale.isBeingDismissed, !stale.isBeingPresented {
                // A sheet taken down whose dismissal never happened: down
                // again, and this runs once it's gone.
                takeDown(stale, animated: false)
            } else if !(other is ApprovalSheetController) || other.isBeingPresented {
                // Another sheet up (a link's), or one still rising: tried
                // again shortly. One going calls this as it goes.
                retryApprovals()
            }
            return
        }
        let sheet = ApprovalSheetController(queue: approvals)
        sheet.presentationController?.delegate = self
        approvalSheet = sheet
        present(sheet, animated: true)
    }

    /// A sheet taken down, once it has finished rising if it's still
    /// rising (UIKit ignores a dismissal then), and the queue looked at
    /// again once it's gone: a request back from an ack's error rises at
    /// once.
    private func takeDown(_ sheet: ApprovalSheetController, animated: Bool) {
        let down = { [weak self, weak sheet] in
            sheet?.dismiss(animated: animated) {
                MainActor.assumeIsolated { self?.syncApprovals() }
            }
        }
        if sheet.isBeingPresented, let rising = sheet.transitionCoordinator,
           rising.animate(alongsideTransition: nil, completion: { _ in MainActor.assumeIsolated { down() } }) {
            return
        }
        down()
    }

    /// The sheet couldn't rise (another sheet is up): tried again in half
    /// a second, while it's wanted.
    private func retryApprovals() {
        guard approvalsRetry == nil else { return }
        let work = DispatchWorkItem { [weak self] in
            MainActor.assumeIsolated {
                self?.approvalsRetry = nil
                self?.syncApprovals()
            }
        }
        approvalsRetry = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5, execute: work)
    }

    /// The hand: how many wait, and a tap brings the sheet back. First on
    /// the right, before the menu.
    private func showApprovalsButton() {
        let waiting = approvals?.waiting ?? 0
        let menu = menuItem.map { [$0] } ?? []
        guard waiting > 0, approvalSheet == nil else {
            if navigationItem.rightBarButtonItems ?? [] != menu { navigationItem.rightBarButtonItems = menu }
            return
        }
        let label = waiting == 1 ? "1 approval waiting" : "\(waiting) approvals waiting"
        if let item = navigationItem.rightBarButtonItem, item.accessibilityLabel == label { return }
        let item = UIBarButtonItem(title: "\(waiting)", image: UIImage(systemName: "hand.raised.fill"), primaryAction: UIAction { [weak self] _ in
            self?.approvalsPutAway = false
            self?.syncApprovals()
        })
        item.accessibilityIdentifier = "approvals-waiting"
        item.accessibilityLabel = label
        item.accessibilityHint = "Shows the approval sheet"
        item.tintColor = .systemOrange
        navigationItem.rightBarButtonItems = [item] + menu
    }

    /// Swiped down (or Escape): put away until a new request comes or the
    /// hand is tapped. Nothing is answered.
    public func presentationControllerDidDismiss(_ presentationController: UIPresentationController) {
        guard presentationController.presentedViewController is ApprovalSheetController else { return }
        // A sheet already taken down (one whose dismissal UIKit ignored)
        // gone at last: the queue looked at again.
        guard presentationController.presentedViewController === approvalSheet else { return syncApprovals() }
        approvalSheet = nil
        approvalsPutAway = true
        showApprovalsButton()
    }

    /// The screen goes: the sheet with it, at once.
    private func putSheetDown() {
        approvalsRetry?.cancel()
        approvalsRetry = nil
        guard let sheet = approvalSheet else { return }
        approvalSheet = nil
        takeDown(sheet, animated: false)
    }

    /// What became of a request, on its tool's row and in a toast.
    private func approvalEvent(_ event: ApprovalQueue.Event) {
        switch event {
        case .arrived(let prompt), .returned(let prompt, _):
            approvalsPutAway = false
            if noticeShown == .nothingWaiting {
                // The route's after all: should it be settled elsewhere
                // too, the notice comes back.
                hideNotice()
                routedSheet = true
            }
            // A route waiting out its grace lands on it now: on what the
            // queue holds, without draining again (this is mid-drain).
            if landing != nil { land(drain: false) }
            mark(prompt, "waiting for approval")
            if UIAccessibility.isVoiceOverRunning, approvalSheet != nil {
                UIAccessibility.post(notification: .announcement, argument: "Another approval is waiting")
            }
        case .answered(let prompt, let verdict):
            routedSheet = false
            mark(prompt, Self.verdictLine(verdict, asks: prompt.asks) + " — sending…")
        case .settled(let prompt, let verdict):
            mark(prompt, Self.verdictLine(verdict, asks: prompt.asks))
        case .resolvedElsewhere(let prompt, let resolution):
            // A turn stopped denies what it asked, as another device would.
            mark(prompt, resolution == .denied ? "denied elsewhere" : "\(resolution.word) on another device")
            if routeSettled(by: event) { return }
            say(resolution == .denied ? "Denied on another device, or as its turn stopped" : "Answered on another device: \(resolution.word)")
        case .dropped(let prompt, let why):
            mark(prompt, nil)
            if routeSettled(by: event) { return }
            if let why { say(ApprovalRules.saysGone(why) ? "Already answered elsewhere, or its turn is over." : why) }
        }
    }

    private func mark(_ prompt: ApprovalPrompt, _ line: String?) {
        model.markApproval(prompt.requestID, session: prompt.sessionID, tool: prompt.tool, input: prompt.input, line: line)
    }

    /// What a row says of an answer given here.
    static func verdictLine(_ verdict: ApprovalVerdict, asks: Bool) -> String {
        switch verdict {
        case .allow: asks ? "answered" : "allowed"
        case .allowSession: "allowed for this session"
        case .deny: asks ? "declined" : "denied"
        }
    }

    /// The toast: `text` for four seconds, and told to VoiceOver.
    func say(_ text: String) {
        toast.text = " \(SafeText.line(text[...], limit: 300)) "
        // Said over the notice's place: what it says is newer. The host
        // away stays said: the bar's line, and still so.
        if noticeShown == .nothingWaiting { hideNotice() }
        toastHide?.cancel()
        view.bringSubviewToFront(toast)
        UIView.animate(withDuration: 0.2) { self.toast.alpha = 1 }
        let hide = DispatchWorkItem { [weak self] in
            MainActor.assumeIsolated {
                guard let self else { return }
                UIView.animate(withDuration: 0.3) { self.toast.alpha = 0 }
            }
        }
        toastHide = hide
        DispatchQueue.main.asyncAfter(deadline: .now() + 4, execute: hide)
        if UIAccessibility.isVoiceOverRunning {
            UIAccessibility.post(notification: .announcement, argument: text)
        }
    }

    // MARK: Opened from a notification (ticket 106)

    /// Joined and welcomed by a host: what comes shows as it comes.
    public var isLive: Bool { statusBar.status == .live || statusBar.status == .catchingUp }

    /// A tapped notification opened this session: once the connection
    /// says (the first welcome from here, or no host), an approval's route
    /// raises the sheet for the oldest request waiting, or says nothing
    /// waits any more; with the host away, either says so and when it was
    /// last seen, and no sheet rises (R-IOS-7). Already live, it lands on
    /// what the session holds now, but says nothing waits only after
    /// `landingGrace`: a request read as the app comes back still raises
    /// the sheet. The sheet only ever shows what the host
    /// sent over the relay: the route picks the session and nothing else.
    public func land(_ route: NotificationRoute) {
        landing = route
        routedSheet = false
        landedLive = isLive
        graceOver = false
        landingWait?.cancel()
        landingWait = nil
        hideNotice()
        // Put away before: the person asked for it again.
        if case .approval = route { approvalsPutAway = false }
        land()
    }

    /// Lands the route, if the connection has said enough: what has come
    /// (the welcome's requests, which the session posts before its status
    /// says live) applied first.
    private func land(drain: Bool = true) {
        guard let route = landing else { return }
        if drain, isViewLoaded { frame() }
        // What the drain brought may have landed it already.
        guard landing == route else { return }
        // Joining again: the next live is a welcome.
        if !isLive { landedLive = false }
        guard let landed = RouteLanding.of(route, status: statusBar.status, waiting: approvals?.waiting ?? 0) else { return }
        if landed == .nothingWaiting, landedLive, !graceOver {
            if landingWait == nil {
                let work = DispatchWorkItem { [weak self] in
                    MainActor.assumeIsolated {
                        guard let self else { return }
                        self.landingWait = nil
                        self.graceOver = true
                        self.land()
                    }
                }
                landingWait = work
                DispatchQueue.main.asyncAfter(deadline: .now() + Self.landingGrace, execute: work)
            }
            return
        }
        landingWait?.cancel()
        landingWait = nil
        landing = nil
        switch landed {
        case .sheet:
            routedSheet = true
            syncApprovals()
        case .nothingWaiting: showNotice(landed, RouteLanding.nothingWaitingNotice)
        case .hostOffline: showNotice(landed, RouteLanding.offlineNotice(lastSeen: row.lastSeen))
        case .shown: break
        }
    }

    /// A request settled elsewhere while a route's notice says nothing
    /// waits, or the last one a route's sheet showed: the notice says it,
    /// and stays, rather than a toast that would take its place. Whether
    /// it did.
    private func routeSettled(by event: ApprovalQueue.Event) -> Bool {
        // Settled elsewhere or by its turn's end: not a refusal of an
        // answer of this phone's, nor the connection stopping, which the
        // toast and the bar say.
        switch event {
        case .resolvedElsewhere: break
        case .dropped(_, nil):
            if case .ended = statusBar.status { return false }
        default: return false
        }
        if noticeShown == .nothingWaiting { return true }
        guard routedSheet, approvals?.waiting == 0 else { return false }
        routedSheet = false
        showNotice(.nothingWaiting, RouteLanding.nothingWaitingNotice)
        return true
    }

    private func showNotice(_ landed: RouteLanding, _ text: String) {
        noticeShown = landed
        notice.text = " \(text) "
        notice.isHidden = false
        guard isViewLoaded else { return }
        if notice.superview == nil { addNotice() }
        view.bringSubviewToFront(notice)
        if UIAccessibility.isVoiceOverRunning {
            UIAccessibility.post(notification: .announcement, argument: text)
        }
    }

    func hideNotice() {
        noticeShown = nil
        notice.isHidden = true
    }

    private func addNotice() {
        notice.font = .preferredFont(forTextStyle: .footnote)
        notice.adjustsFontForContentSizeCategory = true
        notice.numberOfLines = 0
        notice.textAlignment = .center
        notice.textColor = .secondaryLabel
        notice.backgroundColor = .secondarySystemBackground
        notice.layer.cornerRadius = 10
        notice.layer.masksToBounds = true
        notice.accessibilityIdentifier = "route-notice"
        // A tap hides it: VoiceOver reads it as a button that does so.
        notice.accessibilityHint = "Hides this line"
        notice.isUserInteractionEnabled = true
        notice.addGestureRecognizer(UITapGestureRecognizer(target: self, action: #selector(noticeTapped)))
        notice.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(notice)
        NSLayoutConstraint.activate([
            notice.topAnchor.constraint(equalTo: session != nil ? statusBar.bottomAnchor : view.safeAreaLayoutGuide.topAnchor, constant: 8),
            notice.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            notice.widthAnchor.constraint(lessThanOrEqualTo: view.layoutMarginsGuide.widthAnchor),
        ])
    }

    @objc private func noticeTapped() { hideNotice() }

    // MARK: Following the end

    /// The content offset that shows the end.
    private var bottomOffset: CGFloat {
        guard let collectionView else { return 0 }
        let inset = collectionView.adjustedContentInset
        return max(-inset.top, collectionView.contentSize.height + inset.bottom - collectionView.bounds.height)
    }

    /// The list shows its end, within 8 points.
    public var isAtBottom: Bool {
        guard let collectionView else { return true }
        return collectionView.contentOffset.y >= bottomOffset - 8
    }

    /// After each layout pass that changed what's laid out (the frame's,
    /// which sized the streaming cell; a rotation): following, the end is
    /// shown, without animation. A pass that only scrolled is left be, so
    /// a scroll alone is never undone, and nothing moves while a finger is
    /// on the list.
    private func pin() {
        guard following, drewRows, let collectionView, !collectionView.isTracking, !collectionView.isDecelerating else { return }
        let laid = (content: collectionView.contentSize, bounds: collectionView.bounds.size)
        if let pinnedFor, pinnedFor.content == laid.content, pinnedFor.bounds == laid.bounds { return }
        pinnedFor = laid
        let bottom = bottomOffset
        if abs(collectionView.contentOffset.y - bottom) > 0.5 {
            scrollingItself = true
            collectionView.contentOffset.y = bottom
            scrollingItself = false
        }
    }

    /// The person scrolled with a finger: following if they left the list
    /// at its end, not if they scrolled up from it. Other scrolls are the
    /// list's own (cells sizing themselves, offsets clamped) and change
    /// nothing; VoiceOver's and the status bar's say so themselves
    /// (`personScrolled`, `scrollViewShouldScrollToTop`).
    public func scrollViewDidScroll(_ scrollView: UIScrollView) {
        pageOlderIfNeeded()
        guard !scrollingItself, scrollView.isTracking || scrollView.isDragging || scrollView.isDecelerating else { return }
        follow(reachedEnd)
    }

    public func scrollViewWillBeginDragging(_ scrollView: UIScrollView) {
        dragFrom = scrollView.contentSize.height
    }

    /// At the end, for a drag: what streamed in since it began is slack, so
    /// reaching the end while the answer grows under the finger counts.
    private var reachedEnd: Bool {
        guard let collectionView else { return true }
        let grown = dragFrom.map { max(0, collectionView.contentSize.height - $0) } ?? 0
        return collectionView.contentOffset.y >= bottomOffset - 8 - grown
    }

    /// VoiceOver focused `element`: in a row above the last, or in the
    /// last above what's at the bottom of the screen (an early block of a
    /// streaming answer), it's reading back, and the list stops following.
    func focused(_ element: AnyObject?) {
        guard let collectionView, let focused = element as? UIView, focused.isDescendant(of: collectionView) else { return }
        var view = focused
        while !(view is UICollectionViewCell), let up = view.superview { view = up }
        guard let cell = view as? UICollectionViewCell, let path = collectionView.indexPath(for: cell) else { return }
        // The bottom of what's on screen, or of the content if it's shorter.
        let bottom = min(collectionView.contentOffset.y + collectionView.bounds.height - collectionView.adjustedContentInset.bottom,
                         collectionView.contentSize.height)
        let reachesBottom = focused.convert(focused.bounds, to: collectionView).maxY >= bottom - 44
        if path.item < collectionView.numberOfItems(inSection: 0) - 1 || !reachesBottom { follow(false) }
    }

    /// VoiceOver's three-finger scroll (or another assistive one) moved the
    /// list: up is away from the end.
    func personScrolled(_ direction: UIAccessibilityScrollDirection) {
        switch direction {
        case .up, .previous: follow(false)
        case .down, .next:
            // Animated, it hasn't moved yet: decided when it lands too.
            assistiveDown = true
            follow(isAtBottom)
        default: break
        }
    }

    public func scrollViewDidEndDecelerating(_ scrollView: UIScrollView) {
        follow(reachedEnd)
        dragFrom = nil
    }

    public func scrollViewDidEndDragging(_ scrollView: UIScrollView, willDecelerate decelerate: Bool) {
        guard !decelerate else { return }
        follow(reachedEnd)
        dragFrom = nil
    }

    /// An animated assistive scroll down landed: at the end, following.
    public func scrollViewDidEndScrollingAnimation(_ scrollView: UIScrollView) {
        guard assistiveDown else { return }
        assistiveDown = false
        follow(isAtBottom)
    }

    /// A tap on the status bar scrolls to the top: not following.
    public func scrollViewShouldScrollToTop(_ scrollView: UIScrollView) -> Bool {
        follow(false)
        return true
    }

    public func scrollViewDidScrollToTop(_ scrollView: UIScrollView) {
        follow(isAtBottom)
    }

    private func follow(_ atBottom: Bool) {
        if atBottom, !following, let collectionView {
            // From what's laid out now: the next change pins, not this scroll.
            pinnedFor = (collectionView.contentSize, collectionView.bounds.size)
        }
        following = atBottom
        if atBottom { newPill.isHidden = true }
    }

    /// "↓ New" tapped: the end, and following again; VoiceOver moves to
    /// the last row.
    func jumpToEnd() {
        follow(true)
        pinnedFor = nil
        scrollToEnd()
        DispatchQueue.main.async { MainActor.assumeIsolated { [weak self] in
            guard let self, let collectionView = self.collectionView else { return }
            let last = collectionView.visibleCells.max { $0.frame.maxY < $1.frame.maxY }
            UIAccessibility.post(notification: .layoutChanged, argument: last)
        } }
    }

    // MARK: The model to the screen

    /// Applies the model's rows now, and again after its next change.
    private func observeModel() {
        withObservationTracking {
            apply(rows: model.ids, attached: model.attached)
        } onChange: { [weak self] in
            Task { @MainActor [weak self] in self?.observeModel() }
        }
    }

    private func apply(rows: [String], attached: Bool) {
        if rows != applied {
            #if DEBUG
            precondition(Set(rows).count == rows.count, "a transcript row twice")
            #endif
            let first = applied.isEmpty
            // Rows added above (an older kept page): the row at the top of
            // the screen stays where it is, unless the list follows the end.
            let anchor = !first && !following && rows.first != applied.first ? topRow() : nil
            var snapshot = NSDiffableDataSourceSnapshot<Int, String>()
            snapshot.appendSections([0])
            snapshot.appendItems(rows)
            // Not animated: the first load, and (until ticket 89 decides how
            // live rows arrive) every one after. A row's own changes are its
            // cell's, which observes it: nothing is reloaded.
            #if DEBUG
            let applying = FrameCosts.start()
            #endif
            dataSource.apply(snapshot, animatingDifferences: false)
            #if DEBUG
            FrameCosts.end("snapshot \(applied.count)→\(rows.count) rows", applying)
            #endif
            snapshotApplies += 1
            applied = rows
            if first && !rows.isEmpty { scrollToEnd() }
            if let anchor { keep(anchor) }
        }
        drewRows = drewRows || !rows.isEmpty
        updateBody(attached: attached)
    }

    /// The row at the top of the screen, and how far below the top of the
    /// visible content it starts.
    private func topRow() -> (id: String, offset: CGFloat)? {
        guard let collectionView, let path = collectionView.indexPathsForVisibleItems.min(),
              let id = dataSource.itemIdentifier(for: path), let frame = collectionView.layoutAttributesForItem(at: path)?.frame else { return nil }
        return (id, frame.minY - collectionView.contentOffset.y)
    }

    /// Scrolled so `anchor`'s row is where it was.
    private func keep(_ anchor: (id: String, offset: CGFloat)) {
        guard let collectionView, let path = dataSource.indexPath(for: anchor.id) else { return }
        collectionView.layoutIfNeeded()
        guard let frame = collectionView.layoutAttributesForItem(at: path)?.frame else { return }
        scrollingItself = true
        collectionView.contentOffset.y = frame.minY - anchor.offset
        scrollingItself = false
    }

    /// The empty body: an indicator while the session is read and nothing
    /// is drawn (not while what the phone kept is still being read); a
    /// sentence once it's read with nothing to draw, or stopped before it
    /// was read.
    private func updateBody(attached: Bool) {
        if drewRows {
            spinner.stopAnimating()
            emptyLabel.isHidden = true
        } else if attached {
            spinner.stopAnimating()
            emptyLabel.text = "Nothing in this session yet."
            emptyLabel.isHidden = false
        } else if stoppedEarly {
            spinner.stopAnimating()
            emptyLabel.text = "This session couldn't be read. The bar above says why."
            emptyLabel.isHidden = false
        } else if session != nil, !readingKept {
            spinner.startAnimating()
            emptyLabel.isHidden = true
        }
    }

    private func relay(_ status: RelayStatus) {
        statusBar.show(status)
        // The foreground's budget (ticket 128): the open session's bar
        // live again is the app caught up. Not a screen covered or parked.
        if navigationController?.topViewController === self, viewIfLoaded?.window != nil, !parked {
            Budgets.shared.sessionStatus(status)
        }
        model.setHostOffline(status == .hostOffline)
        approvals?.connected = status == .live || status == .catchingUp
        if isLive, noticeShown == .hostOffline { hideNotice() }
        land()
        if case .ended = status {
            if !model.attached {
                stoppedEarly = true
                updateBody(attached: false)
            }
            // The last acks before the end are in the mailbox already (the
            // status is published after them): applied first. Then nothing
            // still out will be answered: said on its bubble.
            frame()
            approvals?.connectionEnded()
            let refused = model.connectionEnded()
            if UIAccessibility.isVoiceOverRunning, !refused.isEmpty {
                UIAccessibility.post(notification: .announcement, argument: "The connection stopped: what you sent may not have gone")
            }
            settleParked()
        }
        updateComposer()
        updateModel()
    }

    /// The last row at the bottom of the screen. The rows' heights are
    /// estimates until they are laid out, so it scrolls, lays out what it
    /// scrolled to, and scrolls again; and once more on the next turn of
    /// the run loop, after the cells sized themselves.
    private func scrollToEnd() {
        let end = { [weak self] in
            guard let self, let collectionView else { return }
            let count = collectionView.numberOfItems(inSection: 0)
            guard count > 0 else { return }
            self.scrollingItself = true
            defer { self.scrollingItself = false }
            collectionView.layoutIfNeeded()
            collectionView.scrollToItem(at: IndexPath(item: count - 1, section: 0), at: .bottom, animated: false)
        }
        end()
        end()
        DispatchQueue.main.async { MainActor.assumeIsolated { end() } }
    }

    /// The text row `id`'s cell draws now, if it's on screen: every block
    /// of a streaming answer, in order (for the tests outside KrowkKit).
    public func drawnText(of id: String) -> String? {
        guard let path = dataSource.indexPath(for: id), let cell = collectionView.cellForItem(at: path) else { return nil }
        return (cell as? TranscriptTextCell)?.text.drawnText ?? (cell as? TranscriptToolCell)?.output.drawnText
    }

    // MARK: Taps

    public func collectionView(_ collectionView: UICollectionView, shouldHighlightItemAt indexPath: IndexPath) -> Bool {
        guard let id = dataSource.itemIdentifier(for: indexPath), let kind = model.item(id)?.kind else { return false }
        return kind == .tool || kind == .reasoning
    }

    /// A tool or reasoning row expands, or collapses: its cell redraws
    /// itself, and the list follows its height.
    public func collectionView(_ collectionView: UICollectionView, didSelectItemAt indexPath: IndexPath) {
        collectionView.deselectItem(at: indexPath, animated: false)
        guard let id = dataSource.itemIdentifier(for: indexPath) else { return }
        toggle(id)
    }

    /// Row `id` expanded or collapsed, if it's a tool or reasoning row: its
    /// state changes (its cell redraws itself), and the item is
    /// reconfigured, never reloaded, so the list sizes it again at once.
    func toggle(_ id: String) {
        guard let state = model.item(id), state.kind == .tool || state.kind == .reasoning else { return }
        state.expanded.toggle()
        // Opened to read: the list stays where it is rather than follow
        // the end past it ("↓ New" says when more comes).
        if state.expanded { follow(false) }
        var snapshot = dataSource.snapshot()
        guard snapshot.indexOfItem(id) != nil else { return }
        snapshot.reconfigureItems([id])
        dataSource.apply(snapshot, animatingDifferences: false)
        snapshotApplies += 1
    }

    /// A link tapped: a sheet naming the whole URL, and only its "Open"
    /// opens it (R-IOS-10).
    func confirm(_ url: URL) {
        let sheet = Self.confirmation(for: url, open: opener)
        if let popover = sheet.popoverPresentationController {
            popover.sourceView = view
            popover.sourceRect = CGRect(x: view.bounds.midX, y: view.bounds.midY, width: 0, height: 0)
        }
        present(sheet, animated: true)
    }

    /// The sheet a tapped link opens: its title asks, its message is the
    /// whole URL, and "Open" calls `open` with it; "Cancel" does nothing.
    static func confirmation(for url: URL, open: @escaping @MainActor (URL) -> Void) -> UIAlertController {
        let whole = SafeText.clean(url.absoluteString)
        // A URL long enough to fill the screen is cut after 2,000
        // characters; its host is named in the title.
        let message = whole.count > 2_000 ? String(whole.prefix(2_000)) + "…" : whole
        let title = url.host(percentEncoded: true).map { "Open a link to \(SafeText.line($0[...], limit: 253))?" } ?? "Open this link?"
        let sheet = UIAlertController(title: title, message: message, preferredStyle: .actionSheet)
        sheet.view.accessibilityIdentifier = "link-confirmation"
        sheet.addAction(UIAlertAction(title: "Open", style: .default) { _ in
            MainActor.assumeIsolated { open(url) }
        })
        sheet.addAction(UIAlertAction(title: "Cancel", style: .cancel))
        return sheet
    }
}

/// The display link's target, holding the screen weakly: a link retains
/// its target until invalidated, which it does itself once the screen is
/// gone. It runs on the main run loop.
@MainActor
private final class TranscriptFrameTarget: NSObject {
    weak var screen: TranscriptViewController?

    init(_ screen: TranscriptViewController) {
        self.screen = screen
    }

    @objc func frame(_ link: CADisplayLink) {
        guard let screen else { return link.invalidate() }
        screen.frame(at: link.timestamp)
    }
}

/// The transcript's list, which says when it has laid out: the screen
/// keeps the end in view then, once the frame's cells have their sizes.
final class TranscriptCollectionView: UICollectionView {
    var afterLayout: (() -> Void)?
    /// A finger on the list, as the tests say it (UIKit's own otherwise).
    var touching: Bool?

    override var isTracking: Bool { touching ?? super.isTracking }

    /// An assistive scroll (VoiceOver's three fingers), after it scrolled.
    var accessibilityScrolled: ((UIAccessibilityScrollDirection) -> Void)?

    override func accessibilityScroll(_ direction: UIAccessibilityScrollDirection) -> Bool {
        let scrolled = super.accessibilityScroll(direction)
        accessibilityScrolled?(direction)
        return scrolled
    }

    override func layoutSubviews() {
        #if DEBUG
        let laying = FrameCosts.start()
        defer { FrameCosts.end("layout", laying) }
        #endif
        super.layoutSubviews()
        afterLayout?()
    }
}
