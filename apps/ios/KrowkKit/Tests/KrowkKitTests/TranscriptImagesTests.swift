// R-IMG-13, R-IMG-15: a prompt's images in the transcript. `[Image #N]`
// stays until the device holds the image's bytes (kept in the caches
// folder, else fetched from the session's own store through the core),
// then gives way to a thumbnail; a tap opens the image full screen. The
// bytes are drawn with `UIImage(data:)` and never read from a URL
// (R-IOS-10).
import Foundation
import Synchronization
import Testing
import UIKit
@testable import KrowkKit

/// A value the fetch closures change and the test reads.
final class Locked<Value: Sendable>: Sendable {
    let value: Mutex<Value>
    init(_ initial: Value) { value = Mutex(initial) }
}

@MainActor
@Suite("R-IMG-13, R-IMG-15: images in a prompt's bubble")
struct TranscriptImagesTests {
    static let sha = String(repeating: "ab", count: 32)
    let row = SessionRow(id: "s1", title: "A demo session", detail: "app", live: true, accessibilityLabel: "A demo session")

    /// A small PNG.
    static func png() -> Data {
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        let renderer = UIGraphicsImageRenderer(size: CGSize(width: 40, height: 30), format: format)
        return renderer.pngData { context in
            UIColor.red.setFill()
            context.fill(CGRect(x: 0, y: 0, width: 40, height: 30))
        }
    }

    static func cache() -> ImageCache {
        ImageCache(root: FileManager.default.temporaryDirectory.appendingPathComponent("images-\(UUID().uuidString)", isDirectory: true))
    }

    static func prompt(hashes: [UInt32: String] = [1: sha]) -> TranscriptEvent {
        TranscriptEvent(id: "e1", sessionID: "s1", change: .userText(item: "u1", text: "Look at [Image #1] please", images: [1], hashes: hashes))
    }

