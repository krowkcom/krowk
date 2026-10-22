import UIKit

/// Session screens left with commands still out (ticket 90, R-HAND-4): a
/// prompt queued while the laptop sleeps survives leaving the screen (not
/// quitting the app; a persistent outbox is M2). A screen popped with
/// unanswered commands is kept here with its connection running, applying
/// what comes as it comes, until the core has them all answered (or the
/// connection stops); opening the same session again shows it, as it was.
@MainActor
public final class ParkedScreens {
    private var screens: [String: TranscriptViewController] = [:]

    public init() {}

    /// The screen kept for session `id`, if one is: taken out, to be shown
    /// again.
    public func take(_ id: String) -> TranscriptViewController? {
        guard let screen = screens.removeValue(forKey: id) else { return nil }
        screen.unpark()
        return screen
    }

    /// How many screens are kept.
    public var count: Int { screens.count }

    func park(_ screen: TranscriptViewController) {
        // A second screen of the same session (opened while this one was
        // parked, before `take`) can't be: `take` is how it opens. If one
        // is, the newer is kept and the older let go.
        if let older = screens[screen.row.id], older !== screen { older.letGo() }
        screens[screen.row.id] = screen
    }

    func release(_ screen: TranscriptViewController) {
        if screens[screen.row.id] === screen { screens[screen.row.id] = nil }
    }
}
