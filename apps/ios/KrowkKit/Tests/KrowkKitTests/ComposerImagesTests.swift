// R-IMG-8, R-IMG-14: the iPhone composer's images, over the composer model
// and the normalizer. An image is added from a paste or the photo picker,
// read in whatever the system reads (HEIC too) and made a PNG or a JPEG of
// at most 2000 px a side and 2.75 MiB, named by its sha256; up to 20 may be
// attached, and each can be removed. The send is in
// ComposerImagesScreenTests.
import ImageIO
import Testing
import UIKit
import UniformTypeIdentifiers
@testable import KrowkKit

/// Images for the tests.
enum TestImages {
    /// A flat PNG.
    static func png(_ width: Int, _ height: Int, _ color: UIColor = .red) -> Data {
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        let renderer = UIGraphicsImageRenderer(size: CGSize(width: width, height: height), format: format)
        return renderer.pngData { context in
            color.setFill()
            context.fill(CGRect(x: 0, y: 0, width: width, height: height))
        }
    }

    /// Random pixels, which no encoder makes small.
    static func noise(_ side: Int) -> Data {
        var pixels = [UInt8](repeating: 0, count: side * side * 4)
        for at in pixels.indices { pixels[at] = UInt8.random(in: 0...255) }
        let data = Data(pixels)
        let image = CGImage(
            width: side, height: side, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: side * 4,
            space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.noneSkipLast.rawValue),
            provider: CGDataProvider(data: data as CFData)!, decode: nil, shouldInterpolate: false, intent: .defaultIntent
        )!
        return UIImage(cgImage: image).pngData()!
    }

    /// `png` written as HEIC, or nil where this system can't write it.
    static func heic(_ width: Int, _ height: Int) -> Data? {
        guard let image = UIImage(data: png(width, height, .blue))?.cgImage else { return nil }
        let out = NSMutableData()
        guard let writer = CGImageDestinationCreateWithData(out, UTType.heic.identifier as CFString, 1, nil) else { return nil }
        CGImageDestinationAddImage(writer, image, nil)
        return CGImageDestinationFinalize(writer) ? out as Data : nil
    }

    /// The pixel size of encoded image bytes.
    static func size(_ data: Data) -> (width: Int, height: Int)? {
        guard let reader = CGImageSourceCreateWithData(data as CFData, nil),
              let properties = CGImageSourceCopyPropertiesAtIndex(reader, 0, nil) as? [CFString: Any],
              let width = properties[kCGImagePropertyPixelWidth] as? Int,
              let height = properties[kCGImagePropertyPixelHeight] as? Int
        else { return nil }
        return (width, height)
    }
}

@Suite("R-IMG-8, R-IMG-14: normalizing an image")
struct ImageNormalizerTests {
    @Test("R-IMG-8: an image over 2000 px is scaled to 2000 px on its longest side, keeping its shape, and written as a PNG when that fits")
    func scalesToTwoThousand() throws {
        let prepared = try #require(ImageNormalizer.normalize(TestImages.png(3000, 1500)))
        #expect(prepared.mediaType == "image/png")
        let size = try #require(TestImages.size(prepared.data))
        #expect(size.width == 2000)
        #expect(size.height == 1000)
    }

    @Test("R-IMG-8: a small image is not scaled up")
    func smallStaysSmall() throws {
        let prepared = try #require(ImageNormalizer.normalize(TestImages.png(100, 50)))
        let size = try #require(TestImages.size(prepared.data))
        #expect(size.width == 100)
        #expect(size.height == 50)
    }

    @Test("R-IMG-8: a PNG over the limit is written as a JPEG that fits")
    func jpegWhenPngIsTooBig() throws {
        let noise = TestImages.noise(400)
        #expect(noise.count > 300_000, "random pixels don't compress")
        let prepared = try #require(ImageNormalizer.normalize(noise, maxBytes: 300_000))
        #expect(prepared.mediaType == "image/jpeg")
        #expect(prepared.data.count <= 300_000)
    }

    @Test("R-IMG-8: an image that can't be made to fit is refused")
    func refusedWhenNothingFits() {
        #expect(ImageNormalizer.normalize(TestImages.noise(400), maxBytes: 1_000) == nil)
    }

    @Test("R-IMG-8: the limit is the host's 2.75 MiB and the side 2000 px")
    func limits() {
        #expect(ImageNormalizer.maxBytes == 2_883_584)
        #expect(ImageNormalizer.maxSide == 2000)
    }

    @Test("R-IMG-14: a HEIC photo becomes a JPEG or a PNG, never HEIC")
    func heicIsConverted() throws {
        // Some systems can't write HEIC to make the test's photo with.
        guard let heic = TestImages.heic(800, 600) else { return }
        let prepared = try #require(ImageNormalizer.normalize(heic))
        #expect(["image/png", "image/jpeg"].contains(prepared.mediaType))
        let size = try #require(TestImages.size(prepared.data))
        #expect(size.width == 800)
        #expect(size.height == 600)
    }

