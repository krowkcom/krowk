#if DEBUG
import KrowkKit
import QuartzCore
import UIKit

/// What a DEBUG build opens with `KROWK_FIXTURE` in its launch environment
/// (ticket 127, R-PERF-7), in place of sign-in, pairing or Sessions:
/// `tenk`, a transcript screen on `TranscriptFixture`'s 10,000 rows, held
/// in memory and followed by no session, so nothing touches the network.
/// Its "Scroll through" button takes the list from its top to its bottom,
/// a screen a display frame, reading "Scrolling…" meanwhile and "Scrolled
/// N" after the Nth pass:
/// BudgetUITests measures the app's memory over those passes. Not in a
/// Release build (scripts/approvals.sh).
@MainActor
enum Fixture {
    static func screen(_ environment: [String: String] = ProcessInfo.processInfo.environment) -> UIViewController? {
        guard environment["KROWK_FIXTURE"] == "tenk" else { return nil }
        let model = TranscriptModel(sessionID: TranscriptFixture.sessionID)
        model.attach(TranscriptFixture.tenThousand())
        let row = SessionRow(id: TranscriptFixture.sessionID, title: "10,000 items", detail: "", live: false, accessibilityLabel: "10,000 items")
        let screen = TranscriptViewController(row: row, model: model)
        let scroller = FixtureScroller(screen)
        let button = UIBarButtonItem(title: "Scroll through", primaryAction: UIAction { _ in scroller.start() })
        button.accessibilityIdentifier = "fixture-scroll"
        scroller.button = button
        screen.navigationItem.rightBarButtonItem = button
        return screen
    }
}

/// Takes the fixture's list from top to bottom, one screen a frame.
@MainActor
final class FixtureScroller: NSObject {
    private weak var screen: TranscriptViewController?
    weak var button: UIBarButtonItem?
    private var link: CADisplayLink?
    private var passes = 0

    init(_ screen: TranscriptViewController) {
        self.screen = screen
    }

    func start() {
        guard link == nil, let screen, let list = screen.collectionView else { return }
        // As a tap on the status bar would: the list stops following the end.
        _ = screen.scrollViewShouldScrollToTop(list)
        list.setContentOffset(CGPoint(x: 0, y: -list.adjustedContentInset.top), animated: false)
        button?.title = "Scrolling…"
        let link = CADisplayLink(target: self, selector: #selector(step))
        link.add(to: .main, forMode: .common)
        self.link = link
    }

    @objc private func step() {
        guard let screen, let list = screen.collectionView else { return stop() }
        let bottom = list.contentSize.height + list.adjustedContentInset.bottom - list.bounds.height
        if list.contentOffset.y >= bottom - 1 {
            passes += 1
            button?.title = "Scrolled \(passes)"
            return stop()
        }
        list.setContentOffset(CGPoint(x: 0, y: min(bottom, list.contentOffset.y + list.bounds.height)), animated: false)
    }

    private func stop() {
        link?.invalidate()
        link = nil
    }
}
#endif
