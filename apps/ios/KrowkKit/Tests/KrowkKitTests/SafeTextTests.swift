// R-IOS-10: the iPhone treats agent output as hostile. Control and
// bidi-override characters are stripped, no remote image loads (an image's
// markdown stays text), and a link shows its full host and opens only
// after a tap that names the whole URL. Every transcript cell draws
// through SafeText; the sheet a link opens is TranscriptViewController's.
import Foundation
import Testing
import UIKit
@testable import KrowkKit

@Suite("R-IOS-10: agent output is drawn as hostile text")
struct SafeTextTests {
    let font = UIFont.preferredFont(forTextStyle: .body)

    @Test("R-IOS-10: C0 controls but \\n and \\t, DEL and C1 are stripped")
    func controls() {
        let hostile = "a\u{0}b\u{7}c\u{1B}[31md\u{8}e\u{7F}f\u{85}g\u{9B}h\r\n\ti"
        #expect(SafeText.clean(hostile) == "abc[31mdefgh\n\ti")
        for c in (0x00...0x1F).compactMap(Unicode.Scalar.init) where c != "\n" && c != "\t" {
            #expect(SafeText.clean("x\(c)y") == "xy", "U+\(String(c.value, radix: 16))")
        }
        for v in [0x7F] + Array(0x80...0x9F) {
            #expect(SafeText.clean("x\(Unicode.Scalar(v)!)y") == "xy", "U+\(String(v, radix: 16))")
        }
        #expect(SafeText.clean("line\nnext\ttab") == "line\nnext\ttab")
    }

    @Test("R-IOS-10: the bidi overrides and isolates (U+202A–U+202E, U+2066–U+2069) are stripped, so a path can't read backwards")
    func bidi() {
        for v in Array(0x202A...0x202E) + Array(0x2066...0x2069) {
            #expect(SafeText.clean("x\(Unicode.Scalar(v)!)y") == "xy", "U+\(String(v, radix: 16))")
        }
        // "invoice‮fdp.exe" reads as "invoiceexe.pdf" with its override.
        #expect(SafeText.clean("invoice\u{202E}fdp.exe") == "invoicefdp.exe")
        // Other text, emoji and scripts stay.
        #expect(SafeText.clean("héllo 👋🏽 שלום") == "héllo 👋🏽 שלום")
        // Drawn, too: the attributed text is the cleaned text.
        #expect(!SafeText.attributed("a\u{202E}b\u{0}c", font: font).string.unicodeScalars.contains { SafeText.stripped($0) })
    }

    @Test("R-IOS-10: a markdown image is shown as its text: nothing is fetched, no attachment is drawn")
    func imageStaysText() {
        let markdown = "Look: ![chart](https://evil.example/pixel.png?u=me)"
        let drawn = SafeText.attributed(markdown, font: font)
        #expect(drawn.string.hasPrefix("Look: ![chart](https://evil.example/pixel.png?u=me"))
        var attachments = 0
        drawn.enumerateAttribute(.attachment, in: NSRange(location: 0, length: drawn.length)) { value, _, _ in
            if value != nil { attachments += 1 }
        }
        #expect(attachments == 0)
    }

    @Test("R-IOS-10: a javascript: link, another scheme and a bare host are not tappable")
    func onlyHTTP() {
        for text in [
            "javascript:alert(1)",
            "[docs](javascript:alert(document.cookie))",
            "ftp://files.example/x",
            "file:///etc/passwd",
            "data:text/html,<script>alert(1)</script>",
            "tel:+15551234567",
            "see example.com or README.md",
            "mailto:a@example.com",
        ] {
            #expect(SafeText.links(in: text).isEmpty, "\(text) has a link")
            let drawn = SafeText.attributed(text, font: font)
            #expect(Self.links(in: drawn).isEmpty, "\(text) drew a link")
            #expect(drawn.string == text, "\(text) became \(drawn.string)")
        }
    }

    @Test("R-IOS-10: an https or http link is tappable, with its full host after it, not tappable itself")
    func hostShown() {
        let drawn = SafeText.attributed("Docs at https://docs.example.com/a/b?c=d and http://plain.example.", font: font)
        let links = Self.links(in: drawn)
        #expect(links.map(\.url.host) == ["docs.example.com", "plain.example"])
        #expect(drawn.string.contains("https://docs.example.com/a/b?c=d (docs.example.com)"))
        #expect(drawn.string.contains("http://plain.example (plain.example)"))
        // The host's text isn't part of the link.
        for (url, range) in links {
            let after = (drawn.string as NSString).substring(from: NSMaxRange(range))
            #expect(after.hasPrefix(" (\(url.host!))"), "\(url) then \(after)")
        }
    }

