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
/// Under the transcript sits the composer (ticket 90): Send starts a turn,
/// Steer adds to the running one, Stop interrupts it. What's typed shows
/// at once as a bubble at the end (R-LAG-8), settled by the host's ack and
/// replaced by the item the host logs; with no host it's queued in the
/// core for the next one (R-HAND-4). The core owns the ids, the queue and
/// the resending; this screen only shows what it reports.
public final class TranscriptViewController: UIViewController, UICollectionViewDelegate {
    let row: SessionRow
    public let model: TranscriptModel
    public let statusBar = RelayStatusBar()
    /// The session followed live, if the app gave one.
    let session: (any LiveSession)?
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

    public init(row: SessionRow, session: (any LiveSession)? = nil, model: TranscriptModel? = nil, open: (@MainActor (URL) -> Void)? = nil) {
        self.row = row
        self.session = session
        let model = model ?? TranscriptModel(sessionID: row.id)
        self.model = model
        self.driver = session.map { FrameDriver(session: $0, model: model) }
        self.opener = open ?? { UIApplication.shared.open($0) }
        super.init(nibName: nil, bundle: nil)
        title = row.title
        navigationItem.largeTitleDisplayMode = .never
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
            cell.openLink = open
            cell.show(state)
        }
        let tool = UICollectionView.CellRegistration<TranscriptToolCell, String> { [weak self] cell, _, id in
            guard let state = self?.model.item(id) else { return }
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
            composer.send = { [weak self] in self?.sendTyped() }
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

        observeFocus()
        observeModel()
        driver?.applied = { [weak self] done in self?.drawFrame(done) }
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
    /// parked, they're applied now.
    private func woken() {
        if parked {
            frame()
        } else {
            driver?.wake()
        }
    }

    override public func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
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
        if let focusObserver { NotificationCenter.default.removeObserver(focusObserver) }
        focusObserver = nil
        frames?.invalidate()
        frames = nil
        driver?.ticker = nil
        // Commands the core still holds go on: a queued prompt survives
        // leaving the screen (R-HAND-4), until the host answers it.
        if model.commandsOut > 0, let parking, !isEnded {
            parked = true
            parking.park(self)
            return
        }
        session?.stop()
    }

    /// Shown again (`ParkedScreens.take`, or pushed back): followed by its
    /// display link from here, not let go.
    func unpark() {
        guard parked else { return }
        parked = false
        parking?.release(self)
    }

    /// Parked, and nothing it holds needs the connection any more: every
    /// command answered and its logged item here (or the connection
    /// stopped): let go. One whose logged item never comes (the host logged
    /// other text) is let go `parkedGrace` after the last answer.
    private func settleParked() {
        guard parked else { return }
        if isEnded || (model.commandsOut == 0 && !model.awaitingLog) { return letGo() }
        guard model.commandsOut == 0, !graceArmed else { return }
        graceArmed = true
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.parkedGrace) { MainActor.assumeIsolated { [weak self] in
            guard let self else { return }
            graceArmed = false
            if parked, model.commandsOut == 0 { letGo() }
        } }
    }

    /// How long a parked screen waits for the logged items of commands the
    /// host took.
    static let parkedGrace: TimeInterval = 30
    private var graceArmed = false

    /// Stopped and forgotten by its parking.
    func letGo() {
        parked = false
        parking?.release(self)
        session?.stop()
    }

    private var isEnded: Bool {
        if case .ended = statusBar.status { true } else { false }
    }

    /// One display frame: what the session brought, into the model.
    func frame(at now: TimeInterval = CACurrentMediaTime()) {
        driver?.tick(at: now)
    }

    /// Inside the frame, after its apply: the streaming cells on screen
    /// append their new text now, so the frame's layout has it; the pill
    /// shows if something came while scrolled up; VoiceOver hears a turn
    /// end (never each word).
    private func drawFrame(_ done: FrameApplied) {
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
            UIAccessibility.post(notification: .announcement, argument: "Not sent: \(SafeText.line(refused.caption[...], limit: 200))")
        }
        if let why = done.stopRefused { stopRefused = why }
        if !done.ended.isEmpty { stopRefused = nil }
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
    /// keeps its focus; nothing waits on the send.
    public func sendTyped() {
        updateComposer()
        guard let session, composerState.canSend else { return }
        let text = composer.text
        let command: LiveCommand = model.isTurnRunning ? .steer(text) : .prompt(text)
        let token = model.issue(command)
        composer.text = ""
        // The bubble's row now, not on the observation's next turn.
        apply(rows: model.ids, attached: model.attached)
        follow(true)
        pinnedFor = nil
        scrollToEnd()
        session.send(command, token: token)
        updateComposer()
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
            var snapshot = NSDiffableDataSourceSnapshot<Int, String>()
            snapshot.appendSections([0])
            snapshot.appendItems(rows)
            // Not animated: the first load, and (until ticket 89 decides how
            // live rows arrive) every one after. A row's own changes are its
            // cell's, which observes it: nothing is reloaded.
            dataSource.apply(snapshot, animatingDifferences: false)
            snapshotApplies += 1
            applied = rows
            if first && !rows.isEmpty { scrollToEnd() }
        }
        drewRows = drewRows || !rows.isEmpty
        updateBody(attached: attached)
    }

    /// The empty body: an indicator while the session is read and nothing
    /// is drawn; a sentence once it's read with nothing to draw, or stopped
    /// before it was read.
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
        } else if session != nil {
            spinner.startAnimating()
            emptyLabel.isHidden = true
        }
    }

    private func relay(_ status: RelayStatus) {
        statusBar.show(status)
        model.setHostOffline(status == .hostOffline)
        if case .ended = status {
            if !model.attached {
                stoppedEarly = true
                updateBody(attached: false)
            }
            // Nothing still out will be answered: said on its bubble.
            let refused = model.connectionEnded()
            if UIAccessibility.isVoiceOverRunning, !refused.isEmpty {
                UIAccessibility.post(notification: .announcement, argument: "The connection stopped: what you sent may not have gone")
            }
            settleParked()
        }
        updateComposer()
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
        super.layoutSubviews()
        afterLayout?()
    }
}
