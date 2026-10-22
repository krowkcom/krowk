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
/// and stopped as it leaves. What it brings is drained once per display
/// frame into the model, on the main thread; the blocking reads behind it
/// run on threads of their own (`LiveSession`).
public final class TranscriptViewController: UIViewController, UICollectionViewDelegate {
    let row: SessionRow
    public let model: TranscriptModel
    let statusBar = RelayStatusBar()
    /// The session followed live, if the app gave one.
    let session: (any LiveSession)?
    /// Opens a URL the person confirmed: `UIApplication.shared.open`.
    private let opener: @MainActor (URL) -> Void

    public private(set) var collectionView: UICollectionView!
    public private(set) var dataSource: UICollectionViewDiffableDataSource<Int, String>!
    let spinner = UIActivityIndicatorView(style: .medium)
    let emptyLabel = UILabel()
    private var frames: CADisplayLink?
    /// Updates taken from the session, frame by frame.
    private(set) var taken = 0
    /// Rows have been drawn: no spinner shows again.
    public private(set) var drewRows = false
    /// The rows as last applied.
    private var applied: [String] = []
    /// The session stopped before it was read: why, as the bar says it.
    private var stoppedEarly = false

    public init(row: SessionRow, session: (any LiveSession)? = nil, model: TranscriptModel? = nil, open: (@MainActor (URL) -> Void)? = nil) {
        self.row = row
        self.session = session
        self.model = model ?? TranscriptModel(sessionID: row.id)
        self.opener = open ?? { UIApplication.shared.open($0) }
        super.init(nibName: nil, bundle: nil)
        title = row.title
        navigationItem.largeTitleDisplayMode = .never
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("TranscriptViewController is built in code")
    }

    deinit {
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
        collectionView = UICollectionView(frame: .zero, collectionViewLayout: layout)
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
            cell.toggle = { [weak state] in state?.expanded.toggle() }
            cell.show(state)
        }
        let mark = UICollectionView.CellRegistration<TranscriptMarkCell, String> { [weak self] cell, _, id in
            guard let state = self?.model.item(id) else { return }
            cell.show(state)
        }
        dataSource = UICollectionViewDiffableDataSource<Int, String>(collectionView: collectionView) { [weak self] view, path, id in
            switch self?.model.item(id)?.kind {
            case .user?, .assistant?: view.dequeueConfiguredReusableCell(using: text, for: path, item: id)
            case .reasoning?, .tool?: view.dequeueConfiguredReusableCell(using: tool, for: path, item: id)
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
        NSLayoutConstraint.activate([
            collectionView.topAnchor.constraint(equalTo: top),
            collectionView.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            collectionView.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            collectionView.bottomAnchor.constraint(equalTo: view.bottomAnchor),
        ])

        observeModel()
        session?.follow { [weak self] status in self?.relay(status) }
    }

    override public func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        guard session != nil, frames == nil else { return }
        let link = CADisplayLink(target: TranscriptFrameTarget(self), selector: #selector(TranscriptFrameTarget.frame(_:)))
        link.add(to: .main, forMode: .common)
        frames = link
    }

    /// Covered, the screen keeps draining what the session brings, so
    /// nothing piles up; leaving, it stops the session and the drain.
    override public func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        if isMovingFromParent || isBeingDismissed || navigationController?.isBeingDismissed == true {
            frames?.invalidate()
            frames = nil
            session?.stop()
        }
    }

    /// One display frame: what the session brought, into the model.
    func frame() {
        guard let session else { return }
        taken += session.takeFrame(into: model)
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
        if case .ended = status, !model.attached {
            stoppedEarly = true
            updateBody(attached: false)
        }
    }

    /// The last row at the bottom of the screen.
    private func scrollToEnd() {
        let count = collectionView.numberOfItems(inSection: 0)
        guard count > 0 else { return }
        collectionView.layoutIfNeeded()
        collectionView.scrollToItem(at: IndexPath(item: count - 1, section: 0), at: .bottom, animated: false)
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
        guard let id = dataSource.itemIdentifier(for: indexPath), let state = model.item(id) else { return }
        guard state.kind == .tool || state.kind == .reasoning else { return }
        state.expanded.toggle()
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
        let sheet = UIAlertController(title: "Open this link?", message: SafeText.clean(url.absoluteString), preferredStyle: .actionSheet)
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
        screen.frame()
    }
}
