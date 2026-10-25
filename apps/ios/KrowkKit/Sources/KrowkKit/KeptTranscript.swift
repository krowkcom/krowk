import Foundation

/// A page of what the phone kept of a session (R-IOS-1, R-IOS-3): its
/// events oldest first, and where the page before it ends, while the phone
/// keeps an older one.
public struct KeptPage: Sendable, Equatable {
    public var events: [TranscriptEvent]
    /// What `KeptTranscript.page(before:)` takes for the page before this
    /// one; nil when this one is the oldest kept.
    public var older: Int?

    public init(events: [TranscriptEvent], older: Int?) {
        self.events = events
        self.older = older
    }
}

/// What the phone kept of one session's transcript (ticket 131): its
/// newest page, drawn before the network says anything, then older pages
/// as the person scrolls up (R-IOS-1, R-IOS-3). KrowkAccount's over the
/// on-device cache; a stub in the tests. Never the network.
public protocol KeptTranscript: AnyObject, Sendable {
    /// The newest page, when it is in memory already (prefetched while a
    /// neighbouring session was open): drawn in the run-loop turn the
    /// screen loads. Never blocks.
    @MainActor func ready() -> KeptPage?
    /// The newest page, read and decoded off the main thread; nil when the
    /// phone keeps none.
    func newest() async -> KeptPage?
    /// The page before `older` (a page's `older`), read and decoded off
    /// the main thread; nil when there is none.
    func page(before older: Int) async -> KeptPage?
}
