import CryptoKit
import Foundation
import ImageIO
import UIKit
import UniformTypeIdentifiers

/// An image ready to send (R-IMG-14): PNG or JPEG, at most 2000 px on its
/// longest side and 2.75 MiB, named by the sha256 of its bytes. `thumbnail`
/// is a small JPEG for the strip above the field.
public struct PreparedImage: Equatable, Sendable, Identifiable {
    public let id: UUID
    public let data: Data
    /// `image/png` or `image/jpeg`.
    public let mediaType: String
    /// Lowercase hex.
    public let sha256: String
    public let thumbnail: Data

    public init(id: UUID = UUID(), data: Data, mediaType: String, sha256: String, thumbnail: Data) {
        self.id = id
        self.data = data
        self.mediaType = mediaType
        self.sha256 = sha256
        self.thumbnail = thumbnail
    }
}

/// An image a prompt or steer names, with no bytes: they went ahead in a
/// `put_image` (R-IMG-2). `number` is its `[Image #N]`.
public struct SentImage: Equatable, Sendable {
    public let number: Int
    public let mediaType: String
    public let sha256: String

    public init(number: Int, mediaType: String, sha256: String) {
        self.number = number
        self.mediaType = mediaType
        self.sha256 = sha256
    }
}

/// Why an image didn't go to the host.
public struct ImageUploadError: Error, Equatable, Sendable {
    public let message: String

    public init(_ message: String) {
        self.message = message
    }
}

/// What every composer does to an image before it is sent (R-IMG-8, the
/// TUI's rules): read whatever the system can (HEIC included), scale to at
/// most 2000 px on the longest side, write a PNG, and write a JPEG instead
/// when the PNG is over 2.75 MiB. Re-encoding drops the photo's metadata
/// (its location among it).
public enum ImageNormalizer {
    /// The most one image may be: the host's 2.75 MiB.
    public static let maxBytes = 2816 * 1024
    public static let maxSide = 2000
    static let thumbnailSide = 160
    /// The JPEG qualities tried, best first, when the PNG is too big.
    static let qualities: [Double] = [0.85, 0.6, 0.4, 0.2]

    /// `source` as an image ready to send, or nil when it isn't an image
    /// the system reads or can't be made to fit.
    public static func normalize(_ source: Data, maxBytes: Int = ImageNormalizer.maxBytes, maxSide: Int = ImageNormalizer.maxSide) -> PreparedImage? {
        guard let reader = CGImageSourceCreateWithData(source as CFData, nil),
              CGImageSourceGetCount(reader) > 0,
              let properties = CGImageSourceCopyPropertiesAtIndex(reader, 0, nil) as? [CFString: Any],
              let width = properties[kCGImagePropertyPixelWidth] as? Int,
              let height = properties[kCGImagePropertyPixelHeight] as? Int,
              width > 0, height > 0
        else { return nil }
        // The thumbnail call scales up as readily as down: never past the
        // image's own size.
        let side = min(maxSide, max(width, height))
        guard let image = scaled(reader, side: side) else { return nil }

        var encoded: (Data, String)?
        if let png = encode(image, as: .png), png.count <= maxBytes {
            encoded = (png, "image/png")
        } else {
            let flat = flattened(image)
            for quality in qualities {
                if let jpeg = encode(flat, as: .jpeg, quality: quality), jpeg.count <= maxBytes {
                    encoded = (jpeg, "image/jpeg")
                    break
                }
            }
        }
        guard let (data, mediaType) = encoded else { return nil }
        let small = scaled(from: image, side: thumbnailSide).flatMap { encode(flattened($0), as: .jpeg, quality: 0.7) } ?? data
        return PreparedImage(data: data, mediaType: mediaType, sha256: hex(data), thumbnail: small)
    }

    static func hex(_ data: Data) -> String {
        SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }

