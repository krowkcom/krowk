import Foundation
import UIKit

/// Agent output, made safe to draw (R-IOS-10). A prompt injection can put
/// links, control characters and image markdown in anything an agent or a
/// tool says, so every transcript cell draws its text through here, and
/// nothing else:
///
/// - Control characters are stripped, keeping `\n` and `\t`: C0 but those
///   two, DEL, C1 (U+0080–U+009F), and the bidi overrides and isolates
///   (U+202A–U+202E, U+2066–U+2069), which can make a path or a URL read
///   backwards.
/// - Nothing is fetched, ever: markdown isn't rendered (M1), so an image's
///   markdown is its text, and the app has no web view.
/// - A link is an `http` or `https` URL written out with its scheme, as
///   `NSDataDetector` finds it, to a host in ASCII. Its full host follows
///   it in the text — `[docs](https://evil.example)` reads
///   `… (evil.example)` — and a tap only asks (`TranscriptViewController`
///   names the whole URL, and only its "Open" opens it). Any other scheme,
///   `javascript:` included, and a bare `example.com`, stay plain text.
public enum SafeText {
    /// At most this much of a tool's output is laid out, expanded: a 64 KiB
    /// output in a self-sizing TextKit 2 view lays out slowly.
    public static let outputLimit = 64 * 1024

    /// `text` without the characters a transcript never draws.
    public static func clean(_ text: String) -> String {
        var scalars = String.UnicodeScalarView()
        scalars.append(contentsOf: text.unicodeScalars.lazy.filter { !stripped($0) })
        return String(scalars)
    }

    /// Whether `scalar` is one `clean` takes out.
    public static func stripped(_ scalar: Unicode.Scalar) -> Bool {
        switch scalar.value {
        case 0x09, 0x0A: false
        case 0x00...0x1F, 0x7F, 0x80...0x9F: true
        case 0x202A...0x202E, 0x2066...0x2069: true
        default: false
        }
    }

    /// One line of `text`, cleaned, for a row that has one: its first
    /// line, tabs as spaces, at most `limit` characters.
    public static func line(_ text: Substring, limit: Int = 200) -> String {
        let first = text.prefix { $0 != "\n" && $0 != "\r\n" }
        let cut = first.prefix(limit)
        let line = clean(String(cut)).replacingOccurrences(of: "\t", with: " ")
        return cut.count < first.count ? line + "…" : line
    }

    /// A tappable link in a cleaned text: where it is, where it goes, and
    /// the host shown beside it.
    public struct Link: Equatable, Sendable {
        public let range: Range<String.Index>
        public let url: URL
        public let host: String
    }

    /// Immutable once made, and so safe to share between threads.
    private static let detector = try? NSDataDetector(types: NSTextCheckingResult.CheckingType.link.rawValue)

    /// The links in `text` (cleaned), by `NSDataDetector`: only those
    /// written with an `http://` or `https://` scheme, to an ASCII host.
    public static func links(in text: String) -> [Link] {
        guard let detector else { return [] }
        let whole = NSRange(text.startIndex..., in: text)
        return detector.matches(in: text, options: [], range: whole).compactMap { match in
            guard let url = match.url, let range = Range(match.range, in: text) else { return nil }
            return link(url, written: text[range], at: range)
        }
    }

    /// `url` as a link, if it may be one: `written` carries its scheme, which
    /// is http or https, and its host is ASCII.
    static func link(_ url: URL, written: Substring, at range: Range<String.Index>) -> Link? {
        let lower = written.lowercased()
        guard lower.hasPrefix("https://") || lower.hasPrefix("http://"),
              let scheme = url.scheme?.lowercased(), scheme == "https" || scheme == "http",
              let host = url.host(percentEncoded: false), !host.isEmpty,
              host.unicodeScalars.allSatisfy({ $0.isASCII && $0.value > 0x20 && $0.value < 0x7F })
        else { return nil }
        return Link(range: range, url: url, host: host)
    }

    /// `text` as a transcript draws it: cleaned, in `font` and `color`,
    /// each link tappable (`.link`, its URL) with its host after it in the
    /// secondary colour, not tappable.
    public static func attributed(_ text: String, font: UIFont, color: UIColor = .label) -> NSAttributedString {
        let text = clean(text)
        let out = NSMutableAttributedString(string: text, attributes: [.font: font, .foregroundColor: color])
        // From the end, so an inserted host leaves the ranges before it be.
        for link in links(in: text).reversed() {
            let range = NSRange(link.range, in: text)
            out.addAttribute(.link, value: link.url, range: range)
            let host = NSAttributedString(string: " (\(link.host))", attributes: [.font: font, .foregroundColor: UIColor.secondaryLabel])
            out.insert(host, at: range.upperBound)
        }
        return out
    }

    /// At most `limit` bytes of `output` (UTF-8), cut at a character, and
    /// how many bytes were left out.
    public static func truncated(_ output: String, limit: Int = outputLimit) -> (shown: String, more: Int) {
        let utf8 = output.utf8
        guard utf8.count > limit else { return (output, 0) }
        var end = utf8.index(utf8.startIndex, offsetBy: limit)
        // Back to the start of the character it falls in.
        while end > utf8.startIndex, end.samePosition(in: output) == nil {
            end = utf8.index(before: end)
        }
        let shown = String(output[..<end])
        return (shown, utf8.count - shown.utf8.count)
    }

    /// What follows a truncated output: "… 1,234 more bytes".
    public static func more(_ bytes: Int) -> String {
        "… \(bytes.formatted()) more byte\(bytes == 1 ? "" : "s")"
    }
}