    @Test("R-IOS-10: a link whose text names one host and whose URL another shows the URL's host")
    func hostOfTheURL() {
        let drawn = SafeText.attributed("[good.example](https://evil.example/login)", font: font)
        let links = Self.links(in: drawn)
        #expect(links.map(\.url.host) == ["evil.example"], "\(links)")
        #expect(drawn.string.contains("(evil.example)"))
        #expect(!drawn.string.contains("(good.example)"))

        // Userinfo that reads like a host: the host is what follows the @.
        let userinfo = SafeText.attributed("https://good.example@evil.example/", font: font)
        #expect(Self.links(in: userinfo).map(\.url.host) == ["evil.example"])
        #expect(userinfo.string.contains("(evil.example)"))

        // A lookalike host not in ASCII ("о" is Cyrillic) is never shown as
        // it reads: not tappable, or shown in its ASCII form.
        let lookalike = SafeText.links(in: "https://gооgle.example/")
        #expect(lookalike.allSatisfy { $0.host.hasPrefix("xn--") }, "\(lookalike.map(\.host))")
    }

    @Test("R-IOS-10: a tapped link asks first: a sheet with the whole URL, whose Open alone opens it")
    @MainActor
    func tapAsks() throws {
        let url = try #require(URL(string: "https://evil.example/a?b=c#d"))
        var opened: [URL] = []
        let sheet = TranscriptViewController.confirmation(for: url) { opened.append($0) }
        #expect(sheet.preferredStyle == .actionSheet)
        #expect(sheet.message == "https://evil.example/a?b=c#d")
        #expect(sheet.actions.map(\.title) == ["Open", "Cancel"])
        #expect(opened.isEmpty, "nothing opens without a tap")

        // Tapping a link only presents the sheet: the screen's opener is
        // never called by the tap itself.
        let row = SessionRow(id: "s", title: "t", detail: "d", live: false, accessibilityLabel: "t")
        let screen = TranscriptViewController(row: row, open: { opened.append($0) })
        let window = UIWindow(frame: CGRect(x: 0, y: 0, width: 390, height: 844))
        window.rootViewController = screen
        window.makeKeyAndVisible()
        screen.confirm(url)
        #expect(opened.isEmpty)
        #expect((screen.presentedViewController as? UIAlertController)?.message == url.absoluteString)
        window.isHidden = true
    }

    @Test("A tool's output is shown up to 64 KiB, cut at a character, then how many more bytes")
    func truncation() {
        let output = String(repeating: "é", count: 40_000) // 80,000 bytes
        let (shown, more) = SafeText.truncated(output)
        #expect(shown.utf8.count <= SafeText.outputLimit)
        #expect(shown.utf8.count == 65_536)
        #expect(more == 80_000 - 65_536)
        #expect(SafeText.more(more) == "… \(more.formatted()) more bytes")
        // Three-byte characters: cut before the one that would cross.
        let wide = String(repeating: "€", count: 30_000)
        let cut = SafeText.truncated(wide)
        #expect(cut.shown.utf8.count == 65_535)
        #expect(cut.more == 90_000 - 65_535)
        #expect(SafeText.truncated("short").shown == "short")
        #expect(SafeText.truncated("short").more == 0)
    }

    @Test("One line: the first, cleaned, cut with an ellipsis")
    func oneLine() {
        #expect(SafeText.line("first\u{202E}\nsecond"[...]) == "first")
        #expect(SafeText.line("a\tb"[...]) == "a b")
        #expect(SafeText.line(String(repeating: "x", count: 300)[...], limit: 10) == "xxxxxxxxxx…")
    }

    /// The tappable links in `drawn`: each URL and where it is.
    static func links(in drawn: NSAttributedString) -> [(url: URL, range: NSRange)] {
        var found: [(URL, NSRange)] = []
        drawn.enumerateAttribute(.link, in: NSRange(location: 0, length: drawn.length)) { value, range, _ in
            if let url = value as? URL { found.append((url, range)) }
        }
        return found
    }
}