    @Test("R-IMG-2: the image is named by the sha256 of its bytes, lowercase hex")
    func named() throws {
        #expect(ImageNormalizer.hex(Data("abc".utf8)) == "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        let prepared = try #require(ImageNormalizer.normalize(TestImages.png(10, 10)))
        #expect(prepared.sha256 == ImageNormalizer.hex(prepared.data))
        #expect(prepared.sha256.count == 64)
    }

    @Test("R-IMG-14: bytes that aren't an image are nothing")
    func notAnImage() {
        #expect(ImageNormalizer.normalize(Data("hello".utf8)) == nil)
        #expect(ImageNormalizer.normalize(Data()) == nil)
    }
}

@MainActor
@Suite("R-IMG-14: the composer's images")
struct ComposerImagesTests {
    @Test("R-IMG-14: an image pasted is attached, normalized, and the composer is told")
    func addFromPaste() async throws {
        let images = ComposerImages()
        let told = Counter()
        images.changed = { told.bump() }
        let added = await images.add([TestImages.png(3000, 1000)])
        #expect(added == ComposerImages.Added(added: 1, overCap: 0, unreadable: 0))
        #expect(added.problem == nil)
        #expect(images.images.count == 1)
        let size = try #require(TestImages.size(images.images[0].data))
        #expect(size.width == 2000)
        #expect(told.count == 1)
    }

    @Test("R-IMG-14: several photos from the picker are attached in the order chosen")
    func addFromPicker() async {
        let images = ComposerImages()
        let added = await images.add([TestImages.png(10, 10, .red), TestImages.png(10, 10, .green), TestImages.png(10, 10, .blue)])
        #expect(added.added == 3)
        #expect(images.images.count == 3)
        #expect(Set(images.images.map { $0.sha256 }).count == 3, "three different pictures")
    }

    @Test("R-IMG-14: a removed image goes, the others stay")
    func remove() async {
        let images = ComposerImages()
        await images.add([TestImages.png(10, 10, .red), TestImages.png(10, 10, .green)])
        let first = images.images[0]
        let second = images.images[1]
        images.remove(first.id)
        #expect(images.images == [second])
        images.remove(first.id)
        #expect(images.images == [second], "removed twice is nothing")
    }

    @Test("R-IMG-14: at most 20 images; the rest are left out, and the person is told")
    func cap() async {
        let images = ComposerImages()
        let added = await images.add(Array(repeating: TestImages.png(10, 10), count: 25))
        #expect(images.images.count == 20)
        #expect(added.added == 20)
        #expect(added.overCap == 5)
        #expect(added.problem == "A message takes at most 20 images.")
        #expect(images.room == 0)
        let more = await images.add([TestImages.png(10, 10)])
        #expect(more.overCap == 1)
        #expect(images.images.count == 20)
        // Removing one makes room again.
        images.remove(images.images[0].id)
        #expect(images.room == 1)
    }

    @Test("R-IMG-14: bytes the system can't read are left out, and the person is told")
    func unreadable() async {
        let images = ComposerImages()
        let added = await images.add([Data("nope".utf8), TestImages.png(10, 10)])
        #expect(added.added == 1)
        #expect(added.unreadable == 1)
        #expect(added.problem == "One image couldn't be read.")
        #expect(images.images.count == 1)
    }

    @Test("R-IMG-2: the message names its images by number, tokens in front of the text")
    func message() async {
        let images = ComposerImages()
        await images.add([TestImages.png(10, 10, .red), TestImages.png(10, 10, .green)])
        let message = images.message("What is this?")
        #expect(message.text == "[Image #1] [Image #2]\nWhat is this?")
        #expect(message.images.map { $0.number } == [1, 2])
        #expect(message.images.map { $0.sha256 } == images.images.map { $0.sha256 })
        #expect(message.images.map { $0.mediaType } == ["image/png", "image/png"])
        #expect(images.message("  \n").text == "[Image #1] [Image #2]", "images alone are a message")
        let plain = ComposerImages.message("hi", images: [])
        #expect(plain.text == "hi")
        #expect(plain.images.isEmpty)
    }

    @Test("R-IMG-14: the composer state: images alone can be sent, not while they go, not to an absent host")
    func state() {
        let alone = ComposerState(text: "", running: false, stopping: false, status: .live, images: 1)
        #expect(alone.canSend)
        #expect(!ComposerState(text: "", running: false, stopping: false, status: .live).canSend)
        let going = ComposerState(text: "hi", running: false, stopping: false, status: .live, images: 3, sending: 2)
        #expect(!going.canSend)
        #expect(!going.canAttach)
        #expect(going.note == "Sending image 2 of 3…")
        let away = ComposerState(text: "hi", running: false, stopping: false, status: .hostOffline, images: 1)
        #expect(!away.canSend)
        #expect(away.note == "host offline: images send when your laptop is back")
        let told = ComposerState(text: "hi", running: false, stopping: false, status: .live, images: 1, imageNote: "One image couldn't be read.")
        #expect(told.note == "One image couldn't be read.")
        #expect(told.canSend)
    }
}

/// A count a closure can bump.
@MainActor
final class Counter {
    private(set) var count = 0
    func bump() { count += 1 }
}
