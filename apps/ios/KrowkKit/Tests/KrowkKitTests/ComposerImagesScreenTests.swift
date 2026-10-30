// R-IMG-2, R-IMG-5, R-IMG-14: the composer sends images over a stub
// session standing for the link: each goes as a put_image, one after
// another, before the prompt that names them by number and sha256; a failed
// upload sends nothing and leaves the composer as it was; the thumbnails
// have a remove button and the photo button a label. Through the fleet
// the same is AppTests' ComposerTests.
import Testing
import UIKit
@testable import KrowkKit

@MainActor
@Suite("R-IMG-2, R-IMG-14: the composer's images on screen")
struct ComposerImagesScreenTests {
    /// The buttons under `view` with this accessibility label.
    static func buttons(in view: UIView, labelled label: String) -> [UIButton] {
        var found: [UIButton] = []
        if let button = view as? UIButton, button.accessibilityLabel == label { found.append(button) }
        for sub in view.subviews { found += buttons(in: sub, labelled: label) }
        return found
    }

    private func waitFor(_ done: () -> Bool) async throws {
        for _ in 0..<300 where !done() {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(done())
    }

    @Test("R-IMG-2: Send puts each image to the host, one after another, and only then sends the prompt naming them by number and sha256; the composer then empties")
    func putsThenPrompts() async throws {
        let (session, screen, window) = try await ComposerScreenTests().opened()
        defer { window.isHidden = true }
        await screen.composer.attach([TestImages.png(40, 30, .red), TestImages.png(40, 30, .green), TestImages.png(40, 30, .blue)])
        let images = screen.composer.attachments.images
        #expect(images.count == 3)
        screen.composer.text = "What is this?"
        #expect(screen.composer.sendButton.isEnabled)

        screen.sendTyped()
        try await waitFor { session.commands.withLock { $0 }.count == 1 }
        #expect(session.order.withLock { $0 } == images.map { "put \($0.sha256)" } + ["send"], "every image first, in order, the prompt last")
        let sent = try #require(session.commands.withLock { $0 }.last)
        let named = images.enumerated().map { SentImage(number: $0.offset + 1, mediaType: $0.element.mediaType, sha256: $0.element.sha256) }
        #expect(sent.command == .prompt("[Image #1] [Image #2] [Image #3]\nWhat is this?", images: named))

        try await waitFor { screen.composer.attachments.images.isEmpty }
        #expect(screen.composer.text.isEmpty)
        #expect(screen.composer.sending == nil)
        #expect(screen.composer.textView.isEditable)
        let id = try #require(screen.model.ids.last)
        #expect(screen.model.item(id)?.text == "[Image #1] [Image #2] [Image #3]\nWhat is this?", "the bubble shows the tokens")
    }

    @Test("R-IMG-2: the same image attached twice is put once and named twice")
    func sameImageOnce() async throws {
        let (session, screen, window) = try await ComposerScreenTests().opened()
        defer { window.isHidden = true }
        let flat = TestImages.png(40, 30, .red)
        await screen.composer.attach([flat, flat])
        let sha = try #require(screen.composer.attachments.images.first?.sha256)
        screen.sendTyped()
        try await waitFor { session.commands.withLock { $0 }.count == 1 }
        #expect(session.order.withLock { $0 } == ["put \(sha)", "send"])
        let sent = try #require(session.commands.withLock { $0 }.last)
        guard case .prompt(_, let images) = sent.command else {
            Issue.record("expected a prompt, got \(sent.command)")
            return
        }
        #expect(images.map { $0.number } == [1, 2])
        #expect(images.map { $0.sha256 } == [sha, sha])
    }

    @Test("R-IMG-5: a failed upload sends nothing and leaves the composer as it was, saying why; sending again works")
    func failedUpload() async throws {
        let (session, screen, window) = try await ComposerScreenTests().opened()
        defer { window.isHidden = true }
        await screen.composer.attach([TestImages.png(40, 30, .red)])
        screen.composer.text = "Look"
        session.uploadFailure.withLock { $0 = "the host has no room" }

        screen.sendTyped()
        try await waitFor { screen.composer.imageNote != nil }
        #expect(session.commands.withLock { $0 }.isEmpty, "no prompt went")
        #expect(screen.composer.text == "Look")
        #expect(screen.composer.attachments.images.count == 1)
        #expect(screen.composer.sending == nil)
        #expect(screen.composer.textView.isEditable)
        #expect(screen.composer.noteLabel.text == "the host has no room")
        #expect(screen.model.ids.filter { $0.hasPrefix("pending:") }.isEmpty, "no bubble")

        session.uploadFailure.withLock { $0 = nil }
        screen.sendTyped()
        try await waitFor { session.commands.withLock { $0 }.count == 1 }
        #expect(screen.composer.attachments.images.isEmpty)
    }

    @Test("R-IMG-14: images alone are a message, and while a turn runs they go as a steer")
    func imagesAlone() async throws {
        let (session, screen, window) = try await ComposerScreenTests().opened()
        defer { window.isHidden = true }
        session.bring([.logged([ComposerScreenTests.event("e7", .turnStarted(turn: "t2"), parent: "e6")])])
        screen.frame()
        await screen.composer.attach([TestImages.png(40, 30, .red)])
        #expect(screen.composer.sendButton.configuration?.title == "Steer")
        #expect(screen.composer.sendButton.isEnabled, "no text typed")
        screen.sendTyped()
        try await waitFor { session.commands.withLock { $0 }.count == 1 }
        let sent = try #require(session.commands.withLock { $0 }.last)
        guard case .steer(let text, let images) = sent.command else {
            Issue.record("expected a steer, got \(sent.command)")
            return
        }
        #expect(text == "[Image #1]")
        #expect(images.map { $0.number } == [1])
    }

    @Test("R-IMG-14: each thumbnail has a Remove Image button that takes it away; the photo button is labelled Add Photo")
    func removeButtons() async throws {
        let (_, screen, window) = try await ComposerScreenTests().opened()
        defer { window.isHidden = true }
        #expect(screen.composer.photoButton.accessibilityLabel == "Add Photo")
        #expect(Self.buttons(in: screen.composer, labelled: "Remove Image").isEmpty)
        await screen.composer.attach([TestImages.png(40, 30, .red), TestImages.png(40, 30, .green)])
        #expect(!screen.composer.strip.isHidden)
        let remove = Self.buttons(in: screen.composer, labelled: "Remove Image")
        #expect(remove.count == 2)
        remove[0].sendActions(for: .primaryActionTriggered)
        #expect(screen.composer.attachments.images.count == 1)
        #expect(Self.buttons(in: screen.composer, labelled: "Remove Image").count == 1)
        Self.buttons(in: screen.composer, labelled: "Remove Image")[0].sendActions(for: .primaryActionTriggered)
        #expect(screen.composer.strip.isHidden)
    }

    @Test("R-IMG-14: the photo button asks the screen for the picker")
    func photoButton() {
        let composer = ComposerView()
        let asked = Counter()
        composer.addPhoto = { asked.bump() }
        composer.photoButton.sendActions(for: .primaryActionTriggered)
        #expect(asked.count == 1)
    }

    @Test("R-IMG-14: an image copied to the clipboard is read from it as an image of its own size")
    func clipboard() {
        let board = UIPasteboard.withUniqueName()
        defer { UIPasteboard.remove(withName: board.name) }
        let flat = TestImages.png(20, 20, .red)
        board.setData(flat, forPasteboardType: "public.png")
        // As bytes, or as the image the system made of them: an image of the
        // same size either way.
        let found = PasteTextView.clipboardImages(board)
        #expect(found.count == 1)
        #expect(found.first.flatMap { TestImages.size($0)?.width } == 20)
    }
}