    /// Waits up to 3 s for `done`; a miss is reported at the caller's line.
    private func waitForImage(_ done: () -> Bool, sourceLocation: SourceLocation = #_sourceLocation) async throws {
        for _ in 0..<300 where !done() {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(done(), sourceLocation: sourceLocation)
    }

    @Test("The placeholder stays while the bytes are not there, then the thumbnail replaces it")
    func swap() async throws {
        let answers = Locked<[Data?]>([nil, nil, Self.png()])
        let asked = Locked(0)
        let model = TranscriptModel(sessionID: "s1")
        let images = TranscriptImages(model: model, cache: Self.cache(), retry: .milliseconds(20), most: .milliseconds(20)) { _ in
            asked.value.withLock { $0 += 1 }
            return answers.value.withLock { $0.isEmpty ? nil : $0.removeFirst() }
        }
        model.apply([Self.prompt()])
        let state = try #require(model.item("item:u1"))
        #expect(state.displayText == "Look at [Image #1] please")
        #expect(state.thumbnails.isEmpty)

        try await Task.sleep(for: .milliseconds(10))
        #expect(state.thumbnails.isEmpty, "no bytes yet: the placeholder")
        #expect(state.displayText.contains("[Image #1]"))

        try await waitForImage { !state.thumbnails.isEmpty }
        #expect(asked.value.withLock { $0 } == 3, "asked again later until the bytes came")
        #expect(state.thumbnails[1] != nil)
        #expect(state.pictures[1]?.size == CGSize(width: 40, height: 30))
        #expect(!state.displayText.contains("[Image #1]"))
        #expect(state.displayText == "Look at please")
        #expect(state.text == "Look at [Image #1] please", "the session's text is as it was")
        try await waitForImage { images.pending == 0 }
    }

    @Test("An image without a hash, or whose bytes never come, stays a placeholder and nothing is shown as an error")
    func placeholderStays() async throws {
        let model = TranscriptModel(sessionID: "s1")
        let asked = Locked(0)
        let images = TranscriptImages(model: model, cache: Self.cache(), retry: .milliseconds(10), most: .milliseconds(10)) { _ in
            asked.value.withLock { $0 += 1 }
            return nil
        }
        model.apply([Self.prompt(hashes: [:])])
        try await Task.sleep(for: .milliseconds(50))
        #expect(asked.value.withLock { $0 } == 0, "no hash: nothing to ask for")
        #expect(model.item("item:u1")?.thumbnails.isEmpty == true)

        let other = TranscriptModel(sessionID: "s1")
        let again = TranscriptImages(model: other, cache: Self.cache(), retry: .milliseconds(10), most: .milliseconds(10)) { _ in
            asked.value.withLock { $0 += 1 }
            return nil
        }
        other.apply([Self.prompt()])
        try await waitForImage { asked.value.withLock { $0 } >= 2 }
        let state = try #require(other.item("item:u1"))
        #expect(state.thumbnails.isEmpty)
        #expect(state.displayText == "Look at [Image #1] please")
        #expect(again.pending == 1)
        _ = images
    }

    @Test("Bytes the device holds are drawn without asking the core, and fetched bytes are kept for next time")
    func cached() async throws {
        let cache = Self.cache()
        cache.store(Self.png(), session: "s1", sha256: Self.sha)
        let asked = Locked(0)
        let model = TranscriptModel(sessionID: "s1")
        let images = TranscriptImages(model: model, cache: cache) { _ in
            asked.value.withLock { $0 += 1 }
            return nil
        }
        model.apply([Self.prompt()])
        let state = try #require(model.item("item:u1"))
        try await waitForImage { !state.thumbnails.isEmpty }
        #expect(asked.value.withLock { $0 } == 0)

        let bytes = Self.png()
        let fresh = Self.cache()
        let other = TranscriptModel(sessionID: "s1")
        let fetching = TranscriptImages(model: other, cache: fresh) { _ in bytes }
        other.apply([Self.prompt()])
        let second = try #require(other.item("item:u1"))
        try await waitForImage { !second.thumbnails.isEmpty }
        #expect(fresh.data(session: "s1", sha256: Self.sha) == bytes)
        _ = (images, fetching)
    }

    @Test("A prompt's cell draws the thumbnail in place of its placeholder, and a tap opens the image full screen, with a close button")
    func viewer() async throws {
        let model = TranscriptModel(sessionID: "s1")
        model.apply([Self.prompt(hashes: [:])])
        let screen = TranscriptViewController(row: row, model: model)
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = UINavigationController(rootViewController: screen)
        window.makeKeyAndVisible()
        window.layoutIfNeeded()
        defer { window.isHidden = true }

        try await waitForImage { screen.collectionView.cellForItem(at: IndexPath(item: 0, section: 0)) != nil }
        window.layoutIfNeeded()
        let cell = try #require(screen.collectionView.cellForItem(at: IndexPath(item: 0, section: 0)) as? TranscriptTextCell)
        #expect(cell.textView.view.text == "Look at [Image #1] please")

        let state = try #require(model.item("item:u1"))
        let picture = try #require(UIImage(data: Self.png()))
        state.pictures[1] = picture
        state.thumbnails[1] = picture
        try await waitForImage { cell.textView.view.text == "Look at please" }
        window.layoutIfNeeded()
        let thumb = try #require(Self.find("transcript-image-1", in: cell) as? UIImageView)
        #expect(thumb.image === picture)

        screen.showImage(state, 1)
        try await waitForImage { screen.presentedViewController is ImageViewerController }
        let viewer = try #require(screen.presentedViewController as? ImageViewerController)
        #expect(viewer.imageView.image === picture)
        // This window has no scene, so a presentation here never finishes
        // and a dismiss is never seen through; the close button is checked
        // for its action instead.
        #expect(viewer.closeButton.allControlEvents.contains(.primaryActionTriggered))
    }

    static func find(_ identifier: String, in view: UIView) -> UIView? {
        if view.accessibilityIdentifier == identifier { return view }
        for sub in view.subviews { if let found = find(identifier, in: sub) { return found } }
        return nil
    }
}
