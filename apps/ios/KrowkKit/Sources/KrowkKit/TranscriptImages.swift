import Foundation
import UIKit

/// The images a session's prompts name (R-IMG-13, R-IMG-15), kept in the
/// app's caches folder by sha256, one folder per session. The system may
/// clear it any time; an image is then fetched again. Only bytes the
/// session's own sealed store gave are ever kept or drawn (R-IOS-10).
public struct ImageCache: Sendable {
    public let root: URL

    public init(root: URL) {
        self.root = root
    }

    /// `Caches/session-images`.
    public static var standard: ImageCache {
        let caches = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
        return ImageCache(root: caches.appendingPathComponent("session-images", isDirectory: true))
    }

    /// Whether `text` is a sha256 in lowercase or uppercase hex: the only
    /// name a file is kept under (a hash from the log is hostile input).
    public static func isHash(_ text: String) -> Bool {
        text.utf8.count == 64 && text.utf8.allSatisfy { ($0 >= 0x30 && $0 <= 0x39) || ($0 >= 0x41 && $0 <= 0x46) || ($0 >= 0x61 && $0 <= 0x66) }
    }

    private func file(_ session: String, _ sha256: String) -> URL? {
        guard Self.isHash(sha256), !session.isEmpty, !session.contains("/"), session != ".", session != ".." else { return nil }
        return root.appendingPathComponent(session, isDirectory: true).appendingPathComponent(sha256)
    }

    /// The bytes kept for `sha256`, if any.
    public func data(session: String, sha256: String) -> Data? {
        file(session, sha256).flatMap { try? Data(contentsOf: $0) }
    }

    /// Forgets the copy of `sha256`, if any.
    public func remove(session: String, sha256: String) {
        if let url = file(session, sha256) { try? FileManager.default.removeItem(at: url) }
    }

    /// Keeps `data`; a failure only means it is fetched again.
    public func store(_ data: Data, session: String, sha256: String) {
        guard let url = file(session, sha256) else { return }
        try? FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try? data.write(to: url, options: .atomic)
    }
}

/// Fetches the bytes of the images a transcript's prompts name, and swaps
/// each `[Image #N]` for its thumbnail when they arrive. The device's own
/// copy first, then `fetch` (the session's sealed store through the core:
/// blocking, so run off the main thread), which answers nil while the
/// image is not there yet: the placeholder stays and it is asked again
/// later. It never shows an error and never asks the host (D6).
@MainActor
public final class TranscriptImages {
    /// The longest side of a thumbnail, in points.
    nonisolated static let thumbnailSide: CGFloat = 160

    private let model: TranscriptModel
    private let cache: ImageCache
    private let fetch: @Sendable (String) async throws -> Data?
    private let first: Duration
    private let most: Duration
    /// Hashes being fetched now, each with the prompts waiting on it.
    private var waiting: [String: [(ItemState, UInt32)]] = [:]
    private var tasks: [String: Task<Void, Never>] = [:]
    /// Hashes held back until a fetch in flight ends: each asks the core
    /// on a thread of its own, which blocks.
    private var queued: [String] = []
    private var running = 0
    private let limit = 4
    /// The wait before each hash is asked for again.
    private var delays: [String: Duration] = [:]

    /// `retry`: the wait before asking again for an image that is not
    /// there yet; it doubles up to `most`.
    public init(
        model: TranscriptModel, cache: ImageCache = .standard, retry: Duration = .seconds(5), most: Duration = .seconds(60),
        fetch: @escaping @Sendable (String) async throws -> Data?
    ) {
        self.model = model
        self.cache = cache
        self.fetch = fetch
        self.first = retry
        self.most = most
        model.imageWanted = { [weak self] state, number, sha in self?.want(state, number, sha) }
    }

    isolated deinit {
        for task in tasks.values { task.cancel() }
    }

    /// Whether any image is still being waited for (for the tests).
    public var pending: Int { tasks.count }

    private func want(_ state: ItemState, _ number: UInt32, _ sha: String) {
        guard ImageCache.isHash(sha) else { return }
        waiting[sha, default: []].append((state, number))
        guard tasks[sha] == nil, !queued.contains(sha) else { return }
        queued.append(sha)
        startQueued()
    }

    private func startQueued() {
        while running < limit, !queued.isEmpty {
            running += 1
            start(queued.removeFirst())
        }
    }

    /// One attempt for `sha`, holding one of the `limit` slots only while
    /// it runs; one that finds nothing waits, off the slot, and queues again.
    private func start(_ sha: String) {
        let session = model.sessionID
        let cache = self.cache
        let fetch = self.fetch
        tasks[sha] = Task { [weak self] in
            let found = await Self.find(sha, session: session, cache: cache, fetch: fetch)
            guard let self, !Task.isCancelled else { return }
            running -= 1
            if let found {
                arrived(sha, found)
                startQueued()
                return
            }
            startQueued()
            // Not there yet, or not an image: asked again later.
            let wait = delays[sha] ?? first
            delays[sha] = min(wait * 2, most)
            try? await Task.sleep(for: wait)
            guard !Task.isCancelled else { return }
            queued.append(sha)
            startQueued()
        }
    }

    /// The image's pictures: from the device's copy, else fetched (blocking
    /// in the core: off the main thread) and kept. Bytes that aren't an
    /// image are dropped, and not kept.
    private nonisolated static func find(
        _ sha: String, session: String, cache: ImageCache, fetch: @escaping @Sendable (String) async throws -> Data?
    ) async -> (UIImage, UIImage)? {
        await Task.detached { () -> (UIImage, UIImage)? in
            if let kept = cache.data(session: session, sha256: sha) {
                if let pictures = decode(kept) { return pictures }
                cache.remove(session: session, sha256: sha)
            }
            guard let fetched = try? await fetch(sha), let pictures = decode(fetched) else { return nil }
            cache.store(fetched, session: session, sha256: sha)
            return pictures
        }.value
    }

    /// The whole picture and its thumbnail, from the bytes alone
    /// (`UIImage(data:)`, R-IOS-10).
    private nonisolated static func decode(_ data: Data) -> (UIImage, UIImage)? {
        guard let whole = UIImage(data: data) else { return nil }
        let scale = min(1, thumbnailSide / max(whole.size.width, whole.size.height))
        let size = CGSize(width: max(1, whole.size.width * scale), height: max(1, whole.size.height * scale))
        return (whole, whole.preparingThumbnail(of: size) ?? whole)
    }

    private func arrived(_ sha: String, _ pictures: (UIImage, UIImage)) {
        tasks[sha] = nil
        delays[sha] = nil
        for (state, number) in waiting.removeValue(forKey: sha) ?? [] {
            state.pictures[number] = pictures.0
            state.thumbnails[number] = pictures.1
        }
    }
}