    private static func scaled(_ reader: CGImageSource, side: Int) -> CGImage? {
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceShouldCacheImmediately: true,
            kCGImageSourceThumbnailMaxPixelSize: side,
        ]
        return CGImageSourceCreateThumbnailAtIndex(reader, 0, options as CFDictionary)
    }

    private static func scaled(from image: CGImage, side: Int) -> CGImage? {
        let longest = max(image.width, image.height)
        guard longest > side else { return image }
        let ratio = Double(side) / Double(longest)
        let size = CGSize(width: max(1, Int(Double(image.width) * ratio)), height: max(1, Int(Double(image.height) * ratio)))
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        return UIGraphicsImageRenderer(size: size, format: format).image { _ in
            UIImage(cgImage: image).draw(in: CGRect(origin: .zero, size: size))
        }.cgImage
    }

    /// JPEG has no alpha: a transparent image goes onto white.
    private static func flattened(_ image: CGImage) -> CGImage {
        switch image.alphaInfo {
        case .none, .noneSkipFirst, .noneSkipLast: return image
        default: break
        }
        let size = CGSize(width: image.width, height: image.height)
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        format.opaque = true
        return UIGraphicsImageRenderer(size: size, format: format).image { context in
            UIColor.white.setFill()
            context.fill(CGRect(origin: .zero, size: size))
            UIImage(cgImage: image).draw(in: CGRect(origin: .zero, size: size))
        }.cgImage ?? image
    }

    private static func encode(_ image: CGImage, as type: UTType, quality: Double? = nil) -> Data? {
        let out = NSMutableData()
        guard let writer = CGImageDestinationCreateWithData(out, type.identifier as CFString, 1, nil) else { return nil }
        var options: [CFString: Any] = [:]
        if let quality { options[kCGImageDestinationLossyCompressionQuality] = quality }
        CGImageDestinationAddImage(writer, image, options as CFDictionary)
        return CGImageDestinationFinalize(writer) ? out as Data : nil
    }
}

/// The images attached to the message being typed (R-IMG-14): added from a
/// paste or the photo picker, up to 20, each normalized off the main
/// thread, and removed before sending.
@MainActor
public final class ComposerImages {
    public nonisolated static let limit = 20

    public private(set) var images: [PreparedImage] = []
    /// Called when `images` changed.
    public var changed: (@MainActor () -> Void)?

    public init() {}

    /// What `add` did: how many images joined, how many were left out for
    /// the 20 cap, and how many the system couldn't read.
    public struct Added: Equatable, Sendable {
        public var added = 0
        public var overCap = 0
        public var unreadable = 0

        /// What to tell the person, if anything went wrong.
        public var problem: String? {
            var parts: [String] = []
            if overCap > 0 { parts.append("A message takes at most \(ComposerImages.limit) images") }
            if unreadable > 0 { parts.append(unreadable == 1 ? "One image couldn't be read" : "\(unreadable) images couldn't be read") }
            return parts.isEmpty ? nil : parts.joined(separator: "; ") + "."
        }
    }

    /// Attaches `sources`, as read from the clipboard or the picker, in
    /// order. Those past the cap are not read at all.
    @discardableResult
    public func add(_ sources: [Data]) async -> Added {
        var result = Added()
        let room = max(0, Self.limit - images.count)
        let taking = Array(sources.prefix(room))
        result.overCap = sources.count - taking.count
        // In order, one after another: a long batch never holds more than
        // one decoded image at a time.
        for source in taking {
            let prepared = await Task.detached(priority: .userInitiated) { ImageNormalizer.normalize(source) }.value
            guard let prepared else {
                result.unreadable += 1
                continue
            }
            // Another add may have filled the room meanwhile.
            guard images.count < Self.limit else {
                result.overCap += 1
                continue
            }
            images.append(prepared)
            result.added += 1
        }
        if result.added > 0 { changed?() }
        return result
    }

    public func remove(_ id: UUID) {
        guard let at = images.firstIndex(where: { $0.id == id }) else { return }
        images.remove(at: at)
        changed?()
    }

    public func clear() {
        guard !images.isEmpty else { return }
        images = []
        changed?()
    }

    /// The room left.
    public var room: Int { Self.limit - images.count }

    /// `[Image #1] [Image #2]` for the images, numbered as they sit: the
    /// text of a message that carries them.
    public static func tokens(_ count: Int) -> String {
        (0..<count).map { "[Image #\($0 + 1)]" }.joined(separator: " ")
    }

    /// The prompt's text with its image tokens in front, and the images it
    /// names (R-IMG-2).
    public func message(_ text: String) -> (text: String, images: [SentImage]) {
        Self.message(text, images: images)
    }

    public static func message(_ text: String, images: [PreparedImage]) -> (text: String, images: [SentImage]) {
        guard !images.isEmpty else { return (text, []) }
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        let named = images.enumerated().map { SentImage(number: $0.offset + 1, mediaType: $0.element.mediaType, sha256: $0.element.sha256) }
        return (trimmed.isEmpty ? tokens(images.count) : tokens(images.count) + "\n" + text, named)
    }
}
