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

    private func file(_ session: String, _ sha256: String) -> URL {
        root.appendingPathComponent(session, isDirectory: true).appendingPathComponent(sha256)
    }

    /// The bytes kept for `sha256`, if any.
    public func data(session: String, sha256: String) -> Data? {
        try? Data(contentsOf: file(session, sha256))
    }

    /// Keeps `data`; a failure only means it is fetched again.
    public func store(_ data: Data, session: String, sha256: String) {
        let url = file(session, sha256)
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
        waiting[sha, default: []].append((state, number))
        guard tasks[sha] == nil else { return }
        let session = model.sessionID
        let cache = self.cache
        let fetch = self.fetch
        let first = self.first
        let most = self.most
        tasks[sha] = Task { [weak self] in
            var wait = first
            while !Task.isCancelled {
                let data = await Task.detached { () -> Data? in
                    if let kept = cache.data(session: session, sha256: sha) { return kept }
                    guard let fetched = try? await fetch(sha) else { return nil }
                    cache.store(fetched, session: session, sha256: sha)
                    return fetched
                }.value
                if let data, let pictures = await Self.decode(data) {
                    self?.arrived(sha, pictures)
                    return
                }
                // Not there yet, or not an image: asked again later.
                try? await Task.sleep(for: wait)
                wait = min(wait * 2, most)
            }
        }
    }

    /// The whole picture and its thumbnail, made off the main thread, from
    /// the bytes alone (`UIImage(data:)`, R-IOS-10).
    private nonisolated static func decode(_ data: Data) async -> (UIImage, UIImage)? {
        await Task.detached { () -> (UIImage, UIImage)? in
            guard let whole = UIImage(data: data) else { return nil }
            let side = thumbnailSide
            let scale = min(1, side / max(whole.size.width, whole.size.height))
            let size = CGSize(width: max(1, whole.size.width * scale), height: max(1, whole.size.height * scale))
            return (whole, whole.preparingThumbnail(of: size) ?? whole)
        }.value
    }

    private func arrived(_ sha: String, _ pictures: (UIImage, UIImage)) {
        tasks[sha] = nil
        for (state, number) in waiting.removeValue(forKey: sha) ?? [] {
            state.pictures[number] = pictures.0
            state.thumbnails[number] = pictures.1
        }
    }
}
